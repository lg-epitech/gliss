# Direct audio-buffer fill: setup history

28 October 2026 · `research/audio-performance`

This experiment tested writing decoded samples directly into a Web Audio
buffer, removing the player's temporary per-channel arrays and extra copies.
The [completed comparison](../audio-direct-fill-256/README.md) rejected the
candidate under its preregistered CPU criteria. The production player remains
unchanged.

Two original 128-task runs stopped before any timed performance positions:

- [First setup](rejected-setup/results.json): PCM samples matched exactly;
  a later browser setup timed out. The original controller did not retain the
  failing phase. All eight cost slots remained blocked.
- [Diagnostic setup](rejected-setup-diagnostic/results.json): PCM and Opus samples
  matched exactly; navigation to the first cost page timed out. All eight cost
  slots remained blocked.

Both reports, registrations and source snapshots were archived before repairs.
They provide no CPU result. Scalar phase logging was the only controller change
between those attempts. Stack logs are hashed rather than published because
they contain private runtime paths.

A separate [navigation probe](navigation/results.json) repeated the setup with
128 then 256 tasks, holding source, 2 GiB memory, CPU affinity and isolation
fixed. The 128-task run reached its cap and recorded **three denied task
creations**. The 256-task run passed, peaked at **132 tasks**, and recorded
**zero denials**. Neither recorded memory-limit or OOM events. Both completed
cleanup and neither ran a timed cost position. This supports resource admission
as the setup problem; it is not a trace of the exact cause of every timeout.

The subsequent 256-task cohort was registered separately. No original audio
deadline, packet cadence, buffer bound, measurement window or retention
criterion was relaxed. The [independent review and disposition](source-review.md)
cover player correctness and the original controller's robustness limits.
All original failures remain present.
