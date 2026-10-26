"""Loopback HTTP/WS bridge for explicit owned mock RFB fixtures only.

Run with `python -m integration.distribox.prototype.gateway --help`. Real
Distribox mode is deliberately unavailable until instance/socket binding is
reviewed. Importing the module starts nothing and accesses no credentials.
"""

import argparse
import asyncio
import contextlib
from dataclasses import dataclass, field, replace
import hashlib
import hmac
import json
import logging
import math
import os
from pathlib import Path
import secrets
import signal
import struct
import time
from urllib.parse import urlsplit

import aiohttp
from aiohttp import web

from ..adapter import AccessDenied, DistriboxAdapter, NodeBroker, StreamSession
from .mock_backend import MockBackend
from .workers import MAX_PACKET, Worker, worker_codec_args


LOG = logging.getLogger(__name__)
MAX_BODY = 4096
MAX_WIDTH = 1920
MAX_HEIGHT = 1080
COOKIE = "gliss_stream"
ROOT = Path(__file__).resolve().parents[3]
LOCAL = Path(__file__).resolve().parent


def parse_json(text):
    def pairs(entries):
        result = {}
        for key, value in entries:
            if key in result:
                raise ValueError("duplicate JSON field")
            result[key] = value
        return result

    def constant(_value):
        raise ValueError("nonfinite JSON number")

    result = json.loads(text, object_pairs_hook=pairs, parse_constant=constant)
    if not isinstance(result, dict):
        raise ValueError("expected JSON object")
    return result


async def json_body(request):
    if request.content_type != "application/json":
        raise web.HTTPUnsupportedMediaType(text="JSON required")
    chunks = bytearray()
    async with asyncio.timeout(2):
        async for chunk in request.content.iter_chunked(1024):
            chunks.extend(chunk)
            if len(chunks) > MAX_BODY:
                raise web.HTTPRequestEntityTooLarge(max_size=MAX_BODY, actual_size=len(chunks))
    try:
        return parse_json(chunks.decode("utf-8"))
    except (ValueError, UnicodeError, RecursionError) as exc:
        raise web.HTTPBadRequest(text="Invalid JSON") from exc


def _unsigned(value, maximum=0xFFFFFFFF):
    return type(value) is int and 0 <= value <= maximum


def validate_command(text, capabilities):
    if len(text.encode("utf-8")) > MAX_BODY:
        raise AccessDenied("command too large")
    try:
        value = parse_json(text)
    except (ValueError, RecursionError) as exc:
        raise AccessDenied("invalid command JSON") from exc
    kind = value.get("type")
    if kind == "ack" and set(value) == {"type", "sequence"} and _unsigned(value["sequence"]):
        return value
    if kind == "resync" and set(value) == {"type"}:
        return value
    if kind == "ping" and set(value) <= {"type", "id"} and _unsigned(value.get("id", 0)):
        return value
    if "input" not in capabilities:
        raise AccessDenied("input capability unavailable")
    if kind == "key" and set(value) <= {"type", "keysym", "down", "id"}:
        if (set(value) >= {"type", "keysym", "down"} and _unsigned(value["keysym"]) and
                type(value["down"]) is bool and _unsigned(value.get("id", 0))):
            return value
    if kind == "pointer" and set(value) <= {"type", "x", "y", "buttons", "id"}:
        if (set(value) >= {"type", "x", "y", "buttons"} and
                all(_unsigned(value[key], 65535) for key in ("x", "y")) and
                _unsigned(value["buttons"], 255) and _unsigned(value.get("id", 0))):
            return value
    raise AccessDenied("command unavailable or malformed")


