# Deployment and security boundaries

Gliss is an alpha remote-display server for an operator-selected source. Its
authentication, queue bounds and expiry controls do not by themselves supply
multi-tenant VM authorization. A platform that hosts many VMs must authorize users,
select the VM and hand Gliss a private graphics connection. Public UDP routing,
real remote clients and browser compatibility remain unvalidated.

## Standalone access

The native server defaults to plain HTTP on `127.0.0.1:8080`. The operator fixes the VNC target, demo or private graphics FD at startup; browser messages cannot select another host, port or VM. A standalone access key grants display and input access to that fixed source, including clipboard and resize when supported. There are no per-user roles or read-only grants in standalone mode.

Set a random `GLISS_TOKEN` of at least 24 characters and keep it private. `GLISS_VNC_PASSWORD` separately supplies optional upstream VNC authentication. Standard VNC authentication does not encrypt its transport; keep that connection node-local or inside an authenticated private tunnel.

`POST /api/session` requires the access key and an allowed browser Origin. Key digests are compared in constant time. The server permits 30 login attempts per minute across the process, rather than a separate allowance per client. A successful login sets `gliss_session`, signed with an in-memory HMAC key, with `HttpOnly; SameSite=Strict; Path=/`. It adds Secure when the configured origin uses HTTPS. The cookie contains an expiry and nonce, not a user identity, VM identity or revocation record. A server restart changes its signing key.

The WebSocket upgrade requires the configured Origin, a valid cookie and a free client permit before source setup. Origin matching is an exact browser-request check, not an identity service. Authenticated statistics are available through `/api/stats`; the viewer assets and health endpoint are public.

## Expiry and logout

`--session-ttl` is 1–3600 seconds, defaulting to 3600. It starts when the cookie is issued. At WS admission the server derives a monotonic deadline from that cookie's remaining lifetime; it bounds both source setup and streaming. Logging in again does not extend a socket already admitted with an earlier expiry.

`DELETE /api/session` checks Origin and clears the browser cookie. It does **not** revoke a copied cookie or signal other active WS/QUIC sessions. The supplied viewer also disconnects its own connection when its Disconnect button is used. Other admitted sessions retain their deadlines unless the worker is stopped. Standalone mode has no account/policy reauthorization callback; an integration needing immediate revocation must own the stream and worker lifecycle.

## HTTP, WSS and QUIC deployment

The native HTTP listener does not terminate TLS. A non-loopback bind requires an HTTPS `--origin`, but that option does not encrypt the listener. For remote use, keep native HTTP private behind a TLS reverse proxy and set `--origin` to the exact public browser origin, including any non-default port. Preserve the real Origin and route HTTP plus WebSocket upgrades to the same worker. Buffering or premature proxy timeouts can break streaming.

The standalone client uses root paths such as `/api/session` and `/ws`, and its cookie uses `Path=/`. A dedicated origin avoids confusing several fixed-source workers' routes and cookies. URL-prefix hosting requires explicit client and cookie routing changes. Do not bypass an Origin rejection by having the proxy manufacture a trusted Origin for arbitrary callers.

The QUIC/WebTransport viewer is a separate transport. By default its server requires a loopback UDP bind and peer. An authenticated, Origin-checked HTTP call issues a single-use ticket valid for five seconds, with at most 32 pending tickets and client capacity reserved while pending. The connection checks the ticket's Origin and cookie expiry, then retains a session deadline. The descriptor carries the runtime certificate fingerprint, which the browser pins. Its ticket-bearing URL is a secret and should not enter access logs.

`--datagram-public-host` advertises a non-loopback media endpoint. It requires private loopback HTTP, a launcher-supplied `--rfb-fd`, `--max-clients 1` and an exact HTTPS `--origin`. `--datagram-display-input-only` removes clipboard and resize, and authenticated `POST /api/datagram-revoke` permanently closes the worker's datagram admission and input. A public HTTP/WSS proxy does not expose this UDP transport. NAT traversal and certificate handling beyond the pinned runtime certificate are the deployer's responsibility. Media datagrams are unreliable; reliable control and input do not make dropped media reliable.

## Private single-use graphics FD

`--rfb-fd` accepts an exclusively owned connected Unix stream from a trusted launcher, together with `--rfb-deadline-unix-ms`. It requires loopback HTTP, one client and no mixed VNC/demo/D-Bus source. The absolute source deadline must be future and within one hour; login cannot renew it.

The server adopts the descriptor before runtime startup, validates its socket shape, sets close-on-exec/nonblocking and claims it only after browser admission. A failed or canceled setup, EOF or disconnect consumes the slot permanently. It does not reconnect to a TCP address. Active streaming ends at the earlier cookie or source deadline, and the worker also stops when the absolute source lease expires.

Socket-shape validation does not authenticate a VM or QEMU generation. The launcher must establish the authorized source, transfer only the intended owner, close its own copy after owning the child, and stop/reap that exact worker. A libvirt UUID or numeric VNC port alone cannot provide this binding.

## Input and resource limits

The default client limit is eight; an operator may select 1–128. One viewer is the conservative setting. Capture/input queues are bounded, and an overflowing input queue terminates that stream rather than accumulating unlimited commands. The server attempts held-key/button release on the original source during cleanup, with a one-second bound. Failed peers can prevent delivery. Shared capture does not arbitrate competing viewers' input or resize ownership.

HTTP bodies and WS messages are bounded to 1 MiB plus 1 KiB; clipboard text is bounded to 1 MiB and base RFB clipboard sending supports Latin-1. Resize requires upstream support; full-frame H.264 requests additionally require even dimensions from 16×16 through 3840×2160. Core frame allocation is limited to 16,777,216 pixels. These are individual protocol/allocation bounds, not a total memory quota.

The media window defaults to one acknowledged frame and an 8 MiB reservation budget. One oversized packet may make exclusive progress, so that budget is not a strict packet-size or process-memory ceiling. Writes and missing acknowledgements have deadlines. FPS/client/window limits and a soft video bitrate target do not impose a hard CPU quota. The deployment supervisor must supply node CPU/memory admission appropriate to the chosen resolution and concurrency.

## Reference gateway

The [owned-mock gateway](../integration/distribox/prototype/README.md) adds one-use offers, path-scoped HttpOnly/SameSite Strict cookies, one WS claim, bounded workers, absolute leases and periodic mock reauthorization. It keeps native secrets server-side and disables clipboard/resize. Its cookies are not Secure because its only supported origin is loopback HTTP. A future HTTPS gateway must adapt this explicitly.

Production still needs the real master authentication and VM-to-node/domain mapping, a narrow authenticated node control channel and authoritative connected-source attachment. The mock TCP pin/relay does not supply that proof. A generation change, revoked policy, expired event or ambiguous attachment must not retarget a replacement guest. The optional `GLISS_PROTOTYPE_TEST_CONTROL` mutation API is a test-only facility, absent by default. Passing owned-fixture tests does not enable real mode or establish a production deployment.

Implementation references: [server authentication and lifecycle](../crates/gliss-server/src/main.rs), [FD admission](../crates/gliss-server/src/rfb_fd.rs), [QUIC tickets](../crates/gliss-server/src/datagram.rs), [RFB input](../crates/gliss-rfb/src/lib.rs) and [gateway adapter](../integration/distribox/adapter.py).
