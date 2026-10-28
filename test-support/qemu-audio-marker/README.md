# Guest audio markers

This opt-in fixture boots a disposable diskless HDA guest. A serial command
selects one of 48 stereo tone pairs for 160 ms. The guest confirms the exact
request token and source sample index. The host timestamps command submission
and the browser's output-side observation on the same monotonic clock.

The observer taps future Gliss buffer sources through two native Web Audio
analysers. It classifies 20 ms windows and requires two matching windows at least
20 ms apart in the audio clock. Its 10 ms polling, analysis time, and IPC are
part of the observed response. AudioContext output can render ahead of the
physical device; these are **not speaker latency measurements**. The guest
requests a 40 ms ALSA buffer. The source-note receipt is diagnostic and is not a
synchronized guest-to-host timestamp.

The retained `max_poll_gap_ms` field measures gaps between accepted analysis
windows, which are at least 20 ms apart, rather than every 10 ms timer wake.

Both channel frequencies must identify the same code. Silence, the 440/880 Hz
baseline, competing tones, mismatched channels and nonfinite samples fail the
classifier. Perfect same-code corruption remains indistinguishable. A study
using all 48 codes once can avoid crediting an old code as a fresh response.

The builder is a byte-identical snapshot of the original QEMU audio builder;
its adjacent `init.c` provides the serial-controlled source. The original
smoke, benchmark fixtures and retained studies remain unchanged.

Use the bounded private-device/network launch from the
[audio development guide](../../docs/development.md#audio-performance-study),
substituting this payload and a fresh unit name:

```sh
env GLISS_MARKER_MIX_FREQUENCY=48000 node scripts/test-qemu-audio-marker.mjs
```

The codec component tests codes 1, 10, 20, 30, 40 and 48 through native Opus and
PCM. The mixer study uses explicit 44.1/48/48/44.1 kHz order, with a fresh guest
each time. [Retained results](../../benchmarks/audio-marker/results.json) include
the complete original logs. [Rejected observer methods](../../benchmarks/audio-marker/observer-methods.json)
record the AudioWorklet-loading failures; those runs produced no marker latency
results. The analyser approach leaves native CSP and transport unchanged.
