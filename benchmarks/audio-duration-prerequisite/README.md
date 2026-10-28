# Native 40 ms Opus packets reach the existing browser player

28 October 2026 · `research/audio-performance` · CPU only

The existing Chromium player accepted native **40 ms / 1,920-frame** Opus
packets with one decoded output and one scheduled buffer per packet. Both
40 ms arms received and scheduled all **9 packets**, with no drops or pending
outputs. Both 20 ms controls did the same for **17 packets** each. All **52
packets** passed, with matching sample-clock timestamps within the player's
existing one-microsecond tolerance.

This closes a browser-accounting prerequisite for a longer-packet experiment.
It is not a speedup or latency result. The production native packetizer still
uses 20 ms; no option, default or Distribox service changed.

## Scope

Four arms ran 20, 40, 40, 20 ms. Each native encoder used the same clean
320 ms stereo 440/880 Hz tone source, Opus Audio, 96 kbit/s and complexity 4.
Every arm appended one explicit silence packet to flush the 312-frame native
lookahead. Thus the controls decoded 16,320 frames and the 40 ms arms decoded
17,280 frames, including padding. These different padded lengths are not a
throughput comparison. Repeated arms produced identical wire packets.

The example calls the same native Opus configuration as the server and emits
GLA1 headers. It does **not** exercise RFB fragment assembly, native oldest-ingress
aging, capture queues, packet delivery over a real connection, or video/audio
contention. Prepared native packets were submitted locally to the existing
player at their declared 20/40 ms cadence. Actual WebCodecs output geometry,
timestamps, schedules and final resource state were recorded. All page-error
arrays were empty; contexts closed and the browser/server owner exited normally.

The 60 ms startup buffer, 200 ms scheduled cap, twelve-source limit and 100 ms
decode-age budget remained unchanged. No source assembly age, perceptual quality,
physical speaker latency, packet-loss behavior or other-browser support is
proved. See the [source design note](../../docs/performance/audio-packet-duration.md)
for remaining packetizer/observability work.

## Evidence and replay

[Inventory](results.json) binds original native/browser reports, the complete
**106-record** fsynced native journal, fourteen source hashes and prospective
registration. The journal writes occur outside native encode timing. Those
timings are retained diagnostics and are not used to claim a performance win.
The verifier checks every wire header, per-arm corpus hash, output count,
sample timestamp, frame count, resource admission and repeated native bytes:

```sh
python3 -B benchmarks/audio-duration-prerequisite/verify.py
```

Two native tests passed, as did Clippy with warnings denied and the locked,
offline serial release build. [Validation](validation.json) retains log hashes.
The host used Chromium **149.0.7827.55**, CPU 0 and nice 19. The browser owner
had private devices/network, no physical GPU, 2 GiB memory, no swap, 256 tasks
and a 60-second limit. The native component had a 30-second limit and 1 GiB
memory. No existing VM or infrastructure was used.

For a fresh reproduction, build `audio-duration-probe` with `-j1 --locked
--offline --release` and supply a fresh JSON output path. The native program
also reserves a sibling `.progress.jsonl` journal and preserves it on failure.
Then run `scripts/probe-audio-duration.mjs --native NATIVE.json --output NEW.json`
inside the CPU/device/network owner described in the
[motion study](../audio-motion/README.md), using this fixture's limits and no
KVM device. The browser proof uses the registered source closure, including the
unchanged player. Replay is needed to remeasure decoder behavior on another
browser build.