def validate_packet(packet, codec="deflate"):
    allowed = (b"GLS1", b"GLB1") if codec == "deflate" else (b"GLH4",) if codec == "hybrid-continuity" else ()
    minimum = 64 if codec == "hybrid-continuity" else 24
    if not minimum <= len(packet) <= MAX_PACKET or packet[:4] not in allowed:
        raise AccessDenied("invalid native display packet")
    width, height = struct.unpack_from("<HH", packet, 12)
    if not 1 <= width <= MAX_WIDTH or not 1 <= height <= MAX_HEIGHT:
        raise AccessDenied("prototype desktop dimensions exceeded")
    if codec == "hybrid-continuity":
        grid, flags, masks, reserved = struct.unpack_from("<HHHH", packet, 16)
        base_length, video_length = struct.unpack_from("<II", packet, 32)
        if (grid != 32 or flags & ~7 or reserved or any(packet[48:64]) or
                masks > math.ceil(width / 32) * math.ceil(height / 32) or base_length < 24 or
                64 + masks * 2 + base_length + video_length != len(packet)):
            raise AccessDenied("invalid bounded GLH4 wrapper")
        # Nested wire/bitstream/reference validation belongs to the reused pure
        # browser decoder; this trusted relay enforces outer size/geometry only.


def validate_native_hello(value, codec, fps=30):
    protocol = "GLS1" if codec == "deflate" else "GLH4" if codec == "hybrid-continuity" else None
    if (value.get("type") != "hello" or value.get("codec") != codec or value.get("protocol") != protocol or
            value.get("source") != "rfb" or type(value.get("fps_limit")) is not int or value["fps_limit"] != fps):
        raise AccessDenied("native worker hello differs from trusted preset")
    if codec == "hybrid-continuity":
        expected = {"base_mode": "global-cached", "video_profile": "camera-scene-off",
                    "video_usage": "camera-video-real-time", "scene_change_detection": False,
                    "mask_grid": 32, "base_tile": 64}
        actual = value.get("hybrid_config")
        if not isinstance(actual, dict) or set(actual) != set(expected) or any(
                type(actual[key]) is not type(item) or actual[key] != item for key, item in expected.items()):
            raise AccessDenied("native Hybrid profile differs from trusted preset")
    elif "hybrid_config" in value:
        raise AccessDenied("unexpected native Hybrid profile")


@dataclass(frozen=True)
class Config:
    worker_binary: Path
    origin: str
    session_ttl: float = 300
    ready_ttl: float = 10
    reauthorize_interval: float = 5
    start_timeout: float = 8
    test_control: str | None = field(default=None, repr=False)
    worker_codec: str = "deflate"

    def validate(self):
        worker_codec_args(self.worker_codec)
        binary = Path(self.worker_binary)
        if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("supply an explicit absolute executable Gliss binary")
        origin = urlsplit(self.origin)
        if (origin.scheme != "http" or origin.hostname != "127.0.0.1" or
                origin.port is None or origin.username or origin.password or origin.path or
                origin.query or origin.fragment or self.origin != f"http://127.0.0.1:{origin.port}"):
            raise ValueError("prototype requires an exact loopback HTTP origin")
        for value, maximum in [(self.session_ttl, 300), (self.ready_ttl, 10),
                               (self.reauthorize_interval, 5), (self.start_timeout, 10)]:
            if not math.isfinite(value) or not 0 < value <= maximum:
                raise ValueError("invalid bounded prototype deadline")
        if self.test_control is not None and len(self.test_control) < 32:
            raise ValueError("test control token must contain at least 32 characters")


@dataclass
class Session:
    sid: str
    node: str
    cookie_hash: bytes = field(repr=False)
    scoped: StreamSession
    worker: Worker
    deadline: float
    state: str = "starting"
    ready_until: float = 0
    browser: web.WebSocketResponse | None = field(default=None, repr=False)
    media: asyncio.Task | None = field(default=None, repr=False)
    monitor: asyncio.Task | None = field(default=None, repr=False)
    startup: asyncio.Task | None = field(default=None, repr=False)
    cleanup: asyncio.Task | None = field(default=None, repr=False)

    @property
    def prefix(self):
        return f"/gliss/nodes/{self.node}/s/{self.sid}/"


