# Audio and TCP scheduling

The kernel-delayed follow-up supports enabling `TCP_NODELAY` for interactive
streaming. The research branch now uses that default; `--tcp-nodelay false`
remains an explicit opt-out. Public releases and Distribox were not changed.

## Kernel TCP comparison

Another 24 positions repeated the order, audio settings and six-second windows
using kernel `netem`: 40 ms delay and 1 Mbit/s per direction. Filters selected
only the owned native HTTP/WebSocket port. TCP data and acknowledgments were
delayed; the guest's VNC port bypassed both queues. All positions completed,
every measured queue reported zero packet drops, and every installed root queue
was removed before the next position. A separate echo check measured 86.7 ms on
the selected port and 0.5 ms on an unrelated port.

With Opus, both policies returned 40/40 probes. Nagle enabled produced per-run
control p95 of **226.9–228.8 ms**; `TCP_NODELAY` produced **83.4–83.9 ms**.
Sampled playback-queue maxima fell from 178.7–184.0 ms to 76.0–84.0 ms. These
are local playback queues, not source-to-speaker latency. Browser CPU increased
from 7.29–7.59% to 9.12–9.44% of one core as delivery became less batched.
This is a latency/CPU tradeoff, not an across-the-board performance win.

PCM still exceeded link capacity: both policies returned only **20/40** probes.
Reply maxima were 2.824–2.826 seconds with Nagle enabled and 2.788–3.097 seconds
with `TCP_NODELAY`. PCM recorded 19–20 local drop-counter events per position
despite zero recorded kernel packet drops. Socket scheduling cannot fix the
fallback's bandwidth requirement.

The [kernel plan](kernel-plan.json) fixes these positions separately from the
proxy cohort. Kernel queue byte accounting, a 1,024-packet queue limit and
default loopback MTU/segmentation make this a controlled TCP treatment, not an
exact reproduction of a WAN. Counters include HTTP setup and warmup. The two
network models' observations must not be pooled as equivalent repetitions.

The default change passed all 163 native server tests, Clippy, all 151 portable
tests, and the real-QEMU stereo smoke. The new default-policy browser regression
failed on the old binary, then passed on the rebuilt server using actual socket
readback. Both codecs scheduled 151/151 packets with no local drops in the smoke.
The [validation record](default-validation.json) identifies that rebuilt binary
separately from the explicit-policy performance cohorts. The
[kernel component observation](kernel-component.json) retains the independent
port-isolation and removal check.

## Payload-proxy comparison

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

## Reproduction

Verify both cohorts without starting a VM:

```sh
python3 -B benchmarks/audio-transport/verify.py
```

Run `scripts/bench-audio-transport.mjs` with the same bounded, isolated launch
as the [audio study](../../docs/development.md#audio-performance-study), using a
fresh output/unit name. Its socket-policy helper is specific to this study and
leaves the original benchmark sources unchanged.

The kernel variant uses `scripts/bench-audio-kernel.mjs`. Capture
`GLISS_AUDIO_HOST_NETNS=$(readlink /proc/self/ns/net)` **before** entering the
fresh namespace, pass it with systemd's `--setenv`, and add
`--cap-add CAP_NET_ADMIN` to the private `bwrap` invocation. The helper refuses
the host namespace and any preexisting loopback queue. All queue changes occur
inside that disposable namespace. The opt-in
`scripts/test-audio-kernel-link.mjs` checks the two selected directions, an
unshaped port and queue removal without a VM. It needs the same namespace and
capability setup; it is intentionally outside the portable suite.
