# Gliss live video specification

All-IDR CPU H.264 over QUIC datagrams · Revision 1 · 24 October 2026

## Intent and scope

This specification describes Gliss's low-latency desktop-video profile. H.264 is a mature video-compression standard. OpenH264 supplies its encoder; Gliss supplies capture, packet framing, authorization, input forwarding and a delivery policy that favors the latest desktop state. H.264 compression internals are outside this specification.

Requirements below apply to this selected profile. Other native codecs, predictive experiments and generic decoder capabilities do not expand its negotiated behavior. Measurements are observations, not requirements or service guarantees.

| Setting | Selected profile |
| --- | --- |
| Native encoder | CPU OpenH264, one encoder thread, ScreenContentRealTime usage, Baseline profile, low complexity |
| Color | RGBA8 capture converted to BT.601 limited-range YUV420; lossy, including chroma subsampling |
| Frame independence | Every access unit is an IDR, with SPS and PPS; intra period 1 |
| Rate control | Bitrate mode, **2,000,000 bit/s soft target**; frame skipping disabled |
| Timing | Capture limit 30 Hz; encoder rate policy 30 fps; synthetic encoder timestamp rate 30 fps |
| Media | GLV1 access units fragmented as GLD1 over WebTransport QUIC datagrams; BBR |
| Native send queue | **262,144 bytes (256 KiB)**, including conservative per-datagram admission charges |
| Control and input | Reliable bidirectional QUIC stream; separate same-origin WSS session channel |

```text
VM graphics FD -> RGBA capture -> CPU OpenH264 -> GLV1 -> GLD1 / QUIC datagrams
                                                                    |
Browser canvas <- VideoFrame <- WebCodecs <- latest complete frame <-+
Browser keyboard / pointer <---- reliable QUIC control ----> native input
Browser session  <---- HTTPS login, single-use ticket, certificate pin
```

Thirty frames per second is a scheduling limit, not a promised paint rate. Only the latest captured state waits behind an active encode. Missed pacing ticks are skipped. Frame dimensions must be even, at least 16 × 16, with the larger side at most 3840 and the smaller at most 2160. This profile does not offer guest resize or clipboard access.

The 2 Mbit/s target, 256 KiB queue and BBR are explicit server options. The native bitrate default remains 4 Mbit/s and the datagram queue default 1 MiB, so a default launch does not select this profile:

```sh
gliss-server --vnc 127.0.0.1:5900 --codec h264 --video-intra-period 1 \
  --h264-bitrate 2000000 --fps 30 --max-clients 1 \
  --datagram-bind 127.0.0.1:4433 --datagram-congestion-control bbr \
  --datagram-send-buffer-bytes 262144 --datagram-display-input-only
```

## GLV1 video access unit

One GLV1 packet contains a 24-byte header followed by one H.264 Annex B access unit. All multibyte integers are unsigned and little-endian. Offsets start at zero.

| Offset | Bytes | Field | Selected-profile meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | Magic | ASCII `GLV1` |
| 4 | 4 | Sequence | Wrapping `u32` frame sequence |
| 8 | 4 | Input ID | Latest forwarded input ID; metadata only |
| 12 | 2 | Width | Decoded display width in pixels |
| 14 | 2 | Height | Decoded display height in pixels |
| 16 | 2 | Reserved | Zero |
| 18 | 2 | Flags | Exactly 1 for this all-IDR profile |
| 20 | 4 | Payload length | Access-unit byte count, excluding the header |
| 24 | Variable | Payload | Annex B NAL units, including IDR, SPS and PPS |

The declared payload length must equal the remaining bytes and must be nonzero. The native packet limit is 16 MiB of payload plus 24 header bytes. The Gliss browser decoder accepts that same limit. The send-queue admission limit below is tighter still.

The encoder caches parameter sets and adds them when an IDR lacks either SPS or PPS. The browser parser checks Annex B framing, a zero forbidden NAL-header bit, no more than 4096 NAL units, SPS profile byte 66 (Baseline), parameter-set presence and the IDR declaration. Full H.264 bitstream validation belongs to WebCodecs; native framing validation alone is not a full decoder or a Baseline verifier.

Generic GLV1 parsers and decoder adapters retain delta-frame support: flag 0 denotes a delta, and decoding requires a contiguous sequence since an IDR. **The selected datagram profile permits only flag 1 and independent IDRs.** Both the native all-IDR route and browser assembler enforce that restriction.

