//! Prerequisite only: native 20/40 ms Opus packets for a browser accounting check.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    f64::consts::TAU,
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::Path,
    time::Instant,
};

const SOURCE_FRAMES: usize = 15_360;

fn source() -> Vec<i16> {
    (0..SOURCE_FRAMES)
        .flat_map(|frame| {
            [440.0, 880.0].map(|frequency| {
                ((TAU * frequency * frame as f64 / 48_000.0).sin() * 12_000.0).round() as i16
            })
        })
        .collect()
}

fn encoder() -> Result<opus::Encoder> {
    let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)?;
    encoder.set_bitrate(opus::Bitrate::Bits(96_000))?;
    encoder.set_complexity(4)?;
    Ok(encoder)
}

fn wire(payload: &[u8], frames: usize, sequence: u32) -> Result<Vec<u8>> {
    ensure!(
        [960, 1920].contains(&frames),
        "only 20/40 ms in this component"
    );
    ensure!(
        !payload.is_empty() && payload.len() <= 1275,
        "native packet cap"
    );
    let mut packet = b"GLA1".to_vec();
    packet.extend_from_slice(&[1, 2, 0, 0]);
    packet.extend_from_slice(&48_000_u32.to_le_bytes());
    packet.extend_from_slice(&sequence.to_le_bytes());
    packet.extend_from_slice(&u32::try_from(frames)?.to_le_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

fn record(writer: &mut BufWriter<std::fs::File>, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    ensure!(args.len() == 2, "supply one fresh output path");
    let path = Path::new(&args[1]);
    let journal_path = path.with_extension("progress.jsonl");
    ensure!(
        !path.exists() && !journal_path.exists(),
        "outputs must be fresh"
    );
    let mut journal = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(journal_path)?,
    );
    let pcm = source();
    let bytes: Vec<_> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
    let source_sha256 = hex::encode(Sha256::digest(bytes));
    record(
        &mut journal,
        &json!({"event":"start","frames":SOURCE_FRAMES,"source_sha256":source_sha256}),
    )?;
    let mut arms = Vec::new();
    for (arm, frames) in [960, 1920, 1920, 960].into_iter().enumerate() {
        let mut encoder = encoder()?;
        let lookahead = encoder.get_lookahead()?;
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo)?;
        let mut chunks = Vec::new();
        let mut total_frames = 0;
        let mut encode_wall_ns = 0_u64;
        let silence = vec![0_i16; frames * 2];
        for (index, samples) in pcm
            .chunks_exact(frames * 2)
            .chain(std::iter::once(silence.as_slice()))
            .enumerate()
        {
            record(
                &mut journal,
                &json!({"event":"attempt","arm":arm,"packet":index,"frames":frames}),
            )?;
            let start = Instant::now();
            let packet = encoder.encode_vec(samples, 1275)?;
            encode_wall_ns += u64::try_from(start.elapsed().as_nanos())?;
            let mut decoded = vec![0_f32; frames * 2];
            let count = decoder.decode_float(&packet, &mut decoded, false)?;
            ensure!(count == frames, "native decoder frame count");
            total_frames += count;
            let wire = wire(&packet, frames, u32::try_from(index)?)?;
            record(
                &mut journal,
                &json!({"event":"packet","arm":arm,"packet":index,
                "frames":frames,"bytes":wire.len(),"sha256":hex::encode(Sha256::digest(&wire))}),
            )?;
            chunks.push(wire);
        }
        arms.push(json!({"packet_frames":frames,"lookahead_frames":lookahead,
            "source_frames":SOURCE_FRAMES,"padding_packets":1,"native_decoded_frames":total_frames,
            "encode_wall_ns":encode_wall_ns,"chunks":chunks}));
    }
    let result = json!({"schema":"gliss-audio-duration-prerequisite-v1","sample_rate":48000,
        "channels":2,"source_frames":SOURCE_FRAMES,"source_pcm_sha256":source_sha256,"arms":arms});
    let mut output = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(path)?);
    serde_json::to_writer_pretty(&mut output, &result)?;
    output.write_all(b"\n")?;
    output.flush()?;
    output.get_ref().sync_all()?;
    record(&mut journal, &json!({"event":"complete"}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_durations_round_trip_through_native_opus_and_wire() {
        for frames in [960, 1920] {
            let payload = encoder()
                .unwrap()
                .encode_vec(&vec![0_i16; frames * 2], 1275)
                .unwrap();
            let bytes = wire(&payload, frames, u32::MAX).unwrap();
            assert_eq!(&bytes[..8], b"GLA1\x01\x02\0\0");
            assert_eq!(
                u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
                u32::MAX
            );
            assert_eq!(
                u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
                frames as u32
            );
            let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
            assert_eq!(
                decoder
                    .decode(&bytes[20..], &mut vec![0_i16; frames * 2], false)
                    .unwrap(),
                frames
            );
        }
    }

    #[test]
    fn component_rejects_unsupported_duration_and_payload_bounds() {
        for frames in [0, 480, 1440, 5760] {
            assert!(wire(&[0], frames, 0).is_err());
        }
        assert!(wire(&[], 960, 0).is_err());
        assert!(wire(&[0; 1276], 1920, 0).is_err());
        assert_eq!(source().len(), SOURCE_FRAMES * 2);
    }
}
