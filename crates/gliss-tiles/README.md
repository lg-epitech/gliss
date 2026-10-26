# GLT2 tile codec

This crate extracts the measured adaptive tile codec into the native workspace. It keeps the original raw RGBA, zlib RGBA and predicted-plane candidates, strict size ties, 64-color exact-tile guard, alpha bytes and fixed policies. Policy `0` is lossless; `1` and `2` allow maximum RGB channel errors of 8 and 17 for a complete fresh tile. Lost, stale or mixed-generation screen regions do not have that bound.

The extraction adds negotiated datagram payloads and a live current-state refresh API. It does not establish live network performance.

```rust,ignore
let mut encoder = gliss_tiles::Encoder::with_payload(1, policy, stride)?;
let summary = encoder.encode_with_refresh_budget(
    &current_frame,
    sequence,
    source_is_new,
    refresh_quota,
    force_all,
    |tile| bounded_pending_queue.replace_current(tile),
)?;
```

`source_is_new` is a trusted capture-generation hint. The caller retains an immutable current capture: `false` means its pixels are the same current source, even when no capture arrives for a long time. Geometry is always checked. A true generation scans tiles for exact input changes and clones one reference after emitting. A false generation visits only the requested refresh quota and performs no full-frame scan or clone. Dirty and refresh selections coalesce; independent live and legacy refresh cursors keep the APIs from changing one another's refresh phase.

The first capture emits all tiles. A resize or `force_all` emits the current complete source under a fresh wrapping epoch; doing both increments once. Ordinary per-tile sequences also use wrapping serial comparison. The caller must clear old-epoch pending updates before inserting new-epoch updates, and must keep session generations less than half the 32-bit range apart. A callback failure propagates after any already-emitted tiles; close or perform a fresh resync before continuing.

`EncodedTile` owns one complete compressed body and its geometry, epoch, sequence, tile ID, kind, policy and raw length. The encoder removes spare selected-body capacity before handing ownership to the caller so a body-byte queue budget includes its allocation. `fragments(stride)` produces canonical packets only when the sender can admit the complete tile. The fixed session stride must be between 1024 and 1168 bytes; headers add 32 bytes and a tile takes at most four datagrams. `wire_bytes` counts body and GLT2 headers. `admission_bytes` additionally reserves the maximum 8-byte session header and 128 bytes of conservative transport metadata per fragment. QUIC/IP overhead is separate.

The encoder does not retain a pending update batch in the live API. The caller owns a queue capped at 4 MiB of bodies and 8160 slots and must consume or drop each emitted tile before returning. `memory_estimate` reserves the incoming capture, one previous exact reference, the fixed pending body/slot budgets and 1 MiB of active scratch before encoding. Input excess capacity is checked as well. The default codec limit is 128 MiB; `with_memory_limit` can lower it. `retained_bytes` describes this object's retained allocations, while the estimate includes the declared external pending queue. The transport's separate 1 MiB queue, allocator internals and process RSS remain separate measurements. The compatibility `encode` method also reserves its materialized whole-update packet batch.

`Decoder::with_payload(stride)` fixes the same stride for a receiver. It bounds partial assemblies to 128 entries and 512 KiB, expires them after two caller-supplied logical steps, accepts only canonical nonoverlapping fragments, and requires exact zlib stream termination and input/output lengths. A future epoch remains staged until its first complete tile passes decompression and transform validation. Rejected future tiles preserve the last committed framebuffer. Decoder budgets also reserve a prospective resized framebuffer, old and new sequence maps, partial bodies/metadata and active scratch before allocation. The caller advances logical steps during idle time.

To validate this workspace member:

```sh
nice -n 19 cargo test -p gliss-tiles -j 2
nice -n 19 cargo clippy -p gliss-tiles --all-targets -j 2 -- -D warnings
nice -n 19 cargo build -p gliss-tiles --release -j 2
```

Tests compare default packets with the actual old Rust library for all policies, and compare all 12 archived native fixture steps byte-for-byte and by decoded RGBA SHA-256. The fixture gzip and uncompressed payload are hash checked before use. Other cases cover signed transforms, alpha, low-palette exactness, malformed/future epochs, canonical minimum/middle/maximum strides, partial geometry extremes, quota-only idle work, deliberate final-tile loss repair, fresh epoch resync and memory rejection before output. The historical experiment is a dev dependency only; production dependencies contain no benchmark crate.
