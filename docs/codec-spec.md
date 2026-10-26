# Batch lossless codec specification

GLB1 profile · 16 October 2026

Batch is the selected custom profile for sharp desktop text. It sends only rectangles that changed. A rectangle contains new RGBA pixels or coordinates to copy from the previous frame. One zlib stream compresses all records together, so repeated content can share a compression history. Unchanged pixels need no rectangle.

The server selects this profile with `--codec batch`; its default remains `deflate`. Software H.264 is a separate, lossy motion baseline. Neither H.264 nor the later GLB2/GLB3 transforms belongs to this specification.

This document describes the current [Rust codec](../crates/gliss-codec/src/lib.rs) and [browser decoder](../web/decoder.js). Wire checks and encoder policy are separated so other encoders can choose valid rectangles without reproducing Gliss's search.

## Pixels and packet boundaries

A decoded frame has unsigned 16-bit width and height, each nonzero, with at most 16,777,216 pixels. Pixels are tightly packed in row order, starting at the upper left. Each pixel is four bytes in **R, G, B, A** order. Rows have no padding. Coordinates increase rightward and downward.

Raw storage and copies preserve all four channel bytes exactly. There is no chroma subsampling, quantization, color conversion or alpha premultiplication in Batch. The capture contract normally supplies opaque pixels, but the codec's frame validation does not require alpha to equal 255. Display composition and Canvas conversion are outside the byte-exact codec contract.

The decoder receives one complete packet, normally one binary WebSocket message. GLB1 has no transport fragment or packet-length field. The enclosing message supplies its exact extent; joining several packets into one message is not part of the grammar.

## The GLB1 envelope

Every multibyte integer in the Gliss header and rectangle records is unsigned and little endian. The zlib member keeps zlib's own byte conventions, including its checksum trailer.

| Byte offset | Size | Field | Meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | Magic | ASCII `GLB1`, hex `47 4c 42 31` |
| 4 | 4 | Sequence | Frame sequence, modulo 2³² |
| 8 | 4 | Input ID | Caller-supplied input association; not a pixel acknowledgement |
| 12 | 2 | Width | Complete output width in pixels |
| 14 | 2 | Height | Complete output height in pixels |
| 16 | 2 | Rectangle count | Number of records after decompression |
| 18 | 2 | Flags | Bit 0 is keyframe; all other bits are zero |
| 20 | 4 | Expanded body length, L | Exact decompressed byte count, excluding this header |
| 24 | Remaining bytes | Compressed body | One complete zlib-wrapped DEFLATE stream |

The decompressed body is exactly `rectangle₀ || rectangle₁ || …`, without a second frame header or per-rectangle compression. The zlib member has a header selecting DEFLATE (CM=8, FDICT=0), a DEFLATE body and a big-endian Adler-32 trailer over the L output bytes. Raw DEFLATE, gzip, incomplete streams and extra decompressed bytes are invalid. A zero-rectangle delta still carries a complete zlib stream that expands to zero bytes.

For reuse of the existing parser, expansion constructs a 24-byte `GLS1` header with the same fields, changes magic to `GLS1`, writes zero at offset 20, then appends the expanded records. Thus offset 20 means a length in GLB1 and a zero reserved field in GLS1. Compressing an entire GLS1 packet, header included, produces the wrong body.

```text
GLB1 header, 24 bytes (offset 20 = L) | one zlib member: compressed rectangle records
        | expand exactly L bytes; rewrite magic and offset 20
        v
GLS1 header, 24 bytes (offset 20 = 0) | rectangle records, L bytes: record header + payload, repeated
```

*Figure 1. The header stays outside the compressed stream. Expansion restores a GLS1 header, changes offset 20 from L to zero and appends the rectangle records.*

## Rectangle records

Each record begins with this 16-byte header. Its payload follows immediately; the next record starts after exactly that payload length.

| Relative offset | Size | Field |
| ---: | ---: | --- |
| 0 | 2 | Destination x |
| 2 | 2 | Destination y |
| 4 | 2 | Rectangle width, w |
| 6 | 2 | Rectangle height, h |
| 8 | 1 | Kind: 0 for raw RGBA, 5 for previous-frame copy |
| 9 | 3 | Reserved; all zero |
| 12 | 4 | Payload length in bytes |

Both dimensions are positive. The complete destination lies inside the declared frame. Destination rectangles cannot overlap, even when they would write identical pixels. All records and payloads together consume exactly L bytes; trailing bytes are rejected. Tile alignment, increasing coordinate order and a fixed rectangle size are encoder conventions, not decoder requirements.

Kind 0 has exactly `4 × w × h` payload bytes. They are the rectangle's RGBA pixels in row order, without full-frame row stride.

