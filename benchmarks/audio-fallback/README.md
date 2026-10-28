# Browser audio fallback

28 October 2026 · `research/audio-performance` · CPU only

Keep native WebCodecs Opus as the preferred path. When that API is unavailable,
the branch now negotiates **IMA ADPCM** instead of PCM with a compatible server.
It uses **394.8 kbit/s**, including Gliss headers, versus PCM's **1,544 kbit/s**.
It preserves stereo and 48 kHz sampling, but is lossy. Older peers retain PCM.
The [packet specification](../../docs/audio-wire.md) defines the format.

The matched guest-marker study ran twelve positions: Opus, ADPCM, PCM, PCM,
ADPCM, Opus on loopback, then the same order on an 80 ms RTT / 1 Mbit/s kernel
link. Every position had 1.5 seconds of warmup, six seconds of observation and
four serial-triggered, uniquely coded stereo tones. All 48 guest commands were
confirmed. The 700 ms observer deadline and original failures are retained.

- **Loopback:** all three codecs identified 8/8 markers. Opus took 188.6–219.7 ms,
  ADPCM 177.9–222.4 ms, and PCM 181.3–221.0 ms.
- **Constrained link, Opus:** 8/8 within deadline, at 247.7–292.3 ms;
  received audio rate about 104.2 kbit/s.
- **Constrained link, ADPCM:** 8/8 within deadline, at 239.1–266.9 ms;
  394.8 kbit/s. Both positions recorded zero local drop-counter events.
- **Constrained link, PCM:** 0/8 within deadline. Four exact markers arrived
  2,307.3–3,089.7 ms after submission; four were unobserved by the cutoff.
  Both positions recorded 21 local drop-counter events.

All positions were valid measurements, all native workers exited normally, all
kernel queues recorded zero packet drops, and cleanup completed. An unobserved
marker is not evidence that it could never arrive later. These timings end at
the controller's receipt of an exact Web Audio analyser code; they include guest
dispatch, the requested 40 ms ALSA buffer, capture, codec, TCP, browser buffering,
analysis, polling and IPC. They do not measure physical speakers. The existing
60 ms startup buffer and 200 ms playback cap were unchanged.

Eight additional loopback positions measured whole-pipeline cost on the static
440/880 Hz guest fixture: muted, Opus, ADPCM, PCM, PCM, ADPCM, Opus, muted. The
two measurements per mode, as percentages of one core, were:

- **Muted:** native 1.16–1.49%; browser 1.49%.
- **Opus:** native 3.48–3.65%; browser 9.78–10.28%.
- **ADPCM:** native 1.82–1.99%; browser 7.46–7.79%.
- **PCM:** native 1.82–1.99%; browser 8.13–8.78%.

All 160 application-control probes answered before cutoff; per-position p95
was 1.1–1.9 ms. Native and browser CPU use independent counter windows, and
browser counts include measurement hooks. Two readings on one shared host do
not establish production capacity or a general CPU ranking. This cost study
retains the original fixture's default QEMU mixer; the marker study uses
explicit 48 kHz mixing. Neither exercises scrolling or moving windows.

## Why the fallback changed

Four component rounds preceded integration. Native Web Audio decoded complete
Ogg Opus files with WebCodecs disabled and the existing CSP. Exact-duration
trimming worked, but resetting the encoder every 20/40 ms increased boundary
errors. Adding 20–80 ms of discarded pre-roll improved overall tonal distortion
without fixing those boundaries. A rolling encoded-packet window also decoded
correctly but differed from the continuous native decoder. Those variants were
not promoted. Their original reports, declarations and source archives remain
in the results inventory. Ogg pre-skip and end trimming follow
[RFC 7845](https://www.rfc-editor.org/rfc/rfc7845.html); complete-file decoding
uses [Web Audio](https://www.w3.org/TR/webaudio/#dom-baseaudiocontext-decodeaudiodata).

The ADPCM component decoded 150 independent packets in Chromium with output
exactly matching the native reference. Synthetic input/output SNR was 42.1 dB
for the stereo tones, 38.6 dB for a modulated multitone signal, and 15.8 dB for
seeded noise. These are waveform-error measurements, not listening-test scores
or a matched-bitrate comparison with Opus. The fixed-vector tests also cover
channel packing, signed clipping, malformed state, packet gaps and old-peer
negotiation. A separate real-guest component identified all 18 markers across
Opus, ADPCM and PCM, with zero local drops and zero video errors.

## Evidence and limits

[Results inventory](results.json) binds the complete latency and cost JSON,
all four original browser component reports, prospective declarations, source
archives and the tested executable hash. Component reports retain per-file
durations, decode timings and distortion observations. The synthetic generator
and archived source revisions support replay; verification checks report
hashes and count/format invariants, rather than recomputing distortion from
waveforms.

Validation passed **166 native tests**, **165 portable tests**, Clippy for all
server targets, and the separate eighteen-marker guest component.

```sh
python3 -B benchmarks/audio-fallback/verify.py
```

The host was an Intel Core i5-13600KF running Chromium 149.0.7827.55 and QEMU
11.1.0. Native, browser and controller shared CPU 0 at nice 19; the disposable
guest used CPU 19. Fresh network/private-device namespaces excluded physical
GPU devices. Each workload had a 3 GiB memory limit, no swap, 256-task cap and
bounded systemd lifetime. Distribox and existing guests were not used.

Firefox was not installed for this round; its actual playback path remains
unverified. Opus still has the lowest measured network rate. ADPCM gives the
tested fallback enough room on this 1 Mbit/s static-desktop link, while PCM
remains the lossless option. Sustained video traffic can still congest their
shared TCP connection. Audio/video transport isolation, motion interaction,
long-session behavior and perceptual quality remain further work.
