# Benchmarks

The current measurement is the [Distribox path](distribox-path.md). It runs the
server with Distribox's flags against a real QEMU guest and reports
input-to-paint latency, frame rate and cadence during motion, CPU per painted
frame and bitrate. [`scripts/bench-distribox-path.mjs`](../scripts/bench-distribox-path.mjs)
produces it, and the [raw record](../benchmarks/distribox-path-2026-11-11.json.gz)
holds every sample; the [development guide](development.md#distribox-path-benchmark)
lists what the benchmark needs.

The earlier comparisons with Guacamole and WebRTC and all retired experiments, with their data and code, remain at tag `v0.1.0-alpha.9`.
