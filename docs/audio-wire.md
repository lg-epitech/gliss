# Gliss audio packets

Audio uses the authenticated WebSocket connection. Capture begins only after an
explicit `audio` control enables a codec. The `audio_status.codecs` array advertises
`opus`, `ima-adpcm`, and `pcm`. A browser with a native WebCodecs Opus decoder
prefers Opus. Otherwise it selects ADPCM only when advertised; an older server
without this field retains PCM compatibility. Older browsers can still request
PCM from a new server.

Every packet starts with a 20-byte header. Multibyte numbers are little-endian:

- Bytes 0–3: ASCII `GLA1`.
- Byte 4: codec, `0` PCM, `1` Opus, or `2` IMA ADPCM.
- Byte 5: channel count, exactly `2`.
- Bytes 6–7: reserved, zero.
- Bytes 8–11: sample rate, exactly `48000`.
- Bytes 12–15: wrapping unsigned sequence number.
- Bytes 16–19: frames per channel; the server emits `960` (20 ms).

The browser rejects mismatching codec, channel/rate/reserved fields and invalid
payload lengths. Its absolute allocation bound is 5,760 frames per channel.
Unknown codecs are rejected. PCM contains interleaved stereo signed 16-bit
samples; its exact length is `frames × 4`. Opus contains one encoded packet,
bounded to 4,096 bytes by the browser.

## ADPCM payload

Gliss uses the IMA four-bit prediction algorithm with its 89-entry step table and
index changes `[-1,-1,-1,-1,2,4,6,8]`. The
[IMA reference algorithm](https://www.cs.columbia.edu/~hgs/audio/dvi/IMA_ADPCM.pdf)
describes the quantizer. Gliss's stereo packing below is a separate format and
is not a WAV file or a WAV ADPCM data block.

The first eight bytes are two four-byte channel headers, left then right. Each
contains a signed 16-bit predictor, an unsigned step index in `0..88`, and a zero
reserved byte. The predictor is the exact first sample for that channel. Each
remaining byte contains the next frame: the low nibble is left, the high nibble
right. The exact payload length is `frames + 7`; a 960-frame packet is 987 bytes
including the Gliss header, or **394.8 kbit/s** at 50 packets per second. This
excludes WebSocket, TCP and IP overhead.

For each nibble, start with the indexed step. Add `step >> 3`, plus the full step
for bit 2, `step >> 1` for bit 1, and `step >> 2` for bit 0. Bit 3 subtracts this
delta instead of adding it. Clip the predictor to signed 16-bit range; that is
the next sample. Apply the indexed change using the low three nibble bits and
clip the step index to `0..88`.

Each block carries its own decoder state. After a sequence gap, playback can
start at the new block without decoding or requesting an earlier packet.
Malformed indices, reserved bytes and lengths stop playback. ADPCM uses no
codec API, worker, WASM asset, retransmission request or asynchronous decode
queue.

All codecs share the existing 60 ms startup buffer and 200 ms playback cap. The
server discards locally stale sound after 100 ms, measured from first-fragment
ingress. These limits cannot expire bytes already queued in TCP. ADPCM's lower
rate helps the fallback fit a constrained link; it does not isolate audio from
video congestion or establish physical speaker latency.
