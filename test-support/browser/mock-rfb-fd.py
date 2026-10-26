#!/usr/bin/env python3
"""Private socket-pair RFB peer and exact native-child supervisor for tests.

Importing starts no worker or socket. The JSON-lines protocol is private IPC:
ready carries the random worker token; callers must never log that message.
Only this script's connected graphics socket is passed to the native child.
CLI: --binary ABS --lease-ms 1000..60000 --handshake normal|stall-security
     --session-ttl 1..60. JSON stdin: {"id":1,"command":"snapshot"|"stop"}.
Ready/events/replies are JSON stdout. Stop replies after native wait/reap and
reports peer input/release events plus exact child-identity absence.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import select
import selectors
import signal
import socket
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request

WIDTH, HEIGHT = 96, 64
FORMAT = bytes([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
INITIAL = [12, 34, 56, 0]
CHANGED = [31, 67, 129, 0]
MAX_INPUT = 16_384
MAX_RFB_OUTPUT = 131_072
MAX_IPC_OUTPUT = 131_072
MAX_EVENTS = 512


def start_ticks(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[19])


class Fixture:
    def __init__(self, binary: Path, lease_ms: int, handshake: str, session_ttl: int = 60):
        if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("explicit absolute executable binary required")
        if not 1000 <= lease_ms <= 60_000 or handshake not in {"normal", "stall-security"} or not 1 <= session_ttl <= 60:
            raise ValueError("invalid bounded fixture options")
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise ValueError("Linux owned-child pidfd signaling required")
        self.selector = selectors.DefaultSelector()
        self.child = None
        self.pidfd = None
        self.peer = None
        self.stopping = False
        self.worker_exit = False
        self.handshake = handshake
        self.state = "version"
        self.rx = bytearray()
        self.tx = bytearray(b"RFB 003.008\n")
        self.ipc_rx = bytearray()
        self.ipc_tx = bytearray()
        self.output_since = None
        self.events = []
        self.keys = set()
        self.pointer = {"x": 0, "y": 0, "buttons": 0}
        self.color = INITIAL
        self.pending_update = False
        self.dirty = True
        self.counts = {"client_versions": 0, "security_selections": 0, "client_inits": 0,
                       "update_requests": 0, "frames": 0, "keys": 0, "pointers": 0}
        self.rfb_closed = None
        self.errors = []
        self.native_tail = bytearray()
        self.started = time.monotonic()
        self.lifetime = self.started + lease_ms / 1000 + 20
        self.deadline_ms = time.time_ns() // 1_000_000 + lease_ms
        token = base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip("=")
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        self.url = f"http://127.0.0.1:{port}"
        graphics, self.peer = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        self.peer.setblocking(False)
        args = [str(binary), "--bind", f"127.0.0.1:{port}", "--origin", self.url,
                "--rfb-fd", str(graphics.fileno()), "--rfb-deadline-unix-ms", str(self.deadline_ms),
                "--max-clients", "1", "--session-ttl", str(session_ttl), "--codec", "global", "--fps", "30"]
        try:
            self.child = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                          stderr=subprocess.PIPE, close_fds=True,
                                          pass_fds=(graphics.fileno(),),
                                          env={"GLISS_TOKEN": token, "RUST_LOG": "warn", "LANG": "C.UTF-8"})
            # No surviving supervisor copy may keep the graphics client alive.
            graphics.close()
            self.pidfd = os.pidfd_open(self.child.pid, 0)
            self.child_start = start_ticks(self.child.pid)
            os.set_blocking(sys.stdin.fileno(), False)
            os.set_blocking(sys.stdout.fileno(), False)
            self.selector.register(sys.stdin, selectors.EVENT_READ, "ipc_in")
            self.selector.register(self.peer, selectors.EVENT_READ | selectors.EVENT_WRITE, "rfb")
            for stream in (self.child.stdout, self.child.stderr):
                os.set_blocking(stream.fileno(), False)
                self.selector.register(stream, selectors.EVENT_READ, "native_output")
            self._ready()
            self.emit({"type": "ready", "url": self.url, "token": token,
                       "worker_pid": self.child.pid, "worker_start_ticks": self.child_start,
                       "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                       "deadline_unix_ms": self.deadline_ms, "width": WIDTH, "height": HEIGHT,
                       "transport": "one-inherited-unix-stream", "passed_fds": 1,
                       "numeric_vnc_target": False})
        except BaseException:
            graphics.close()
            self.close()
            raise

    def _ready(self):
        deadline = min(time.monotonic() + 6, self.lifetime)
        while time.monotonic() < deadline:
            if self.child.poll() is not None:
                raise RuntimeError("owned native worker exited before HTTP readiness")
            try:
                with urllib.request.urlopen(self.url + "/health", timeout=0.2) as response:
                    if response.status == 200:
                        return
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.02)
        raise RuntimeError("owned native HTTP readiness timeout")

    def emit(self, message):
        encoded = json.dumps(message, separators=(",", ":"), allow_nan=False).encode() + b"\n"
        if len(self.ipc_tx) + len(encoded) > MAX_IPC_OUTPUT:
            raise RuntimeError("private IPC output bound exceeded")
        if not self.ipc_tx:
            self.output_since = time.monotonic()
            self.selector.register(sys.stdout, selectors.EVENT_WRITE, "ipc_out")
        self.ipc_tx.extend(encoded)

    def record(self, value):
        if len(self.events) >= MAX_EVENTS:
            raise RuntimeError("mock input/event bound exceeded")
        event = {"sequence": len(self.events) + 1, "at_ms": (time.monotonic() - self.started) * 1000, **value}
        self.events.append(event)
        self.emit({"type": "event", "event": event})

    def _rfb_interest(self):
        if self.peer is not None:
            self.selector.modify(self.peer, selectors.EVENT_READ | (selectors.EVENT_WRITE if self.tx else 0), "rfb")

    def queue(self, data):
        if len(self.tx) + len(data) > MAX_RFB_OUTPUT:
            raise RuntimeError("mock RFB output bound exceeded")
        self.tx.extend(data)
        self._rfb_interest()

    def update(self):
        if not self.pending_update or not self.dirty:
            return
        pixels = bytes(self.color) * (WIDTH * HEIGHT)
        self.queue(struct.pack("!BBHHHHHi", 0, 0, 1, 0, 0, WIDTH, HEIGHT, 0) + pixels)
        self.pending_update = False
        self.dirty = False
        self.counts["frames"] += 1
        self.record({"kind": "frame", "rgb": self.color[:3]})

    def parse_rfb(self):
        while self.rx:
            if self.state == "version":
                if len(self.rx) < 12:
                    return
                version = bytes(self.rx[:12]); del self.rx[:12]
                if version != b"RFB 003.008\n":
                    raise RuntimeError("unexpected independent client version")
                self.counts["client_versions"] += 1
                self.record({"kind": "client_version", "version": "3.8"})
                if self.handshake == "stall-security":
                    self.state = "stalled"
                else:
                    self.queue(b"\x01\x01")
                    self.state = "security"
            elif self.state == "stalled":
                raise RuntimeError("native client wrote before security offer")
            elif self.state == "security":
                if self.rx[0] != 1:
                    raise RuntimeError("unexpected security selection")
                del self.rx[:1]
                self.counts["security_selections"] += 1
                self.queue(struct.pack("!I", 0))
                self.state = "init"
            elif self.state == "init":
                if self.rx[0] != 1:
                    raise RuntimeError("unexpected ClientInit shared flag")
                del self.rx[:1]
                self.counts["client_inits"] += 1
                name = b"owned-fd-peer"
                self.queue(struct.pack("!HH", WIDTH, HEIGHT) + FORMAT + struct.pack("!I", len(name)) + name)
                self.state = "commands"
                self.record({"kind": "client_init"})
            else:
                kind = self.rx[0]
                lengths = {0: 20, 3: 10, 4: 8, 5: 6}
                if kind == 2:
                    if len(self.rx) < 4:
                        return
                    count = struct.unpack_from("!H", self.rx, 2)[0]
                    if count > 64:
                        raise RuntimeError("excess native encoding count")
                    length = 4 + count * 4
                elif kind == 6:
                    if len(self.rx) < 8:
                        return
                    size = struct.unpack_from("!I", self.rx, 4)[0]
                    if size > 4096:
                        raise RuntimeError("mock clipboard exceeds its test bound")
                    length = 8 + size
                elif kind in lengths:
                    length = lengths[kind]
                else:
                    raise RuntimeError("unsupported native RFB command")
                if len(self.rx) < length:
                    return
                message = bytes(self.rx[:length]); del self.rx[:length]
                if kind == 0:
                    if message[4:] != FORMAT:
                        raise RuntimeError("unexpected native pixel format")
                    self.record({"kind": "pixel_format"})
                elif kind == 2:
                    self.record({"kind": "encodings", "count": (length - 4) // 4})
                elif kind == 3:
                    incremental, x, y, width, height = struct.unpack("!BHHHH", message[1:])
                    if incremental not in (0, 1) or width == 0 or height == 0 or x + width > WIDTH or y + height > HEIGHT:
                        raise RuntimeError("native update rectangle out of fixture bounds")
                    self.counts["update_requests"] += 1
                    self.pending_update = True
                    if not incremental:
                        self.dirty = True
                    self.update()
                elif kind == 4:
                    down, keysym = message[1], struct.unpack_from("!I", message, 4)[0]
                    if down not in (0, 1):
                        raise RuntimeError("invalid native key state")
                    if down:
                        self.keys.add(keysym)
                    else:
                        self.keys.discard(keysym)
                    self.counts["keys"] += 1
                    self.record({"kind": "key", "keysym": keysym, "down": bool(down)})
                    if keysym == 97 and down:
                        self.color = CHANGED
                        self.dirty = True
                        self.update()
                elif kind == 5:
                    buttons, x, y = struct.unpack("!BHH", message[1:])
                    if buttons > 127 or x >= WIDTH or y >= HEIGHT:
                        raise RuntimeError("native pointer outside fixture bounds")
                    self.pointer = {"x": x, "y": y, "buttons": buttons}
                    self.counts["pointers"] += 1
                    self.record({"kind": "pointer", **self.pointer})
                elif kind == 6:
                    self.record({"kind": "clipboard", "bytes": length - 8})

    def peer_closed(self, reason):
        if self.peer is None:
            return
        self.selector.unregister(self.peer)
        self.peer.close()
        self.peer = None
        self.rfb_closed = reason
        if self.rx:
            self.errors.append("native EOF inside an RFB message")
        self.record({"kind": "rfb_closed", "reason": reason})

    def read_rfb(self):
        try:
            data = self.peer.recv(4096)
        except BlockingIOError:
            return
        except ConnectionResetError:
            self.peer_closed("reset-unread-data")
            return
        if not data:
            self.peer_closed("eof")
            return
        if len(self.rx) + len(data) > MAX_INPUT:
            raise RuntimeError("native input buffer bound exceeded")
        self.rx.extend(data)
        self.parse_rfb()

    def identity_absent(self):
        try:
            return start_ticks(self.child.pid) != self.child_start
        except (FileNotFoundError, ProcessLookupError):
            return True

    def snapshot(self):
        return {"counts": self.counts.copy(), "state": self.state, "events": self.events.copy(),
                "held_keys": sorted(self.keys), "pointer": self.pointer.copy(), "rfb_closed": self.rfb_closed,
                "errors": self.errors.copy(), "worker_exit_code": self.child.poll(),
                "worker_identity_absent": self.identity_absent(),
                "parent_graphics_copy_closed": True, "pending_native_bytes": len(self.rx)}

    def observe_exit(self):
        code = self.child.poll()
        if code is not None and not self.worker_exit:
            self.worker_exit = True
            self.record({"kind": "worker_exit", "code": code, "identity_absent": self.identity_absent()})

    def signal_child(self, signum):
        if self.child.poll() is None:
            # pidfd stays bound to our Popen child even if a numeric PID is reused.
            try:
                signal.pidfd_send_signal(self.pidfd, signum)
            except ProcessLookupError:
                pass  # Already exited; Popen.wait still reaps that exact child.

    def stop_child(self):
        if self.child is None:
            return
        if self.child.poll() is None:
            if self.pidfd is None:
                # Only startup failure before pidfd acquisition uses Popen's
                # unreaped exact-child handle; no independent wait thread exists.
                self.child.kill()
            else:
                self.signal_child(signal.SIGINT)
            try:
                # Private server drains for 2s and runtime shutdown can take 1s.
                self.child.wait(timeout=4)
            except subprocess.TimeoutExpired:
                if self.pidfd is None:
                    self.child.kill()
                else:
                    self.signal_child(signal.SIGKILL)
                self.child.wait(timeout=2)

    def flush_peer_after_exit(self):
        deadline = time.monotonic() + 0.5
        while self.peer is not None and time.monotonic() < deadline:
            if not select.select([self.peer], [], [], 0.01)[0]:
                continue
            self.read_rfb()

    def command(self, message):
        if not isinstance(message, dict) or type(message.get("id")) is not int or not 1 <= message["id"] <= 1_000_000:
            raise RuntimeError("invalid private command identity")
        command = message.get("command")
        if command == "snapshot":
            self.emit({"type": "reply", "id": message["id"], "snapshot": self.snapshot()})
        elif command == "stop":
            self.stop_child()
            self.flush_peer_after_exit()
            self.observe_exit()
            self.emit({"type": "reply", "id": message["id"], "snapshot": self.snapshot(), "stopped": True})
            self.stopping = True
        else:
            raise RuntimeError("private fixture command outside allowlist")

    def run(self):
        while not self.stopping or self.ipc_tx:
            if time.monotonic() >= self.lifetime:
                raise RuntimeError("bounded fixture lifetime exceeded")
            if self.output_since is not None and time.monotonic() - self.output_since > 5:
                raise RuntimeError("private IPC reader stalled")
            self.observe_exit()
            for key, mask in self.selector.select(0.05):
                if key.data == "rfb":
                    if mask & selectors.EVENT_READ:
                        self.read_rfb()
                    if self.peer is not None and mask & selectors.EVENT_WRITE and self.tx:
                        try:
                            written = self.peer.send(self.tx)
                            del self.tx[:written]
                        except BlockingIOError:
                            pass
                        except BrokenPipeError:
                            self.peer_closed("broken-pipe")
                        self._rfb_interest()
                elif key.data == "ipc_in":
                    data = os.read(key.fd, 4096)
                    if not data:
                        return
                    if len(self.ipc_rx) + len(data) > MAX_INPUT:
                        raise RuntimeError("private command input bound exceeded")
                    self.ipc_rx.extend(data)
                    while b"\n" in self.ipc_rx:
                        line, _, tail = self.ipc_rx.partition(b"\n")
                        self.ipc_rx = bytearray(tail)
                        if len(line) > 4096:
                            raise RuntimeError("private command line bound exceeded")
                        self.command(json.loads(line))
                elif key.data == "ipc_out":
                    try:
                        written = os.write(key.fd, self.ipc_tx)
                    except BlockingIOError:
                        continue
                    del self.ipc_tx[:written]
                    if not self.ipc_tx:
                        self.selector.unregister(sys.stdout)
                        self.output_since = None
                else:
                    data = os.read(key.fd, 4096)
                    if not data:
                        self.selector.unregister(key.fileobj)
                    else:
                        self.native_tail.extend(data)
                        del self.native_tail[:-8192]

    def close(self):
        self.stop_child()
        if self.peer is not None:
            self.peer.close()
            self.peer = None
        if self.child is not None:
            for stream in (self.child.stdout, self.child.stderr):
                if stream is not None:
                    stream.close()
        if self.pidfd is not None:
            os.close(self.pidfd)
            self.pidfd = None
        self.selector.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--lease-ms", type=int, default=60_000)
    parser.add_argument("--handshake", choices=["normal", "stall-security"], default="normal")
    parser.add_argument("--session-ttl", type=int, default=60)
    args = parser.parse_args()
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    fixture = None
    try:
        fixture = Fixture(args.binary, args.lease_ms, args.handshake, args.session_ttl)
        fixture.run()
        return 0
    except BaseException as error:
        # Never dump ready messages, native stderr/env or private credentials.
        sys.stderr.write(f"FD mock fixture failed: {type(error).__name__}\n")
        return 1
    finally:
        if fixture is not None:
            fixture.close()


if __name__ == "__main__":
    sys.exit(main())
