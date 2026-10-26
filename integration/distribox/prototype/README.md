# Scoped gateway prototype

This is a loopback alpha gateway for explicitly owned mock RFB peers. It wraps the Gliss native server with VM-scoped offers, one-use tickets, cookies and lease enforcement. It does not import Distribox, access its database or libvirt, start a VM, or accept a browser-selected graphics target. `--backend distribox` refuses startup before opening a listener: real VM-generation and connected-socket attestation remain required.

See the [benchmark guide](../../../docs/benchmarks.md) for the scope of performance evidence. This prototype supplies correctness tests, not a production deployment or a throughput result.

## Setup and self-contained checks

Run commands from the Gliss repository root. The gateway requires a Unix host, Python 3.11 or newer and the pinned aiohttp dependency. Browser cases also require the repository's Node dependencies and Playwright Chromium. Build only one native executable at a time:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r integration/distribox/prototype/requirements.txt
npm ci
npx playwright install chromium
cargo build --release --workspace -j 2
export GLISS_BINARY="$PWD/target/release/gliss-server"
export GLISS_PROTOTYPE_PYTHON="$PWD/.venv/bin/python"
```

`GLISS_BINARY` must name an explicit absolute executable. `$PWD` makes that choice portable; no cached binary or archived fixture is needed. `GLISS_PROTOTYPE_PYTHON` selects the Python interpreter used by the browser harness when it launches the gateway.

First run the checks that need no native worker or browser:

```sh
.venv/bin/python -B -m unittest \
  integration.distribox.prototype.tests.test_codec_presets \
  integration.distribox.prototype.tests.test_gateway.SchemaTests -v
node --test integration/distribox/prototype/tests/viewer-media.test.mjs
```

Run the native and CPU-browser cases sequentially:

```sh
.venv/bin/python -B -m unittest discover \
  -s integration/distribox/prototype/tests -v
node --test --test-concurrency=1 \
  integration/distribox/prototype/tests/browser.test.mjs
node --test --test-concurrency=1 \
  integration/distribox/prototype/tests/browser-glh4.test.mjs
```

These harnesses create fresh loopback RFB peers, credentials, configuration files and gateway workers, then clean up their own resources. They use the current `tests/helpers.mjs`, `tests/mock-rfb.mjs`, the local moving peer and the production browser decoders; they do not require `bench/` or historical `refs/` files. The shared browser helper disables GPU rendering, compositing and hardware video paths. Set `CHROMIUM_PATH` only when choosing a different installed Chromium executable explicitly.

Without `GLISS_BINARY`, the Python native cases and lossless browser case skip; those skips are not integration evidence. The GLH4 browser case requires the explicit executable and fails if it is missing. Both codecs require a build supporting their actual native protocol. The cases exercise exact pixels and input, scoped cookies, ticket replay and target-injection rejection, revocation, held-input release and worker cleanup. GLH4 additionally exercises actual software video output, dependent frames, independent-key recovery and revocation during delayed decoder loading. No test attaches to a real VM or Distribox backend.

Optional `GLISS_PROTOTYPE_REPORT` and `GLISS_PROTOTYPE_GLH4_REPORT` paths retain sanitized JSON from the respective browser cases; choose separate output files. TAP diagnostics also contain the proof and partial cleanup diagnostics. Reports include executable/source hashes, browser configuration and sampled worker CPU/RSS. A short passing fixture is not representative capacity, latency or VM-identity evidence.

## Manual owned-peer use

The automated cases above are the simplest complete example. For a manual session, first start RFB peers that the fixture owner controls, then create a local configuration such as `owned-mock.json`. Replace the example ports and mock secrets with those of those peers:

```json
{
  "mode": "owned-mock",
  "vms": [
    {"vm_id":"00000000-0000-4000-8000-000000000001", "node":"master", "host":"127.0.0.1", "port":5901, "credential":"mock-secret-a"},
    {"vm_id":"00000000-0000-4000-8000-000000000002", "node":"node_a", "host":"127.0.0.1", "port":5902, "credential":"mock-secret-b"}
  ],
  "users": [{"id":"00000000-0000-4000-8000-000000000010", "token":"mock-dashboard-token", "policies":["vms:connect"]}]
}
```

```sh
.venv/bin/python -B -m integration.distribox.prototype.gateway \
  --worker-binary "$GLISS_BINARY" \
  --fixtures ./owned-mock.json --port 18080
```

Only `127.0.0.1` is bound. The accepted Origin and Host are exactly `http://127.0.0.1:18080`; using `localhost` instead is a different origin. Worker HTTP connections ignore environment proxies. This loopback HTTP prototype does not implement a public TLS deployment.

The mock bearer is a literal fixture token, not a JWT implementation. Its `vms:connect` policy is global, matching the inspected Distribox policy; this prototype invents no VM-owner predicate. A mock credential selects exactly one VM, duplicate active matches deny access, and credential/event deadlines are rechecked. Fixture-only `capabilities:["display"]` permits read-only viewing. Browser requests cannot choose capabilities, node, port, generation or codec.

The browser flow is:

