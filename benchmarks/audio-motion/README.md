# Audio during desktop motion

28 October 2026 · `research/audio-performance` · CPU only

Video congestion can delay Opus audio even though audio decoding bypasses the
video queue. On an 80 ms RTT / 1 Mbit/s kernel link, this moving desktop missed
**all 12 audio deadlines** with the original 2 Mbit/s video target. The same
static desktop identified **12/12 markers at 288–304 ms**. The shared TCP
connection is the immediate limit in this workload.

The branch adds two changes:

- Pong callbacks bypass video decoding and painting. A controlled stalled
  decoder regression fails on the old client and passes on the corrected
  client, including a real Chromium/WebSocket check.
- `--video-frame-skip true` allows OpenH264 to discard current pictures for rate
  control. It is **off by default**. Skips emit no packet, consume no video ACK
  credit and create no wire sequence gap. This option supports WebSocket H.264
  without native benchmark capture; it does not change datagram streaming.

The skipping trial recovered timely audio but reduced video updates too far
for a default desktop policy. A smaller video window recovered 9/12 timely
markers while delivering more video updates. Distribox and release defaults
were not changed.

## Four fixed comparisons

Each comparison ran eight positions with 1.5 seconds of warmup and a six-second
browser window. Every position submitted six unique stereo tone codes through
an owned guest's serial input. The response deadline was 700 ms. All **32
positions and 192 source commands** completed; late and unobserved responses
remain in the original reports.

The source draws document shapes, scrolling stripes and a 360×180 animated
texture at a nominal 30 Hz. A large binary counter identifies its own region
on the painted Canvas. It is a procedural workload, not a real window manager.
The rendered area is 1024×768 inside the observed 1280×800 RFB surface. Video
uses H.264, a 30 fps cap and **eight in-flight frames**, matching the read-only
Distribox backend defaults at review time. Operator overrides and running
sessions were not inspected. Earlier static audio studies used four frames.

**Motion baseline:** static, motion, motion, static on loopback, then the same
order on the constrained link. The executable comes from `9b4de093`.

- Loopback motion received **2,892–2,991 kbit/s** of audio-plus-video application
  media. Both positions identified 6/6 audio markers. They painted **10.83–11.17
  distinct marker IDs per second** despite the nominal 30 Hz source.
- Constrained motion identified **0/12 timely markers**. Two exact codes arrived
  at 2.36–2.40 seconds; ten were unobserved by the cutoff. Only **28/40 pongs**
  arrived. Conditional per-position wire p95 was 2.41–2.44 seconds; an overall
  p95 cannot be calculated from the incomplete sets.
- Constrained static identified **12/12 markers**, answered **40/40 pongs** and
  received about **108–110 kbit/s**. Control p95 was 85.6–85.8 ms.

**Bitrate-only comparison:** 2 Mbit/s, 500 kbit/s, 500 kbit/s, 2 Mbit/s, all
with motion; repeat on the constrained link using the same older executable.

- The 500 kbit/s target still received **3,058–3,219 kbit/s** on loopback.
- On the constrained link, both 500 kbit/s positions identified **0/12 timely
  markers** and answered **29/40 pongs**. Both 2 Mbit/s positions also missed all
  twelve audio deadlines.