Kind 5 has exactly four payload bytes: source x as `u16`, then source y as `u16`. Source width and height equal the destination's w and h, and the complete source lies inside the previous frame. Copies always read the immutable previous frame, including when a source overlaps its destination or another destination. They never read pixels written earlier in the current packet. This permits scroll copies without an ordering-dependent `memmove` rule.

GLB1 rejects all other kind values. In particular, kinds accepted by a general GLS1 decoder do not become valid merely because they are placed inside a GLB1 stream. A keyframe cannot contain kind 5 because it has no reference.

```text
Previous frame (read only)         Next frame (destinations)
source (0, 10), 16x16       --->   destination (0, 20), 16x16
source (16, 10), 16x16      --->   destination (16, 20), 16x16
Overlapping y ranges still read from the previous buffer.
```

*Figure 2. Every copy reads the previous frame, including overlapping source and destination coordinates. Earlier writes to the next frame cannot change a later copy's source.*

## Reference state, empty updates and recovery

A keyframe has flag bit 0 set and covers every output pixel exactly once. Its sequence can be any `u32`. It replaces the prior reference and may change geometry. A decoder with no reference accepts a valid keyframe independently of earlier sequence values.

A delta has flags zero. Its dimensions match the last successfully decoded frame, and its sequence is exactly the previous sequence plus one, wrapping from `0xffffffff` to zero. Unmentioned pixels retain their previous RGBA values. An empty delta changes sequence and input association while leaving every pixel unchanged; an empty keyframe cannot cover a nonempty frame.

Duplicate, skipped or reordered deltas fail the sequence check. There is no frame reordering, retransmission or independent tile recovery in GLB1. Input IDs do not prove a causal input-to-pixel relationship.

Both implementations commit their next reference only after a successful decode. Rust constructs a separate output frame; the browser also writes to a separate array before assigning its reference. Rejected packets leave the committed reference and sequence intact during ordinary sequential use. Browser callers must serialize asynchronous decode calls and avoid modifying returned pixel arrays, which alias its committed frame. No concurrent-call or caller-mutation isolation is provided.

The current viewer resets its decoder after a decode failure and sends the control message `{"type":"resync"}`. The server resets its encoder once its media work drains, so the next encode is independent. Reset clears the encoder reference and sequence. Geometry changes and a nonconsecutive encode sequence also force a keyframe. Recovery requests and acknowledgements belong to the surrounding session protocol, not the GLB1 header.

```text
40 keyframe: accept -> 41 delta: accept -> 42 missing: no update -> 43 delta: reject
Reference remains 41 -> reset + resync -> independent keyframe
Valid consecutive wrap: 0xffffffff -> 0
```

*Figure 3. A missing delta breaks the chain. Rejection preserves the last reference; the viewer then resets and requests a keyframe. Sequence wrap from `0xffffffff` to zero remains consecutive.*

## Current encoder policy

The Rust encoder accepts a configured tile edge from 16 through 256 pixels; the server default is 64. It visits tiles left to right within each row, then top to bottom. Right and bottom edge tiles are clipped to the frame. The grid contains at most 65,535 tiles. A keyframe emits every tile. A delta emits only tiles whose complete RGBA bytes differ from the same rectangle in the previous frame.

Batch writes a dirty tile as either an exact previous-frame copy or raw RGBA. Copy search runs only for a delta with at least four dirty tiles and sufficient temporal similarity. For each dirty tile, it samples up to 64 evenly spaced pixels, using `floor(sample × area / sample_count)`. At least one eighth of all sampled RGB triples must equal the previous frame at the same coordinates. Alpha does not enter this search gate; final copy verification compares every RGBA byte.

The search builds an index of previous grid tiles keyed by width, height and a 64-bit FNV-1a hash. Each bucket keeps the first eight rectangles in traversal order. Hashing uses offset basis `0xcbf29ce484222325` and multiplier `0x100000001b3`, with wrapping arithmetic. A hash match only finds candidates; exact comparison establishes equality.

Full-width row hashes also vote for vertical shifts. Each row-hash bucket retains up to eight previous row positions. Nonzero shifts with magnitude at most 512 pixels compete by vote count; the four highest counts are retained. Equal counts prefer the smaller signed shift. For a dirty tile, the encoder first tries these shifts at the same x coordinate, in ranked order, then tries matching indexed tiles in stored order. The first byte-exact source wins. A failed search emits raw pixels.

After assembling all records, the encoder compresses their concatenation once with `flate2::ZlibEncoder` and `Compression::fast()`. It always uses GLB1, even when compression increases size. Batch has no per-tile compressed-size contest or raw-versus-zlib packet tie. A successful encode commits an owned clone of the current frame; an error does not advance encoder state.

These search rules describe the pinned encoder. Decoders accept other nonoverlapping partitions. DEFLATE bytes are not canonical across compressors or dependency revisions; pixels and framing define interoperability.

