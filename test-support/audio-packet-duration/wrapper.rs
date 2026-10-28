//! Test-support wrapper for the 20/40 ms audio packet-duration candidate.
//! Root includes it from a gliss-server example, which supplies `fn main`:
//!
//! ```text
//! // crates/gliss-server/examples/audio-packet-duration.rs
//! #[path = "../../../test-support/audio-packet-duration/wrapper.rs"]
//! mod wrapper;
//! fn main() {}
//! ```
//!
//! Module paths below are relative to this file. `audio_adpcm` is the real
//! server module, so `super::audio_adpcm::…` inside both packetizers resolves
//! exactly as it does from `crates/gliss-server/src/main.rs`. The dead-code
//! allowances exist only because this wrapper is a test harness whose `main`
//! uses nothing; the candidate itself carries none.

#[allow(dead_code)]
#[path = "../../crates/gliss-server/src/audio_adpcm.rs"]
pub mod audio_adpcm;

/// Candidate packetizer (`audio.rs` beside this file).
#[allow(dead_code)]
#[path = "audio.rs"]
pub mod audio;

/// Byte-identical original `crates/gliss-server/src/audio.rs` plus a test bridge.
/// Skipped by rustfmt so the frozen prefix stays byte-identical.
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "legacy.rs"]
pub mod legacy;

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gliss_rfb::audio::{Event, SAMPLE_RATE};
    use serde_json::json;
    use tokio::sync::broadcast;

    use super::{audio, legacy};

    const BLOCK_BYTES: usize = 3840;
    const CODECS: [(&str, legacy::Codec); 3] = [
        ("pcm", legacy::Codec::Pcm),
        ("opus", legacy::Codec::Opus),
        ("ima-adpcm", legacy::Codec::ImaAdpcm),
    ];

    /// `count` consecutive 20 ms blocks of one continuous stereo tone.
    fn tone_blocks(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|block| {
                (block * 960..(block + 1) * 960)
                    .flat_map(|i| {
                        [440.0, 880.0].into_iter().flat_map(move |frequency| {
                            let sample = (12_000.0
                                * (i as f64 * frequency * std::f64::consts::TAU
                                    / f64::from(SAMPLE_RATE))
                                .sin()) as i16;
                            sample.to_le_bytes()
                        })
                    })
                    .collect()
            })
            .collect()
    }

    /// Runs either public `encode` pipeline with the same fragmented input and
    /// returns `(generation, received, wire bytes)` per emitted packet.
    macro_rules! pipeline_packets {
        ($module:ident, $codec:expr, $blocks:expr, $fragment:expr) => {{
            let (session, requests, _) = $module::channel();
            let (raw, receiver) = broadcast::channel(64);
            let mut packets = session.packets.subscribe();
            let task = tokio::spawn($module::encode(receiver, requests, session.packets.clone()));
            $module::request(&session.request, json!({"enabled":true, "codec":$codec})).unwrap();
            tokio::task::yield_now().await;
            let mut result: Vec<(u64, Instant, Vec<u8>)> = Vec::new();
            for block in $blocks.iter() {
                let received = Instant::now();
                for chunk in block.chunks($fragment) {
                    raw.send(Event::Data {
                        bytes: chunk.into(),
                        received,
                    })
                    .unwrap();
                }
                let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
                    .await
                    .unwrap()
                    .unwrap();
                result.push((packet.generation, packet.received, packet.bytes.to_vec()));
            }
            drop(raw);
            task.await.unwrap();
            result
        }};
    }

    #[test]
    fn live_source_matches_the_validated_candidate_and_keeps_the_frozen_original() {
        let live = include_bytes!("../../crates/gliss-server/src/audio.rs");
        assert_eq!(live.as_slice(), include_bytes!("audio.rs").as_slice());
        let frozen = include_bytes!("legacy.rs");
        // The binding verifier checks the original prefix's fixed SHA-256.
        let suffix = &frozen[18914..];
        assert!(suffix.starts_with(b"\n// ---- test-support bridge"));
        assert!(suffix.windows(12).any(|window| window == b"#[cfg(test)]"));
        assert_eq!(legacy::FRAMES, audio::FRAMES);
        assert_eq!(legacy::MAX_AGE, audio::MAX_AGE);
        assert_eq!(audio::PacketDuration::DEFAULT.frames(), legacy::FRAMES);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_20ms_packets_are_byte_identical_to_the_frozen_original() {
        let blocks = tone_blocks(12);
        for (name, codec) in CODECS {
            let expected = legacy::continuous_packets(codec, &blocks).unwrap();
            let actual = pipeline_packets!(audio, name, blocks, BLOCK_BYTES);
            assert_eq!(actual.len(), expected.len(), "{name}");
            for (index, ((generation, _, bytes), expected)) in
                actual.iter().zip(&expected).enumerate()
            {
                assert_eq!(*generation, 1, "{name} packet {index}");
                assert_eq!(
                    bytes, expected,
                    "{name} packet {index} differs from the original"
                );
                assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 960);
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fragmented_default_pipelines_and_status_match_the_frozen_original() {
        assert_eq!(
            audio::Status::default().message(),
            legacy::Status::default().message()
        );
        let invalid = json!({"enabled":true, "codec":"ima-adpcm-unknown"});
        let (candidate, ..) = audio::channel();
        let (original, ..) = legacy::channel();
        assert!(audio::request(&candidate.request, invalid.clone()).is_err());
        assert!(legacy::request(&original.request, invalid).is_err());
        assert_eq!(candidate.request.borrow().generation, 0);
        assert_eq!(original.request.borrow().generation, 0);

        let blocks = tone_blocks(8);
        for (name, _) in CODECS {
            let expected = pipeline_packets!(legacy, name, blocks, 1024);
            let actual = pipeline_packets!(audio, name, blocks, 1024);
            assert_eq!(actual.len(), 8, "{name}");
            assert_eq!(actual.len(), expected.len(), "{name}");
            for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
                assert_eq!(actual.0, expected.0, "{name} packet {index} generation");
                assert_eq!(actual.2, expected.2, "{name} packet {index} bytes");
            }
        }
    }
}
