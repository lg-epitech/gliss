# Fixed-criteria confirmation of the low-bandwidth profile

28 October 2026 · `research/audio-performance` · CPU only

Requested **42..51** passed a fresh confirmation against `26..51`. On the
80 ms RTT / 1 Mbit/s link it identified **12/12 sound codes at 243–280 ms**,
answered all **40 control probes**, and painted **9.67–10.83 distinct source
IDs/s**. Complete per-position control p95 was **94–104 ms**. The control
policy identified **9/12 sound codes**, with control p95 **637–826 ms**.

This confirms a useful opt-in low-bandwidth setting on the synthetic workload.
It does not establish acceptable text quality, physical speaker latency,
production capacity or the best codec for Distribox. No default or service
changed.

## Criteria fixed before launch

Selection followed the exploratory [36-versus-42 pair](../video-qp-high-floor/README.md).
The [registration](registration.json) then froze 151 source hashes, the same
native binary, a new [plan](plan.json) and separate [criteria](criteria.json)
before any confirmation position ran. Each candidate position had to meet:

- All six unique sound responses before the unchanged **700 ms** deadline.
- All twenty control replies, with wire p95 **at most 150 ms**.
- At least **48 distinct painted source IDs in six seconds**.
- At least one refresh picture inside the observation window.
- No invalid video headers, decode errors, sequence gaps or observer failures.

All eight measurements and cleanup also had to complete. The unchanged order
was 26, 42, 42, 26 on loopback, then the same on the constrained link. This is
an independent confirmation of a selected candidate, not a new codec search.
The [decision](decision.json) retains every check for all four candidate slots;
all passed.

## Observations

Each position kept 1.5 seconds of warmup, a six-second window, six unique sound
codes and twenty pings. Both policies used Opus, Screen H.264 at a 500 kbit/s
target, 30 fps cap, eight in-flight frames, 60-frame refresh interval, disabled
skipping and unchanged jitter/freshness buffers.

On **loopback**, both identified all twelve codes and answered all forty
controls. Received audio-plus-video application media was **517–532 kbit/s**
at minimum 26 and **341–342 kbit/s** at minimum 42. Marker cadence was
**11.00–11.17 IDs/s** and **10.17–10.33 IDs/s**, respectively. Native CPU was
**9.63–9.93%** versus **9.12–9.43%** of one core; browser CPU was
**16.39–16.77%** versus **15.72–15.76%**. These two-repetition differences
do not establish a CPU speedup at equal quality.

On the **constrained link**:

- **26..51:** 9/12 timely codes at **225–358 ms**; three unobserved. All 40
  pongs arrived, p95 **637–826 ms**. Received media **487–493 kbit/s**,
  marker cadence **9.67–9.83 IDs/s**. Periodic refreshes were **53,455–53,512
  bytes**.
- **42..51:** 12/12 timely codes at **243–280 ms**. All 40 pongs arrived,
  p95 **94–104 ms**. Received media **337–348 kbit/s**, marker cadence
  **9.67–10.83 IDs/s**. Refreshes were **18,123–18,205 bytes**.

All source commands were confirmed. Every native worker exited normally,
private shapers were removed and the guest was reaped. No video error, sequence
gap or invalid marker was observed; kernel snapshots recorded zero packet
drops. All 48 codes remain in the report, including the three missing controls.
The native/browser CPU cost on the constrained link was about 10%/21% of one
core for minimum 42; this is not a capacity measurement.

## Quality remains a tradeoff

The existing shared-input components encoded the same twelve clean frames:
**26..51** used 104,857 bytes with document-region RGB mean absolute error
**3.869** and texture error **59.378**. **42..51** used 54,227 bytes with
errors **4.486** and **61.828**. The [inventory](results.json) binds both
original component references. These are prior, separate quality observations;
no new quality arm ran during this confirmation.

The procedural document has no real font samples. RGB errors on a 0–255 scale
cannot decide readability or perceptual acceptance. Live RFB source phases
also differ. The profile is a further-testing option for a narrow link, with
visible detail loss requiring an operator's judgment. Keep release defaults
until real desktop quality and longer sessions have been checked.

## Verify or repeat

```sh
python3 -B benchmarks/video-qp-confirmation/test_evaluate.py
python3 -B benchmarks/video-qp-confirmation/verify.py
```

Four evaluator regressions passed. The unchanged runtime's 180-test portable
validation is linked from [validation](validation.json). The verifier checks
all probes, original bytes, source bindings, policy declarations, native argv,
control distributions and the exact recomputed decision. Image-error claims
remain scoped to the separate components.

For a fresh replay, use `scripts/bench-video-qp-range.mjs --plan
benchmarks/video-qp-confirmation/plan.json --output NEW.json` inside the fixed
namespace/resource owner described in the [motion study](../audio-motion/README.md).
Native/browser/controller used CPU 0 at nice 19; the owned diskless guest used
CPU 19. Private devices excluded physical GPUs. Bounds were 3 GiB memory, no
swap, 256 tasks and a 180-second watchdog. Source additions inherit the frozen
parent archives without overriding members. No existing VM, infrastructure or
Distribox service was changed.

Sound timing includes analysis, polling and IPC, not physical speakers. Marker
cadence is not whole-screen FPS, coherence or scanout. The test had no packet
loss and only two positions per condition; it does not establish long-session
reliability or behavior on the user's Quebec–France route.
