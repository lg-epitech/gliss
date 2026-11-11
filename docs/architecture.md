# Architecture

Gliss has three stages: capture an RGBA framebuffer from a VNC server, encode it as H.264 on the CPU, and display it in a browser. Input follows a separate bounded control path back to the same VNC connection. The server embeds its browser assets, so a deployment needs one executable rather than a frontend build or gateway service.

## Data path

```text
VNC server (RFB over TCP) -> gliss-rfb -> newest RGBA frame
                                               |
                                   OpenH264, one GLV1 packet per frame
                                               |
                             WebSocket, frame and byte acknowledgement window
                                               |
                                 web/client.js -> WebCodecs -> Canvas

browser keyboard / pointer / clipboard / resize -> WebSocket -> the same RFB connection
VNC audio -> Opus, IMA ADPCM or PCM (GLA1 packets) -> the same WebSocket -> Web Audio
```

The workspace has four crates: `gliss-core` (the frame and input types), `gliss-rfb`, `gliss-video` and `gliss-server`.

## Capture and input

`gliss-rfb` speaks RFB 3.3, 3.7 and 3.8, with no authentication or with a VNC password. It accepts Raw and CopyRect rectangles, cursor shapes, desktop size changes, the extended clipboard and QEMU's audio extension; a rectangle outside the framebuffer or any other encoding ends the connection. Capture and input use the VNC server's existing interface, so the guest needs no extra software. The upstream address is fixed by the operator (`--vnc HOST:PORT`) and never accepted from the browser. A server that stores 32-bit little-endian BGRX, as QEMU does, is asked for that format and its pixels are converted locally; any other server is asked for RGBX. Each admitted viewer gets its own VNC connection, opened as a shared session.

`--demo` replaces the VNC source with a synthetic 960×600 desktop (`--workload static`, `scroll`, `drag` or `video`). The test suites use it; it needs no VM.

The browser sends `key`, `pointer`, `clipboard` and `resize` messages. Keys are X11 keysyms and pointer messages carry a button mask. A resize must have an even width and height from 16×16 to 3840×2160. It is forwarded only to a server that announced the extended desktop size, and the server's answer returns as a `resize_result` event. When a stream ends, the server releases the keys and pointer buttons that viewer still held.

Browser clipboard text reaches the VNC server in one of three forms:

- A server with the extended clipboard gets the protocol's format: UTF-8 with CRLF line endings and a terminating NUL. Some servers need that NUL; neatvnc drops text without it.
- QEMU gets UTF-8 with LF line endings and no NUL, because it hands these bytes to guest applications unchanged. Gliss recognizes QEMU by the fixed desktop name in its `ServerInit` message, `QEMU` or `QEMU (<guest>)`. Another server using that name also gets this form.
- A server without the extended clipboard gets Latin-1 with LF line endings. Text outside Latin-1 is rejected.

QEMU doesn't tell VNC clients which line endings a guest wants, so Windows guests also get LF. Neither QEMU nor the Windows SPICE agent converts them, so applications that need CRLF may show pasted lines joined. This comes from reading their source and hasn't been tested on a Windows guest. A NUL that is part of the browser's own text is not removed for QEMU.

## Video

`gliss-video` encodes each captured frame with bundled OpenH264 on one thread, in the Baseline profile with the encoder's real-time screen-content setting. A GLV1 packet carries one complete Annex B access unit with its dimensions, its sequence number and the identifier of the last input forwarded before the frame was captured. The browser decodes with WebCodecs and closes each VideoFrame after drawing it. Video is YUV 4:2:0 and lossy at any bitrate; `--h264-bitrate` (default 4,000,000 bit/s) is a soft target, not a link cap.

The first frame of a connection is a keyframe, and so is the first frame after a desktop size change or a client `resync`. Otherwise a keyframe is sent every 3600 frames, or every `--video-intra-period` frames. The encoder needs an even width and height, from 16×16 up to 3840×2160 or 2160×3840; a stream from a desktop of any other size ends with an error. The [video crate](../crates/gliss-video/README.md) describes the encoder settings and the packet layout.

## Scheduling and transport

Each viewer has a window of frames that are queued or written but not yet acknowledged: `--in-flight-frames` (default 1, at most 8) and `--in-flight-bytes` (default 8 MiB). The client acknowledges a frame after painting it. Once media enters TCP it cannot be withdrawn, and one packet larger than the byte budget may progress alone, so the byte window is not a hard memory ceiling. A viewer that leaves a written frame unacknowledged for 10 seconds is disconnected. After a decode error the client sends `resync`; the server drops the packets it has not written yet and starts again from a keyframe.

