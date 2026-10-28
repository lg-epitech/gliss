# Audio performance review

28 October 2026 · branch `research/audio-performance` · baseline `v0.1.0-alpha.4`

Keep Opus as the preferred audio path for Distribox. In the first study it used **104.4 kbit/s**, including Gliss audio headers, and about **3% of one native server core**. The branch now adds a **394.8 kbit/s ADPCM fallback** for browsers without WebCodecs Opus; the [latest comparison](../../benchmarks/audio-fallback/README.md) confirmed timely guest-audio markers under the constrained link. PCM requires **1,544 kbit/s** and remains the lossless/older-peer option. On a modeled 1 Mbit/s connection, PCM delayed application-control replies by seconds. Reducing the browser's jitter buffer would not fix that bottleneck.

The branch fixes two freshness bugs, enables `TCP_NODELAY` by default and negotiates ADPCM support. The latest [motion study](../../benchmarks/audio-motion/README.md) also found video overload: on the modeled 1 Mbit/s link, moving content missed every audio deadline. Opt-in native frame skipping restored 12/12 timely responses but delivered fewer than one distinct video marker update per second. It remains off by default. These changes have not been released or installed in Distribox.

## What changed

The server could retain half an audio packet across a pause, then combine it with new sound and give it the new fragment's timestamp. It now retains the first fragment's RFB ingress timestamp and discards an expired partial packet. The existing age limit remains 100 ms; it does not measure age inside the guest or QEMU.

The browser could schedule an Opus output long after its decode request. `decodeQueueSize` alone also missed work already handed to the codec. The player now tracks at most eight outstanding outputs and discards output more than 100 ms after local receipt, before copying or scheduling it. The existing 60 ms startup buffer and 200 ms playback cap remain unchanged.

The first implementation exposed a separate problem: this Chromium build rounded 10 of 320 decoded timestamps down by one microsecond. Exact matching left entries pending and caused false resets. That candidate was rejected. The corrected version accepts a one-microsecond difference while preserving the original receipt time. Unknown timestamps and expired output still fail the freshness check.

## Measurements

Each of three binaries ran the same twelve positions: muted, Opus, PCM, PCM, Opus, muted on loopback, then the same order on the limited link. Each position had 1.5 seconds of warmup and a six-second observation window. All **36 positions** and their cleanup completed. Missing control replies are retained as missing.

The source was an owned diskless QEMU guest playing 440/880 Hz stereo tones over a static VGA console. Video used software H.264 with a 2 Mbit/s target, a 30 fps cap and four in-flight frames. This does not exercise moving windows or scrolling.

On the corrected binary, loopback measurements were:

- **Muted:** native CPU 0.66–1.16%; browser CPU 1.49–1.98%; no audio traffic.
- **Opus:** native CPU 2.82–3.15%; browser CPU 8.12–9.29%; 104.4 kbit/s per second of received audio samples. All 40 control probes answered; per-run p95 was 1.2–1.7 ms.
- **PCM:** native CPU 1.49–1.65%; browser CPU 7.63–8.08%; 1,544 kbit/s per second of received audio samples. All 40 probes answered; per-run p95 was 3.3–41.8 ms. The occasional roughly 40 ms stall was retained; its cause was not established.

The limited link added a nominal **80 ms RTT and 1 Mbit/s in each direction per connection**, using a bounded FIFO TCP byte-rate/delay model without packet loss:

- **Muted:** 40/40 replies; per-run p95 84.2–85.8 ms.
- **Opus:** 40/40 replies; per-run p95 83.1–86.0 ms. Native CPU was 3.15–3.31%, browser CPU 9.12–9.28%, and received audio traffic 104.4 kbit/s.
- **PCM:** only **22/40 replies arrived before the cutoff**. The replies that arrived had per-run medians of 1.871–1.876 seconds and maxima of 2.691–2.696 seconds. A p95 across all submitted probes cannot be reported from those incomplete observations. Received audio traffic was 988–993 kbit/s, below the stream's 1,544 kbit/s requirement.

PCM's sampled local playback queue stayed at or below 100 ms even while control replies were seconds late. The browser can bound its own scheduled sound while old bytes remain upstream in TCP. Audio does not consume the video ACK window, but audio, video and controls still share the WebSocket connection.

## Comparison with the release

The released baseline's Opus path already had similar steady-state costs: 3.15–3.31% native CPU and 8.79–9.78% browser CPU on loopback; limited-link control p95 was 83.8–86.0 ms. Its limited-link PCM positions also answered only 22/40 probes. These two-repetition ranges do not establish a CPU speedup for the branch.

The first candidate recorded eleven drop-counter events in each six-second Opus position and reached seven or eight pending outputs. The corrected candidate recorded **zero drop-counter events** in all four Opus positions and a **sampled pending-output maximum of one**. The baseline did not expose the pending-output counter, so its recorded fallback zero cannot be used as a comparison.

This is a correctness improvement backed by controlled stale-fragment/output regressions. The steady-state test did not force a 250 ms decoder stall. It also does not establish physical speaker latency, perceptual music quality, lip sync or motion performance.

## Setup and evidence

Native, browser and controller processes shared CPU 0 at nice 19; the guest used CPU 19. The host was an Intel Core i5-13600KF. Chromium 149.0.7827.55 used software rendering and decoding inside fresh network and private-device namespaces, with no physical GPU devices exposed. Each cohort had a 3 GiB memory limit, no swap, 256-task ceiling and a 300-second service watchdog. Existing VMs and Distribox services were not used.

