# Audio isolation on a congested video link

28 October 2026 · `research/audio-performance` · research fixture

A separate audio TCP connection did **not** solve congestion in this test. With
an eight-frame video window, split audio identified only **2/12 timely sound
markers**, versus **0/12** with combined traffic. With one video frame in flight,
the counts were **10/12 split** and **7/12 combined**. The remaining missing codes
rule out treating either configuration as a complete fix.

No production companion endpoint was added. The prototype remains under
`test-support/audio-isolation/`; Gliss's native runtime, production browser
modules, release defaults and Distribox were unchanged for these trials.

## What was compared

Each arm uses one native Gliss/RFB session, the same loopback relay and two
browser WebSockets. In **combined** mode, audio, video and control replies use
the main socket; the companion is idle. In **split** mode, audio packets and
audio-status/control messages use the companion. Video, its real browser ACKs,
input and application pongs stay on the main socket. The relay never creates
an ACK, retries media or opens another native/RFB session.

Both browser sockets terminate on the **same gateway port**. The kernel filters
place both into the **same aggregate 1 Mbit/s queue per direction**, with nominal
80 ms RTT and no injected packet loss. Splitting does not give audio another
1 Mbit/s allowance. The local relay/native hop is unshaped. Both arms pass
through that hop, rather than comparing a direct connection with a proxy.

Two separately registered cohorts ran combined, split, split, combined on
loopback, then the same order on the constrained link. The first fixes eight
video frames in flight; the second fixes one. Both use the same immutable native
executable from `5a6fc848`, H.264 at a 500 kbit/s soft target and 30 fps cap,
disabled native frame skipping, stream stats, preferred Opus and explicit 48 kHz
QEMU mixing. No encoder setting changes between the two routing arms.

Each position has 1.5 seconds of warmup and six seconds of observation. Six
serial-triggered stereo codes have a 700 ms response deadline. Codes 1–48 are
used once per cohort. The same procedural motion source and counter reader as
the [earlier motion study](../audio-motion/README.md) are retained. Source phase
progresses through the positions; this is not a matched-pixel/quality trial or
a real window-manager workload.

## Results

All **16 positions and 96 source commands** completed as valid measurements.
Each loopback arm identified all twelve markers in each cohort. All loopback
control sets were complete. Eight-frame loopback media rates were
**2,531–2,739 kbit/s** despite the 500 kbit/s target, again demonstrating that the
target does not cap this screen-content workload.

On the constrained link with **eight video frames in flight**:

- **Combined:** 0/12 timely audio markers. Three codes arrived at 1.20–1.76
  seconds; nine remained unobserved before cutoff. Only 30/40 pongs arrived.
- **Split:** 2/12 timely markers, at about 387 ms. Ten remained unobserved.
  Only 33/40 pongs arrived.
- Painted marker cadence was 3.67–3.83 distinct IDs/s combined and 3.33–3.67
  split. These counts cover the marker region, not whole-frame fidelity,
  freshness or physical scanout.
- Post-window queue snapshots retained **208–223 kB** of server-to-browser
  backlog: **1.66–1.79 seconds of nominal drain time** at the configured
  125,000 bytes/s. That calculation describes queued bytes, not an independent
  latency measurement. The relay's own queued-byte snapshots were zero; they
  do not include kernel queues.

With **one video frame in flight**:

- **Combined:** 7/12 timely markers at **335–466 ms**, five unobserved, 39/40
  pongs received, and 16–18 distinct painted marker IDs per six seconds.
- **Split:** 10/12 timely markers at **349–417 ms**, two unobserved, 39/40
  pongs received, and 16–17 distinct painted marker IDs per six seconds.
- Each routing arm had one 19/20 control set and one complete set. Per-position
  reply p95 was about 313 ms combined and 304–305 ms split; the incomplete
  positions support conditional p95 only. There is no overall complete-set p95.
