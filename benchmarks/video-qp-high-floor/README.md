# Higher quantization versus audio interruption

28 October 2026 · `research/audio-performance` · CPU only

Requested **42..51** was the first non-skipping policy in these moving-content
comparisons to identify every constrained-link sound code: **12/12 at
243–304 ms**. It answered all 40 control probes with per-position wire p95
**96–100 ms**. That result costs image fidelity and needs independent
confirmation before it becomes a recommendation for deployment.

## Live results

Eight positions ran `36..51`, `42..51`, `42..51`, `36..51` on loopback and then
on the 80 ms RTT / 1 Mbit/s link. The native binary, source, six-second windows,
six unique sound codes per position, 700 ms deadline, buffers and resource caps
match the [26-versus-36 study](../video-qp-floor/README.md). Both retain the
500 kbit/s Screen H.264 target, 30 fps cap, eight in-flight frames, 60-frame
refresh interval, Opus and disabled native skipping.

On loopback, both identified all twelve codes and answered all forty pings.
Minimum 36 received **396–414 kbit/s** of audio-plus-video application media;
minimum 42 received **335–351 kbit/s**. Both painted roughly ten to eleven
distinct source IDs per second. On the constrained link:

- **36..51:** **11/12** timely codes at **239–500 ms**, one unobserved. All 40
  pongs arrived; per-position wire p95 **132–204 ms**. Received media
  **373–392 kbit/s**, marker cadence **9.50–10.33 IDs/s**.
- **42..51:** **12/12** timely codes at **243–304 ms**. All 40 pongs arrived;
  per-position wire p95 **96–100 ms**. Received media **356–360 kbit/s**,
  marker cadence **10.67–11.33 IDs/s**.

Every position included one periodic IDR. On the constrained link it was
**31,724–31,882 bytes** at minimum 36 and **18,131–18,218 bytes** at minimum
42. An 18 kB packet still needs about 145 ms to serialize at 1 Mbit/s. This
reduces a burst; it provides no media priority and cannot discard bytes already
queued in TCP.

All measurement and cleanup gates passed, with no video decode errors, sequence
gaps, invalid markers or recorded kernel packet drops. Native workers exited
normally, private shapers were removed and the guest was reaped. The earlier
study's **10/12** count for minimum 36 remains separate from this study's 11/12.

## Image cost on identical inputs

Four component arms used the same twelve clean frames, ordered 36, 42, 42, 36.
All 48 pictures decoded, with identical observations across repetitions:

- **36..51:** **87,300 packet bytes**, largest picture **31,824 bytes**;
  document-region RGB mean absolute error **3.588**, texture error **60.169**.
- **42..51:** **54,227 bytes**, largest picture **18,222 bytes**;
  document error **4.486**, texture error **61.828**.

The **37.9%** byte reduction came with higher error in both regions. These
0–255 RGB errors do not establish readability or perceptual acceptance.
The procedural document contains no real fonts. The texture intentionally
contains difficult moving detail. Live phases differ, so this component does
not measure each streamed frame's fidelity. Its minimum-36 control matches
the frozen preceding component, excluding wall timing.

Keep the range opt-in. Two successful repetitions do not prove long-session
reliability, capacity, other resolutions or the best quality for Distribox.
A separately registered confirmation with fixed acceptance criteria is next.
No production default or service changed.

## Verification and reproduction

```sh
python3 -B benchmarks/video-qp-high-floor/verify.py
```

[Inventory](results.json) retains all eight live positions, 48 sound probes,
four image arms, the 102-record journal, prospective registrations and original
bytes. Six new source files inherit 141 unchanged parents. The verifier checks
those bindings, every outcome, complete control distributions, native arguments
and the shared-input control match. Replay to remeasure image error and timing.

The reusable live command is:

```sh
node scripts/bench-video-qp-range.mjs \
  --plan benchmarks/video-qp-high-floor/plan.json --output NEW.json
```

Run it inside the CPU/device/network isolation and resource owner described in
the [motion study](../audio-motion/README.md), using the registered executable.
The range policy rejects changes to every inherited workload, budget, deadline
or buffer field. Only an ordered pair of positive requested minimum QPs may
vary; maximum stays 51. Build `video-qp-range` serially with `-j1 --locked
--offline`, then provide the unchanged input, a fresh report path and
`36 42 51`. The [parent component](../video-qp/README.md) documents generation
and the input hash. No raw RGB file or executable is published.

The portable suite passed **180 tests**. An earlier policy test reversed a
palindromic ABBA order, leaving it unchanged. Correcting that test mutation
made both new regressions pass. The original failure and repaired run are
hashed in [validation](validation.json), before prospective registration.
No measured source changed after registration.

Native/browser/controller used CPU 0 at nice 19; guest used CPU 19. Private
devices excluded physical GPUs. Limits were 3 GiB memory, no swap, 256 tasks
and a 180-second live watchdog. Sound timing includes analysis and IPC, not
speakers. Marker IDs are not whole-frame FPS, coherence or scanout. No existing
VM, infrastructure or Distribox setting was changed.
