# Architecture

Gliss has three replaceable stages: capture a canonical RGBA framebuffer, encode updates, and display them in a browser. Input follows a separate bounded control path back to the configured source. The server embeds its browser assets, so a deployment needs one executable rather than a frontend build or gateway service.

## Data paths

```text
QEMU VNC / private graphics FD -> RFB -> latest canonical RGBA frame
                                    |
          +-------------------------+--------------------------+
          |                                                    |
  Batch / Deflate tiles (GLS1, GLB1)          CPU OpenH264, every frame an IDR (GLV1)
          |                                                    |
  WebSocket, acknowledged frames               GLD1 fragments over QUIC datagrams
          |                                                    |
  JavaScript decoder -> Canvas                 newest complete picture -> WebCodecs -> Canvas

browser keyboard / pointer -> WebSocket or reliable QUIC stream -> original RFB connection
```

`gliss-rfb` negotiates RFB, validates bounded framebuffer updates and handles pointer, keyboard, cursor, clipboard and supported resize messages. VNC capture and input use QEMU's existing interface, so the guest needs no extra software. The upstream address is fixed by the operator and never accepted from the browser. The optional connected Unix graphics FD is one-use and deadline-bound; it lets a trusted launcher hand over an authorized display without opening a new port.

`gliss-dbus` provides experimental private QEMU D-Bus capture, still using VNC for input. Optional shared capture reuses one producer across viewers; encoders and input connections remain separate.

## Lossless desktop codec

`gliss-codec` implements the GLS1 desktop protocol, encoder and decoder. It splits the framebuffer into fixed tiles, emits only changed regions, and compares candidates such as raw RGBA, zlib, palette indices, XOR residuals and copies from the previous frame. Copy search is bounded. A new geometry, sequence discontinuity or explicit reset starts an independent keyframe.

Batch encodes the selected region bodies together to reduce repeated compression overhead; the [Batch specification](codec-spec.md) defines its wire format. Global also compresses the complete update representation. GlobalCached reuses copy-search metadata tied to an owned previous frame; its packets and decoded pixels must match Global. The custom representation uses standard Deflate as a building block, not a new entropy coder.

The browser validates packet geometry, sizes, sequence and codec tags before painting. Copy regions read from the previous frame, avoiding order-dependent corruption. Inflation is bounded to the declared output. Native roundtrips, older reference implementations and pinned browser goldens exercise the matching decoders.

## Video and hybrid alternatives

`gliss-video` encodes H.264 with bundled OpenH264 on one native encoder thread. GLV1 packets carry complete Annex B access units, dimensions, sequence and input association. The browser decodes with WebCodecs and closes each VideoFrame after drawing it. Video is YUV420 and lossy even at high bitrates; its bitrate is a soft target, not a link cap. See the [video crate](../crates/gliss-video/README.md).

The live-video profile makes every frame an IDR and sends it as GLD1 fragments over WebTransport datagrams, with no retransmission, parity or frame ACK. The [live-video specification](live-video-spec.md) defines its framing, queue bounds and recovery.

GLH1/GLH2/GLH4 combine a lossless base with classified video regions. A region selected for video can contain text, so the lossless base does not guarantee exact text everywhere. GLT2 uses independently refreshable tiles with bounded fragment reassembly and current-state replacement; see the [tile crate](../crates/gliss-tiles/README.md). Predictive GOP4 video and XOR parity remain experiments. These modes are all opt-in.

## Scheduling and transport

The WebSocket path acknowledges frames and bytes, with a default window of one frame. Once media enters TCP, dropping a newer frame cannot remove older reliable data, and one oversized packet may progress alone, so the byte window is not a hard memory ceiling.

The datagram path publishes the newest captured state and admits whole pictures into a bounded send queue. The browser keeps at most two incomplete assemblies and one replaceable waiting frame. Painting releases browser-local ownership without acknowledging anything to the server. Loss or expiry triggers a rate-limited request for a fresh current-state picture; old media is never replayed.

