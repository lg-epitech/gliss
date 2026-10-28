# Video quantization and audio contention

28 October 2026 · `research/audio-performance` · CPU only

Allowing a wider requested QP range substantially reduced this workload's video
traffic without native frame skipping. It did not fully protect sound. On the
80 ms RTT / 1 Mbit/s link, `26..51` identified **8/12 timely audio markers** and
painted **8.83–9.50 distinct source IDs/s**. The unchanged default identified
**0/12 markers** and painted **3.33–3.50 IDs/s**.

The branch exposes `--video-qp-min MIN --video-qp-max MAX` as an opt-in WebSocket
H.264 setting. Defaults, datagram behavior and Distribox remain unchanged. This
is a quality/bandwidth control, not a general speedup or a complete congestion
fix.

## Why this setting

The Rust wrapper's default QP request is `0..51`. The pinned native
[OpenH264 validation](https://github.com/cisco/openh264/blob/v2.6.0/codec/encoder/core/src/encoder_ext.cpp#L375)
substitutes its screen defaults when either requested bound is nonpositive.
Tests confirm that explicitly requesting `26..35` produces the same packets as
the existing default on the bounded reset/resize fixture. An explicit positive
`26..51` request lets the encoder use stronger quantization when its rate
control calls for it.

These are requested bounds. Native validation and macroblock overflow retries
can adjust coded QP; neither the CLI metadata nor these tests attest every
coded quantizer value. GLV1 framing and video sequence rules remain unchanged.
Both CLI bounds are required, with `1 <= MIN <= MAX <= 51`. They are unavailable
with datagrams or native benchmark capture. `hello.video_qp_bounds` declares
the request when enabled. Existing constructors retain their previous bytes;
`VideoOptions` and `QpBounds` expose the optional policy to library callers.

## Shared-input image check

Four component arms ran default, `26..51`, `26..51`, default. Each encoded the
same twelve clean 1280×800 RGBA frames. The generator calls the unchanged guest
pixel function, renders source counters 0, 3, …, 33, and pads the area outside
1024×768 with black. It opens no framebuffer or audio device. Persistent native
encoders used the same 500 kbit/s target, 30/30 timing, 60-frame IDR interval and
disabled skipping. Every emitted picture was decoded by the native decoder.

Both repetitions produced the same packet-size and image-error observations:

- **Default:** 371,824 total packet bytes; full-image RGB mean absolute error
  **4.565**, document-region error **2.397**, texture-region error **46.824**.
- **Requested 26..51:** 104,857 bytes, **71.8% fewer**; corresponding errors
  **6.062**, **3.869** and **59.378**.
- Both emitted twelve pictures with one initial IDR. The largest packet was
  **37,059 bytes in both arms**. Wider upper bounds did not shrink that IDR.

Errors are on a 0–255 RGB scale. The document region contains procedural window
shapes and scrolling stripes, not real font samples or a readability test.
Quantization and YUV420 lose information; the byte reduction buys less image
fidelity. The clean component frames also differ from asynchronous live RFB
captures, so these values do not establish fidelity of each live picture.

Encode wall time for twelve pictures was 80.8–96.1 ms by default and 63.0–65.0 ms
with wider bounds. First encode includes native initialization. Timing excludes
journal writes, native decoding and error calculations. This is not a CPU
speedup at equal quality or a full-pipeline throughput measurement.

## Live motion and sound

Eight positions ran default, wider, wider, default on loopback, then the same
order on the constrained link. Both settings used the same newly built binary,
software H.264, 500 kbit/s target, 30 fps cap, eight in-flight frames, disabled
native skipping, stream stats and preferred Opus. Source, warmup, six-second
windows, six unique codes per position, 700 ms deadline and all observer gates
were inherited from the [motion study](../audio-motion/README.md).

On **loopback**, both settings identified 12/12 markers and answered 40/40
pongs. Default received **2,655–2,661 kbit/s** of audio-plus-video application
media. Wider bounds received **498–516 kbit/s**, with similar marker cadence:
10.33–11.00 distinct IDs/s wider versus 10.33–10.50 default. Source phase differs
between positions; this is not a matched-live-pixel quality comparison.

On the **constrained link**:

- **Default:** 0/12 timely markers; all twelve were unobserved before cutoff.
  Only 29/40 pongs arrived. Conditional per-position wire p95 was 1.98–2.37
  seconds. Marker cadence was 3.33–3.50 distinct IDs/s.
- **Requested 26..51:** **8/12 markers at 234–372 ms**; four were unobserved.
  All 40 pongs arrived, with complete per-position p95 **771–840 ms**. Received
  media was **492–498 kbit/s**, and marker cadence was **8.83–9.50 IDs/s**.

All eight positions were valid measurements. Video had no decode errors,
sequence gaps, invalid painted markers or invalid observed headers. Every
source command was confirmed, all native workers exited normally, private
kernel shapers were removed and the guest was reaped. Kernel snapshots recorded
zero packet drops. Missing or late responses remain failures; green correctness
and cleanup gates do not turn them into timely sound.

Loopback native CPU fell from 12.2–12.6% to 9.6–10.1% of one core with wider
bounds, while browser CPU was 15.9–16.3% default and 16.4–17.2% wider. Under the
constrained link, wider bounds increased CPU because more video/audio work
arrived: native 8.8–10.4%, browser 18.9–19.9%, versus 6.0–6.3% and 8.9–9.3% by
default. A blocked stream's lower CPU cost is not a performance win.

## The remaining burst

Each wider constrained-link position received a periodic IDR of **52,942–53,223
bytes**, around 3.5 seconds into the browser window. Default loopback IDRs were
similarly large. Wider constrained-link P packets had a median of only
**2,395–2,428 bytes**. The upper-QP change shrank ordinary pictures while leaving
a much larger refresh picture.

A 53 kB packet alone needs about 424 ms of serialization at 1 Mbit/s, before
other queued traffic or RTT. The slow control replies and local audio
drop-counter events occurred around the refresh and its recovery interval. That
association identifies the next targeted comparison; it does not prove the
cause of every missing code. The present data does not distinguish all browser
drop-counter causes.

Keep the new bounds opt-in. A higher minimum QP, targeted IDR size control,
resolution adaptation and pacing remain candidates. Increasing the IDR interval
just to move a refresh outside this six-second test would hide the burst; any
follow-up must retain the workload positions and account for refresh behavior.
Retain the current audio freshness limits and buffers. This result also does
not overturn the [separate-audio prototype](../audio-isolation/README.md):
an independent TCP stream did not eliminate the common queue.

## Evidence and reproduction

[Inventory](results.json) binds the eight-position original report, prospective
registrations, source archives, component report and complete 102-record
progress journal. Component attempts and observations are durably written
outside encode timing. The shared-input hash is
`824c59818eb68261460a5c44f3746bb7077b94931339aa5db38f34079169466c`.
No generated RGB file, credential or executable is published.

```sh
python3 -B benchmarks/video-qp/verify.py
```

The verifier recomputes packet/count/timing summaries, checks all audio outcomes,
source bindings, QP declarations/argv, video flags, sequence continuity and
journal consistency. Image error values are retained measurements; replay the
shared-input component to remeasure them. The four component arms and all 48
live probes remain present.

Validation passed **168 server tests, 15 video tests, 178 portable tests** and
Clippy for all server/video targets with warnings denied. Original test/build
failures are hashed in [validation](validation.json): a new CLI test initially
omitted its dummy token, the example needed an allocation check, and the C
wrapper needed GNU feature definitions. Those repairs preceded measurements.

The host was an Intel Core i5-13600KF with Chromium 149.0.7827.55 and QEMU 11.1.0.
Native, browser and controller used CPU 0 at nice 19; the owned diskless guest
used CPU 19. Fresh network/private-device namespaces excluded physical GPUs.
The live cohort retained a 3 GiB memory limit, no swap, 256-task cap and 180-second
watchdog. The component used CPU 0 and the same resource ceilings with a
60-second outer limit. No existing VM, host network policy or Distribox service
was changed.

The opt-in live command is `scripts/bench-video-qp.mjs --output NEW.json`; use
its recorded executable revision and the namespace/resource launch requirements
in the motion study. The component sources are
`test-support/video-qp/fixture.c` and
`crates/gliss-video/examples/video-qp.rs`. Build serially with `-j1 --locked
--offline`, generate a fresh owned RGBA file, then run the example with that
file and a fresh report path. The existing QEMU audio fixture's ALSA development
headers/library are needed to compile the shared C source, although its guest
entry point is never called by the generator.

These short, two-repetition comparisons do not establish production capacity,
perceptual acceptance, real window-manager behavior, long-session stability,
physical speaker/scanout latency or a globally best Distribox codec.
