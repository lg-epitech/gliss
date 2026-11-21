# Round-2 Distribox CPU and latency optimizations

Following [buffered QEMU framebuffer reads](distribox-rfb-reads.md), we profiled
and hill-climbed the full Distribox streaming path (`1920×1080`, 30 fps cap,
8 Mbit/s H.264 target, `--in-flight-frames 8`, `--max-clients 1`, through both
Distribox Python/Uvicorn WebSocket relays and headless Chromium with software
decode).

## What changed

1. **64×64 tile damage tracking (`gliss-core`, `gliss-rfb`, `gliss-server`, `gliss-video`)**
   - `Damage` stores a 256-byte stack bitset (`32 × u64`) covering up to 2048
     64×64 tiles (both `3840×2160` and `2160×3840` with zero heap allocation,
     falling back to full damage above that).
   - `gliss-rfb` marks dirty tiles on every Raw/CopyRect rectangle, and
     `gliss-server` accumulates damage under the same mutex that publishes new
     captures so coalesced frames never drop dirty regions.
   - `gliss-video` maintains a persistent planar I420 backing store
     (`YuvPlanes`) and converts only dirty tile spans via AVX2 (with scalar
     fallback), matching OpenH264's limited-range BT.601 integer math
     bit-for-bit.

2. **Zero-copy CoW RFB frame pooling and hybrid AVX2 decode (`gliss-rfb`, `gliss-server`, `gliss-video`)**
   - `gliss-rfb` stores frames in `Arc<PooledFrame>` backed by a bounded
     generation-ordered `FramePool`. When `Arc::get_mut` succeeds (no live
     strong or weak handles), decoding mutates the pixel buffer in place with
     zero copies; otherwise it detaches into a pooled buffer synced only on
     dirty tiles since that buffer's generation.
   - `gliss-video` splits `encode_damaged` into `prepare_damaged` (YUV staging)
     and `encode_prepared` (OpenH264 encode + single-allocation `GLV1` packet
     assembly). `gliss-server` drains the RFB `watch` slot via
     `send_if_modified` and drops `Captured` immediately after
     `prepare_damaged`, returning `Arc` ownership to the RFB decoder before
     OpenH264 runs. On an idle-display `resync`, `gliss-server` re-encodes an
     IDR directly from the cached `YuvPlanes`.
   - Contiguous full-width Raw rectangles `>= 32 KiB` bypass the 32 KiB
     `BufReader` directly into the framebuffer in L2-resident 256 KiB chunks
     before in-place AVX2 `BGRX→RGBA` conversion; smaller rectangles convert
     in a single fused AVX2 pass straight out of `fill_buf`. Small RFB client
     messages (`Key`, `Pointer`, `Resize`, `FramebufferUpdateRequest`, audio
     hints/controls) use an inline 24-byte stack buffer (`WriteMessage::Inline`).

3. **Realtime OpenH264 configuration and WAN rate adaptation (`gliss-video`, `gliss-server`)**
   - `gliss-video` initializes OpenH264 `ISVCEncoder` directly via
     `InitializeExt` with `CAMERA_VIDEO_REAL_TIME`, `LOW_COMPLEXITY`,
     `DEBLOCKING_IDC_1` (disabling the in-loop deblocking filter at init time),
     `iNumRefFrame = 1`, `bEnableSceneChangeDetect = false`, and QP bounds
     `28..=46` (46 avoids an un-deblocked QP 47..51 reference degradation cycle
     under `DEBLOCKING_IDC_1`).
   - Dynamic bitrate updates apply `ENCODER_OPTION_BITRATE` and
     `ENCODER_OPTION_MAX_BITRATE` in safe order across both `SPATIAL_LAYER_0`
     and `SPATIAL_LAYER_ALL`, with a ceiling that accounts for OpenH264's
     internal `f32` spatial bitrate rescaling above `2^24` bps.
   - `gliss-server` caps the WAN adaptation floor at `250 kbit/s` (instead of
     `configured / 8`, which kept 8 Mbit/s streams stuck at `1 Mbit/s` on slow
     links), aims at `0.8×` the measured link rate for framing/burst headroom,
     deserializes WebSocket messages directly into a typed `ClientMessage`
     enum, and dispatches uncontended encodes straight to `spawn_blocking`.
   - `web/video-decoder.js` scans Annex B NAL start codes with
     `Uint8Array.prototype.indexOf(0, end)`.

