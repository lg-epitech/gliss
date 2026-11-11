# Deployment and security boundaries

Gliss is an alpha remote-display server for one operator-selected source. Its
authentication, queue bounds and expiry controls do not by themselves supply
multi-tenant VM authorization. A platform that hosts many VMs must authorize its
users, select the VM and run a server for it. The test suites drive Chromium
against servers on the same host; other browsers and remote networks are
untested.

## Standalone access

The server defaults to plain HTTP on `127.0.0.1:8080`. The operator fixes the VNC target (`--vnc`) or the synthetic `--demo` source at startup; browser messages cannot select another host, port or VM. The access key grants display and input access to that fixed source, including clipboard and resize when the VNC server supports them, plus VM audio on request. There are no per-user roles or read-only grants.

Set a random `GLISS_TOKEN` of at least 24 characters and keep it private. `GLISS_VNC_PASSWORD` separately supplies optional upstream VNC authentication. Standard VNC authentication does not encrypt its transport, and Gliss supports no encrypted RFB security type; keep that connection node-local or inside an authenticated private tunnel.

`POST /api/session` requires the access key and an allowed browser Origin. Key digests are compared in constant time. The server permits 30 login attempts per minute across the process, rather than a separate allowance per client. A successful login sets `gliss_session`, signed with an in-memory HMAC key, with `HttpOnly; SameSite=Strict; Path=/`. It adds Secure when the configured origin uses HTTPS. The cookie contains an expiry and nonce, not a user identity, VM identity or revocation record. A server restart changes its signing key.

The WebSocket upgrade at `/ws` requires an allowed Origin, a valid cookie and a free client permit before the VNC connection is opened. With `--origin` the only allowed Origin is that exact string, less any trailing slash; without it, the allowed Origins are `http://` followed by the bind address, and `http://localhost:` followed by the bind port. Origin matching is an exact browser-request check, not an identity service. `/api/stats` requires the cookie. The viewer assets and `/health`, which reports the server version, are public.

Responses carry `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and a Content-Security-Policy that limits scripts and connections to the same origin and forbids framing.

## Expiry and logout

`--session-ttl` is 1–3600 seconds, defaulting to 3600. It starts when the cookie is issued. At WebSocket admission the server derives a monotonic deadline from that cookie's remaining lifetime; it bounds both source setup and streaming. Logging in again does not extend a socket already admitted with an earlier expiry.

`DELETE /api/session` checks Origin and clears the browser cookie. It does **not** revoke a copied cookie or signal other active streams. The supplied viewer also disconnects its own connection when its Disconnect button is used. Other admitted streams retain their deadlines unless the server is stopped. There is no account or policy reauthorization callback; an integration needing immediate revocation must close the stream or stop the server itself.

## HTTP and WSS deployment

The HTTP listener does not terminate TLS. A non-loopback bind requires an HTTPS `--origin`, but that option does not encrypt the listener. For remote use, keep the listener private behind a TLS reverse proxy and set `--origin` to the exact public browser origin, including any non-default port. Preserve the real Origin and route HTTP plus WebSocket upgrades to the same server process. Buffering or premature proxy timeouts can break streaming.

The standalone client uses root paths such as `/api/session` and `/ws`, and its cookie uses `Path=/`. A dedicated origin avoids confusing several servers' routes and cookies. URL-prefix hosting requires explicit client and cookie routing changes. Do not bypass an Origin rejection by having the proxy manufacture a trusted Origin for arbitrary callers.

## Input and resource limits

The default client limit is eight; an operator may select 1–128. One viewer is the conservative setting. Each viewer has its own VNC connection to the same desktop, and Gliss does not arbitrate input or resize between viewers. Capture and input queues are bounded, and an overflowing input queue terminates that stream rather than accumulating unlimited commands. The server attempts held-key/button release on the original source during cleanup, with a one-second bound. Failed peers can prevent delivery.

HTTP bodies and WebSocket messages are bounded to 1 MiB plus 1 KiB; clipboard text is bounded to 1 MiB. RFB clipboard uses UTF-8 when the extended clipboard is negotiated, with Latin-1 as the base-protocol fallback. Compressed clipboard messages and decompressed text have separate bounds, including CRLF expansion. QEMU VNC audio chunks are bounded to 64 KiB. Resize requires upstream support and even dimensions from 16×16 through 3840×2160. Frame allocation is limited to 16,777,216 pixels. These are individual protocol and allocation bounds, not a total memory quota.

The media window defaults to one acknowledged frame and an 8 MiB reservation budget. One oversized packet may make exclusive progress, so that budget is not a strict packet-size or process-memory ceiling. Writes and missing acknowledgements have deadlines. FPS, client and window limits and a soft video bitrate target do not impose a hard CPU quota. The deployment supervisor must supply node CPU and memory admission appropriate to the chosen resolution and concurrency.

Implementation references: [server authentication and sessions](../crates/gliss-server/src/main.rs), [media window](../crates/gliss-server/src/scheduler.rs) and [RFB capture and input](../crates/gliss-rfb/src/lib.rs).