1. POST JSON `{"credential":"mock-secret-a"}` to `/gliss/session`, with the exact Origin. Alternatively, send `Authorization: Bearer mock-dashboard-token` and JSON `{"vm_id":"00000000-0000-4000-8000-000000000001"}`. The two authentication forms cannot be mixed.
2. POST `{"ticket":"…"}` to the returned `exchange_path`, again with the exact Origin and in the same browser context. The response sets a scoped HttpOnly cookie and returns `view_path` and `ws_path`.
3. Open the returned `view_path` on that origin. The viewer receives pixels and forwards keyboard/pointer input. Disconnect consumes that stream claim; obtain a fresh offer for another connection.

The gateway intentionally has no generic login page. The browser tests demonstrate this offer/exchange/view sequence. Do not put a VM credential, dashboard bearer, native token or cookie in a viewer URL.

## Trusted codec presets

Deflate remains the default: omit `--worker-codec` or pass `--worker-codec deflate`. The only alternative is the explicit experiment `--worker-codec hybrid-continuity`, which fixes:

```text
--codec hybrid-continuity
--hybrid-base global-cached
--hybrid-video-profile camera-scene-off
--hybrid-mask-grid 32
--tile-size 64
--h264-bitrate 3000000
```

Both presets keep one viewer per worker, a 30 fps cap and a short private native TTL. GLH4 preserves exact base pixels outside signaled video masks and uses lossy software H.264 inside them; text within a mask can be lossy. Selecting it changes the fidelity contract. The gateway verifies the authenticated native hello against the selected codec/profile before forwarding media. Requests, tickets and controls cannot change encoder flags or upstream targets. Other standalone codecs are not gateway presets.

The viewer loads only the negotiated decoder. It bounds media to two packets including active decode, with each packet at most 12 MiB and 24 MiB aggregate packet storage. Epoch changes on recovery/close dispose decoder state and suppress stale paint/ACK, including late module loads. Decoder import has a five-second deadline. A reference failure waits for a fully decoded independent key packet; a key header alone cannot authorize painting.

## Authentication, leases and resource bounds

Tickets expire within 30 seconds and are consumed once. Each of the two logical nodes has four pending-offer slots. Redemption creates an independent `HttpOnly; SameSite=Strict` cookie scoped to a random session path. This permits separate VM tabs in one browser context. Native tokens and native `gliss_session` cookies remain server-side. Cookies in this HTTP-only mock prototype are not Secure; a future HTTPS integration must add Secure cookies and retain exact Origin enforcement.

At most two sessions may reserve workers, with one launch progressing at a time. Session lifetime is the minimum of authorization expiry, 300 seconds and the configured gateway TTL, enforced by a monotonic deadline. An unclaimed ready session expires within ten seconds. Active authorization checks run at most five seconds apart. Current hooks are in-memory and nonblocking; the constructor rejects real backends rather than treating blocking database/libvirt calls as safely canceled work.

Browser request bodies and WS controls are bounded to 4 KiB. Strict schemas permit ACK/resync/ping/key/pointer and reject query parameters, binary controls, target injection, clipboard and resize. Native media is bounded to 12 MiB and 1920×1080. Relay writes have deadlines and no application frame queue. aiohttp applies byte-level WS flow control; this is not a global memory cap or a strict message-count limit.

Two worker reservations are not a two-core quota. Each worker has its own runtime, capture buffers and encoder, with additional Python relay and browser costs. FPS and bitrate limits do not impose a hard CPU quota. Measure whole-session CPU/RSS and use an external supervisor's resource limits before considering increased concurrency.

Cleanup stops commands, closes the native WS so held input can be released through the original pinned RFB connection, closes that connection, then stops and reaps the owned worker. Cancellation during launch retains child ownership. Release delivery remains bounded best effort if the peer has failed; cleanup never reconnects to a recycled port to send input.

`GLISS_PROTOTYPE_TEST_CONTROL`, when explicitly set to a token of at least 32 characters, enables loopback-only `GET/POST /__test__/state` for deterministic fixture mutation and diagnostics. It is absent by default, separate from browser credentials and never returned in offers. It is a test facility, not a deployed control API.

## Real integration boundary

Before spawning, the current mock path opens one approved RFB socket and checks its instance again. A private loopback relay forwards that established socket without another target lookup. The mock generation is authoritative only for its owned peer. A numeric VNC port, libvirt domain UUID/running ID, or checks before and after TCP connect do not attest that a production connection belongs to the authorized QEMU generation. The relay also accepts its first local TCP client without a production worker-attestation protocol. Keep real mode unavailable.

A Distribox integration still needs master authorization, trusted node selection, an authenticated narrow node control operation, short scoped leases, ongoing policy/event reauthorization and authoritative graphics attachment. Database VM IDs and libvirt domain UUIDs must not be assumed equal. Acquiring an already connected graphics FD avoids reconnecting to a port, but does not by itself supply an atomic expected-generation check. Define either a provider that compares the expected generation while attaching, or an explicitly versioned attachment-time socket lease acquired before ticket issuance. Native FD support does not wire this provider into the gateway automatically.

FD acquisition, cancellation and worker spawn must remain owned until late descriptors are closed and exact children reaped. A failed or ambiguous attachment must not silently retry against a replacement guest. Node-local attachment must happen on the assigned node; master-to-slave HTTP cannot transfer a usable FD number. Clipboard and resize remain disabled here until separately authorized implementations exist.