Capture and encoding are paced separately. The server asks the VNC source for
the next update as soon as the previous one has arrived, and keeps only the
newest capture. Two requests are at least 25 ms apart at caps from 20 to
40 fps, one frame period apart above that and half a period below. The
frame-rate cap (`--fps`, default 60, at most 60) then admits encodes on a
schedule with one period of slack: a source slightly faster than the cap loses
a frame now and then instead of every frame waiting, and the last frame of a
burst is always encoded. QEMU's VNC server looks for changes on a timer that
slows down while nothing is sent, so for a second after input or a display
update the server also repeats QEMU's audio format message every 25 ms, which
keeps that timer at its 30 ms base rate. This needs the VNC display to have an
audio backend, as Distribox configures; without one QEMU keeps its own timing.
`--rfb-request-policy tick` requests on a fixed clock at the frame-rate cap
instead. The [Distribox path notes](distribox-path.md) give the reasons and the
measurements.

One frame is encoded at a time per viewer, and at most two at once across all viewers of a server.

Accepted HTTP/WebSocket sockets use `TCP_NODELAY=true` by default. The operator
can restore coalescing with `--tcp-nodelay false`; the option is applied and
verified before HTTP uses each socket. Pong callbacks bypass video decoding and
painting, so a local decoder stall does not delay an already-received reply.

## Embedding the viewer

`web/client.js` exports `GlissClient`, the browser side of the stream. Another web app can serve its modules and connect through its own authenticated relay:

```js
const { GlissClient } = await import("/gliss/client.js");
const client = new GlissClient(canvas, onStatus, onEvent, { url: "wss://example.org/desktop" });
client.connect();
client.send({ type: "key", keysym: 0xff0d, down: true });
client.dispose(); // disconnects and removes its listeners
```

`gliss-server --export-web DIR` writes the modules embedded in that binary, so the browser code always matches the server. The relay logs in with the access key (`POST /api/session`), then forwards text and binary messages unchanged between the browser and the server's `/ws`, presenting the session cookie and the server's own Origin. `onEvent` receives the server's `hello`, `clipboard`, `cursor`, `bell`, `resize_result`, `input_error`, `error`, `audio_status` and `pong` messages (and `stream_stats` when the server runs with `--stream-stats`) and the client's own `frame`, `audio_state` and `close` events; input messages are `key`, `pointer`, `clipboard` and `resize`.

`hello` and `error` are delivered in order with the frames around them; every other server message is delivered when it arrives, without waiting for earlier frames to paint. An `error` does not always end the session: when the guest selects a display mode H.264 cannot encode, the picture pauses, one `error` says so, and the next `frame` means it is back. The server sends only the newest unsent cursor, clipboard, resize result and input error, so a slow connection skips intermediate ones, and it sends no cursor larger than 128×128.

### Audio

The viewer's sound button calls `client.setAudioEnabled(true)` directly from a user gesture. Audio starts off for each connection; `client.setAudioEnabled(false)` stops playback and capture. The `audio_state` event reports `available`, `enabled`, `blocked` and an optional `message`. Closing or disposing the client releases the audio decoder, context and scheduled sources.

QEMU advertises its VNC audio extension through pseudoencoding `-259` when its VNC display has an audio backend. Gliss turns capture on, as 48 kHz stereo s16le, only while sound is enabled. For a headless VM, give the sound device and the VNC display the same `none` backend (`-audiodev none,id=a1` with `-vnc ...,audiodev=a1`; in libvirt, `<audio id='1' type='none'/>`). No host audio daemon is needed. A VNC server without the extension keeps streaming the desktop with audio unavailable.

The browser prefers native WebCodecs Opus decoding, then advertised IMA ADPCM,
then PCM for older peers. Opus uses a 96 kbit/s target and complexity 4. At the
default 20 ms packet duration, ADPCM uses 394.8 kbit/s and PCM uses about
1.54 Mbit/s including Gliss headers, before transport overhead. ADPCM is lossy.
Raw and encoded queues are bounded; packet age starts at the first raw fragment,
and expired partial packets are discarded after 100 ms. The browser also bounds
pending Opus outputs and rejects output more than 100 ms after local receipt.
Decoding runs outside the video paint queue, with a 60 ms startup buffer and a
200 ms playback cap. Audio and video share TCP, so congestion can delay both.

The control message is `{"type":"audio","enabled":true,"codec":"opus"}` (or
`"ima-adpcm"` or `"pcm"`); disabling uses `"enabled":false`. The server returns
`audio_status` with `available`, `enabled`, supported `codecs`, the chosen `codec`
and an optional `message`. Audio messages use [GLA1 framing](audio-wire.md) and
do not consume the video ACK window. Packet duration is fixed by the operator
at startup: `--audio-packet-ms 20|40`, default 20 ms. It does not change the
freshness budget or browser queue limits.

## Authentication

The listener authenticates an access key and issues a signed cookie for its fixed desktop. It checks Origin and bounds session lifetime and client admission. This is a single-source operator model, not multi-tenant authorization; the [security guide](security.md) describes its limits.
