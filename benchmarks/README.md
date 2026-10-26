# Measurements

[reference.json](reference.json) is a portable extract of the historical desktop experiments, not a timing run of the current build. It contains all 20 terminal trials, all 36 moving-desktop positions and the 12 kernel-loss summary groups, including excluded results. Report and executable hashes identify their provenance.

Run `python3 -B benchmarks/verify.py` from the repository root to recompute the terminal percentiles, whole-sequence traffic and CPU totals, and check the motion eligibility counts. The verifier reads only the shipped extract.

[motion/](motion/) holds the per-run results of the moving-desktop comparison between Gliss all-IDR video and the WebRTC baseline, including failed runs.

The `codec-bench` executable generates deterministic synthetic frames, verifies decoded pixels and records codec CPU time and bytes. It creates its selected output parent and replaces the output file. This is a component measurement, separate from input-to-browser latency, guest capture, network behavior or a Guacamole comparison.

```sh
cargo run --locked --release -p gliss-codec -j 2 --bin codec-bench -- \
  --repetitions 3 --output benchmarks/output.json
```

Units and limitations are explained in [the benchmark guide](../docs/benchmarks.md).