class Gateway:
    def __init__(self, config, backend):
        config.validate()
        if type(backend) is not MockBackend:
            raise ValueError("reviewed real instance/socket binding is unavailable; owned mock only")
        self.config = config
        self.backend = backend
        self.sessions = {}
        self.lock = asyncio.Lock()
        self.launch = asyncio.Semaphore(1)
        self.tasks = set()
        self.stopping = False
        self.peak_active = 0
        self.peak_launching = 0
        self.launching = 0
        self.attempts = []
        self.nodes = {}
        for node in backend.nodes:
            secret = secrets.token_urlsafe(32)
            broker = NodeBroker(
                exchange_path=f"/gliss/nodes/{node}/session", allowed_origin=config.origin,
                control_secret=secret, max_pending=4,
                resolve_target=lambda vm, node=node: backend.resolve(vm, node),
                reauthorize=lambda grant, node=node: backend.reauthorize(grant, node))
            self.nodes[node] = (broker, secret)
        self.adapter = DistriboxAdapter(
            allowed_origin=config.origin, authenticate=backend.authenticate,
            route_node=backend.route_node, nodes=self.nodes)

    def tracked(self, coroutine):
        task = asyncio.create_task(coroutine)
        self.tasks.add(task)

        def finished(task):
            self.tasks.discard(task)
            if not task.cancelled():
                error = task.exception()
                if error is not None and not isinstance(error, (AccessDenied, TimeoutError)):
                    LOG.warning("prototype lifecycle task failed: %s", type(error).__name__)
        task.add_done_callback(finished)
        return task

    def origin(self, request):
        if request.headers.getall("Origin", []) != [self.config.origin]:
            raise AccessDenied("Origin rejected")

    def rate(self):
        now = asyncio.get_running_loop().time()
        self.attempts = [entry for entry in self.attempts if now - entry < 60]
        if len(self.attempts) >= 30:
            raise web.HTTPTooManyRequests(text="Session attempt limit")
        self.attempts.append(now)

    async def offer(self, request):
        self.origin(request)
        self.rate()
        headers = request.headers.getall("Authorization", [])
        if len(headers) > 1:
            raise AccessDenied("ambiguous authentication")
        bearer = None
        if headers:
            if not headers[0].startswith("Bearer ") or not 1 <= len(headers[0][7:]) <= 1024:
                raise AccessDenied("invalid bearer header")
            bearer = headers[0][7:]
        offer = self.adapter.create_session(await json_body(request), bearer=bearer, origin=self.config.origin)
        return web.json_response(offer.public_dict())

    async def exchange(self, request):
        self.origin(request)
        self.rate()
        node = request.match_info["node"]
        if node not in self.nodes:
            raise AccessDenied("node unavailable")
        body = await json_body(request)
        if set(body) != {"ticket"}:
            raise AccessDenied("ticket only")
        broker = self.nodes[node][0]
        scoped = broker.redeem(body["ticket"], origin=self.config.origin)
        scoped = replace(scoped, expires_at=min(scoped.expires_at, time.time() + self.config.session_ttl))
        sid, cookie = secrets.token_urlsafe(24), secrets.token_urlsafe(32)
        record = Session(sid, node, hashlib.sha256(cookie.encode()).digest(), scoped,
                         Worker(self.config.worker_binary, self.config.origin, scoped, codec=self.config.worker_codec),
                         asyncio.get_running_loop().time() + scoped.expires_at - time.time())
        async with self.lock:
            if self.stopping or len(self.sessions) >= 2:
                raise web.HTTPServiceUnavailable(text="Session capacity reached")
            self.sessions[sid] = record
            self.peak_active = max(self.peak_active, len(self.sessions))
        try:
            record.startup = self.tracked(self.start_worker(record, broker))
            await asyncio.shield(record.startup)
            if not broker.still_authorized(scoped) or record.deadline <= asyncio.get_running_loop().time():
                raise AccessDenied("authorization changed during startup")
            async with self.lock:
                if record.state != "starting" or self.stopping:
                    raise AccessDenied("session already closed")
                record.state = "ready"
                record.ready_until = min(record.deadline, asyncio.get_running_loop().time() + self.config.ready_ttl)
            record.monitor = self.tracked(self.watch(record))
            result = web.json_response({"view_path": record.prefix, "ws_path": record.prefix + "ws",
                                        "expires_at": scoped.expires_at,
                                        "capabilities": sorted(scoped.grant.capabilities)})
            result.set_cookie(COOKIE, cookie, path=record.prefix, httponly=True, samesite="Strict",
                              max_age=max(1, math.ceil(scoped.expires_at - time.time())))
            return result
        except BaseException:
            await self.close(record)
            raise

    async def start_worker(self, record, broker):
        async with asyncio.timeout(self.config.start_timeout):
            async with self.launch:
                self.launching += 1
                self.peak_launching = max(self.peak_launching, self.launching)
                try:
                    await record.worker.start(broker.still_authorized, self.backend.before_connect)
                finally:
                    self.launching -= 1

    def authenticated(self, request):
        record = self.sessions.get(request.match_info["sid"])
        if record is None or record.node != request.match_info["node"] or record.state == "closed":
            raise AccessDenied("session unavailable")
        headers = request.headers.getall("Cookie", [])
        if len(headers) != 1:
            raise AccessDenied("one stream cookie header required")
        fragments = headers[0].split(";")
        if sum(part.strip().startswith(COOKIE + "=") for part in fragments) != 1:
            raise AccessDenied("stream cookie required")
        cookie = request.cookies.get(COOKIE, "")
        if not 1 <= len(cookie) <= 128 or not hmac.compare_digest(
                hashlib.sha256(cookie.encode()).digest(), record.cookie_hash):
            raise AccessDenied("stream cookie rejected")
        if record.deadline <= asyncio.get_running_loop().time():
            raise AccessDenied("stream lifetime elapsed")
        return record

    async def viewer(self, request):
        self.authenticated(request)
        return web.FileResponse(LOCAL / "viewer.html", headers={"Content-Type": "text/html"})

    async def delete(self, request):
        self.origin(request)
        record = self.authenticated(request)
        await self.close(record)
        response = web.Response(status=204)
        response.del_cookie(COOKIE, path=record.prefix)
        return response

    async def socket(self, request):
        self.origin(request)
        record = self.authenticated(request)
        broker = self.nodes[record.node][0]
        if not broker.still_authorized(record.scoped):
            await self.close(record)
            raise AccessDenied("stream authorization changed")
        async with self.lock:
            if record.state != "ready" or record.ready_until <= asyncio.get_running_loop().time():
                raise AccessDenied("one stream claim required")
            record.state = "claimed"
        browser = web.WebSocketResponse(max_msg_size=MAX_BODY, compress=False, heartbeat=20,
                                        timeout=0.5, writer_limit=65536)
        record.browser = browser
        try:
            native = await record.worker.open_ws()
            await browser.prepare(request)
            await browser.send_json({"type": "capabilities", "capabilities": sorted(record.scoped.grant.capabilities),
                                     "clipboard": False, "resize": False, "backend": "owned-mock"})
            record.media = self.tracked(self.media(record, native))
            bucket_start, count = time.monotonic(), 0
            async for message in browser:
                if record.state == "closed" or record.deadline <= asyncio.get_running_loop().time():
                    break
                if message.type != aiohttp.WSMsgType.TEXT:
                    raise AccessDenied("browser text controls only")
                value = validate_command(message.data, record.scoped.grant.capabilities)
                if time.monotonic() - bucket_start >= 1:
                    bucket_start, count = time.monotonic(), 0
                count += 1
                if count > 240:
                    raise AccessDenied("command rate exceeded")
                await asyncio.wait_for(native.send_str(json.dumps(value, separators=(",", ":"))), 1)
        except AccessDenied:
            if not browser.prepared:
                raise web.HTTPForbidden(text="Stream claim rejected") from None
        except (aiohttp.ClientError, TimeoutError, ConnectionError):
            if not browser.prepared:
                raise web.HTTPServiceUnavailable(text="Native stream unavailable") from None
        finally:
            await self.close(record)
        return browser

    async def media(self, record, native):
        hello_received = False
        try:
            async for message in native:
                if record.state == "closed":
                    break
                if message.type == aiohttp.WSMsgType.BINARY:
                    if not hello_received:
                        raise AccessDenied("native media preceded negotiated hello")
                    validate_packet(message.data, record.worker.codec)
                    await asyncio.wait_for(record.browser.send_bytes(message.data), 2)
                elif message.type == aiohttp.WSMsgType.TEXT:
                    if len(message.data.encode()) > MAX_PACKET:
                        raise AccessDenied("native event too large")
                    value = parse_json(message.data)
                    if value.get("type") == "hello":
                        if hello_received:
                            raise AccessDenied("duplicate native hello")
                        validate_native_hello(value, record.worker.codec, record.worker.fps)
                        hello_received = True
                    if value.get("type") == "clipboard":
                        continue
                    await asyncio.wait_for(record.browser.send_str(message.data), 2)
                else:
                    break
        except (ValueError, AccessDenied, aiohttp.ClientError, TimeoutError, ConnectionError):
            pass
        finally:
            # The request handler owns browser receive; close() wakes it.
            self.tracked(self.close(record))

    async def watch(self, record):
        next_check = asyncio.get_running_loop().time() + self.config.reauthorize_interval
        try:
            while record.state != "closed":
                now = asyncio.get_running_loop().time()
                if (now >= record.deadline or record.worker.process.returncode is not None or
                        (record.state == "ready" and now >= record.ready_until) or
                        (record.worker.pin.claimed and record.worker.pin.done.is_set())):
                    break
                if now >= next_check:
                    # This backend is explicitly in-memory and nonblocking.
                    # A real synchronous DB/libvirt backend is not accepted.
                    if not self.nodes[record.node][0].still_authorized(record.scoped):
                        break
                    next_check = now + self.config.reauthorize_interval
                await asyncio.sleep(min(0.05, max(0, record.deadline - now)))
        except asyncio.CancelledError:
            return
        await self.close(record)

    async def close(self, record):
        async with self.lock:
            if record.cleanup is None:
                record.state = "closed"
                record.cleanup = self.tracked(self.cleanup(record))
            cleanup = record.cleanup
        await asyncio.shield(cleanup)

    async def cleanup(self, record):
        for task in (record.startup, record.monitor, record.media):
            if task is not None and task is not asyncio.current_task():
                task.cancel()
        # Finish a canceled launch before closing its child/pin. Otherwise a
        # launch completing after cleanup could create an orphaned worker.
        if record.startup is not None:
            await asyncio.gather(record.startup, return_exceptions=True)
        if record.browser is not None:
            with contextlib.suppress(Exception):
                await asyncio.wait_for(record.browser.close(code=1008, message=b"Stream ended"), 0.75)
        try:
            await record.worker.close()
        finally:
            async with self.lock:
                self.sessions.pop(record.sid, None)

    async def shutdown(self, _app):
        self.stopping = True
        await asyncio.gather(*(self.close(record) for record in list(self.sessions.values())), return_exceptions=True)
        while self.tasks:
            await asyncio.gather(*list(self.tasks), return_exceptions=True)

    def control(self, request):
        value = request.headers.get("X-Gliss-Test-Control", "")
        if self.config.test_control is None or not hmac.compare_digest(value, self.config.test_control):
            raise AccessDenied("test control unavailable")

    async def test_state(self, request):
        self.control(request)
        if request.method == "POST":
            try:
                self.backend.mutate(await json_body(request))
            except (ValueError, KeyError) as exc:
                raise web.HTTPBadRequest(text="Invalid mock mutation") from exc
        return web.json_response({**self.backend.state(), "active": len(self.sessions),
                                  "launching": self.launching, "peak_active": self.peak_active,
                                  "peak_launching": self.peak_launching,
                                  "workers": [record.worker.diagnostics() for record in self.sessions.values()]})


