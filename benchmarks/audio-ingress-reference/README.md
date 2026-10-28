# Audio ingress timing leaves less headroom for larger packets

28 October 2026 · `research/audio-performance` · CPU only

Four fresh-guest reference positions recorded **2,427 RFB audio chunks**. A
model using the actual chunk sizes and ingress timestamps reached roughly
**21–27 ms** maximum oldest-fragment age for 20 ms packets and **41–47 ms**
for 40 ms packets. Neither model expired data under the existing 100 ms limit.
The 40 ms model therefore left about **53 ms** in its worst observed case
before encoding and writer delay. This is headroom, not a latency guarantee.

## What ran

One owned diskless QEMU guest ran a fixed static, moving, moving, static order.
Each native connection requested one initial framebuffer update to negotiate
RFB audio, enabled 48 kHz stereo s16le, discarded 1.5 seconds of warmup, then
recorded an exact six-second ingress window. It did not request ongoing video
updates, encode audio, or stream media to a browser. Motion exercises the
guest's procedural renderer; it does not reproduce full video contention.

The median chunk was **1,940 bytes**, about ten milliseconds of stereo audio.
Chunk boundaries did not align with output packets. A tiny remaining fragment
can start the next packet's age, so assembly ages approached the full target
duration even though chunks were smaller:

- **20 ms model:** 299–300 completed packets per position, median ingress age
  **20.21–20.25 ms**, p95 **20.31–22.21 ms**, maxima **20.73–26.52 ms**.
- **40 ms model:** 149–150 packets, median **40.43–40.47 ms**, p95
  **40.58–42.49 ms**, maxima **41.01–46.72 ms**.

Each window received **1,150,544–1,152,852 bytes**. Raw-receiver lag and in-window
Begin/End transitions were zero. The largest measured ingress-to-consumer delay
was **5.30 ms**. All four native workers exited normally, audio was disabled,
and the owned guest was stopped and reaped. Pending tail bytes at the cutoff
remain pending; they are not relabeled as expired data.

The RFB reader timestamps **after receiving each complete chunk**. These clocks
do not measure age inside the guest, QEMU's mixer or before the wire read. A
whole packet-sized batch can arrive with almost zero local ingress age. The
modeled results establish this fixture's assembly behavior, not a universal
20/40 ms delay rule.

## Limits and next work

Both assembly models replay the same raw events and retain the original
100 ms oldest-ingress budget. They exclude codec execution, socket queues and
video streaming. The production packetizer remained unchanged. Actual 40 ms
stale/drop counters and full live comparisons are still required; these modeled
zeros are not measured production discard counters.

The independent [browser prerequisite](../audio-duration-prerequisite/README.md)
already confirmed one 1,920-frame output per native 40 ms packet on Chromium149.
That closes a separate accounting question. Neither result proves better CPU,
waveform quality, source-to-speaker latency or behavior on the Quebec–France
path. The proposed policy remains opt-in with the 20 ms release default intact.

## Evidence

```sh
python3 -B benchmarks/audio-ingress-reference/verify.py
```

[Inventory](results.json) binds every original native report, the complete
cohort, sixteen frozen source hashes and prospective registration. The verifier
replays both models from the raw event sizes and timestamps, checks every
modeled age and byte balance, and preserves all four positions. The source
timestamps and chunk observations remain measurements requiring replay to
remeasure; the model calculations can be recomputed offline.

Two native model tests, Clippy with warnings denied and the locked/offline
serial release build passed. [Validation](validation.json) records the log
identities. The cohort used native/controller CPU 0, guest CPU 19 and nice 19,
with 3 GiB memory, no swap, 256 tasks and a 90-second owner limit. Devices and
network were private; KVM was exposed for the fresh guest and physical GPUs
were absent. No existing VM, infrastructure or Distribox service was changed.

The replay entry points are `audio-ingress-probe` and
`scripts/probe-audio-ingress.mjs --output NEW.json`, using the registered binary
and the resource/network/device owner described in the
[motion study](../audio-motion/README.md). Reserve fresh output names. The
controller persists each completed position before starting the next and
retains blocked positions after a failure.