WebTransport needs browser support for unreliable datagrams and direct UDP routing; an ordinary HTTP reverse proxy cannot carry it. By default the datagram listener is loopback-only. A public media host requires the managed-FD mode described in the [security guide](security.md).

## Embedding the viewer

`web/client.js` exports `GlissClient`, the browser side of the WebSocket path. Another web app can serve its modules and connect through its own authenticated relay:

```js
const { GlissClient } = await import("/gliss/client.js");
const client = new GlissClient(canvas, onStatus, onEvent, { url: "wss://example.org/desktop" });
client.connect();
client.send({ type: "key", keysym: 0xff0d, down: true });
client.dispose(); // disconnects and removes its listeners
```

`gliss-server --export-web DIR` writes the modules embedded in that binary, so the browser code always matches the server. The relay logs in with the access key (`POST /api/session`), then forwards text and binary messages unchanged between the browser and the server's `/ws`, presenting the session cookie and the server's own Origin. `onEvent` receives `hello`, `frame`, `clipboard`, `cursor`, `error` and `close` events; input messages are `key`, `pointer`, `clipboard` and `resize`.

### Audio over WebSocket

The viewer's sound button calls `client.setAudioEnabled(true)` directly from a user gesture. Audio starts off for each connection; `client.setAudioEnabled(false)` stops playback and capture. The `audio_state` event reports `available`, `enabled`, `blocked` and an optional `message`. Closing or disposing the client releases the audio decoder, context and scheduled sources.

QEMU advertises its VNC audio extension through pseudoencoding `-259` when its VNC display has an audio backend. Gliss requests 48 kHz stereo s16le only while sound is enabled. For a headless VM, map both the sound device and VNC graphics to the same `<audio id='1' type='none'/>` backend. No host audio daemon is needed. Unsupported VNC servers keep streaming the desktop with audio unavailable. Audio currently uses the WebSocket path.

The browser chooses native WebCodecs Opus decoding when supported, otherwise Web Audio PCM playback. The server encodes Opus at a 96 kbit/s target in 20 ms packets with complexity 4. PCM preserves compatibility at roughly 1.54 Mbit/s before transport overhead. Raw and encoded audio queues are bounded; the 100 ms server age limit starts at the first fragment of a packet, and expired partial packets are discarded. The browser also discards Opus outputs more than 100 ms after local receipt and bounds all pending outputs, including work no longer counted by `decodeQueueSize`. Browser decoding runs outside the video paint queue, with a 60 ms startup buffer for packet jitter and a 200 ms playback cap. These are local queue limits, not an end-to-end latency guarantee. Video and audio still share TCP, so network congestion can delay both.

The control message is `{"type":"audio","enabled":true,"codec":"opus"}` (or `"pcm"`); disabling uses `"enabled":false`. The server returns `audio_status` with `available`, `enabled`, the chosen `codec` and an optional `message`. Each binary audio message has a 20-byte little-endian header: `GLA1`, codec byte (`0` PCM or `1` Opus), channel count byte (`2`), zero reserved u16, sample rate u32 (`48000`), wrapping sequence u32, and frame count u32 (`960`). Its payload is one Opus packet or interleaved signed PCM16. Audio messages do not consume the video ACK window.

## Authentication

The listener authenticates an access key and issues a signed cookie for its fixed desktop. It checks Origin and bounds session lifetime and client admission. WebTransport connections additionally need a single-use ticket and a pinned certificate. This is a single-source operator model, not multi-tenant authorization; the [security guide](security.md) describes its limits.

`integration/distribox/` contains a reference gateway for the [Distribox](https://github.com/PoCInnovation/Distribox) VM platform. It wraps the native server with VM-scoped offers, one-use tickets, cookies and lease enforcement, and is tested only against mock RFB peers; its real backend mode refuses to start.