## Through the Distribox relays (8 Mbit/s, 4 rounds ABBA)

Each cell shows `master` (`5084ab44`, `before`) → this branch (`after`), median
of 4 ABBA rounds pinned to six P-cores (`taskset -c 0,2,4,6,8,10`) on an Intel
i5-13600KF (`Linux 7.1.9`, `QEMU 11.1.0`, host load `4.64 → 7.28`). Both builds
had 0 failures, 0 warnings, 0 unreadable markers and 0 client errors.

| Measure | typing | scroll | drag | video | pages |
| --- | ---: | ---: | ---: | ---: | ---: |
| Gliss CPU / frame (ms) | 12.08 → **3.75** (-69%) | 10.06 → **3.54** (-65%) | 11.15 → **3.16** (-72%) | 10.54 → **4.00** (-62%) | 35.83 → **9.17** (-74%) |
| All-process CPU / frame (ms) | 38.75 → **30.83** (-20%) | 26.92 → **19.88** (-26%) | 28.45 → **19.54** (-31%) | 25.70 → **18.88** (-27%) | 70.83 → **42.08** (-41%) |
| QEMU CPU / frame (ms) | 12.92 → 13.33 | 5.24 → 5.06 | 4.16 → 3.95 | 2.99 → 2.96 | 16.67 → 16.25 |
| Chromium CPU / frame (ms) | 11.67 → 11.67 | 11.10 → 10.77 | 12.07 → 11.44 | 11.52 → 11.42 | 15.83 → 13.75 |
| Relay CPU / frame (ms, 2 procs) | 2.08 → 2.08 | 0.54 → 0.50 | 0.98 → 0.92 | 0.52 → 0.52 | 2.50 → 2.50 |
| Distinct fps | 0.87 → 0.87 | 30.06 → 30.00 | 29.91 → 29.96 | 30.06 → 30.00 | 0.80 → 0.80 |
| Key / pointer p50 (ms) | 30.9 → **23.1** | 41.5 → **31.8** | 37.7 → **36.8** | — | 72.8 → **44.3** |
| Key / pointer p95 (ms) | 49.0 → **39.5** | 56.2 → 57.4 | 70.6 → 69.8 | — | 80.6 → **49.1** |
| Frame age at paint p50 (ms) | — | 30.6 → **24.5** | — | 36.3 → **30.2** | — |
| Browser decode p50 (ms) | 0.92 → **0.60** | 0.90 → **0.60** | 0.95 → **0.70** | 1.10 → **1.00** | 3.20 → **1.50** |
| Media bitrate (Mbit/s) | 0.00 → 0.00 | 0.70 → 0.85 | 1.32 → 1.51 | 1.60 → 1.82 | 0.48 → **0.26** |

Idle (5 s static desktop): `0.0 fps`, `0.00 Mbit/s`, `0 ms` Gliss CPU, `50 ms`
QEMU CPU, `0 ms` Chromium CPU for both builds.

Per-round Gliss CPU per frame (`ms/frame`) showed zero overlap between builds
in every motion and page-turn scenario:

| Scenario | `before` (4 rounds) | `after` (4 rounds) |
| --- | --- | --- |
| scroll | 10.00, 10.04, 10.08, 10.75 | 3.54, 3.46, 3.54, 3.82 |
| drag | 11.05, 11.26, 11.00, 11.59 | 3.10, 3.25, 2.75, 3.22 |
| video | 10.29, 10.50, 10.59, 10.87 | 3.71, 4.08, 5.08, 3.92 |
| pages | 35.00, 35.83, 36.67, 35.83 | 9.17, 9.17, 8.33, 9.17 |

