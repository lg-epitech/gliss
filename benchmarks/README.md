# Measurements

[distribox-path-2026-11-11.json.gz](distribox-path-2026-11-11.json.gz) holds the real-QEMU comparison described in [the Distribox path notes](../docs/distribox-path.md): every raw sample of the final three-build run, the per-step ablation summaries, the hill-climb session record and its evaluator. Inspect it with `gzip -dc benchmarks/distribox-path-2026-11-11.json.gz | python3 -m json.tool | less`.

[wan-path-2026-11-12.json.gz](wan-path-2026-11-12.json.gz) holds the comparison behind an emulated WAN described in [the WAN notes](../docs/wan-path.md): every metric of the eight-link run, the hill-climb session record and its evaluator.

[`scripts/bench-distribox-path.mjs`](../scripts/bench-distribox-path.mjs) runs both benchmarks; the [development guide](../docs/development.md#distribox-path-benchmark) lists what it needs. Results for earlier, retired code paths remain at tag `v0.1.0-alpha.9`, as [the benchmark page](../docs/benchmarks.md) says.
