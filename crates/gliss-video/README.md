# Conventional H.264 baseline

This crate implements `H264/software/OpenH264/WebCodecs-over-WS` as a comparator for Gliss's lossless desktop codec. It is a software video path, with no WebRTC signaling/media implementation. The [benchmark guide](../../docs/benchmarks.md) compares its recorded desktop results with the lossless codecs and Guacamole, with eligibility and fidelity limits.

`openh264 = 0.9.8` builds bundled Cisco OpenH264 2.6.0 source through `openh264-sys2`; runtime encoding uses one native thread and no GPU library. The current Rust API was verified against the [maintainer's encoder documentation](https://docs.rs/openh264/0.9.8/openh264/encoder/struct.Encoder.html) and fetched crate source. Source builds and [Cisco's supported features](https://github.com/cisco/openh264/tree/v2.6.0) make native software decoding available for tests as well.

## Public interface

```rust
let mut encoder = gliss_video::VideoEncoder::new(width, height, fps)?;
let packet = encoder.encode(&rgba_frame, sequence, input_id)?;
encoder.reset(); // Next encoded frame is an IDR with SPS/PPS.
```

`with_bitrate(width, height, fps, bitrate_bps)` exposes a reproducible requested bitrate point; `new` requests 4,000,000 bit/s. Both preserve realtime screen-content mode, Baseline profile (I/P, no B frames), low complexity, one temporal/spatial layer, no long-term references, no adaptive quantization/background detection, and an IDR period of two seconds. Each native encode emits one access unit without a delayed flush/lookahead queue. SPS/PPS accompany every IDR, with cached parameter sets prepended if needed.

`with_profile(width, height, fps, bitrate_bps, VideoProfile)` is an explicit experiment constructor. `VideoProfile::Screen` is the unchanged default; `CameraSceneOn` selects realtime camera usage with scene detection enabled, and `CameraSceneOff` selects that same camera usage with scene detection disabled. `profile()` returns the retained choice. Resizes rebuild with the same profile; reset still requests an independent IDR. All other requested native settings remain the same, and GLV1 framing gains no profile field.

Pinned OpenH264 enables scene detection by default and forces it on during native validation of realtime screen usage. A screen profile with the knob disabled would therefore misdescribe the effective configuration and is deliberately absent. Changing screen to camera also changes native preprocessing/rate-control policy; camera on/off compares one scene knob within that usage. More or fewer IDRs alone do not identify which native scene/reference condition caused them.

The outer encoder and YUV buffer are constructed in the constructor, but OpenH264's Rust wrapper defers native `InitializeExt` until the first `encode_at`. Component measurements must retain that initialization cost in first encode as well as separately recording constructor cost.

Frame skipping is disabled by default to preserve one output per captured frame and simplify latency association. OpenH264 warns that this prevents a hard bitrate guarantee. **Requested bitrate is a soft control, not a bandwidth cap**; measure actual bytes and quality. Do not claim results at a matched bitrate merely from the constructor argument. The real screen-content preset is a conventional codec's own desktop optimization and should remain enabled during fair comparisons.

`with_timing_and_frame_skip(..., true)` opts into native rate-control skips. Call `encode_optional` with that policy: `None` discards the current picture and emits no packet. Assign the next wire sequence only when output exists. The source timestamp advances for every attempted picture, including skips; reset and resize preserve the policy. The server exposes this experiment as `--video-frame-skip true` for WebSocket H.264 only, without native benchmark capture. `hello.video_frame_skip` is present only when enabled; `--stream-stats` then reports cumulative `native_skipped_frames`. Default constructors and datagram paths retain the existing behavior.

`with_options(..., VideoOptions { frame_skip, qp_bounds })` also accepts an optional `QpBounds::new(min, max)?`. Both bounds must be positive, ordered, and no greater than 51. Without explicit bounds, the existing wrapper request remains `0..51`; OpenH264 substitutes its screen defaults when either bound is nonpositive. Tests compare the default packets with an explicit `26..35` request, including reset and resize. A wider requested range permits more quantization and can reduce picture sizes at the cost of image fidelity. Native validation and overflow retries can adjust coded QP, so the request is not a bitstream attestation.

The operator-only WebSocket H.264 flags `--video-qp-min MIN --video-qp-max MAX` select that range; both are required together. They are disabled by default and unavailable with datagrams or native benchmark capture. Enabled `hello` metadata adds `video_qp_bounds` with the requested values and scope. GLV1 framing stays unchanged, and reset/resize preserve the selected bounds.

The encoder accepts even dimensions at least 16×16, up to 3840×2160 landscape or 2160×3840 portrait, and 1–60 configured FPS (OpenH264's native range). A changed supported size rebuilds the encoder and starts a new IDR stream. Odd dimensions such as 641×359 return a clear error; no implicit padding/cropping changes the compared pixels. Invalid input lengths/dimensions return errors before touching native state.

RGBA becomes YUV420 through the crate's software conversion, which uses limited-range BT.601 coefficients; SPS VUI explicitly signals BT.601 limited range. Alpha is discarded because the capture contract is opaque. Both quantization and 2×2 chroma subsampling lose information. Colored small text can degrade even at a high bitrate. Compare source-frame crops/fidelity and freshness, not compressed size alone.

## GLV1 framing

All multibyte fields are little endian. One packet carries one full H.264 Annex B access unit, not an arbitrary slice of a subprocess byte stream.

| Offset | Type | Meaning |
| --- | --- | --- |
| 0 | 4 bytes | ASCII `GLV1` |
| 4 | u32 | Stream sequence |
| 8 | u32 | Associated input identifier |
| 12 | u16 | Display width |
| 14 | u16 | Display height |
| 16 | u16 | Reserved; zero |
| 18 | u16 | Flags; bit zero means IDR/keyframe; all other bits zero |
| 20 | u32 | Exact payload length |
| 24 | bytes | H.264 Annex B SPS/PPS/slices for this access unit |

Payloads are limited to 16 MiB. `parse_packet` validates framing, dimensions and IDR/parameter-set declarations, then exposes a borrowed payload. It is not a full native-bitstream verifier.

`web/video-decoder.js` exports `VideoDecoderAdapter`. It validates the packet before native decode, derives `avc1.<profile><compatibility><level>` from the actual SPS, probes `VideoDecoder.isConfigSupported`, and supplies Annex B without an AVC description. It requests `hardwareAcceleration: 'prefer-software'` and `optimizeForLatency: true`. The software preference is a hint according to [WebCodecs](https://www.w3.org/TR/webcodecs/); actual benchmark browsers must have hardware decode disabled and record evidence.

```js
const frame = await decoder.decode(arrayBuffer);
try {
  ctx.drawImage(frame.videoFrame, 0, 0);
  // Read the visible test marker from the canvas, then ACK actual paint.
} finally {
  frame.videoFrame.close();
}
```

The result is `{ width, height, videoFrame, sequence, inputId, keyframe, tiles: 1, bytes }`. Promises resolve on the output `VideoFrame` matched by the chunk timestamp/sequence. No flush is required per frame. Pending work is bounded to two frames, output has a five-second deadline, delta sequences must be contiguous, and resize/reconfiguration requires a drained queue and an IDR. Reset/close reject pending work and close late outputs. The caller owns returned frames and must close them after drawing.

## Validation

```sh
CARGO_BUILD_JOBS=2 cargo test -p gliss-video -j2 -- --test-threads=2
CARGO_BUILD_JOBS=2 cargo clippy -p gliss-video --all-targets -j2 -- -D warnings
node --test --test-concurrency=1 crates/gliss-video/tests/video-decoder.test.mjs
CARGO_BUILD_JOBS=2 cargo run -p gliss-video -j2 --example fixtures -- video-fixtures
node crates/gliss-video/tests/browser.mjs video-fixtures
```

The original six native tests exercise actual encode/software decode, metadata, simple-scene RGB quality, independently decodable reset/resize IDRs, rejected malformed framing and rejected input. New source tests compare default packets with the saved pre-experiment implementation and exercise native camera profile output, reset and resize without changing the original tests. Eight JavaScript tests cover framing, sequence/state, output association, ownership, queue limits, timeout, reset, errors and unsupported configurations. The separate browser probe decodes and paints **native generated** I/P/reset/resize fixtures, then checks Chromium's `chrome://gpu` report for software-only video decoding using the project's existing CPU browser flags.

The recorded Chromium 151.0.7922.34 probe painted four fixtures (96×64 and resized 98×66) with mean RGB absolute error 2.12–2.88 on a 0–255 scale. It reported software-only video encode/decode with hardware acceleration disabled. This is a correctness probe on small fixtures, not a full-pipeline performance benchmark or a measured fidelity claim for desktop text/natural video.