## Through the Distribox relays (2 Mbit/s default, 3 rounds ABBA)

Host load `7.57 → 9.49` (unrelated background load shared equally via ABBA):

| Measure | typing | scroll | drag | video | pages |
| --- | ---: | ---: | ---: | ---: | ---: |
| Gliss CPU / frame (ms) | 12.50 → **3.33** | 10.83 → **3.79** | 11.97 → **3.43** | 10.96 → **4.21** | 36.67 → **9.17** |
| All-process CPU / frame (ms) | 41.67 → **31.67** | 28.08 → **20.75** | 29.33 → **20.54** | 26.40 → **19.21** | 73.33 → **43.33** |
| Distinct fps | 0.87 → 0.87 | 30.12 → 30.00 | 29.91 → 29.90 | 29.88 → 30.00 | 0.80 → 0.80 |
| Key / pointer p50 (ms) | 33.0 → **22.8** | 39.4 → **29.3** | 38.1 → **36.9** | — | 72.4 → **44.4** |
| Frame age at paint p50 (ms) | — | 30.9 → **22.0** | — | 38.9 → **32.2** | — |
| Browser decode p50 (ms) | 1.15 → **0.60** | 1.00 → **0.70** | 1.10 → **0.70** | 1.20 → **1.00** | 3.15 → **1.65** |
| Media bitrate (Mbit/s) | 0.02 → 0.00 | 0.70 → 0.97 | 1.33 → 1.32 | 1.53 → 1.82 | 0.46 → **0.21** |

## Emulated WAN (1 Mbit/s down, 100 ms RTT, 8 Mbit/s target, 3 rounds ABBA)

Direct `scripts/bench-distribox-path.mjs --link rtt=100,down=1` (host load
`9.49 → 10.10`):

| Measure | typing | scroll | drag | video | pages |
| --- | ---: | ---: | ---: | ---: | ---: |
| Gliss CPU / frame (ms) | 10.83 → **4.17** | 10.75 → **4.70** | 14.45 → **3.59** | 11.04 → **3.71** | 40.91 → **9.17** |
| All-process CPU / frame (ms) | 37.50 → **28.33** | 27.54 → **23.80** | 36.82 → **20.00** | 25.60 → **17.46** | 80.00 → **41.67** |
| Distinct fps | 0.87 → 0.87 | 30.00 → 20.75 | 13.75 → **26.03** | 30.00 → 30.00 | 0.73 → **0.80** |
| Key / pointer p50 (ms) | 133.6 → **125.8** | 179.2 → 210.1 | 353.4 → **187.4** | — | 774.8 → **378.1** |
| Key / pointer p95 (ms) | 172.1 → **143.0** | 210.2 → 771.7 | 603.4 → **369.7** | — | 883.6 → **422.5** |
| Frame age at paint p50 (ms) | — | 110.2 → 136.3 | — | 116.8 → **105.7** | — |
| Longest frame gap (ms) | 1398.9 → 1399.0 | 87.1 → 445.1 | 349.8 → **200.3** | 64.7 → 69.3 | 1674.2 → **1578.7** |
| Media bitrate (Mbit/s) | 0.00 → 0.00 | 0.70 → 0.89 | 0.82 → **0.71** | 0.81 → **0.76** | 0.41 → **0.18** |

**Tradeoffs to note:**
- **1 Mbit/s WAN scroll:** `CAMERA_VIDEO_REAL_TIME` drops OpenH264's expensive
  screen-content block-hash scan (delivering the 62–78% server CPU reduction
  and large WAN improvements on `drag` and `pages`), but starts 8 Mbit/s
  scrolling at ~0.89 Mbit/s instead of ~0.70 Mbit/s. On a 1 Mbit/s link,
  initial queueing can pause a frame long enough for the 60 Hz scroll to jump
  beyond `CAMERA_VIDEO_REAL_TIME`'s motion search window, causing a transient
  large frame (`intervalMax` 493 / 105 / 445 ms across the 3 rounds; unique
  fps 18.9 / 24.9 / 20.8) before rate control settles.
