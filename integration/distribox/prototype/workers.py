"""Private fixed-target native workers and one-use preconnected RFB relays."""

import asyncio
import contextlib
import math
import os
from pathlib import Path
import secrets
import signal
import socket
import time

import aiohttp

from ..adapter import AccessDenied


MAX_PACKET = 12 * 1024 * 1024


def worker_codec_args(codec):
    """Trusted operator presets only; browser/session bodies never enter here."""
    if codec == "deflate":
        return ["--codec", "deflate"]
    if codec == "hybrid-continuity":
        return ["--codec", "hybrid-continuity", "--hybrid-base", "global-cached",
                "--hybrid-video-profile", "camera-scene-off", "--hybrid-mask-grid", "32",
                "--tile-size", "64", "--h264-bitrate", "3000000"]
    raise ValueError("unsupported trusted worker codec preset")


async def _finish_writer(writer):
    writer.close()
    with contextlib.suppress(Exception):
        await asyncio.wait_for(writer.wait_closed(), 0.5)


class PinnedRfb:
    """One approved established connection, one accepted worker, no reconnect."""

    def __init__(self):
        self.server = None
        self.upstream = None
        self.downstream = None
        self.task = None
        self.claimed = False
        self.closed = False
        self.done = asyncio.Event()
        self.bytes_up = 0
        self.bytes_down = 0

    @classmethod
    async def open(cls, session, still_authorized, before_connect):
        pin = cls()
        if not still_authorized(session):
            raise AccessDenied("instance authorization changed")
        await before_connect(session.grant.vm_id)
        try:
            pin.upstream = await asyncio.wait_for(
                asyncio.open_connection(session.target.host, session.target.port, limit=65536), 2)
            # Nothing can reach the worker/browser before this second check.
            if not still_authorized(session):
                raise AccessDenied("instance changed while connecting")
            pin.server = await asyncio.start_server(pin._accept, "127.0.0.1", 0, limit=65536)
            return pin
        except BaseException:
            await pin.close()
            raise

    @property
    def address(self):
        return f"127.0.0.1:{self.server.sockets[0].getsockname()[1]}"

    async def _accept(self, reader, writer):
        if self.claimed or self.closed:
            await _finish_writer(writer)
            return
        self.claimed = True
        self.downstream = (reader, writer)
        self.server.close()
        self.task = asyncio.current_task()

        async def pump(source, destination, direction):
            while chunk := await source.read(65536):
                destination.write(chunk)
                await asyncio.wait_for(destination.drain(), 2)
                if direction == "up":
                    self.bytes_up += len(chunk)
                else:
                    self.bytes_down += len(chunk)

        tasks = [asyncio.create_task(pump(reader, self.upstream[1], "up")),
                 asyncio.create_task(pump(self.upstream[0], writer, "down"))]
        try:
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            await _finish_writer(writer)
            await _finish_writer(self.upstream[1])
            self.done.set()

    async def close(self):
        self.closed = True
        if self.server is not None:
            self.server.close()
            await self.server.wait_closed()
        if self.task is not None and self.task is not asyncio.current_task():
            self.task.cancel()
            await asyncio.gather(self.task, return_exceptions=True)
        for streams in (self.downstream, self.upstream):
            if streams is not None:
                await _finish_writer(streams[1])
        self.done.set()


