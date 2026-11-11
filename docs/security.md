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

`POST /api/session` requires the access key and an allowed browser Origin. Key digests are compared in constant time. Failed logins are limited to 30 per minute per client address (an IPv4 address or an IPv6 /64); successful logins are not counted. 4096 addresses are tracked and further addresses share one allowance. The Origin header and the allowance are checked before the body, of at most 4096 bytes, is read. A successful login sets `gliss_session`, signed with an in-memory HMAC key, with `HttpOnly; SameSite=Strict; Path=/`. It adds Secure when the configured origin uses HTTPS. The cookie contains an expiry and nonce, not a user identity, VM identity or revocation record. A server restart changes its signing key.

The WebSocket upgrade at `/ws` requires an allowed Origin, a valid cookie and a free client permit before the VNC connection is opened. With `--origin` the only allowed Origin is that exact string, less any trailing slash; without it, the allowed Origins are `http://` followed by the bind address, and `http://localhost:` followed by the bind port. Origin matching is an exact browser-request check, not an identity service. `/api/stats` requires the cookie. The viewer assets and `/health`, which reports the server version, are public.

Responses carry `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and a Content-Security-Policy that limits scripts and connections to the same origin and forbids framing.

## Expiry and logout

`--session-ttl` is 1–3600 seconds, defaulting to 3600. It starts when the cookie is issued. At WebSocket admission the server derives a monotonic deadline from that cookie's remaining lifetime; it bounds both source setup and streaming. Logging in again does not extend a socket already admitted with an earlier expiry.

`DELETE /api/session` checks Origin and clears the browser cookie. It does **not** revoke a copied cookie or signal other active streams. The supplied viewer also disconnects its own connection when its Disconnect button is used. Other admitted streams retain their deadlines unless the server is stopped. There is no account or policy reauthorization callback; an integration needing immediate revocation must close the stream or stop the server itself.

## HTTP and WSS deployment

The HTTP listener does not terminate TLS. A non-loopback bind requires an HTTPS `--origin`, but that option does not encrypt the listener. For remote use, keep the listener private behind a TLS reverse proxy and set `--origin` to the exact public browser origin, including any non-default port. Preserve the real Origin and route HTTP plus WebSocket upgrades to the same server process. Buffering or premature proxy timeouts can break streaming.

The standalone client uses root paths such as `/api/session` and `/ws`, and its cookie uses `Path=/`. A dedicated origin avoids confusing several servers' routes and cookies. URL-prefix hosting requires explicit client and cookie routing changes. Do not bypass an Origin rejection by having the proxy manufacture a trusted Origin for arbitrary callers.

Behind a reverse proxy every client has the proxy's address, so they share one login allowance and one client's failed logins can block every login for up to a minute. Forwarding headers are not read; rate-limit logins at the proxy.

The listener holds at most 7 × `--max-clients` + 16 connections, streams included; further connections wait until one closes. The limit is not per address, so one address can fill it for as long as it keeps opening connections. A connection has 5 seconds to send a complete request head, also between requests, and a login has 5 more seconds to send its body. A peer that reads nothing of a response for 10 seconds is disconnected. Established streams are not subject to the request timeouts.

## Input and resource limits

The default client limit is eight; an operator may select 1–128. One viewer is the conservative setting. Each viewer has its own VNC connection to the same desktop, and Gliss does not arbitrate input or resize between viewers.

From the browser: WebSocket messages are bounded to 1 MiB plus 1 KiB and clipboard text to 1 MiB. The input queue is bounded, and overflowing it terminates that stream rather than accumulating commands. A session holds at most 256 keys down; a further key press is refused with an `input_error` event. A browser must acknowledge a frame within 10 seconds of the oldest frame it has not acknowledged, and a resync does not restart that time. Resyncs start at most once every 250 ms. The server attempts held-key and button release on the VNC connection during cleanup, with a one-second bound. Failed peers can prevent delivery.

From the VNC server, whose bytes a guest can influence:

- Frame allocation is limited to 16,777,216 pixels. One framebuffer update may write or move at most four times the frame area and change the desktop size once; more is a protocol error that ends the session. An update that changes no pixel is not encoded.
- A display mode H.264 cannot encode (an odd width or height, a side under 16, or above 3840×2160) pauses the picture and sends one `error` event per mode change, after the frames encoded before the change; a pause that ends before its event is sent sends none. Input keeps working and the picture resumes when the mode fits. Nothing is cropped or padded.
- Cursor shapes up to 1024×1024 are read; shapes above 128×128 are discarded and never sent to the browser.
- Guest events (cursor, clipboard, bell, resize result) are not queued: only the newest unsent event of each kind is kept, at most one 128×128 cursor and one 1 MiB clipboard per viewer. A slow browser cannot end its session through them.
- RFB clipboard uses UTF-8 when the extended clipboard is negotiated, with Latin-1 as the base-protocol fallback. The 1 MiB limit applies to the UTF-8 text sent to the browser, and a Latin-1 clipboard that exceeds it after conversion is dropped. Compressed clipboard messages and decompressed text have separate bounds, including CRLF expansion. A clipboard message over its limit on the wire is a protocol error that ends the session.
- QEMU VNC audio chunks are bounded to 64 KiB.
- A write to the VNC server that makes no progress for 10 seconds ends the VNC connection and the viewer's session.
- Text from the VNC server is escaped and cut to 256 characters in error messages, and the line that logs the end of a stream is escaped and cut to 512 characters.

Resize requires upstream support and even dimensions from 16×16 through 3840×2160. These are individual protocol and allocation bounds, not a total memory quota: a hostile VNC byte stream can still keep one core busy within the per-update budget.

The media window defaults to one acknowledged frame and an 8 MiB reservation budget. One oversized packet may make exclusive progress, so that budget is not a strict packet-size or process-memory ceiling. FPS, client and window limits and a soft video bitrate target do not impose a hard CPU quota. The deployment supervisor must supply node CPU and memory admission appropriate to the chosen resolution and concurrency.

Implementation references: [server authentication and sessions](../crates/gliss-server/src/main.rs), [the listener](../crates/gliss-server/src/http.rs), [login allowance](../crates/gliss-server/src/logins.rs), [guest events](../crates/gliss-server/src/notices.rs), [held keys](../crates/gliss-server/src/held.rs), [media window](../crates/gliss-server/src/scheduler.rs) and [RFB capture and input](../crates/gliss-rfb/src/lib.rs).