def create_app(config, backend):
    gateway = Gateway(config, backend)

    @web.middleware
    async def boundaries(request, handler):
        if request.headers.getall("Host", []) != [urlsplit(config.origin).netloc]:
            raise web.HTTPForbidden(text="Host rejected")
        if request.query_string:
            raise web.HTTPBadRequest(text="Query parameters unavailable")
        try:
            response = await handler(request)
        except AccessDenied:
            response = web.Response(status=403, text="Stream access rejected")
        except web.HTTPException as exc:
            response = web.Response(status=exc.status, reason=exc.reason, body=exc.body, headers=exc.headers)
        except (TimeoutError, aiohttp.ClientError, OSError):
            response = web.Response(status=503, text="Stream unavailable")
        if not response.prepared:
            response.headers.update({"Cache-Control": "no-store", "X-Content-Type-Options": "nosniff",
                                     "Referrer-Policy": "no-referrer",
                                     "Content-Security-Policy": "default-src 'self'; script-src 'self'; connect-src 'self'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'"})
        return response

    app = web.Application(middlewares=[boundaries], client_max_size=MAX_BODY)
    app[web.AppKey("gateway", Gateway)] = gateway
    async def health(_request):
        return web.json_response({"status": "ok", "mode": "owned-mock"})
    app.router.add_get("/health", health)
    app.router.add_post("/gliss/session", gateway.offer)
    app.router.add_post("/gliss/nodes/{node}/session", gateway.exchange)
    app.router.add_get("/gliss/nodes/{node}/s/{sid}/", gateway.viewer)
    app.router.add_delete("/gliss/nodes/{node}/s/{sid}/", gateway.delete)
    app.router.add_get("/gliss/nodes/{node}/s/{sid}/ws", gateway.socket)
    for name, path in {"decoder.js": ROOT / "web" / "decoder.js",
                       "hybrid-continuity-decoder.js": ROOT / "web" / "hybrid-continuity-decoder.js",
                       "video-decoder.js": ROOT / "web" / "video-decoder.js",
                       "viewer-media.js": LOCAL / "viewer-media.js", "viewer.js": LOCAL / "viewer.js"}.items():
        async def asset(_request, path=path):
            return web.FileResponse(path, headers={"Content-Type": "text/javascript"})
        app.router.add_get(f"/gliss/assets/{name}", asset)
    if config.test_control is not None:
        app.router.add_get("/__test__/state", gateway.test_state)
        app.router.add_post("/__test__/state", gateway.test_state)
    app.on_shutdown.append(gateway.shutdown)
    return app, gateway


