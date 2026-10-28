# Smaller refresh pictures under audio load

28 October 2026 · `research/audio-performance` · CPU only

Raising the requested minimum QP from **26 to 36**, with maximum 51 fixed,
reduced the periodic refresh packet from about **53 kB to 32 kB**. On the
modeled 80 ms RTT / 1 Mbit/s link, timely sound responses improved from **9/12
to 10/12**. Two codes still went unobserved. This setting reduced the burst;
it did not eliminate audio interruption.

## Fixed comparison

Eight positions ran `26..51`, `36..51`, `36..51`, `26..51` on loopback, then
the same order on the constrained link. Each retained 1.5 seconds of warmup,
a six-second measurement window, six unique audio probes, a 700 ms deadline
and twenty control pings. Both policies used the same native binary, Screen
H.264 at a 500 kbit/s target, 30 fps cap, eight in-flight frames, 60-frame
refresh interval, Opus and disabled frame skipping. No buffer or freshness
limit changed.

On loopback, both identified all 12 codes and answered all 40 pings. Received
audio-plus-video application media was **482–506 kbit/s** with minimum 26 and
**385–392 kbit/s** with minimum 36. On the constrained link:

- **26..51:** 9/12 timely codes at **253–307 ms**; three unobserved. All 40
  pongs arrived, with per-position wire p95 **775–796 ms**. Received media
  was **496–504 kbit/s**, painted marker cadence **9.33–9.50 IDs/s**.
- **36..51:** 10/12 timely codes at **239–630 ms**; two unobserved. All 40
  pongs arrived, with per-position wire p95 **175–401 ms**. Received media
  was **380–386 kbit/s**, marker cadence **9.67–10.17 IDs/s**.

Each constrained position received one periodic refresh picture inside its
measurement window: **53,158–53,311 bytes** with minimum 26 and
**31,704–31,822 bytes** with minimum 36. The interval stayed fixed.

These are two repetitions per condition. Minimum 36 missed fewer codes here
but its slowest timely response was later. All measurement and cleanup gates
passed, with no video decode errors, sequence gaps, invalid markers or recorded
kernel packet drops. A valid measurement can still contain missing sound.

## Shared-input image cost

A separate component encoded the same twelve clean 1280×800 RGBA frames with
both policies, in the same four-arm order. All 48 pictures decoded. Repeated
arms produced identical packet and image-error observations:

- **26..51:** **104,857 packet bytes**; largest picture **37,059 bytes**;
  document-region RGB mean absolute error **3.869**, texture error **59.378**.
- **36..51:** **87,300 bytes**; largest picture **31,824 bytes**;
  document error **3.588**, texture error **60.169**.

Errors use a 0–255 RGB scale. Lower mean document error here does not establish
better perceptual quality: individual frames differ, texture error increased,
and the procedural document contains no real fonts. Clean component frames
are separate from asynchronous live RFB captures. The control observations
match the frozen [preceding component](../video-qp/README.md), excluding wall
timing.

The [next pair](../video-qp-high-floor/README.md) tests minimum 42 against 36.
It retains this partial result and all earlier failures. Requested bounds do
not attest every coded QP. Release defaults and Distribox remain unchanged.

## Evidence and replay

[Inventory](results.json) binds original reports, prospective registrations,
source archives and the complete 102-record component journal. The live archive
inherits 136 frozen parent sources and adds five files without overriding any
parent. The verifier checks every probe, native arguments, packet continuity,
bindings and journal, and recomputes the summaries:

```sh
python3 -B benchmarks/video-qp-floor/verify.py
```

Image-error values require replay to remeasure. The generator and input hash
are documented in the [parent study](../video-qp/README.md). Build the
`video-qp-range` example serially with `-j1 --locked --offline`, then provide
a fresh owned input, a fresh report path and `26 36 51`.

The live controller is `scripts/bench-video-qp-floor.mjs --output NEW.json`.
Use its registered native revision and the namespace/resource launch
requirements in the [motion study](../audio-motion/README.md). Native, browser
and controller ran on CPU 0 at nice 19; the fresh diskless guest used CPU 19.
Private devices excluded physical GPUs. Limits were 3 GiB memory, no swap,
256 tasks and a 180-second live watchdog. No existing VM, Distribox service
or host network policy changed.

Clippy, the serial release example build and **180 portable tests** passed;
[validation](validation.json) records log identities. Marker cadence is not
whole-screen FPS or scanout. Sound timing includes analysis and IPC, not
speakers. This short synthetic study does not establish production capacity,
real window-manager behavior or acceptable image quality.
