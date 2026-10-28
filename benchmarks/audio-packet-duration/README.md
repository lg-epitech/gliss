# Full-pipeline audio packet duration

**Completed: eight positions, all 48 sound probes timely, default stays 20 ms.**
The registered further-testing gate failed. All four constrained-link positions
recorded browser drop/reset counter events and were ineligible for the complete
CPU comparison. Those observations remain in the [original cohort](cohort.json.gz);
the [decision](decision.json) contains no comparison aggregates for them.

The 40 ms option remains experimental. Its smaller packet count does not justify
changing the default or the browser jitter buffer.

## Observations

- Loopback: 20 ms sound responses were **185–214 ms**, versus **220–247 ms**
  for 40 ms. On the constrained link the ranges were **247–288 ms** and
  **259–323 ms**. All six codes per position met the unchanged 700 ms deadline.
- Every position answered all twenty controls. Constrained-link control p95
  was **98–109 ms**; source-marker cadence was **9.9–10.7 distinct IDs/s**.
- Audio traffic was **104.0–104.4 kbit/s** at 20 ms and **99.8 kbit/s** at
  40 ms, including Gliss headers. Loopback received exactly **1,000 versus
  500 packets** per window. No received audio sequence gaps were recorded.
- The two healthy loopback 20 ms positions used **9.6–9.8% native CPU** and
  **15.5–15.8% browser CPU** of one core. The 40 ms positions used
  **9.6–10.0% native** and **14.4–14.7% browser CPU**. These are descriptive
  observations; the full eight-position comparison failed eligibility.
- On the constrained link, the browser counter rose **3 and 4 events** at
  20 ms, and **4 and 4 events** at 40 ms. That counter includes queue resets
  and discards; it is not a count of lost packets or proof of audible glitches.
  Its branch of origin was not retained. Sampled pending decodes reached at
  most two, and queued playback reached 177–188 ms. Intervening peaks remain
  unknown.
- Native stale, partial-reset, lag and encoder-error counters showed no
  increments between the first and last retained diagnostics. Those spans
  were about 19 seconds, rather than the entire 20-second window. Their zero
  deltas cannot prove delivery or cover an unobserved tail.

A post hoc [arrival-trace derivation](audit-evidence.json) found sixteen audio
arrival gaps over 100 ms, all on the constrained link. They lasted **149–183 ms**
and ended **2.4–12.1 ms after 18.1–18.4 kB keyframes**. At the configured
125 kB/s rate, one such picture takes roughly 145 ms to serialize. This supports
shared-TCP head-of-line blocking followed by playback resets as an explanation.
The exact reset subcondition was not recorded, and a repeat is not guaranteed
to fail identically. These events affected both packet durations; the failed
gate does not isolate a packet-duration effect.

All **2,000 live 40 ms packets** and **18 prerequisite 40 ms packets** were
499 bytes each, while live 20 ms packets ranged from 252 to 450 bytes. The
encoder settings were the same; observed fixed sizes do not prove VBR was
disabled. Most of the roughly 4.5 kbit/s saving comes from sending 25 fewer
20-byte Gliss headers per second. The waveform check below compares these
encoded outputs, not an assurance of matched quality.

The aggregate browser counter prevents accepting a CPU reduction earned by
clearing scheduled sound. A false `measurement_valid` here means **ineligible
for the registered CPU comparison**; it does not erase the measured responses
or reset events. The [selection criteria](criteria.json) and selector were
unchanged after execution. There is no statistical regression or speedup claim.

## Limited waveform check

A separate, retrospective analysis decoded the previously retained 320 ms
440/880 Hz stereo Opus fixtures using libopus 1.6.1. Reconstruction matched the
original input hash exactly. Each arm used a fresh decoder, the recorded
312-frame lookahead, and exactly 15,360 scored frames after alignment. The
aligned float samples and input PCM are retained alongside [quality.json](quality.json).

Channel SNR was **26.36/27.38 dB** for 20 ms and **25.93/27.01 dB** for 40 ms;
same-duration repetitions were byte-identical. This short two-tone result does
not establish music, voice or subjective quality, browser output fidelity, or
physical speaker latency. It performed no new encoding and is separate from
the prospective live comparison.

## Method and verification

The order was 20/40/40/20 ms on loopback, then the same order on a private
1 Mbit/s, 80 ms RTT kernel path, without configured loss. Each profile used a
fresh owned diskless guest. Video stayed at requested QP 42..51, a 500 kbit/s
target, 30 fps cap, eight in-flight frames and no native skipping. Real video
traffic was lower than that target. The 20-second windows improve CPU tick
resolution and do not relabel any earlier six-second result.

Six unique sound codes and twenty controls per position retain their original
deadlines. Every received audio packet has bounded header/arrival records.
CPU boundaries retain PID/start identities, process ticks and CDP endpoint
values. All endpoint sets were complete. The independent counter windows and
10 ms quantization remain explicit; two repetitions on a shared core provide
no confidence interval. Source-marker IDs/s are not whole-frame FPS or scanout.

Native, browser and controller shared CPU 0 at nice 19; guests used CPU 19.
The owner enforced 3 GiB, no swap, 256 tasks and a 300-second outer deadline in
private user, network and device namespaces. Both guests and all eight native
processes exited, and the owner finished successfully. No existing VM,
Distribox service or physical GPU device was used.

The [prospective registration](registration.json) binds 132 source files and
the immutable native executable. Their bytes remain in [sources.tar.gz](sources.tar.gz).
After execution, the offline verifier was repaired to validate ineligible rows
without accepting them or inventing aggregates. The three analysis-file changes
are separately bound in [results.json](results.json); the original verifier,
runtime, observer, criteria and selector remain available in the registered
archive. The waveform analysis has its own declaration and file identities.

Root validation passed 182 native server tests, 30 packetizer tests, Clippy,
eight observer/assessment tests, seven selector/verifier tests and three pure
waveform-analysis tests. Recompute the published arithmetic without starting
a VM, browser or media decoder:

```sh
python3 -I -B benchmarks/audio-packet-duration/verify.py
python3 -I -B benchmarks/audio-packet-duration/quality_verify.py
```

The live controller is opt-in and requires the private isolation, resource
limits and immutable executable specified above. It is not an unconfined
launch recipe. The release default remains 20 ms.

The final [independent audit](audit.json) reproduced the published figures,
raw CPU arithmetic, failed selection and aligned-waveform metrics. Its scope
excludes independent verification of runtime/test attestations. It also notes
that observer work scales with packet count and that a scheduled-output count
cannot detect sound later cleared from the queue. Those limits favour caution;
no speedup is claimed. `audit_evidence.py` retains the source-set hash recipe
and the post hoc packet-size/keyframe-correlation derivation. Neither changes
the original registration, selector, observations or runtime.
