# Direct audio-buffer fill did not justify a player change

28 October 2026 · `research/audio-performance` · CPU only

The candidate writes PCM and decoded Opus samples directly into a Web Audio
buffer, avoiding temporary per-channel arrays and an extra copy. It produced
identical samples, but **failed the preregistered CPU criteria for both codecs**.
The production audio player remains unchanged. Fewer copies did not establish
a useful application performance improvement.

## Paired component

Eight positions ran baseline, direct, direct, baseline for PCM, then the same
order for Opus. Each had 1.5 seconds of warmup and six seconds of measurement:
**300 stereo packets**, 960 samples per channel, at 48 kHz and 20 ms intervals.
The same prepared payload pool was reused across both policies. Opus encoding
ran before measurement. No VM, video, network-media path or native server was
part of this component.

The candidate retained the 60 ms startup buffer, 200 ms scheduled cap, twelve
source limit, eight pending decode limit, 100 ms decode-age limit, sequence
resets and mute handling. Seven source regressions passed, including exact PCM
samples at 1/960/5,760 frames and stale-output release. A separate real Chromium
check compared eight scheduled buffers per policy and codec: every float32
sample matched. PCM also matched the original integer payload independently.
Those checks observe buffers before playback, not speakers.

[Registration](registration.json) froze the sources, plan and criteria before
any cost position ran. Retention required every measurement/sample gate to pass,
plus **at least 10% lower mean CPU and a candidate CPU range entirely below the
baseline range for each codec**. That is a deliberately demanding threshold for
changing the production player. It was not lowered after seeing the data.

## Results

All eight positions were valid. They retained **2,400 timed packets and schedule
callbacks**, with no new drop-counter events, pending outputs at the cutoff or
page errors. Browser process sets were complete at both CPU-counter endpoints.

- **PCM baseline:** **2.97–3.81%** of one core; schedule wall-time totals
  **27.93–31.55 ms** per 300 packets.
- **PCM direct:** **3.32–4.14%**; schedule totals **28.40–28.86 ms**.
- **Opus baseline:** **4.63–5.80%**; schedule totals **34.46–40.41 ms**.
- **Opus direct:** **5.14–6.46%**; schedule totals **33.96–39.19 ms**.

Candidate mean CPU was 9.94% higher for PCM and 11.35% higher for Opus. The
ranges overlap. Two repetitions on a shared host do not prove a regression or
identify its cause; they clearly fail to demonstrate the required reduction.
The small differences in secondary schedule totals also overlap and do not
replace the primary CPU gate. [Decision](decision.json) retains the raw paired
values and exact failed checks.

CPU covers the browser processes, including audio service, renderer and other
reported endpoints. It uses its own counter-acquisition window and denominator.
Schedule wall time wraps the same method in both policies; it includes buffer
creation, fill, node setup and any descheduling during the call. Neither metric
is a source-to-speaker latency measurement.

The last packet is planned at 5,980 ms. Its send and decode/schedule callback
must still finish before 6,000 ms. Thus the fixed cutoff is stricter than the
general 100 ms submission ceiling near the edge. All 300 callbacks in every
position met that intersection of gates; no post-window drain was credited.

## Setup failures retained

The [original setup history](../audio-direct-fill/README.md) preserves two
128-task attempts that produced **zero cost positions**, plus a separate
own-cgroup probe. That probe recorded three thread-denial events at 128 tasks;
the same setup passed at 256 tasks, peak 132, with zero denials. No memory-limit
or OOM event was recorded. A fresh 256-task cohort was then registered, leaving
all media, window, buffer and retention gates fixed.

The final cohort peaked at **133 tasks** and recorded no task/memory admission
denials before or after any measured position. Player resources were closed,
the browser/server stopped and the owner returned exit status zero. The test
used Chromium **149.0.7827.55**, CPU 0, nice 19, private devices and a fresh
network namespace, with 2 GiB memory, no swap, 256 tasks and a 120-second owner
limit. No physical GPU, existing VM, Distribox service or infrastructure changed.

The [T3 source review and disposition](../audio-direct-fill/source-review.md)
record no player correctness finding and several controller robustness limits.
The runner can abort later positions after an invalid one, and an owner kill
can prevent its final report write. Those paths did not occur in this completed
cohort. The runner is retained as experimental evidence, not offered as a
general fault-tolerant benchmark framework.

## Verify

```sh
python3 -B benchmarks/audio-direct-fill-256/test_verify.py
python3 -B benchmarks/audio-direct-fill-256/verify.py
```

[Inventory](results.json) binds original reports, sources, registrations,
validation, the rejection decision and resource probes. The verifier recomputes
CPU arithmetic and the decision, checks every timed send/callback and the
sample-observation identities, and preserves both setup failures as unmeasured.
Two verifier tests include seven mutation denials. Sample hashes are retained
observations; replay is needed to remeasure exact buffer contents.

To repeat, use the registered source archive, the pinned dependency lock and a
fresh output with `scripts/bench-audio-direct-fill-256.mjs --output NEW.json`.
Use the CPU-only private network/device owner described in the
[motion study](../audio-motion/README.md), with this component's 2 GiB/256-task/
120-second limits and no KVM device. The exact source version is archived;
changing a gate, budget or implementation requires a new registration.

This experiment does not test Firefox, real VM contention, long sessions,
perceptual quality or physical output latency. It narrows the current work:
the measured video-burst/quantization tradeoff is useful; this small browser
copy optimization has not earned a production change.
