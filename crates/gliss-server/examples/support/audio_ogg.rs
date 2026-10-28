//! Independent Ogg Opus files for the Web Audio fallback experiment.
//! This module is not wired into the server until the browser proof passes.
use anyhow::{Result, ensure};

const RATE: u32 = 48_000;
const BLOCK: usize = 960;

pub struct IndependentEncoder {
    encoder: opus::Encoder,
    lookahead: u16,
}

impl IndependentEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder =
            opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(96_000))?;
        encoder.set_complexity(4)?;
        encoder.set_vbr(false)?;
        encoder.set_dtx(false)?;
        let lookahead = u16::try_from(encoder.get_lookahead()?)?;
        ensure!(
            usize::from(lookahead) < BLOCK,
            "Opus lookahead exceeds padding"
        );
        Ok(Self { encoder, lookahead })
    }

    pub fn encode(
        &mut self,
        samples: &[i16],
        history: &[i16],
    ) -> Result<(Vec<u8>, Vec<Vec<u8>>, u16)> {
        ensure!(
            [BLOCK * 2, BLOCK * 4].contains(&samples.len()),
            "expected 20 or 40 ms of stereo PCM"
        );
        ensure!(
            [0, BLOCK * 2, BLOCK * 4, BLOCK * 8].contains(&history.len()),
            "invalid pre-roll duration"
        );
        self.encoder.reset_state()?;
        let mut packets = Vec::new();
        for block in history
            .as_chunks::<{ BLOCK * 2 }>()
            .0
            .iter()
            .chain(samples.as_chunks::<{ BLOCK * 2 }>().0.iter())
        {
            packets.push(self.encoder.encode_vec(block, 1275)?);
        }
        // Flush the encoder delay. Pre-skip and the final granule retain exactly
        // the source frames, rather than losing the end of every small file.
        packets.push(self.encoder.encode_vec(&[0; BLOCK * 2], 1275)?);
        ensure!(
            packets.iter().all(|packet| packet.len() == 240),
            "unexpected CBR packet size"
        );
        let frames = samples.len() / 2;
        let preskip = self.lookahead + (history.len() / 2) as u16;
        let mut head = b"OpusHead\x01\x02".to_vec();
        head.extend_from_slice(&preskip.to_le_bytes());
        head.extend_from_slice(&RATE.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut result = page(2, 0, 0, &[&head]);
        result.extend(page(0, 1, 0, &[b"OpusTags\0\0\0\0\0\0\0\0"]));
        let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
        result.extend(page(4, 2, u64::from(preskip) + frames as u64, &refs));
        Ok((result, packets, preskip))
    }
}

fn page(flags: u8, sequence: u32, granule: u64, packets: &[&[u8]]) -> Vec<u8> {
    let mut lacing = Vec::new();
    for packet in packets {
        lacing.extend(std::iter::repeat_n(255, packet.len() / 255));
        lacing.push((packet.len() % 255) as u8);
    }
    assert!(lacing.len() <= 255);
    let mut result = b"OggS\0".to_vec();
    result.push(flags);
    result.extend(granule.to_le_bytes());
    result.extend(1_u32.to_le_bytes());
    result.extend(sequence.to_le_bytes());
    result.extend([0; 4]);
    result.push(lacing.len() as u8);
    result.extend(lacing);
    for packet in packets {
        result.extend_from_slice(packet);
    }
    let crc = checksum(&result);
    result[22..26].copy_from_slice(&crc.to_le_bytes());
    result
}

pub fn rolling_file(packets: &[Vec<u8>], frames: usize) -> Result<Vec<u8>> {
    ensure!([960, 1920].contains(&frames), "invalid current duration");
    let current_packets = frames / BLOCK;
    ensure!(
        (current_packets..=current_packets + 4).contains(&packets.len()),
        "invalid pre-roll window"
    );
    ensure!(packets.iter().all(|p| p.len() == 240), "invalid CBR packet");
    let preskip = ((packets.len() - current_packets) * BLOCK) as u16;
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&preskip.to_le_bytes());
    head.extend_from_slice(&RATE.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    let mut result = page(2, 0, 0, &[&head]);
    result.extend(page(0, 1, 0, &[b"OpusTags\0\0\0\0\0\0\0\0"]));
    let refs: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
    result.extend(page(4, 2, u64::from(preskip) + frames as u64, &refs));
    Ok(result)
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_independent_files_have_exact_preskip_and_end_granules() -> Result<()> {
        let mut encoder = IndependentEncoder::new()?;
        for frames in [960, 1920] {
            let (file, packets, preskip) = encoder.encode(&vec![1234; frames * 2], &[])?;
            assert_eq!(file.len(), if frames == 960 { 600 } else { 841 });
            let mut offset = 0;
            let mut pages = 0;
            while offset < file.len() {
                let segments = usize::from(file[offset + 26]);
                let size = 27
                    + segments
                    + file[offset + 27..offset + 27 + segments]
                        .iter()
                        .map(|v| usize::from(*v))
                        .sum::<usize>();
                let mut copy = file[offset..offset + size].to_vec();
                let crc = u32::from_le_bytes(copy[22..26].try_into()?);
                copy[22..26].fill(0);
                assert_eq!(checksum(&copy), crc);
                if pages == 2 {
                    assert_eq!(copy[5], 4);
                    assert_eq!(
                        u64::from_le_bytes(copy[6..14].try_into()?),
                        u64::from(preskip) + frames as u64
                    );
                }
                offset += size;
                pages += 1;
            }
            assert_eq!(pages, 3);
            assert_eq!(packets.len(), frames / BLOCK + 1);
        }
        assert!(encoder.encode(&[0; 2], &[]).is_err());
        assert!(encoder.encode(&[0; BLOCK * 2], &[0; 2]).is_err());
        for history_frames in [960, 1920, 3840] {
            let (file, packets, preskip) =
                encoder.encode(&[0; BLOCK * 2], &vec![0; history_frames * 2])?;
            assert_eq!(
                preskip as usize,
                history_frames + encoder.lookahead as usize
            );
            assert_eq!(packets.len(), history_frames / BLOCK + 2);
            assert_eq!(file.len(), 600 + 241 * history_frames / BLOCK);
        }
        Ok(())
    }
}
