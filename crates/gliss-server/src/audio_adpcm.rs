//! IMA four-bit prediction with an independently decodable stereo block.
//! Gliss packs one subsequent frame per byte (left low nibble, right high).
use anyhow::{Result, ensure};

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const INDEX: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Default)]
pub struct Encoder {
    indices: [usize; 2],
}

impl Encoder {
    pub fn encode(&mut self, samples: &[i16]) -> Result<Vec<u8>> {
        ensure!(
            !samples.is_empty() && samples.len().is_multiple_of(2) && samples.len() <= 5760 * 2,
            "invalid ADPCM stereo duration"
        );
        let mut predictors = [i32::from(samples[0]), i32::from(samples[1])];
        let mut result = Vec::with_capacity(samples.len() / 2 + 7);
        for (channel, sample) in samples[..2].iter().enumerate() {
            result.extend_from_slice(&sample.to_le_bytes());
            result.extend_from_slice(&[self.indices[channel] as u8, 0]);
        }
        for frame in samples[2..].as_chunks::<2>().0 {
            let mut byte = 0_u8;
            for channel in 0..2 {
                let step = STEPS[self.indices[channel]];
                let difference = i32::from(frame[channel]) - predictors[channel];
                let mut remaining = difference.abs();
                let mut nibble = if difference < 0 { 8 } else { 0 };
                for (bit, threshold) in [(4, step), (2, step >> 1), (1, step >> 2)] {
                    if remaining >= threshold {
                        nibble |= bit;
                        remaining -= threshold;
                    }
                }
                advance(&mut predictors[channel], &mut self.indices[channel], nibble);
                byte |= nibble << (channel * 4);
            }
            result.push(byte);
        }
        Ok(result)
    }
}

fn advance(predictor: &mut i32, index: &mut usize, nibble: u8) {
    let step = STEPS[*index];
    let difference = (step >> 3)
        + if nibble & 4 != 0 { step } else { 0 }
        + if nibble & 2 != 0 { step >> 1 } else { 0 }
        + if nibble & 1 != 0 { step >> 2 } else { 0 };
    *predictor = (*predictor
        + if nibble & 8 != 0 {
            -difference
        } else {
            difference
        })
    .clamp(-32768, 32767);
    *index = (*index as i32 + INDEX[usize::from(nibble & 7)]).clamp(0, 88) as usize;
}

/// Reference decoder for tests; the browser has its own decoder.
#[cfg(test)]
pub fn decode(bytes: &[u8], frames: usize) -> Result<Vec<f32>> {
    ensure!(
        (1..=5760).contains(&frames) && bytes.len() == frames + 7,
        "invalid ADPCM block length"
    );
    let mut predictors = [0_i32; 2];
    let mut indices = [0_usize; 2];
    for channel in 0..2 {
        let header = &bytes[channel * 4..channel * 4 + 4];
        ensure!(header[2] <= 88 && header[3] == 0, "invalid ADPCM state");
        predictors[channel] = i32::from(i16::from_le_bytes([header[0], header[1]]));
        indices[channel] = usize::from(header[2]);
    }
    let mut result = Vec::with_capacity(frames * 2);
    result.extend(predictors.map(|v| v as f32 / 32768.0));
    for byte in &bytes[8..] {
        for channel in 0..2 {
            advance(
                &mut predictors[channel],
                &mut indices[channel],
                (byte >> (channel * 4)) & 15,
            );
            result.push(predictors[channel] as f32 / 32768.0);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_vectors_preserve_channel_order_and_saturate() -> Result<()> {
        // Index zero/step seven: code 7 adds 11, code 15 subtracts 11;
        // the next step is 16, and signed clipping happens before output.
        let bytes = [0xfe, 0x7f, 0, 0, 0x01, 0x80, 0, 0, 0xf7, 0xf7];
        assert_eq!(
            decode(&bytes, 3)?,
            vec![
                32766.0 / 32768.0,
                -32767.0 / 32768.0,
                32767.0 / 32768.0,
                -1.0,
                32767.0 / 32768.0,
                -1.0
            ]
        );
        let mut encoder = Encoder::default();
        assert_eq!(
            encoder.encode(&[0, 0, 11, -11, 41, -41])?,
            [0, 0, 0, 0, 0, 0, 0, 0, 0xf7, 0xf7]
        );
        Ok(())
    }

    #[test]
    fn every_packet_can_decode_without_earlier_packets() -> Result<()> {
        let mut encoder = Encoder::default();
        for phase in 0..30 {
            let source: Vec<i16> = (0..960)
                .flat_map(|i| {
                    [
                        (i * 37 + phase * 71) as i16,
                        (-(i * 23) - phase * 31) as i16,
                    ]
                })
                .collect();
            let bytes = encoder.encode(&source)?;
            assert_eq!(bytes.len(), 967);
            assert_eq!(
                &decode(&bytes, 960)?[..2],
                &[source[0] as f32 / 32768.0, source[1] as f32 / 32768.0]
            );
        }
        for bad in [Vec::new(), vec![0; 7], vec![0; 9]] {
            assert!(decode(&bad, 1).is_err());
        }
        let mut bad = vec![0; 8];
        bad[2] = 89;
        assert!(decode(&bad, 1).is_err());
        bad[2] = 0;
        bad[7] = 1;
        assert!(decode(&bad, 1).is_err());
        assert!(encoder.encode(&[0]).is_err());
        Ok(())
    }
}