async def serve(arguments):
    if arguments.backend != "owned-mock":
        raise ValueError("real Distribox deployment requires reviewed instance/socket binding; unavailable")
    with Path(arguments.fixtures).open("rb") as stream:
        encoded = stream.read(65537)
    if len(encoded) > 65536:
        raise ValueError("mock configuration exceeds limit")
    backend = MockBackend(parse_json(encoded.decode("utf-8")))
    config = Config(Path(arguments.worker_binary), f"http://127.0.0.1:{arguments.port}",
                    session_ttl=arguments.session_ttl, ready_ttl=arguments.ready_ttl,
                    reauthorize_interval=arguments.reauthorize_interval,
                    test_control=os.environ.get("GLISS_PROTOTYPE_TEST_CONTROL"), worker_codec=arguments.worker_codec)
    app, _gateway = create_app(config, backend)
    runner = web.AppRunner(app, handler_cancellation=True, access_log=None)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", arguments.port)
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for event in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(event, stop.set)
    try:
        await site.start()
        print(json.dumps({"kind": "ready", "origin": config.origin, "backend": "owned-mock"}), flush=True)
        await stop.wait()
    finally:
        await runner.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker-binary", required=True, help="explicit absolute Gliss executable")
    parser.add_argument("--fixtures", required=True, help="owned mock JSON configuration")
    parser.add_argument("--worker-codec", choices=["deflate", "hybrid-continuity"], default="deflate",
                        help="trusted fixed native codec preset; browser requests cannot select it")
    parser.add_argument("--backend", choices=["owned-mock", "distribox"], default="owned-mock")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--session-ttl", type=float, default=300)
    parser.add_argument("--ready-ttl", type=float, default=10)
    parser.add_argument("--reauthorize-interval", type=float, default=5)
    arguments = parser.parse_args()
    if not 1 <= arguments.port <= 65535:
        parser.error("port must be 1..65535")
    logging.basicConfig(level=logging.WARNING)
    try:
        asyncio.run(serve(arguments))
    except (ValueError, AccessDenied) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