- **Fixture reconstruction error:** On the small 96×64 / 98×66 WebCodecs
  correctness probe (`crates/gliss-video/tests/browser.mjs`, Chromium 159),
  mean RGB absolute error moved from `2.12–2.88` to `3.31–4.22` (limit 15.0)
  as expected from `iMinQp = 28` and `DEBLOCKING_IDC_1`.

## Hill-climb progression

The hill climb minimized a weighted geometric mean (`distribox_perf_index`,
lower is better: 55% server CPU/frame, 15% all-process CPU/frame, 30%
interactive latency p50) over three 1-round two-relay 8 Mbit/s samples per
experiment, requiring 0 failures/unreadable markers, `>= 28` distinct fps, and
idle `serverCpuMs <= 10`:

| Run | Status | Median index | Summary |
| --- | --- | ---: | --- |
| 0 | baseline | 103.12 | `master` (`5084ab44`) |
| 1 | **keep** | 96.27 | Tile `Damage`, pooled RFB frames, dirty-only `YuvCache`, single-alloc `GLV1` |
| 2 | **keep** | 55.36 | `CAMERA_VIDEO_REAL_TIME` + QP bounds `26..=38` |
| 3 | discard | 54.92 | `RefreshHold=1600ms` hurt QEMU CPU and `typing.keyP50` |
| 4 | crash | — | Mid-stream `ENCODER_OPTION_SVC_ENCODE_PARAM_EXT` perturbed OpenH264 RC |
| 5 | discard | 56.86 | Raising `YuvCache` strip threshold to 75% hurt `scroll` |
| 6 | **keep** | 46.23 | Direct `InitializeExt` (`DEBLOCKING_IDC_1`, `SCD=false`, `QP 28..=46`), ordered two-layer bitrate, WAN floor `<= 250 kbit/s`, AVX2 span `YuvPlanes`, AVX2 `BGRX→RGBA`, inline RFB writes, typed WS JSON, direct `spawn_blocking`, `indexOf` NAL scan |
| 7 | discard | 47.95 | Replacing `read_exact` on `>= 32 KiB` rects defeated `BufReader` bypass |
| 8 | discard | 46.54 | Restored `>= 32 KiB` bypass + CoW + `iNumRefFrame=1` + `0.8×` WAN headroom (noisy host load `10.3`; sample 3 dropped to `43.98` at load `4.9`) |
| 9 | **keep** | **43.08** | Re-evaluated Run 8 patch at quiet host load (`3.9–4.2`): `[44.13, 43.08, 42.95]` |

## Reproduce

Extract the benchmark driver from
[`benchmarks/distribox-round2-2026-11-21.json.gz`](../benchmarks/distribox-round2-2026-11-21.json.gz)
and run against `5084ab44` (`before`) and this branch (`after`), pinned to six
performance cores:

```sh
driver=$(mktemp -d)
python3 - "$driver" <<'PY'
import gzip, json, pathlib, sys
with gzip.open('benchmarks/distribox-round2-2026-11-21.json.gz', 'rt') as f:
    driver = json.load(f)['replayFiles']
for name, content in driver.items():
    dest = pathlib.Path(sys.argv[1]) / name
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(content)
PY
TMPDIR=/dev/shm taskset -c 0,2,4,6,8,10 bash "$driver/run.sh" \
  --binary /absolute/path/to/before --label before \
  --binary /absolute/path/to/after --label after \
  --h264-bitrate 8000000 --rounds 4 --typing-keys 12 \
  --scenarios typing,scroll,drag,video,pages --output /dev/shm/relays-8m.json
```

The compressed archive holds the full raw JSON of all three final ABBA suites,
binary SHA-256 hashes, per-round samples, the hill-climb history and scorer,
and the relay driver files.