- Received media rates were **758–842 kbit/s combined** and **757–799 kbit/s
  split**. The similar marker cadence does not establish equal image quality.

No decoded-video errors, sequence gaps, invalid painted markers, relay buffer
overflows or kernel packet drops were recorded. Browser audio still recorded
local drop-counter events under the constrained link: 27–28 combined and 33–40
split with eight frames, then 20 combined and 18–19 split with one frame. That
counter combines several causes; these values do not identify a particular
playback or decoder path. Late and missing audio codes remain failures even
though the measurement and cleanup gates passed.

## Decision

Keep the isolation prototype out of the production protocol. The modest
one-frame difference is insufficient evidence for a new authenticated channel,
its lifecycle rules and a second Distribox relay path. Two repetitions per
condition also do not establish a general benefit from the three extra timely
codes.

The eight-frame result shows that independent TCP ordering does not remove the
common congested queue. Smaller video windows reduce that backlog, but still
leave missing sound. The next performance work should limit video bursts and
actual encoded demand through pacing, picture-size/quality control or
resolution adaptation. Retain the present freshness limits while comparing
those changes; increasing audio buffering would spend latency instead.

Opus remains the preferred audio codec. This experiment does not change the
previous ADPCM fallback findings or establish that another codec/transport is
best for Distribox. It also does not show that the research relay is faster or
slower than the direct native path: compare routing arms within each cohort.

## Reproduction and verification

[Inventory](results.json) binds the unchanged original reports, prospective
registrations, executable and source hashes. The first source archive includes
the pinned Playwright WebSocket bundle used by the Node relay, with its license
notices. The one-frame archive adds only its two new files and inherits all 135
unchanged parent sources. No binary or credential is published.

```sh
python3 -B benchmarks/audio-isolation/verify.py
```

The verifier checks the original bytes, all positions/probes, marker deadlines,
CPU arithmetic, video continuity, socket identities, correct audio routing,
the single aggregate shaper, and cleanup. All **177 portable tests** passed on
the unchanged native runtime. Four real-WebSocket routing tests
exercise combined/split delivery, exact ACK forwarding, bounded-buffer refusal
and companion-protocol rejection:

```sh
node --test --test-concurrency=1 tests/audio-isolation-relay.test.mjs
```

The opt-in guest controllers are `scripts/bench-audio-isolation.mjs` and
`scripts/bench-audio-isolation-window.mjs`, each accepting `--output NEW.json`.
Use the recorded runtime, an absolute `GLISS_BINARY`, CPU 0 for controller,
relay, browser and native, and CPU 19 for the fresh diskless guest. As in the
motion study, require nice 19, a 3 GiB memory limit, no swap, 256-task cap,
180-second outer watchdog, a fresh user/network namespace, loopback up, private
`/dev` exposing KVM and no physical GPU, and `CAP_NET_ADMIN` confined to that
namespace. Set `GLISS_AUDIO_HOST_NETNS` to the caller's original network
namespace before entering the new one. Existing guests and host networking
were not used or changed.

The host was an Intel Core i5-13600KF with Chromium 149.0.7827.55 and QEMU
11.1.0. CPU percentages use independent counter windows. Controller CPU includes
the relay, scheduling, counters and IPC; it is not an isolated relay cost.
Cumulative relay/kernel counters include warmup, while media/probe counts use
the exact browser window. Native NODELAY readback describes the local upstream
hop. Relay sockets request NODELAY without a retained OS readback. Loopback
segmentation and queue behavior remain qualifications, not an Internet-path
simulation guarantee.

Marker timing ends at an exact Web Audio analyser observation received by the
controller. It includes guest/ALSA buffering, capture, transport, scheduling,
analysis, polling and IPC. It does not measure speakers or lip sync. The
transport facade's receive hook observes browser message delivery, not a NIC
packet timestamp. Whole-frame fidelity, real input responsiveness, long
sessions and browser coverage beyond this Chromium build remain unproved.
