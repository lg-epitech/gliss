# Full-pipeline audio packet duration

Prospective, unexecuted. This compares native Opus packets of20 and40ms while
holding video at requested QP42..51,500kbit/s,30fps, eight in-flight frames and
no native skipping. Release/default20ms and the production browser are retained.

There are eight ordered positions:20/40/40/20ms on loopback, then the same order
on a private aggregate1Mbit/s,80msRTT kernel path without configured loss. Each
profile starts a fresh owned diskless guest, using the unchanged fixture. A
20-second window improves CPU tick resolution; it does not replace or relabel
any prior six-second result. Six sound codes have the original700ms deadline;
twenty controls have independent receipt/dispatch times. All48 codes are unique.

The observer checks exact GLA1Opus frame counts, wrap-safe packet continuity,
scheduled-output counts and received sample duration. Optional native snapshots
retain all seventeen counters. Those receiver-timestamped cumulative samples
cover only their first/last span, not necessarily the entire window. Raw input
while muted, native input age before RFB ingress, in-progress socket writes,
and unobserved diagnostic tails remain outside that loss accounting.

CPU boundaries include PID/start identities, raw process ticks and CDP endpoint
values. Browser CPU covers the full matched process set. Independent counter
windows must be20..22seconds;10ms tick quantization remains, and two repetitions
on a shared core provide no statistical confidence. Sources, tools and current
native binary are registered before launch. Completed positions are checkpointed;
blocked and partial slots cannot support performance aggregates.

The fixed [selection gate](criteria.json) requires all eight positions eligible,
all candidate sound probes and controls, control p95≤150ms, at least8distinct
painted IDs/s, and at least5% lower mean native-plus-browser CPU with separate
two-position ranges in both profiles. This is only a further-testing decision,
not a significance test or permission to promote40ms. Music quality and physical
speaker latency are not established by the tone-code observer. Marker IDs/s do
not establish whole-frame FPS or scanout.

Root runs pure source checks first, then one registered cohort under a fresh
private user/network/device namespace: CPU0 for native/browser/controller,
CPU19 for the guest, nice19,3GiB/no swap,256tasks,300s outer unit limit and
control-group cleanup. The300s limit includes the270s controller deadline.
No existing VM, Distribox service, GPU device or external endpoint is used.

```sh
node --test --test-concurrency=1 test-support/audio-packet-duration-study/*.test.mjs
python3 -I -B benchmarks/audio-packet-duration/test_evaluate.py
node scripts/bench-audio-packet-duration.mjs \
  --plan benchmarks/audio-packet-duration/plan.json --output /ABS/FRESH/cohort.json
```

The final command requires the root-owned isolation and immutable executable
specified by the registration; it is not an unconfined launch recipe. No result
or improvement is claimed before the cohort, cleanup and offline verifier pass.
