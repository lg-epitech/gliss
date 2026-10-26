"""Real loopback HTTP/WS + supplied native binary; no VM/backend/GPU.

Run only after resource clearance with GLISS_BINARY set to an absolute binary.
Without it, native integration cases are explicitly skipped, never called proof.
"""

import asyncio
import contextlib
from dataclasses import replace
import json
import os
from pathlib import Path
import secrets
import socket
import struct
import time
import unittest
import zlib

import aiohttp
from aiohttp import web

from integration.distribox.adapter import AccessDenied
from integration.distribox.prototype.gateway import Config, create_app, validate_command, validate_packet
from integration.distribox.prototype.mock_backend import MockBackend


A = "00000000-0000-4000-8000-000000000001"
B = "00000000-0000-4000-8000-000000000002"
USER = "00000000-0000-4000-8000-000000000010"


class OwnedRfb:
    """Independent raw RFB peer with observable input and connection counts."""

    def __init__(self, color):
        self.initial = color
        self.inputs = []
        self.connections = 0
        self.writers = set()
        self.tasks = set()
        self.server = None

    async def start(self, port=0):
        self.server = await asyncio.start_server(self.client, "127.0.0.1", port)
        self.port = self.server.sockets[0].getsockname()[1]
        return self

    async def client(self, reader, writer):
        self.connections += 1
        self.writers.add(writer)
        self.tasks.add(asyncio.current_task())
        try:
            writer.write(b"RFB 003.008\n")
            await writer.drain()
            await reader.readexactly(12)
            writer.write(bytes([1, 1]))
            await reader.readexactly(1)
            writer.write(struct.pack(">I", 0))
            await reader.readexactly(1)
            pixel_format = bytes([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
            writer.write(struct.pack(">HH", 96, 64) + pixel_format + struct.pack(">I", 7) + b"fixture")
            pending, dirty, color = False, True, self.initial

            async def update():
                nonlocal pending, dirty
                if not pending or not dirty:
                    return
                header = bytes([0, 0]) + struct.pack(">HHHHHi", 1, 0, 0, 96, 64, 0)
                writer.write(header + bytes([*color, 0]) * (96 * 64))
                await writer.drain()
                pending, dirty = False, False

            while True:
                kind = (await reader.readexactly(1))[0]
                if kind == 0:
                    await reader.readexactly(19)
                elif kind == 2:
                    header = await reader.readexactly(3)
                    await reader.readexactly(struct.unpack_from(">H", header, 1)[0] * 4)
                elif kind == 3:
                    await reader.readexactly(9)
                    pending = True
                    await update()
                elif kind == 4:
                    data = await reader.readexactly(7)
                    down, keysym = bool(data[0]), struct.unpack_from(">I", data, 3)[0]
                    self.inputs.append({"type": "key", "keysym": keysym, "down": down})
                    if down:
                        color, dirty = (31, 67, 129), True
                        await update()
                elif kind == 5:
                    data = await reader.readexactly(5)
                    x, y = struct.unpack_from(">HH", data, 1)
                    self.inputs.append({"type": "pointer", "buttons": data[0], "x": x, "y": y})
                elif kind == 6:
                    header = await reader.readexactly(7)
                    size = struct.unpack_from(">I", header, 3)[0]
                    if size > 1048576:
                        raise ValueError("excessive clipboard")
                    self.inputs.append({"type": "clipboard", "text": await reader.readexactly(size)})
                else:
                    raise ValueError("unexpected RFB command")
        except (asyncio.IncompleteReadError, ConnectionError, ValueError):
            pass
        finally:
            writer.close()
            with contextlib.suppress(Exception):
                await writer.wait_closed()
            self.writers.discard(writer)
            self.tasks.discard(asyncio.current_task())

    async def stop(self):
        self.server.close()
        await self.server.wait_closed()
        for writer in list(self.writers):
            writer.close()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*list(self.tasks), return_exceptions=True)


async def wait_until(predicate, seconds=5):
    async with asyncio.timeout(seconds):
        while not predicate():
            await asyncio.sleep(0.01)


def raw_keyframe(packet, previous=None):
    """Independent GLS1 raw/deflate parser, not the browser decoder."""
    magic, sequence, input_id, width, height, count, flags, reserved = struct.unpack_from("<4sIIHHHHI", packet)
    if magic != b"GLS1" or flags not in (0, 1) or reserved or (not flags and previous is None):
        raise AssertionError("expected native GLS1 frame with reference")
    pixels = bytearray(width * height * 4) if flags else bytearray(previous)
    position = 24
    for _ in range(count):
        x, y, w, h, codec, zero1, zero2, size = struct.unpack_from("<HHHHBBHI", packet, position)
        position += 16
        payload = packet[position:position + size]
        position += size
        if codec == 1:
            payload = zlib.decompress(payload)
        if codec not in (0, 1) or zero1 or zero2 or len(payload) != w * h * 4:
            raise AssertionError("unexpected native tile")
        for row in range(h):
            start = ((y + row) * width + x) * 4
            pixels[start:start + w * 4] = payload[row * w * 4:(row + 1) * w * 4]
    if position != len(packet):
        raise AssertionError("trailing packet")
    return sequence, input_id, width, height, pixels


class SchemaTests(unittest.TestCase):
    def test_commands_and_packet_bounds(self):
        display = frozenset({"display"})
        full = display | {"input"}
        self.assertEqual(validate_command('{"type":"ack","sequence":0}', display)["sequence"], 0)
        for text in ['{"type":"key","keysym":65,"down":true}', '{"type":"resize","width":640,"height":480}',
                     '{"type":"clipboard","text":"secret"}', '{"type":"ack","sequence":true}',
                     '{"type":"key","keysym":-1,"down":true}', '{"type":"pointer","x":1,"y":0,"buttons":256}',
                     '{"type":"resync","host":"127.0.0.1"}', '{"type":"resync","type":"ack"}']:
            with self.assertRaises(AccessDenied):
                validate_command(text, display if '"keysym":65' in text else full)
        self.assertEqual(validate_command('{"type":"key","keysym":65,"down":true}', full)["keysym"], 65)
        packet = bytearray(24)
        packet[:4] = b"GLS1"
        struct.pack_into("<HH", packet, 12, 96, 64)
        validate_packet(packet)
        struct.pack_into("<HH", packet, 12, 1921, 64)
        with self.assertRaises(AccessDenied):
            validate_packet(packet)

    def test_real_backend_is_unavailable(self):
        config = Config(Path("/bin/true"), "http://127.0.0.1:8080")
        with self.assertRaisesRegex(ValueError, "instance/socket"):
            create_app(config, object())


@unittest.skipUnless(os.environ.get("GLISS_BINARY"), "set explicit GLISS_BINARY after resource clearance")
class NativeGatewayTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.a = await OwnedRfb((12, 34, 56)).start()
        self.b = await OwnedRfb((89, 123, 177)).start()
        self.extra_peers = []
        self.backend = MockBackend({"mode": "owned-mock", "vms": [
            {"vm_id": A, "node": "master", "port": self.a.port, "credential": "mock-secret-a"},
            {"vm_id": B, "node": "node_a", "port": self.b.port, "credential": "mock-secret-b"}],
            "users": [{"id": USER, "token": "mock-dashboard-token", "policies": ["vms:connect"]}]})
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(128)
        listener.setblocking(False)
        self.origin = f"http://127.0.0.1:{listener.getsockname()[1]}"
        self.control = secrets.token_urlsafe(32)
        config = Config(Path(os.environ["GLISS_BINARY"]), self.origin, session_ttl=20,
                        ready_ttl=5, reauthorize_interval=0.1, test_control=self.control)
        app, self.gateway = create_app(config, self.backend)
        self.runner = web.AppRunner(app, handler_cancellation=True, access_log=None)
        await self.runner.setup()
        await web.SockSite(self.runner, listener).start()
        self.client = aiohttp.ClientSession(cookie_jar=aiohttp.DummyCookieJar(),
                                            timeout=aiohttp.ClientTimeout(total=12), trust_env=False)

    async def asyncTearDown(self):
        await self.client.close()
        await self.runner.cleanup()
        self.assertEqual(len(self.gateway.sessions), 0)
        await self.a.stop()
        await self.b.stop()
        for peer in self.extra_peers:
            await peer.stop()

    async def post(self, path, body, *, origin=None, bearer=None):
        headers = {"Origin": self.origin if origin is None else origin}
        if bearer:
            headers["Authorization"] = f"Bearer {bearer}"
        response = await self.client.post(self.origin + path, json=body, headers=headers)
        return response

    async def offer(self, credential="mock-secret-a"):
        async with await self.post("/gliss/session", {"credential": credential}) as response:
            self.assertEqual(response.status, 200)
            return await response.json()

    async def exchange(self, offer):
        async with await self.post(offer["exchange_path"], {"ticket": offer["ticket"]}) as response:
            self.assertEqual(response.status, 200, await response.text())
            scoped = await response.json()
            cookie = response.cookies["gliss_stream"]
            self.assertTrue(cookie["httponly"])
            self.assertEqual(cookie["samesite"], "Strict")
            self.assertEqual(cookie["path"], scoped["view_path"])
            self.assertNotIn("gliss_session", response.headers.get("Set-Cookie", ""))
            return scoped, f"gliss_stream={cookie.value}"

    async def connect(self, scoped, cookie, *, origin=None):
        return await self.client.ws_connect(self.origin.replace("http:", "ws:") + scoped["ws_path"],
                                            headers={"Cookie": cookie},
                                            origin=self.origin if origin is None else origin,
                                            timeout=aiohttp.ClientWSTimeout(ws_close=0.5))

    async def first_packet(self, ws):
        async with asyncio.timeout(5):
            while True:
                message = await ws.receive()
                if message.type == aiohttp.WSMsgType.BINARY:
                    return message.data
                self.assertEqual(message.type, aiohttp.WSMsgType.TEXT)

    async def test_native_pixels_input_and_release(self):
        scoped, cookie = await self.exchange(await self.offer())
        ws = await self.connect(scoped, cookie)
        sequence, _, width, height, pixels = raw_keyframe(await self.first_packet(ws))
        self.assertEqual((width, height), (96, 64))
        self.assertEqual(pixels, bytes([12, 34, 56, 255]) * (96 * 64))
        await ws.send_json({"type": "ack", "sequence": sequence})
        await ws.send_json({"type": "key", "keysym": 65, "down": True, "id": 17})
        await ws.send_json({"type": "pointer", "x": 7, "y": 9, "buttons": 1, "id": 18})
        next_sequence, _, _, _, next_pixels = raw_keyframe(await self.first_packet(ws), pixels)
        self.assertEqual(next_pixels, bytes([31, 67, 129, 255]) * (96 * 64))
        await ws.send_json({"type": "ack", "sequence": next_sequence})
        await wait_until(lambda: len(self.a.inputs) >= 2)
        self.assertIn({"type": "key", "keysym": 65, "down": True}, self.a.inputs)
        self.assertIn({"type": "pointer", "x": 7, "y": 9, "buttons": 1}, self.a.inputs)
        self.assertEqual(self.b.inputs, [])
        await ws.close()
        await wait_until(lambda: not self.gateway.sessions)
        self.assertIn({"type": "key", "keysym": 65, "down": False}, self.a.inputs)
        self.assertTrue(any(row.get("type") == "pointer" and row["buttons"] == 0 for row in self.a.inputs))

    async def test_two_sessions_cookies_nodes_and_capacity(self):
        a, ac = await self.exchange(await self.offer())
        async with await self.post("/gliss/session", {"vm_id": B}, bearer="mock-dashboard-token") as response:
            self.assertEqual(response.status, 200)
            offer_b = await response.json()
        b, bc = await self.exchange(offer_b)
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.connect(a, bc)
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.connect(b, ac)
        wa, wb = await self.connect(a, ac), await self.connect(b, bc)
        self.assertEqual(raw_keyframe(await self.first_packet(wa))[4][:4], bytes([12, 34, 56, 255]))
        self.assertEqual(raw_keyframe(await self.first_packet(wb))[4][:4], bytes([89, 123, 177, 255]))
        third = await self.offer()
        async with await self.post(third["exchange_path"], {"ticket": third["ticket"]}) as response:
            self.assertEqual(response.status, 503)
        self.assertEqual(self.gateway.peak_active, 2)
        self.assertEqual(self.gateway.peak_launching, 1)
        await wa.close()
        await wb.close()

    async def test_origin_target_injection_replay_and_double_claim(self):
        before = self.backend.resolutions
        for body, origin in [({"credential": "mock-secret-a", "host": "127.0.0.1"}, self.origin),
                             ({"credential": "unknown"}, self.origin),
                             ({"credential": "mock-secret-a"}, "http://evil.invalid")]:
            async with await self.post("/gliss/session", body, origin=origin) as response:
                self.assertEqual(response.status, 403)
        self.assertEqual(before, self.backend.resolutions)
        offer = await self.offer()
        async with await self.post("/gliss/nodes/node_a/session", {"ticket": offer["ticket"]}) as response:
            self.assertEqual(response.status, 403)
        results = await asyncio.gather(*(self.post(offer["exchange_path"], {"ticket": offer["ticket"]}) for _ in range(2)))
        self.assertEqual(sorted(response.status for response in results), [200, 403])
        accepted = next(response for response in results if response.status == 200)
        scoped, cookie = await accepted.json(), f"gliss_stream={accepted.cookies['gliss_stream'].value}"
        for response in results:
            response.release()
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.connect(scoped, cookie, origin="http://evil.invalid")
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.connect(scoped, "gliss_stream=forged")
        ws = await self.connect(scoped, cookie)
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.connect(scoped, cookie)
        await ws.close()

    async def test_changed_instance_while_connecting_is_denied(self):
        offer = await self.offer()
        self.backend.mutate({"action": "pause_connect", "vm_id": A})
        exchange = asyncio.create_task(self.post(offer["exchange_path"], {"ticket": offer["ticket"]}))
        await wait_until(lambda: self.backend.vms[A].connect_attempts == 1)
        self.backend.mutate({"action": "replace_instance", "vm_id": A})
        self.backend.mutate({"action": "resume_connect", "vm_id": A})
        async with await exchange as response:
            self.assertEqual(response.status, 403)
        self.assertEqual(self.gateway.sessions, {})
        self.assertEqual(self.a.inputs, [])
        self.assertEqual(self.gateway.peak_launching, 1)

    async def test_revocation_forbidden_command_and_unclaimed_timeout(self):
        scoped, cookie = await self.exchange(await self.offer())
        ws = await self.connect(scoped, cookie)
        await self.first_packet(ws)
        self.backend.mutate({"action": "expire_event", "vm_id": A})
        await wait_until(lambda: not self.gateway.sessions)
        await ws.close()
        b, bc = await self.exchange(await self.offer("mock-secret-b"))
        wb = await self.connect(b, bc)
        await self.first_packet(wb)
        await wb.send_json({"type": "clipboard", "text": "unavailable"})
        await wait_until(lambda: not self.gateway.sessions)
        self.assertFalse(any(row["type"] == "clipboard" for row in self.b.inputs))
        await wb.close()
        await self.exchange(await self.offer("mock-secret-b"))
        await wait_until(lambda: not self.gateway.sessions, 8)

    async def test_cancel_startup_releases_reserved_capacity(self):
        offer = await self.offer()
        self.backend.mutate({"action": "pause_connect", "vm_id": A})
        exchange = asyncio.create_task(self.post(offer["exchange_path"], {"ticket": offer["ticket"]}))
        await wait_until(lambda: self.backend.vms[A].connect_attempts == 1)
        exchange.cancel()
        await asyncio.gather(exchange, return_exceptions=True)
        await wait_until(lambda: not self.gateway.sessions)
        self.assertEqual(self.gateway.launching, 0)
        self.assertEqual(self.a.connections, 0)

    async def test_rfb_eof_cannot_reconnect_to_recycled_port(self):
        scoped, cookie = await self.exchange(await self.offer())
        ws = await self.connect(scoped, cookie)
        await self.first_packet(ws)
        await self.a.stop()
        replacement = await OwnedRfb((200, 100, 10)).start(self.a.port)
        self.extra_peers.append(replacement)
        await wait_until(lambda: not self.gateway.sessions)
        await ws.close()
        self.assertEqual(replacement.connections, 0)
        self.assertEqual(replacement.inputs, [])
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.connect(scoped, cookie)

    async def test_display_only_grant_receives_pixels_but_denies_input(self):
        self.backend.vms[A].capabilities = frozenset({"display"})
        scoped, cookie = await self.exchange(await self.offer())
        ws = await self.connect(scoped, cookie)
        sequence, _, _, _, pixels = raw_keyframe(await self.first_packet(ws))
        self.assertEqual(pixels[:4], bytes([12, 34, 56, 255]))
        await ws.send_json({"type": "ack", "sequence": sequence})
        await ws.send_json({"type": "key", "keysym": 65, "down": True, "id": 1})
        await wait_until(lambda: not self.gateway.sessions)
        self.assertFalse(any(row["type"] == "key" for row in self.a.inputs))
        await ws.close()

    async def test_live_session_deadline_and_oversized_body(self):
        before = self.backend.resolutions
        async with await self.post("/gliss/session", {"credential": "x" * 5000}) as response:
            self.assertEqual(response.status, 413)
        self.assertEqual(self.backend.resolutions, before)
        self.gateway.config = replace(self.gateway.config, session_ttl=3)
        started = time.monotonic()
        scoped, cookie = await self.exchange(await self.offer())
        ws = await self.connect(scoped, cookie)
        await self.first_packet(ws)
        await wait_until(lambda: not self.gateway.sessions, 5)
        self.assertLess(time.monotonic() - started, 5)
        await ws.close()
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.connect(scoped, cookie)


if __name__ == "__main__":
    unittest.main()