The input ID does not prove that an input caused a visible change. GLV1 has no capture-time or presentation-time field. OpenH264 receives a synthetic timestamp in milliseconds, `floor(encoded_frame_index × 1000 / 30)`. The browser separately uses the sequence as its `EncodedVideoChunk.timestamp` correlation token. Neither value measures end-to-end latency.

## GLD1 media datagram

Each GLD1 datagram has a 20-byte header and one fragment of the complete GLV1 packet. All multibyte integers are unsigned and little-endian.

| Offset | Bytes | Field | Required meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | Magic | ASCII `GLD1` |
| 4 | 4 | Sequence | Must match the enclosed GLV1 sequence |
| 8 | 4 | Total length | Complete GLV1 byte count, including its 24-byte header |
| 12 | 2 | Fragment index | Zero-based index, less than fragment count |
| 14 | 2 | Fragment count | `ceil(total_length / stride)`, at most 65,535 |
| 16 | 2 | Stride | Fragment payload size, 256 through 1000 bytes |
| 18 | 2 | Reserved | Zero |
| 20 | Variable | Fragment bytes | Slice starting at `index × stride` in GLV1 |

All fragments for one sequence must agree on total length, count and stride. A fragment body is exactly `min(stride, total_length − index × stride)` bytes. The complete packet must exceed 24 bytes and fit the 16 MiB + 24-byte packet cap. Duplicate fragments are ignored; inconsistent geometry invalidates that pending assembly. Missing bytes are never decoded.

The sender chooses `stride = min(1000, max_datagram_size − 20)` and requires at least 256 bytes. For a complete GLV1 packet of `B` bytes and `N = ceil(B / stride)` fragments, whole-frame admission charges:

```text
admission_bytes = B + N × (20 + 8 + 128)
admit only if admission_bytes <= 262144
```

The charges are the GLD1 header (20), a maximum WebTransport session-ID wrapper (8), and conservative Quinn queue metadata (128) per fragment. The metadata allowance is tied to pinned Quinn 0.11.19 on the supported x86_64 target. It is memory accounting, not wire overhead.

Admission checks the frame's own full cost before its first fragment is sent. It does not reserve space against current queue occupancy or promise delivery of the batch. Nonblocking sends can evict older unsent datagrams as the bounded Quinn queue fills. Oversized frames are dropped whole at admission; loss or later eviction can still leave an incomplete receiver assembly.

Media has no application frame ACK, retransmission, FEC or TCP fallback in this profile. QUIC transport ACKs and congestion control still operate. They do not acknowledge canvas presentation and do not gate capture or encoding on browser paint completion. The native compatibility handler ignores `ack` messages; the browser control API does not expose them.

## Latest-state delivery and browser ownership

Sequences wrap modulo 2³². The browser defines `newer(a, b)` as `((a − b) | 0) > 0`: forward differences from 1 through 2³¹ − 1 are newer. Equality and the half-range boundary are not newer. This comparison assumes relevant outstanding sequences remain within a half-range.

| Stage | Bound and behavior |
| --- | --- |
| Capture and encode | One active encode per stream; newer capture replaces the waiting source state |
| Fragment assembly | At most two incomplete frames, each capped at the packet limit plus a fragment bitmap |
| Assembly age | Eligible for expiry 250 ms after its first fragment; checked on arrival and a 250 ms sweep |
| Browser datagram queue | Requests and reads back an incoming age of 250 ms and a count limit of 4096 through the WebTransport datagram accessors |
| Decode and draw handoff | One delivered frame plus one replaceable complete waiting frame |
| Decoder adapter | At most two pending submissions and a decode queue below two at submission; the selected handoff serializes use |
| Decoder deadlines | Configuration probe 3000 ms; output 5000 ms; failure ends the viewer path |

When both assembly slots are occupied, a newer sequence can evict the oldest incomplete frame. A completed frame advances the assembly watermark and discards older incomplete assemblies. Later frames at or behind that watermark are stale. While one frame is being decoded or drawn, each newer complete frame replaces the single waiting frame.

These limits bound particular allocations and waits. They do not establish a latency ceiling. A delayed event loop can delay expiry; browser accessor readback does not prove effective eviction, kernel-buffer behavior or network delay. Reassembly buffers are only part of browser memory use.

The browser derives `avc1.PPCCLL` from the SPS profile, constraint and level bytes, then checks `VideoDecoder.isConfigSupported`. Configuration uses coded dimensions, `hardwareAcceleration: 'prefer-software'` and `optimizeForLatency: true`. Omitting `description` selects Annex B input. Software preference is a browser hint, not proof of the decoder implementation. Initialization or a dimensions/codec change requires an IDR and no pending outputs.

