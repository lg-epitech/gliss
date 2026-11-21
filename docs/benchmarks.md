# Benchmarks

[Round-2 Distribox CPU and latency optimizations](distribox-performance-round2.md)
measures Gliss, QEMU, Chromium and Distribox relay CPU, interactive latency and
emulated 1 Mbit/s WAN behavior across 8 Mbit/s and 2 Mbit/s targets.

[Buffering QEMU framebuffer reads](distribox-rfb-reads.md) measures Gliss CPU
per painted frame through both Distribox WebSocket relays at 1080p and an
8 Mbit/s H.264 target.

The earlier [Distribox path comparison](distribox-path.md) runs the
server with Distribox's flags against a real QEMU guest and reports
input-to-paint latency, frame rate and cadence during motion, CPU per painted
frame and bitrate. [`scripts/bench-distribox-path.mjs`](../scripts/bench-distribox-path.mjs)
produces it, and the [raw record](../benchmarks/distribox-path-2026-11-11.json.gz)
holds every sample; the [development guide](development.md#distribox-path-benchmark)
lists what the benchmark needs.

The earlier comparisons with Guacamole and WebRTC and all retired experiments, with their data and code, remain at tag `v0.1.0-alpha.9`.
