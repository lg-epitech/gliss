# Audio packet duration: 20 ms versus an opt-in 40 ms policy

28 October 2026 · branch `research/audio-performance`

**Status: native/browser accounting prerequisite passed; performance remains
unmeasured.** The [root-run probe](../../benchmarks/audio-duration-prerequisite/README.md)
matched all 52 packets to outputs, including 18 outputs of 1,920 frames. It
does not exercise the production packetizer or source assembly aging.

## Audit of `benchmarks/audio-direct-fill-256`

Independent recomputation reproduced eight valid positions, all 2,400 timed
sends/callbacks, matching sample hashes, CPU arithmetic and the rejection
verdict. Task peak was 133 with no denial events. File bindings and both
zero-position setup failures matched.

No material defect in the final data was found. Two disclosure gaps:

- **Counter resolution.** Recorded CPU deltas fall on 10 ms steps. PCM means
  differ by two ticks per window. Root rounded the published percentages and
  disclosed the counter steps; exact JSON arithmetic is retained.
- **Verifier coverage.** `verify.py` checks the CPU arithmetic but not that
  `elapsed_ms` was near the 6,000 ms window. Root added broad span, pool and
  setup-label checks without changing data or gates. Per-position payload traces are
  absent: shared-pool selection is established by archived source. The plan's
  64 input batches produced 65 Opus payloads after flush; this is now disclosed.
  The final `phase` is only the last setup label. None changes the outcome.

The harness limits in `source-review.md` (abort after an invalid position,
owner-kill report loss, window edge) did not occur; they are not data defects. **Verdict: the rejection is correctly derived from
valid data; no new material error.**

## Candidate: 40 ms packets, release default unchanged

`crates/gliss-server/src/audio.rs` fixes `FRAMES = 960` and `PCM_BYTES`, and
`Encoder::packet` requires exactly 20 ms. The GLA1 header already carries the
frame count at bytes 16..20, and `web/audio-player.js` reads it, accepting
1..5760 frames, so no wire format change is needed. Audio uses WebSocket binary
messages, so packet size is not an MTU issue.

At 1,920 frames:

- **Opus.** libopus accepts 1,920 samples per channel at 48 kHz for 40 ms
  packets ([encoder API](https://opus-codec.org/docs/opus_api-1.5/group__opus__encoder.html)).
  `encode_vec(.., 1275)` caps instant bitrate at about 255 kbit/s per 40 ms;
  at the configured 96 kbit/s this is ample.
- **ADPCM.** `audio_adpcm::Encoder::encode` accepts up to 5760 frames and
  `decodeAdpcm` checks `frames + 7` bytes; no change needed.
- **Aging.** `pending_received` is the first fragment's ingress. Real-time
  fragments can add about 20 ms of assembly age with larger packets, reducing
  headroom under the fixed 100 ms `MAX_AGE`; the writer rechecks age before send.
  A whole batch can arrive at once, so ingress age is not guest-source age.
  Native discards are silent, the main observability gap.
- **Browser mapping.** `push` keys `pendingDecodes` on the chunk timestamp and
  advances `timestamp` by `frames`; `duration` is derived from `frames`.
  FFmpeg's libopus wrapper decodes a whole packet into one frame of up to
  5760 samples
  (<https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/libopusdec.c>),
  so one 40 ms chunk should yield one 1,920-frame `AudioData`. Chromium's
  decoder path was not inspected here. If Chromium ever emitted two 960-frame
  outputs, the second would carry an unknown timestamp, count as dropped and
  halve the audio. The WebCodecs Opus registration
  (<https://www.w3.org/TR/webcodecs-opus-codec-registration/>) does not
  promise one output per chunk. The probe confirmed it on Chromium149; other
  browser builds remain unproved.
- **Caps.** The 60 ms start delay, 200 ms cap, 12 sources and 8 pending
  decodes all still hold; the 200 ms cap stays the binding limit.

Smallest isolated tests, root-run:

1. Rust unit tests: an encoder built with 1,920 frames emits header frames
   1920, PCM payload 7,680 bytes, ADPCM payload 1,927 bytes, and an Opus
   packet that `opus::Decoder` decodes to 1,920 frames; two 20 ms fragments
   assemble into one packet keeping the first ingress time; a first fragment
   older than `MAX_AGE` at completion is discarded.
2. Node test with the mocked decoder from `tests/audio-freshness.test.mjs`:
   a 1,920-frame Opus packet produces a chunk with `duration` 40000, the
   next timestamp advances 40000 µs, and a 1,920-frame output schedules one
   0.04 s source; an unexpected second 960-frame output is counted dropped.
3. Real Chromium test like `tests/audio.test.mjs`: encode with
   `opus: {frameDuration: 40000}` after `isConfigSupported`, send eight
   packets with header frames 1920, require received = scheduled = 8 and
   dropped 0. If Chromium rejects that encoder config, use native fixtures.

Prospective paired design, same gates as the fallback study: loopback and the
1 Mbit/s link, muted/20 ms/40 ms positions in a fixed order, native and
browser CPU, received kbit/s, control p95, audio-marker latency, plus new
native discard and browser drop counters. Expected before any run: packet rate
halves (50 to 25/s), saving 4 kbit/s of GLA1 headers, plus variable WebSocket
overhead. The measured scheduling method cost about 0.1 ms per packet; halving
only that work suggests roughly 0.25% of one core. This is no bound on decoder
or audio-service changes. Assembly can add 20 ms. Opus output differs across
durations; compare aligned error and tone energy. PCM must stay exact; ADPCM
is lossy and also needs a duration-specific error comparison.

Blockers before a live cohort: add a native discard counter; decide whether
`MAX_AGE` stays fixed at 100 ms (recommended for the first run); plumb an
`--audio-packet-ms` option (values 20 or 40, default 20) through
`audio::channel`/`encode` the way `configured_video_qp` is wired; retain the
proved Chromium mapping while testing full fragment assembly. No end-to-end
compatibility or speed result is claimed.
