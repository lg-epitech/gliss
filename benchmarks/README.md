# Measurements

[distribox-round2-2026-11-21.json.gz](distribox-round2-2026-11-21.json.gz)
contains the four-round 8 Mbit/s and three-round 2 Mbit/s two-relay ABBA
comparisons, the three-round 1 Mbit/s emulated WAN ABBA comparison, the
hill-climb history and scorer, and the relay driver used to reproduce them.
See [Round-2 Distribox CPU and latency optimizations](../docs/distribox-performance-round2.md).

[distribox-rfb-reads-2026-11-21.json.gz](distribox-rfb-reads-2026-11-21.json.gz)
contains the four-round comparison through Distribox’s Python relays, the socket
read counts, the hill-climb samples and the fixtures used to reproduce them.
See [Buffering QEMU framebuffer reads](../docs/distribox-rfb-reads.md).

[distribox-path-2026-11-11.json.gz](distribox-path-2026-11-11.json.gz) holds the real-QEMU comparison described in [the Distribox path notes](../docs/distribox-path.md): every raw sample of the final three-build run, the per-step ablation summaries, the hill-climb session record and its evaluator. Inspect it with `gzip -dc benchmarks/distribox-path-2026-11-11.json.gz | python3 -m json.tool | less`.

[wan-path-2026-11-12.json.gz](wan-path-2026-11-12.json.gz) holds the comparison behind an emulated WAN described in [the WAN notes](../docs/wan-path.md): every metric of the eight-link run, the hill-climb session record and its evaluator.

[`scripts/bench-distribox-path.mjs`](../scripts/bench-distribox-path.mjs) runs both benchmarks; the [development guide](../docs/development.md#distribox-path-benchmark) lists what it needs. Results for earlier, retired code paths remain at tag `v0.1.0-alpha.9`, as [the benchmark page](../docs/benchmarks.md) says.