A lower target alone was ineffective here. [OpenH264's known issues](https://github.com/cisco/openh264#known-issues)
state that disabling frame skipping prevents its rate control from respecting
the target. The measured rates are application bytes, not a configured-bitrate
or matched-quality comparison.

**Frame-skipping comparison:** disabled, enabled, enabled, disabled, all with
motion and a fixed 500 kbit/s target; repeat on the constrained link. Both arms
use the same new executable (`5a6fc848`), direct pong dispatch and stream stats.

- **Disabled, loopback:** 2,657–2,809 kbit/s; **10.33–10.83 distinct painted
  marker IDs/s**; 12/12 audio markers.
- **Enabled, loopback:** 311–352 kbit/s; **0.83–1.00 distinct painted marker
  IDs/s**; 12/12 audio markers. Native CPU fell from 11.62–12.24% to 5.14–5.15%
  of one core; browser CPU fell from 15.87–16.44% to 10.13–10.60%. Much less
  video was delivered, so these reductions are not an equal-work speedup.
- **Disabled, constrained:** 0/12 timely audio markers; **30/40 pongs**;
  conditional per-position wire p95 2.19–2.81 seconds. One audio code arrived at
  2.26 seconds; eleven remained unobserved.
- **Enabled, constrained:** **12/12 audio markers at 241–459 ms**, **40/40 pongs**,
  complete per-position wire p95 297–331 ms and **308–312 kbit/s**. Each position
  painted only **five distinct source IDs in six seconds**.

All emitted video sequences in the skipping comparison were contiguous, with
zero decode errors and zero invalid painted markers. Enabled sessions reported
78–88 cumulative native skips in their last in-window stats sample. Disabled
sessions omit that field; absence is not a measured zero. All kernel queue
snapshots recorded zero packet drops and every private shaper was removed.

**Video-window comparison:** eight, one, one, eight in-flight frames; repeat on
the constrained link. The 500 kbit/s target, new executable, stream stats and
disabled native skipping stay fixed. The controller records and checks each
native process's window argument before measurement.

- **Loopback:** both windows identified all 12 markers, answered all 40 pongs
  and painted 10.50–11.17 distinct source IDs/s. The one-frame positions received
  2,718–2,726 kbit/s; the eight-frame positions received 2,694–2,806 kbit/s.
- **Eight, constrained:** 0/12 timely markers, 32/40 pongs, conditional wire
  p95 2.37–2.42 seconds, and 3.50–3.67 distinct painted source IDs/s.
- **One, constrained:** **9/12 timely markers at 323–431 ms**, **40/40 pongs**,
  complete per-position wire p95 **307–314 ms**, about **800 kbit/s**, and
  **17 distinct painted source IDs per six-second position**. Three exact audio
  codes were unobserved before cutoff; the smaller window is not a complete fix.

All eight positions were valid with contiguous video sequences, zero decode
errors, zero invalid paint markers and confirmed cleanup. The first controller
revision failed module linking before any guest/browser launch. Its source,
registration and sanitized log remain in the inventory; the measured revision
was registered separately before execution.

## What this changes for Distribox

Keep Opus preferred. Optimizing its roughly 104 kbit/s stream cannot solve a
video stream demanding several Mbit/s on a 1 Mbit/s link. Bounding local audio
queues also cannot remove bytes already queued in TCP. The new control callback
fix removes a separate local delay; it does not isolate control or audio traffic
from network congestion.

Native skipping is an opt-in safeguard for an overloaded connection. At the
selected 500 kbit/s target, its video cadence is unsuitable for scrolling or
window movement. Retain the default until a comparison demonstrates a better
motion tradeoff. A one-frame window is already Gliss's default and is the better
starting point for constrained links in this workload, although it still missed
three audio codes. Raising it to eight was a Distribox backend default observed
in source, not a requirement of Gliss. No deployed setting was changed.

The retained choices expose a tradeoff: a smaller window improves audio/control
delivery with moderate video cadence; aggressive skipping protects all tested
audio markers at a much lower video cadence. Neither establishes whole-frame
quality or freshness. Resolution or quality adaptation, picture-size control
and separate audio/control transport remain candidates.
The roughly 11 distinct marker IDs/s on uncongested loopback also warrants
capture-cadence investigation. This trial does not establish its cause.

## Evidence and qualifications

[Inventory](results.json) binds the three unchanged result reports, declarations,
source archives and executable hashes. [Validation](validation.json) records
167 server tests, 13 video tests, 173 portable tests and Clippy for all server
and video targets. It also retains hashes of original readiness failures and
the first server compile failure. Those failures preceded their repairs; none
was counted as a passing comparison position.

```sh
python3 -B benchmarks/audio-motion/verify.py
```

The verifier recomputes position/probe counts, marker outcomes, application
rates, CPU arithmetic and video/control continuity from the original reports.
Archives contain the actual controller/helper bytes; the skipping archive also
contains its runtime sources. The earlier executable's sources are retained in
the [runtime snapshot](../audio-fallback/runtime-sources.json.gz).

The first readiness assumptions failed: the initial Canvas was black, the RFB
surface differed from the guest framebuffer geometry, and QEMU sent a complete
initial RFB update despite a crop request. The final readiness check verified
source pixels independently, waited for the actual source marker and observed
the correct image 53.8 ms after the initial black picture. The raw helper now
streams that response within fixed byte and time caps. Original failed and
successful log identities remain in validation.

The host was an Intel Core i5-13600KF, Chromium 149.0.7827.55 and QEMU 11.1.0.
Native, browser and controller shared CPU 0 at nice 19; the diskless guest used
CPU 19. Fresh network/device namespaces excluded physical GPU devices. Each
cohort had a 3 GiB memory ceiling, no swap, 256-task cap and 180-second watchdog.
Only the owned native port was shaped; existing services and VMs were untouched.

Important limits are retained in the inventory:

- Marker cadence covers its sampled region. It does not prove whole-frame
  coherence, text fidelity, physical scanout or smooth motion.
- Tone timing ends at the controller's receipt of an exact analyser code. It
  includes the guest, ALSA buffering, capture, transport, browser scheduling,
  analysis, polling and IPC. It is not speaker latency or lip sync.
- The source phase progresses through the cohort. Repetitions share a workload
  class rather than identical source pixels; quality was not matched.
- The baseline's EventTarget capture flag did not prove listener order. Small
  negative dispatch-minus-wire gaps are preserved. Later cohorts install the
  wire hook before the production handler; only those support ordered gaps.
- Application rates exclude WebSocket/TCP/IP and controls. Kernel counters also
  include HTTP and warmup. CPU uses separate counter windows with complete
  browser endpoint sets. Two readings do not establish capacity or confidence.
- The baseline did not retain guest image digests. Later cohorts did. Two old
  marker provenance-parent files were hashed at result collection but omitted
  from prospective lists; they were not the motion fixture executed. These
  omissions are qualified, not retrospectively attested.

The opt-in controllers are `scripts/bench-audio-motion.mjs`,
`scripts/bench-audio-budget.mjs`, `scripts/bench-audio-skip.mjs` and
`scripts/bench-audio-window.mjs`. Use the
recorded executable/source revision for each, an absolute `GLISS_BINARY`, a
fresh output, and the same resource and CPU restrictions. The constrained
studies require a fresh user/network namespace, loopback up, private `/dev`
with only KVM added, `CAP_NET_ADMIN` inside that namespace, and
`GLISS_AUDIO_HOST_NETNS` set to the caller's original network namespace.
They refuse a visible GPU device or the original network namespace. Do not
substitute a production guest or shape the host network.