CPU figures are percentages of one core, with independent counter windows. Browser CPU covers the measured browser processes, rather than just the decoder. Two runs per condition on a shared host provide observations, not capacity estimates or statistical confidence. The decoder timing probe uses exact timestamp matching and excludes unmatched outputs; its roughly 1–2 ms matched-output p95 is not a complete latency distribution. Audio byte counts exclude WebSocket, TCP and IP overhead. Samples are taken every 50 ms and can miss intervening peaks.

The [results inventory](../../benchmarks/audio/results.json) binds all three complete compressed JSON cohorts to their original bytes, binary hashes and source commits. It retains every audio observation and control probe, including the rejected candidate. The [timestamp observation](../../benchmarks/audio/timestamp-observation.json) retains the component diagnostic, and the [stereo observations](../../benchmarks/audio/stereo-observations.json) retain both QEMU smoke results. Verify the numerical summaries without starting a VM:

```sh
python3 -B benchmarks/audio/verify.py
```

Validation passed five native audio tests, Clippy for all server targets, and all 149 portable browser/protocol tests. A fresh real-QEMU stereo smoke on the corrected build scheduled all 151 Opus packets and all 151 PCM packets, with no local drops or video decode errors. The distinct channel tones were observed at the Web Audio output. See [development](../development.md#audio-performance-study) for the opt-in benchmark command.

## Next performance work

The subsequent [TCP-policy studies](../../benchmarks/audio-transport/README.md) completed another 48 positions. The payload proxy showed no repeatable benefit, but the kernel-delayed TCP path exposed Opus control p95 of 227–229 ms with Nagle enabled versus 83–84 ms with `TCP_NODELAY`, with all replies received and zero recorded kernel packet drops. The branch now enables `TCP_NODELAY` by default, retaining `--tcp-nodelay false` as an opt-out. Browser CPU rose from roughly 7.3–7.6% to 9.1–9.4% of one core in that comparison. Neither policy fixed PCM overload. The two network models remain separate evidence, not equivalent repetitions or measurements of an Internet path.

The sustained-motion comparison below now confirms congestion from video. Input responsiveness under real window movement and audio/video transport isolation remain unmeasured. Total browser CPU with Opus enabled was roughly 8–9% of one core in the first study; a persistent audio worklet is a candidate to compare with the current per-packet buffer sources, with its benefit still unmeasured. Keep the present jitter buffer until a controlled comparison demonstrates a better tradeoff.

The [guest marker component](../../benchmarks/audio-marker/README.md) now identifies serial-triggered stereo tones at the browser's Web Audio output. Four fresh guests confirmed all 48 marker responses. Explicit 48 kHz QEMU mixing avoided about 3.3 dB of 11 kHz PCM attenuation seen with 44.1 kHz mixing. The response-time ranges overlapped, so this supports a VM audio configuration improvement, not a speedup. The observer includes analysis, polling and IPC; it does not measure physical speakers.

The subsequent fixed-window marker cohort confirmed the fallback problem in actual sound. On the 80 ms RTT / 1 Mbit/s kernel link, Opus identified all 12 markers within the 700 ms observer deadline, at 255–273 ms. PCM identified none within that deadline: three arrived 1.67–3.09 seconds after submission and nine were not identified before the six-second cutoff. Every guest source command was confirmed. [Full latency results and measurement limits](../../benchmarks/audio-marker/README.md#trigger-to-observer-latency) retain all 48 unique probes across eight positions.

The branch now negotiates an independent ADPCM fallback when WebCodecs Opus is unavailable and the server advertises support. Its fixed rate is **394.8 kbit/s**, including Gliss headers. In a new twelve-position comparison, ADPCM identified **8/8 constrained-link markers at 239–267 ms**, Opus identified 8/8 at 248–292 ms, and PCM identified none before the deadline. A separate eight-position cost comparison recorded ADPCM native CPU of 1.82–1.99% and browser CPU of 7.46–7.79% of one core. Opus remains preferred for bandwidth, and PCM remains the lossless compatibility option. The [fallback study](../../benchmarks/audio-fallback/README.md) preserves all positions, rejected Ogg variants, fidelity qualifications and remaining browser/transport limits. Original audio runners are retained byte-for-byte in the [source snapshot](../../benchmarks/audio/retained-sources.json.gz); current runners explicitly select PCM for PCM comparison arms.

Three further motion cohorts retain **24 positions and 144 unique audio probes**, with eight in-flight video frames. Moving content received 2.9–3.0 Mbit/s on loopback despite a 2 Mbit/s target; a separate 500 kbit/s target still received 3.1–3.2 Mbit/s. Both settings missed all audio deadlines under the constrained link. The matched native-skipping comparison then reduced received media to 308–312 kbit/s and identified all 12 constrained-link markers at 241–459 ms, but painted only five distinct source IDs per six-second position. That is a bandwidth/cadence tradeoff, not a general streaming improvement. The branch also dispatches pongs outside the video queue, backed by stalled-decoder and real-browser regressions. [Full measurements, source archives and qualifications](../../benchmarks/audio-motion/README.md) distinguish the old executable, the new paired trial and the baseline observer's listener-order limitation.
