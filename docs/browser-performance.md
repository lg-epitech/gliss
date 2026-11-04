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
