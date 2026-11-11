//! Fuzzes the ADPCM encoder and the decoder model beside it, and writes the
//! vectors that hold the browser's decoder to the same samples.
//!
//! `tests/fuzz-audio.test.mjs` decodes every block in the vector file with
//! `web/audio-adpcm.js` and compares the fingerprint of what it gets.

use std::io::Write;

use super::{Encoder, decode};
use crate::fuzz::support::{self, Rng};

/// Stereo samples of `frames` frames: silence, the loudest square wave, noise
/// and slow movement, which take the step index to both of its ends.
fn samples(rng: &mut Rng, frames: usize) -> Vec<i16> {
    let kind = rng.below(6);
    let mut level = [rng.next() as i16, rng.next() as i16];
    (0..frames * 2)
        .map(|index| match kind {
            0 => 0,
            1 => {
                if (index / 2) % 2 == 0 {
                    i16::MAX
                } else {
                    i16::MIN
                }
            }
            2 => rng.next() as i16,
            3 => {
                let channel = &mut level[index % 2];
                *channel = channel.saturating_add(rng.between(0, 64) as i16 - 32);
                *channel
            }
            4 => level[index % 2],
            _ => rng.pick(&[i16::MIN, -1, 0, 1, i16::MAX]),
        })
        .collect()
}

/// What the decoder model makes of a block, as a vector file states it: the
/// fingerprint of the samples as 16-bit numbers, or that it is refused.
fn outcome(bytes: &[u8], frames: usize) -> String {
    match decode(bytes, frames) {
        Ok(decoded) => {
            let samples: Vec<u8> = decoded
                .iter()
                .flat_map(|sample| ((sample * 32768.0) as i16).to_le_bytes())
                .collect();
            format!("{:016x}", support::fingerprint(&samples))
        }
        Err(_) => "refused".to_owned(),
    }
}

fn vector(bytes: &[u8], frames: usize) -> String {
    format!(
        "{frames} {} {}\n",
        support::hex(bytes),
        outcome(bytes, frames)
    )
}

/// Whether a block is one the format allows, read from `docs/audio-wire.md`:
/// 1 to 5760 frames, two four-byte channel headers with a step index of at
/// most 88 and a zero byte, then one byte for each frame after the first.
fn well_formed(bytes: &[u8], frames: usize) -> bool {
    (1..=5760).contains(&frames)
        && bytes.len() == frames + 7
        && [2, 6].iter().all(|at| bytes[*at] <= 88)
        && bytes[3] == 0
        && bytes[7] == 0
}

/// A block and the frame count it is decoded with: what the encoder writes
/// for some samples, any nibbles behind a valid header, or one of them with
/// bytes changed.
fn block(rng: &mut Rng, encoder: &mut Encoder, most_frames: usize) -> (Vec<u8>, usize) {
    let frames = match rng.below(8) {
        0 => 1,
        1 => 2,
        2 => most_frames,
        _ => rng.between(1, most_frames.min(48)),
    };
    let mut bytes = if rng.chance(2) {
        let source = samples(rng, frames);
        let bytes = encoder.encode(&source).unwrap();
        assert_eq!(bytes.len(), frames + 7);
        // Each block starts from the exact first samples and the step index
        // the encoder had reached, so it decodes without the one before.
        let decoded = decode(&bytes, frames).unwrap();
        assert_eq!(decoded.len(), frames * 2);
        assert_eq!(decoded[0], f32::from(source[0]) / 32768.0);
        assert_eq!(decoded[1], f32::from(source[1]) / 32768.0);
        bytes
    } else {
        let mut bytes = rng.bytes(frames + 7);
        (bytes[2], bytes[6]) = (rng.below(89) as u8, rng.below(89) as u8);
        (bytes[3], bytes[7]) = (0, 0);
        bytes
    };
    let mut frames = frames;
    match rng.below(10) {
        0 => support::mutate(rng, &mut bytes, &[2, 3, 6, 7], false),
        1 => frames = rng.pick(&[0, frames + 1, frames.saturating_sub(1), 5760, 5761]),
        // The last step index there is, the first there is not, and a
        // reserved byte that is not zero.
        2 => bytes[rng.pick(&[2, 6])] = rng.pick(&[88, 89, 255]),
        3 => bytes[rng.pick(&[3, 7])] = rng.pick(&[1, 255]),
        _ => {}
    }
    (bytes, frames)
}

fn checked(bytes: &[u8], frames: usize) {
    match decode(bytes, frames) {
        Ok(decoded) => {
            assert!(well_formed(bytes, frames));
            assert_eq!(decoded.len(), frames * 2);
            assert!(decoded.iter().all(|sample| (-1.0..1.0).contains(sample)));
        }
        Err(_) => assert!(!well_formed(bytes, frames)),
    }
}

const DEFAULT_ITERATIONS: u64 = 300;

#[test]
fn fuzz_adpcm_blocks() {
    let mut vectors = support::campaign_vectors("adpcm-vectors.txt");
    support::run("server-adpcm", DEFAULT_ITERATIONS, |rng, _| {
        let mut encoder = Encoder::default();
        for _ in 0..rng.between(1, 4) {
            // A campaign's blocks are mostly short, so that the Node side
            // gets through as many of them.
            let most = if rng.chance(20) { 5760 } else { 200 };
            let (bytes, frames) = block(rng, &mut encoder, most);
            checked(&bytes, frames);
            if let Some(file) = &mut vectors {
                file.write_all(vector(&bytes, frames).as_bytes()).unwrap();
            }
        }
    });
}

#[test]
fn the_vectors_for_the_browser_decoder_are_current() {
    // The same blocks on every run: these are the cases the Node suite
    // checks without a campaign.
    let mut rng = Rng::new(0x61_64_70_63_6d);
    let mut encoder = Encoder::default();
    let mut vectors = String::new();
    for most in [1, 2, 3, 960, 1920, 5760] {
        let source = samples(&mut rng, most);
        vectors.push_str(&vector(&encoder.encode(&source).unwrap(), most));
    }
    for _ in 0..90 {
        let (bytes, frames) = block(&mut rng, &mut encoder, 64);
        checked(&bytes, frames);
        vectors.push_str(&vector(&bytes, frames));
    }
    support::assert_vectors("adpcm-vectors.txt", &vectors);
}
