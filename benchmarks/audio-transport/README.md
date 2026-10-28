# Audio and TCP scheduling

The 28 October 2026 matched cohort tested the existing explicit
`--tcp-nodelay false|true` option on the corrected audio binary. All 24 positions
completed. Each native worker reported its actual accepted-socket option
readbacks in the WebSocket hello; every position verified its requested policy.
Neither the audio codec settings nor the production TCP default changed.

Each audio mode and network had two repetitions per policy, in reversed order.
Warmup was 1.5 seconds, observation six seconds, and 20 control probes were sent
per position. The [plan](plan.json) fixes the order and resource limits. The
[inventory](results.json) binds the complete compressed JSON to its original
bytes, measured binary and source hashes. CPU placement and isolation match the
[audio study](../../docs/performance/audio.md).

On the limited link, both policies' Opus positions returned 40/40 probes. Per-run
p95 was 86.4–86.9 ms with Nagle enabled and 83.0–86.0 ms with `TCP_NODELAY`.
PCM returned only 22/40 probes under either policy. Received reply maxima were
2.715–2.731 seconds with Nagle enabled and 2.695–2.716 seconds with
`TCP_NODELAY`. These incomplete probe sets do not support an overall p95.

Loopback p95 values overlapped: Opus 1.6–1.8 ms versus 1.5–2.3 ms; PCM
1.2 ms versus 1.2–1.6 ms. This cohort did not reproduce the earlier 40 ms tail
often enough to attribute it or justify changing the default. All positions
recorded zero local drop-counter events; sampled Opus pending output stayed at
or below one.

The proxy limits application bytes to 1 Mbit/s per direction and adds a nominal
80 ms RTT. It terminates TCP locally: the native kernel's ACK path is **not**
delayed by 80 ms. The result demonstrates that the option does not fix this
PCM queue overload; it is not a general test of Nagle over a real WAN.

Verify without starting a VM:

```sh
python3 -B benchmarks/audio-transport/verify.py
```

Run `scripts/bench-audio-transport.mjs` with the same bounded, isolated launch
as the [audio study](../../docs/development.md#audio-performance-study), using a
fresh output/unit name. Its socket-policy helper is specific to this study and
leaves the original benchmark sources unchanged.
