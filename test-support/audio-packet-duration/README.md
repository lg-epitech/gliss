# Audio packet duration candidate (20 ms default, opt-in 40 ms)

28 October 2026 · branch `research/audio-performance` · declared head
`37962fd6` (as stated by the brief; no Git command was run here)

**Status: root validated, unserved.** The formatted candidate passed all
28 native tests and Clippy with warnings denied. The [validation record](validation.json)
binds the checked source bytes and log identities. The original handoff was
archived before formatting; legacy and parent source bytes stayed unchanged.
Production source, runtime options and services remain unchanged. CPU, latency
and fidelity benefits remain unmeasured.

## Files (7 of at most 7)

| File | Role |
| --- | --- |
| `audio.rs` | Candidate replacement for `crates/gliss-server/src/audio.rs`. Same module layout and `super::audio_adpcm` path, so root can later drop it in place. |
| `legacy.rs` | Byte-identical copy of the current `crates/gliss-server/src/audio.rs` followed by one `#[cfg(test)]` bridge (`continuous_packets`). The original prefix is hash-bound; it was not edited. |
| `wrapper.rs` | Module shim an example includes with `#[path]`; binds the real `audio_adpcm.rs`, the candidate and the frozen original, and holds the byte-comparison tests. |
| `verify_binding.py` | Stdlib-only hash verifier for `source-bindings.json` (authored, not run). |
| `source-bindings.json` | Parent hashes, derivations and owned hashes. Carries no hash of itself. |
| `README.md` | This note. |
| `validation.json` | Root execution results; no production or performance claim. |

Parent sources frozen at authoring time (SHA-256):

| Parent | SHA-256 |
| --- | --- |
| `crates/gliss-server/src/audio.rs` (18,914 bytes) | `d5f9c489e89e71b3afd3d9b1274ec1ad6b0dc4ef4aa210f8847fde57cd14a264` |
| `crates/gliss-server/src/audio_adpcm.rs` | `eb2b6984172ca9a809c29fc8aeb64ca4707b1d5aac6a4b1595ed565024198740` |
| `crates/gliss-rfb/src/audio.rs` | `1008da7fab60733ad91780a55dddc764d3226ff6c39b47e6539314bf708c1ee6` |
| `docs/performance/audio-packet-duration.md` | `e16b3eb48ec55bd68bac3276d6566958190c7cd970b3505d35e98d574a8dd3f7` |

## What the candidate changes, and what it keeps

Kept exactly: `MAX_AGE` of 100 ms for raw fragments, partial packets and the
writer; first-fragment ingress timestamp on every packet; Opus Audio at
96 kbit/s, complexity 4, `encode_vec(.., 1275)`; the 4-slot packet broadcast;
`watch`-coalesced requests with generation increments; sequence restarting per
encoder (per generation); `resubscribe` on every request change; `Begin`/`End`
and lag clearing the partial packet; the GLA1 header (`GLA1`, codec, channels,
two zero bytes, rate, sequence, frames, payload); `Status::message()` JSON. No
retry, FEC, ACK, backlog or freshness change was added. No protocol or status
emission changed.

Added:

- `PacketDuration` — a two-variant policy (`Ms20` default, `Ms40`). Only the
  canonical values exist; `from_millis` accepts 20 or 40 and `FromStr` accepts
  exactly `"20"` or `"40"`.
- Per-encoder frozen sizing: an `Encoder` fixes `frames` and `pcm_bytes` at
  construction and rejects any other input length. `FRAMES` (960) and the
  private `PCM_BYTES` (3,840) remain the default-policy constants.
- `EncodeConfig { packet_duration, counters }` and `encode_with(..)`; the
  historical `encode(..)` is now a wrapper that passes `EncodeConfig::default()`.
- `Counters` / `CountersSnapshot` and a writer-side increment path.

## Exact public API of `audio.rs`

Unchanged signatures:

```rust
pub const FRAMES: u32;                       // 960, the 20 ms default
pub const MAX_AGE: Duration;                 // 100 ms, policy-independent
pub enum Codec { Pcm, Opus, ImaAdpcm }       // serde names unchanged
pub struct Request { pub codec: Option<Codec>, pub generation: u64 }
pub struct Status { pub available: bool, pub enabled: bool, pub codec: Option<Codec>, pub message: Option<String> }
impl Status { pub fn message(&self) -> serde_json::Value }
pub struct Packet { pub generation: u64, pub received: Instant, pub bytes: Arc<[u8]> }
pub struct Session { pub request: watch::Sender<Request>, pub status: watch::Receiver<Status>, pub packets: broadcast::Sender<Packet> }
pub fn channel() -> (Session, watch::Receiver<Request>, watch::Sender<Status>)
pub fn request(sender: &watch::Sender<Request>, value: serde_json::Value) -> anyhow::Result<()>
pub async fn update_capture(writer: &mut gliss_rfb::RfbWriter, request: Request, enabled: &mut bool, status: &watch::Sender<Status>, allowed: bool)
pub async fn encode(raw: broadcast::Receiver<Event>, requests: watch::Receiver<Request>, packets: broadcast::Sender<Packet>)
```

New:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PacketDuration { #[default] Ms20, Ms40 }
impl PacketDuration {
    pub const DEFAULT: Self;                         // Ms20
    pub const fn millis(self) -> u32;                // 20 | 40
    pub const fn frames(self) -> u32;                // 960 | 1920
    pub const fn pcm_bytes(self) -> usize;           // 3840 | 7680
    pub fn from_millis(millis: u32) -> anyhow::Result<Self>;
}
impl FromStr for PacketDuration { type Err = anyhow::Error; }   // "20" | "40" only

#[derive(Debug, Default)]
pub struct Counters { /* relaxed AtomicU64 fields, private */ }
impl Counters {
    pub fn snapshot(&self) -> CountersSnapshot;
    pub fn record_writer_stale_packet(&self, wire_bytes: usize);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CountersSnapshot {
    pub stale_raw_events: u64,       pub stale_raw_bytes: u64,
    pub stale_pending_discards: u64, pub stale_pending_bytes: u64,
    pub reset_pending_discards: u64, pub reset_pending_bytes: u64,
    pub raw_lag_events: u64,         pub raw_lag_messages: u64,
    pub encoder_init_errors: u64,    pub encoder_errors: u64,
    pub emitted_packets: u64,        pub emitted_frames: u64,  pub emitted_wire_bytes: u64,
    pub writer_stale_packets: u64,   pub writer_stale_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct EncodeConfig { pub packet_duration: PacketDuration, pub counters: Arc<Counters> }
impl EncodeConfig { pub fn new(packet_duration: PacketDuration) -> Self }

pub async fn encode_with(raw: broadcast::Receiver<Event>, requests: watch::Receiver<Request>, packets: broadcast::Sender<Packet>, config: EncodeConfig)
```

Private: `Encoder::with_duration(codec, duration)` replaces the old
constructor in the pipeline; `Encoder::new(codec)` and `PCM_BYTES` survive
under `#[cfg(test)]` so the original tests run verbatim. `Encoder::clear_pending` now returns the
dropped byte count.

### Counter units

All counters are cumulative for the lifetime of one `Counters` value (one
`encode_with` task owns the packetizer fields; whoever calls
`record_writer_stale_packet` owns the writer fields). Each field is an
independent relaxed atomic; `snapshot()` loads them one at a time, so a
snapshot taken during activity can show, for example, an emitted packet count
that is one ahead of its byte total. They count this process's discards and packets prepared for broadcast; a receiver
may already be absent. They do not measure guest-side age or delivery.

| Field | Increment | Unit |
| --- | --- | --- |
| `stale_raw_events` / `_bytes` | raw `Event::Data` whose ingress is older than `MAX_AGE` on arrival | events / stereo s16le bytes |
| `stale_pending_discards` / `_bytes` | partial or just-completed packet whose *first* fragment aged past `MAX_AGE` | discards / PCM bytes |
| `reset_pending_discards` / `_bytes` | non-empty partial dropped by a stale raw event, `Begin`/`End`, lag or a request change | discards / PCM bytes |
| `raw_lag_events` / `_messages` | `RecvError::Lagged(n)` | notifications / skipped messages |
| `encoder_init_errors` | `Encoder::with_duration` failed; request stays unencoded | errors |
| `encoder_errors` | `Encoder::packet` failed on a complete fresh packet | errors |
| `emitted_packets` / `_frames` / `_wire_bytes` | packet produced for broadcast; receivers may be absent | packets / frames / GLA1 header+payload bytes |
| `writer_stale_packets` / `_bytes` | only via `record_writer_stale_packet` | packets / GLA1 wire bytes |

## Tests authored (28 passed under root execution)

Candidate `audio.rs` tests (17): the six original cases verbatim —
`opus_roundtrip_preserves_stereo_at_bounded_bitrate`,
`adpcm_is_advertised_and_framed_only_when_requested`,
`fragmented_pcm_is_exact_and_audio_is_only_encoded_on_request`,
`old_sound_and_partial_samples_do_not_cross_mute_or_overload`,
`fragmented_packet_retains_its_first_chunk_ingress_time`,
`expired_partial_packet_does_not_mix_with_fresh_sound_after_a_pause` — plus
`packet_duration_accepts_only_canonical_20_and_40`,
`forty_ms_encoder_freezes_header_and_payload_sizes_per_codec`,
`forty_ms_opus_roundtrip_preserves_stereo_at_bounded_bitrate`,
`two_20ms_fragments_assemble_into_one_40ms_packet_keeping_first_ingress`,
`forty_ms_pipeline_emits_opus_and_adpcm_that_decode_to_1920_frames`,
`forty_ms_pipeline_splits_a_large_chunk_into_whole_packets_and_one_partial`,
`first_fragment_age_is_bounded_by_the_fixed_100ms_budget_for_both_durations`,
`pause_reset_mute_and_stale_sound_discard_partial_40ms_packets`,
`raw_lag_clears_partial_sound_and_is_counted`,
`counters_start_at_zero_and_the_writer_stale_path_is_separate`,
`default_encode_matches_explicit_20ms_config_byte_for_byte`.

Wrapper tests (3):
`legacy_source_is_a_byte_identical_prefix_of_the_real_packetizer`
(`include_bytes!` against the live source),
`default_20ms_packets_are_byte_identical_to_the_frozen_original` (12
continuous tone blocks, all three codecs, candidate public pipeline versus the
original encoder), and
`fragmented_default_pipelines_and_status_match_the_frozen_original` (1,024-byte
fragments through both public `encode` pipelines, plus status JSON and invalid
control parity).

The example also runs the six original tests inside `legacy` and the two
`audio_adpcm` tests, for 28 in total. Three tests sleep about 120 ms (one of
them twice) to age a partial packet in real time, as the original does.

## Root-only serial commands (not executed here)

Root first writes a new example (nothing in this directory is a crate target):

```rust
// crates/gliss-server/examples/audio-packet-duration.rs
#[path = "../../../test-support/audio-packet-duration/wrapper.rs"]
mod wrapper;

fn main() {}
```

Then, serially and offline:

```sh
nice -n 19 python3 -I -B test-support/audio-packet-duration/verify_binding.py
nice -n 19 cargo fmt -p gliss-server -- --check
nice -n 19 cargo clippy -p gliss-server --example audio-packet-duration --all-targets -j1 --locked --offline -- -D warnings
nice -n 19 cargo test -p gliss-server --example audio-packet-duration -j1 --locked --offline -- --test-threads=1
```

If `cargo fmt` rewrites `audio.rs`, `legacy.rs` or `wrapper.rs`, the owned
hashes in `source-bindings.json` stop matching. The verifier prints the actual
hashes; re-freeze deliberately. A reformat of `legacy.rs` would also break its
byte-identical prefix. The `mod legacy;` item in `wrapper.rs` carries
`#[rustfmt::skip]` for that reason; if rustfmt still touches the file, add a
`rustfmt.toml` `ignore` entry or re-copy the prefix from the original source.

## Later integration sketch (root's call, not done here)

- `main.rs`: build one `audio::EncodeConfig::new(duration)` next to
  `audio::channel()`, pass a clone to `audio::encode_with(..)` instead of
  `audio::encode(..)`, and in the writer's age check call
  `config.counters.record_writer_stale_packet(packet.bytes.len())` on the
  `received.elapsed() > MAX_AGE` branch only.
- CLI: an operator-only `--audio-packet-ms` parsed with
  `PacketDuration::from_str` (clap `value_parser!(PacketDuration)` works
  through `FromStr`), default 20, wired like `configured_video_qp`.
- Status/protocol: none. A later `counters.snapshot()` exposure is a separate
  decision.

## Limits

- Not compiled, formatted, linted or tested; hand-checked only. `rustfmt`
  differences are likely in places and are formatting-only.
- Promoted into the binary crate, any new public item root leaves unwired
  (`millis`, `from_millis`, `snapshot`, `record_writer_stale_packet`,
  `EncodeConfig::new`) warns as dead code under `-D warnings`; the example
  wrapper's allowances cover that only there.
- `encoder_errors` and `encoder_init_errors` have no trigger in tests: the
  pipeline cannot feed a wrong length and Opus construction does not fail in
  practice; tests only assert they stay zero.
- The 100 ms completion-time age check and the pre-append check are both
  counted as `stale_pending`; tests cannot distinguish which fired.
- Counters are process-local and per `Counters` value; they say nothing about
  guest-source age, browser drops or delivery.
- Nothing about Chromium, other browsers, CPU, latency or fidelity is claimed.
