# Lossless browser work

The lossless client paints the decoder's damage rectangle instead of submitting
the whole framebuffer for every update. An explicit `null` damage rectangle
means unchanged pixels; omitted damage metadata retains full painting. Full-frame
updates use the ordinary Canvas call. Restoring the context or repairing Canvas
dimensions forces a complete repaint, including an otherwise empty delta.
Painting still precedes the frame ACK.

Legacy raw and compressed palettes now use the same word-copy loop as global
palettes. This removes a typed-array view allocation for every expanded pixel.
Both views use the host byte order, so RGBA bytes remain unchanged. Legacy
one-color and unused palette entries remain valid; global palettes retain their
stricter validation. A failed decode still preserves its previous reference.

## Reproduce

Install the pinned npm dependencies and Playwright Chromium as described in
[development](development.md). On Linux, run:

```sh
node scripts/bench-lossless-browser.mjs \
  --baseline=25642b57 --codec=raw,palette,palette-zlib \
  --frames=60 --repetitions=4 --output=/tmp/gliss-browser.json
```

The output path must be new. The baseline commit must exist locally. The runner
compares that commit's client and decoder with the working tree's versions;
other browser modules are shared. It records source and packet hashes, Chromium
version, CPU flags, per-update samples, process counter snapshots and failures.
It exits unsuccessfully if pixels, ACKs, process identities or samples disagree.

Packets pass through the actual client, decoder, animation-frame wait, Canvas and
ACK path. Raw and 16-color palette packets use 64-pixel tiles at 1280×800 and
1920×1080. Sparse cycles revisit three regions with changed pixels and include
two empty deltas per eight updates. Motion cycles replace every tile. Packet
construction and whole-canvas pixel checks occur outside measured intervals;
each run includes one preflight readback. Variant order alternates across runs.

Decode and Canvas timings are browser elapsed time. CPU sums the CDP-reported
processes present at both sample endpoints, checked using Linux process start
times; intermediate processes that exit between samples are unobserved. Renderer
task time is reported separately. Compare CPU milliseconds for the same number
of completed updates, since faster decoding can increase update cadence.

This is a controlled CPU-browser measurement. It contains no native encoder,
capture, VM, network or standalone viewer UI. Canvas submission is not physical
presentation, and final raster work can extend beyond the counter interval.
These results do not establish a general remote-desktop latency improvement.

The portable browser suite also checks native packet fixtures, whole-canvas
pixels before ACK, sparse and empty updates, restoration, resize, recovery,
reconnect, close during painting, palette bounds and failed-decode atomicity.

## Recorded run

The [complete report](../benchmarks/browser-lossless-2026-11-04.json.gz) compares
`25642b57` with implementation `446e547e`. All 96 positions passed, with 5,760
timed updates, matching pixels and ACKs, and no exclusions. The host was an Intel
i5-13600KF running Linux and Chromium 149.0.7827.55, launched with
`taskset -c 0,2,4,6` and the recorded CPU browser flags. CPU placement and browser
runtime behavior can change these measurements.

These 1920×1080 examples are medians of four run means; CPU values are medians
of interval totals for 60 completed updates:

- Sparse raw updates: Canvas submission 1.570 → 0.027 ms/update;
  stable-process CPU 775 → 505 ms per 60 updates.
- Full-frame raw palettes: decode 48.62 → 9.03 ms/update;
  stable-process CPU 3195 → 820 ms per 60 updates.
- Full-frame compressed palettes: decode 117.21 → 77.53 ms/update;
  stable-process CPU 9735 → 7225 ms per 60 updates.

Raw full-screen updates remain controls. Repeated measurements varied for both
revisions and did not establish a raw-motion speedup or a reproducible
source-specific regression. All raw cases and per-run samples remain in the
report; the palette and sparse results are not a claim of universal CPU savings.

To inspect the archived JSON:

```sh
gzip -dc benchmarks/browser-lossless-2026-11-04.json.gz > /tmp/gliss-recorded.json
```

## Other hypotheses evaluated

These exploratory results explain the scope of this change. They are separate
from the archived browser comparison above and do not establish live VM gains.

- **Gentle motion followed by exact tile refresh:** synthetic traces saved
  modest complete-trace GLT2 bytes versus fixed exact encoding, including
  refinement, but increased encoding CPU. Candidates converged to exact after
  a delivered clean refresh sweep. This remains a fidelity option for gentle
  mode, rather than a speed improvement over exact mode.
- **Read capture after encoder admission:** deterministic checks reproduced
  the stale-capture opportunity, but the native demo measured little encoder
  contention. There was no measured material gain to justify a sender
  state-machine change.
- **Two frames in flight:** a modeled delayed round trip increased completed
  frames, but input-response tails were mixed. The default remains one.
- **Skip the coverage bitmap for single-tile updates:** results were mixed,
  including a sparse-update regression. No change was retained.

The existing software-video RGBA presentation experiment remains a separate
follow-up: native CPU-browser tests were positive, but it adds a framebuffer
copy, retained memory and presentation API choices. It needs evaluation with
GPU decoding and real guest captures before selecting a default. This proposal
adds no codec mode, sender state, dependency or framebuffer cache.

## Empty lossless updates

Empty deltas now retain their unchanged pixel reference and skip the unused
coverage bitmap. Updates that write pixels still copy before writing, so earlier
returned frames stay unchanged. Header, sequence, compressed-envelope and
trailing-body validation still run. Keyframes still require complete coverage.
There is no quantization or wire-format change.

The [hill-climb record](../benchmarks/browser-lossless-hill-climb-2026-11-05.json.gz)
contains the frozen evaluator, raw logs and every trial. The session stopped at
`WALL`: 11 experiments, one kept, ten discarded, including nine consecutive
unsuccessful trials after the retained change. The configured ceiling was 25.
The [evaluator source](https://github.com/lg-epitech/gliss/tree/0cb973ccf65c3da193803f25322a463e5ebf07db/.hill-climb)
is also retained in the research branch.

The primary score was the natural logarithm of geometric mean decode milliseconds
over 24 equally weighted synthetic cases in CPU Chromium. Three samples were
required for each decision. Gains had to exceed both `ln(1.03)` and three times
either sample set's median absolute deviation. Baseline score was 1.90140;
the accepted score was 1.27058, corresponding to 6.695 and 3.563 ms. Empty cases
drive this aggregate; it is not an estimate of a desktop workload's CPU savings.
Per-case samples remain in the archive because aggregate scores can hide
regressions.

Independent checks covered native wire/pixel fixtures, damage and Canvas
composition, palette alpha, malformed compressed packets, reference atomicity,
recovery and sequence wrap. Additional held-out checks ran 500 geometry cases
and eight valid zlib variants with 32 malformed streams on both revisions.

To measure the unchanged stream through the full client, including its
animation-frame wait, Canvas and ACK path:

```sh
taskset -c 0,2,4,6 node scripts/bench-lossless-browser.mjs \
  --baseline=168d0ed9 --codec=raw --workload=empty \
  --frames=180 --repetitions=4 --output=/tmp/gliss-empty.json
```

Omitting `--workload` retains the sparse and full-motion comparisons. The full
client measurement has the CPU sampling and physical-presentation limits
described above.