Decoded dimensions must match GLV1. The viewer draws the `VideoFrame` onto its canvas and closes it in `finally`, then releases local frame ownership with a painted/discarded result. Stale-generation and unmatched outputs are also closed. That local release sends no network message. A successful draw is the viewer's paint marker, not proof of physical display scanout.

Loss, expiry or an encoded-head watermark ahead of paint can trigger a reliable `{ "type": "resync" }` request. Requests are rate-limited to one per second after the recovery checks. At most three are sent without paint progress; the next eligible request fails closed. The server samples its current source state and encodes it with a fresh sequence, including an unchanged current desktop if needed. It never replays a cached old packet or accepts a requested historical sequence.

## Authorization and reliable control

The server fixes its source, codec, bitrate and queue at launch; the browser cannot change them. The browser first logs in with the access key and receives the signed session cookie described in the [security guide](security.md). An authenticated, Origin-checked `POST /api/datagram-ticket` then returns a media descriptor, a single-use ticket and the runtime certificate's SHA-256 fingerprint, which the browser pins through WebTransport `serverCertificateHashes`. The WebTransport connection checks the ticket, Origin and cookie expiry, then keeps that session deadline. Authenticated `POST /api/datagram-revoke` permanently revokes the worker's datagram admission and input and closes its connections.

| Boundary | Policy |
| --- | --- |
| Native connection ticket | Single use, 5000 ms lifetime, at most 32 pending |
| Capabilities | With `--datagram-display-input-only`: display, keyboard and pointer; no guest resize or clipboard |
| Non-loopback media | `--datagram-public-host` requires private loopback HTTP, a launcher-supplied `--rfb-fd`, one client and an exact HTTPS `--origin` |

Authorizing a VM and handing its graphics connection to Gliss belongs to the launcher. EOF consumes that attachment; there is no automatic reconnection to a replacement VM process. Expiry or revocation ends the session and releases held input.

Reliable QUIC control messages are UTF-8 JSON prefixed by a four-byte little-endian length. Bodies must be 1 through 1,049,600 bytes. The browser sends only keyboard, pointer, ping and the restricted fresh-state request; outbound messages are at most 2048 bytes, with at most 32 queued messages and 65,536 queued body bytes. Guest input is received independently of outgoing media/control writes.

Before reading media, the browser requires one authenticated `hello` consistent with its descriptor: `h264`, `GLV1`, `GLD1`, WebTransport datagrams, BBR, RFB source, display/input-only, independent frames, no media retransmission, capture limit 30, and video timing `{ "fps": 30, "timestamp_fps": 30, "intra_period": 1 }`. Missing, duplicate or contradictory hello data fails the connection. There is no fallback video channel.

## Observed behavior and limits

A matched comparison of the 2 and 4 Mbit/s targets passed all twelve runs ([data](../benchmarks/motion/gliss-all-idr-2026-10-24.json), [method](benchmarks.md)). With 150 ms RTT and 1% periodic loss:

| Observed metric | 2 Mbit/s target | 4 Mbit/s target |
| --- | ---: | ---: |
| Crop changes per second | 5.93–6.40 | 3.80–4.13 |
| Slowest reported latency | 460–464 ms | 744–779 ms |
| UDP rate | 4.21–4.25 Mbit/s | 6.67–6.90 Mbit/s |

Crop changes count changes in the sampled crop, not whole-screen FPS. A canvas draw does not establish physical display scanout.

The local 2 Mbit/s run's slowest reported latency was 177–178 ms. Both targets used about 14% of one CPU core in the scoped native measurements. A soft encoder target is not a cap on measured UDP traffic. On a separate offline 12-frame workload, retained text contrast was 95.2% at 2 Mbit/s and 97.3% at 4 Mbit/s; the lower target trades some text sharpness for smoother motion under loss.

Only Chromium has been tested. Firefox compatibility, public UDP reachability through NAT and proxies, and real Internet paths remain untested.

## Reference implementations

- [Native encoder and GLV1 parser](../crates/gliss-video/src/lib.rs): OpenH264 configuration, color conversion, timestamps, parameter sets and packet fields.
- [Native datagram route](../crates/gliss-server/src/datagram.rs): GLD1, whole-frame admission, tickets, reliable control and current-state resync.
- [Browser datagram transport](../web/datagram-transport.js) and [video decoder](../web/video-decoder.js): GLD1 reassembly and the GLV1/WebCodecs adapter. The decoder's delta support is broader than this profile.