## Bounds and rejection behavior

The input packet is between 24 and 84,934,656 bytes inclusive. Before expanding GLB1, both implementations check positive dimensions, the pixel cap and these body bounds:

```text
16 × rectangle_count ≤ L ≤ 4 × width × height + 16 × rectangle_count
```

Rust also explicitly caps L at 68,157,424 bytes, the maximum possible right-hand bound. With 16-bit count and the pixel cap, the browser's formula implies the same ceiling. The decoder then checks supported flags/kinds, reserved bytes, source/destination bounds, exact payload lengths, nonoverlap, complete keyframe coverage and exact record consumption.

Rust inflates into a buffer of L + 1 bytes. It requires output no longer than L, a completed zlib stream, exactly L output bytes and consumption of every supplied compressed byte. This rejects appended bytes and concatenated streams explicitly. The browser reads `DecompressionStream('deflate')`, counts output chunks, cancels on excess and checks exact output length. Stream grammar and trailer validation are delegated to that browser API; the JavaScript does not independently expose or compare compressed-input consumption. Its extra-input rejection therefore relies on the engine's implementation.

These are decompressed-size protections, not a small process-memory guarantee. Browser chunks, joined expansion, reference/output arrays and a one-byte-per-pixel coverage array can coexist; an oversized chunk is observed after the browser produces it. Rust likewise holds several buffers and uses a bitset for coverage. Neither decoder specifies a wall-clock decompression deadline or a whole-process RSS cap. Applications still need their own bounded input scheduling and resource policy.

## Worked packet

This 44-byte expanded GLS1 packet is an independent 1×1 opaque red frame, sequence 1, input ID 7, with one raw rectangle. Addresses at left are hexadecimal offsets.

```text
00: 47 4c 53 31 01 00 00 00 07 00 00 00 01 00 01 00
10: 01 00 01 00 00 00 00 00 00 00 00 00 01 00 01 00
20: 00 00 00 00 04 00 00 00 ff 00 00 ff
```

Its GLB1 equivalent has this exact header, followed by a complete zlib member whose output is the last 20 bytes of the packet above:

```text
47 4c 42 31 01 00 00 00 07 00 00 00 01 00 01 00
01 00 01 00 14 00 00 00
ZLIB(00 00 00 00 01 00 01 00 00 00 00 00 04 00 00 00 ff 00 00 ff)
```

`ZLIB(…)` is symbolic, not literal wire bytes. The example fixes the uncompressed content without inventing compression output or requiring a particular compressor.

## Measured scope

The historical fixed-desktop study completed 20 trials and 200 scheduled responses, using two reversed acquisition orders. Each row below contains 20/20 responses. These recorded-binary measurements are not alpha-release timings. The [benchmark account](benchmarks.md) retains the comparison and source links.

| Network | Pipeline | Response p50 / p95, ms | Whole-sequence downstream Mbit/s, r0 / r1 |
| --- | --- | ---: | ---: |
| Local | Batch | 145.9 / 159.9 | 0.191 / 0.191 |
| Local | Guacamole | 136.1 / 169.9 | 0.049 / 0.049 |
| Modeled WAN | Batch | 194.4 / 211.6 | 0.193 / 0.192 |
| Modeled WAN | Guacamole | 191.9 / 218.2 | 0.050 / 0.051 |

Latency runs from Enter keydown to returned root-window pixels at the next animation frame; earlier typing queues are excluded. p50 and p95 are the nearest-rank 50th and 95th percentiles. For 20 samples, p50 is the lower central value and p95 is the 19th sorted response. Batch's observed p95 was lower, while its median latency and downstream traffic were higher. Traffic covers the complete 24-second idle/color/typing/scroll/drag sequence, excluding startup, evidence capture and cleanup. It counts browser-facing HTTP/WebSocket bytes, without TCP/IP/TLS overhead or retransmissions.

The modeled WAN schedules ordered TCP chunks at nominal 40 ms RTT, per-chunk jitter and 20 Mbit/s per connection, with no packet loss. It is not the separate kernel-loss model. Two orders on a shared CPU host supply limited repeat evidence, not population confidence intervals or a capacity result. Guacamole's effective capture cap and cached compiled-patch provenance were not independently matched. Batch's lower p95 in these rows does not establish a general latency advantage.

The fidelity result concerns a 480×24 static terminal crop checked against an independent guest screendump after typing ended. All 12 crops pooled across Deflate, Adaptive and Batch were RGB-exact. Guacamole's four crops had RGB mean absolute error 0.212–0.240 and maximum channel error 12–16. These crops do not establish exact moving-text presentation, OCR accuracy or physical display fidelity. Software H.264 remains the practical low-bandwidth motion baseline when lossy pixels are acceptable.