class Worker:
    def __init__(self, binary, origin, session, *, fps=30, codec="deflate"):
        self.binary = Path(binary)
        self.origin = origin
        self.session = session
        self.fps = fps
        self.codec = codec
        self.codec_args = worker_codec_args(codec)
        self.token = secrets.token_urlsafe(32)
        self.cookie = None
        self.pin = None
        self.process = None
        self.http = None
        self.ws = None
        self.port = None
        self.log_tasks = []
        self.log_tail = bytearray()
        self.cleanup = None

    async def _drain(self, pipe):
        while chunk := await pipe.read(4096):
            self.log_tail.extend(chunk)
            del self.log_tail[:-8192]

    async def start(self, still_authorized, before_connect):
        self.pin = await PinnedRfb.open(self.session, still_authorized, before_connect)
        self.http = aiohttp.ClientSession(
            cookie_jar=aiohttp.DummyCookieJar(), trust_env=False,
            timeout=aiohttp.ClientTimeout(total=1, connect=0.5),
            connector=aiohttp.TCPConnector(limit=2), read_bufsize=65536)
        # Native listener FD inheritance is unavailable. Fresh-token login and
        # child liveness authenticate readiness after bounded bind retries.
        for _ in range(3):
            probe = socket.socket()
            try:
                probe.bind(("127.0.0.1", 0))
                self.port = probe.getsockname()[1]
            finally:
                probe.close()
            remaining = min(300, math.ceil(self.session.expires_at - time.time()))
            if remaining <= 0:
                raise AccessDenied("stream lifetime elapsed")
            environment = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL") if key in os.environ}
            environment.update(GLISS_TOKEN=self.token, RUST_LOG="warn")
            creation = asyncio.create_task(asyncio.create_subprocess_exec(
                str(self.binary), "--bind", f"127.0.0.1:{self.port}",
                "--vnc", self.pin.address, "--origin", self.origin,
                "--max-clients", "1", "--session-ttl", str(remaining),
                *self.codec_args, "--fps", str(self.fps),
                env=environment, stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE))
            try:
                self.process = await asyncio.shield(creation)
            except asyncio.CancelledError:
                # Keep ownership of a child created while the caller canceled.
                self.process = await creation
                raise
            self.log_tasks.extend([asyncio.create_task(self._drain(self.process.stdout)),
                                   asyncio.create_task(self._drain(self.process.stderr))])
            deadline = asyncio.get_running_loop().time() + 2
            while asyncio.get_running_loop().time() < deadline:
                if self.process.returncode is not None:
                    break
                try:
                    async with self.http.get(self.url + "/health", allow_redirects=False) as response:
                        healthy = response.status == 200
                    if healthy:
                        async with self.http.post(
                                self.url + "/api/session", json={"token": self.token},
                                headers={"Origin": self.origin}, allow_redirects=False) as response:
                            cookie = response.cookies.get("gliss_session")
                            if response.status != 200 or cookie is None or self.process.returncode is not None:
                                raise AccessDenied("native worker readiness rejected")
                            self.cookie = cookie.value
                            return
                except (aiohttp.ClientError, TimeoutError):
                    pass
                await asyncio.sleep(0.025)
            await self._stop_process()
        raise AccessDenied("native worker did not become ready")

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}"

    async def open_ws(self):
        if self.ws is not None or not self.cookie or self.process.returncode is not None:
            raise AccessDenied("native stream cannot be reused")
        self.ws = await self.http.ws_connect(
            f"ws://127.0.0.1:{self.port}/ws", origin=self.origin,
            headers={"Cookie": f"gliss_session={self.cookie}"},
            compress=0, max_msg_size=MAX_PACKET, heartbeat=20,
            timeout=aiohttp.ClientWSTimeout(ws_close=0.5))
        return self.ws

    async def _stop_process(self):
        process = self.process
        if process is None:
            return
        if process.returncode is None:
            with contextlib.suppress(ProcessLookupError):
                process.send_signal(signal.SIGINT)
            try:
                await asyncio.wait_for(process.wait(), 2)
            except TimeoutError:
                with contextlib.suppress(ProcessLookupError):
                    process.kill()
                await process.wait()

    async def close(self):
        if self.cleanup is None:
            self.cleanup = asyncio.create_task(self._close())
        await asyncio.shield(self.cleanup)

    async def _close(self):
        try:
            if self.ws is not None:
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(self.ws.close(), 0.75)
            # Native disconnect cleanup releases held keys while this relay is
            # still usable. It never reopens a real target on cleanup failure.
            if self.pin is not None and self.pin.claimed:
                with contextlib.suppress(TimeoutError):
                    await asyncio.wait_for(self.pin.done.wait(), 1.25)
            if self.pin is not None:
                await self.pin.close()
        finally:
            await self._stop_process()
            if self.http is not None:
                await self.http.close()
            for task in self.log_tasks:
                task.cancel()
            await asyncio.gather(*self.log_tasks, return_exceptions=True)
            self.cookie = None
            self.token = ""

    def diagnostics(self):
        result = {"pid": self.process.pid if self.process else None,
                  "running": self.process is not None and self.process.returncode is None,
                  "relay_up_bytes": self.pin.bytes_up if self.pin else 0,
                  "relay_down_bytes": self.pin.bytes_down if self.pin else 0}
        if result["running"]:
            # Only the owned child PID, never host-wide or backend inspection.
            try:
                status = Path(f"/proc/{self.process.pid}/status").read_text()
                for line in status.splitlines():
                    if line.startswith("VmRSS:"):
                        result["rss_kib"] = int(line.split()[1])
                    elif line.startswith("VmHWM:"):
                        result["rss_peak_kib"] = int(line.split()[1])
                stat = Path(f"/proc/{self.process.pid}/stat").read_text()
                fields = stat[stat.rfind(")") + 2:].split()
                result["cpu_seconds"] = (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
            except (OSError, ValueError, IndexError):
                pass
        return result
