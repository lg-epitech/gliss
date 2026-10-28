//! Component only: fixed synthetic inputs, without capture/network/playback.
#[path = "../src/audio_adpcm.rs"]
mod audio_adpcm;
#[path = "support/audio_ogg.rs"]
mod audio_ogg;

use anyhow::{Result, ensure};
use serde_json::json;
use std::{f64::consts::TAU, time::Instant};

fn signal(kind: usize) -> Vec<i16> {
    let mut seed = 0x1357_2468_u32;
    (0..48_000)
        .flat_map(|frame| {
            let t = frame as f64 / 48_000.0;
            let envelope = if kind == 0 {
                1.0
            } else {
                (TAU * 3.0 * t).sin().powi(2) * 0.8 + 0.2
            };
            [0, 1].map(|channel| {
                let value = match kind {
                    0 => (TAU * [440.0, 880.0][channel] * t).sin() * [0.36, 0.24][channel],
                    1 => {
                        [180.0, 700.0, 1600.0, 3200.0]
                            .iter()
                            .enumerate()
                            .map(|(i, f)| {
                                (TAU * (f * (1.0 + channel as f64 * 0.13) * t + 25.0 * t * t)).sin()
                                    * 0.14
                                    / (i + 1) as f64
                            })
                            .sum::<f64>()
                            * envelope
                    }
                    _ => {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        (seed as f64 / u32::MAX as f64 * 2.0 - 1.0) * 0.25 * envelope
                    }
                };
                (value * 32767.0).round() as i16
            })
        })
        .collect()
}

fn decode(packets: &[Vec<u8>], preskip: usize, frames: usize) -> Result<Vec<f32>> {
    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo)?;
    let mut result = Vec::new();
    for packet in packets {
        let mut block = vec![0_f32; 1920];
        ensure!(
            decoder.decode_float(packet, &mut block, false)? == 960,
            "decode duration"
        );
        result.extend(block);
    }
    Ok(result[preskip * 2..(preskip + frames) * 2].to_vec())
}

fn main() -> Result<()> {
    let rolling = std::env::var("GLISS_OGG_COMPONENT_ROLLING").as_deref() == Ok("1");
    let adpcm = std::env::var("GLISS_AUDIO_COMPONENT_ADPCM").as_deref() == Ok("1");
    let mut arms = Vec::new();
    let mut sources = Vec::new();
    for (kind, name) in ["tones", "modulated-multitone", "seeded-noise"]
        .iter()
        .enumerate()
    {
        let source = signal(kind);
        sources.push(json!({"name": name, "pcm": source}));
        if adpcm {
            let mut encoder = audio_adpcm::Encoder::default();
            let mut chunks = Vec::new();
            let mut reference = Vec::new();
            let mut elapsed_ns = 0_u64;
            for samples in source.as_chunks::<1920>().0.iter() {
                let start = Instant::now();
                let packet = encoder.encode(samples)?;
                elapsed_ns += u64::try_from(start.elapsed().as_nanos())?;
                reference.extend(audio_adpcm::decode(&packet, 960)?);
                chunks.push(packet);
            }
            arms.push(
                json!({"signal": name, "mode": "ima-adpcm", "frames": 960, "chunks": chunks,
                "native_reference": reference, "encode_wall_ns": elapsed_ns}),
            );
            continue;
        }
        if rolling {
            let mut encoder =
                opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)?;
            encoder.set_bitrate(opus::Bitrate::Bits(96_000))?;
            encoder.set_complexity(4)?;
            encoder.set_vbr(false)?;
            encoder.set_dtx(false)?;
            let delay = encoder.get_lookahead()?;
            let start = Instant::now();
            let packets: Vec<Vec<u8>> = source
                .as_chunks::<1920>()
                .0
                .iter()
                .map(|s| encoder.encode_vec(s, 1275))
                .collect::<Result<_, _>>()?;
            let encoded_ns = u64::try_from(start.elapsed().as_nanos())?;
            let reference = decode(&packets, 0, 48_000)?;
            for frames in [960, 1920] {
                let mut chunks = Vec::new();
                let start = Instant::now();
                for end in (frames / 960..=packets.len()).step_by(frames / 960) {
                    let begin = (end - frames / 960).saturating_sub(4);
                    chunks.push(audio_ogg::rolling_file(&packets[begin..end], frames)?);
                }
                let mux_ns = u64::try_from(start.elapsed().as_nanos())?;
                arms.push(json!({"signal": name, "mode": "rolling-ogg", "frames": frames, "history_frames": 3840,
                    "source_delay_frames": delay, "chunks": chunks, "native_reference": reference,
                    "encode_wall_ns": encoded_ns + mux_ns}));
            }
            arms.push(json!({"signal": name, "mode": "continuous-cbr-reference", "frames": 960,
                "source_delay_frames": delay, "chunks": packets, "native_reference": reference, "encode_wall_ns": encoded_ns}));
            continue;
        }
        for frames in [960, 1920] {
            for history_frames in [0, 960, 1920, 3840] {
                let mut encoder = audio_ogg::IndependentEncoder::new()?;
                let mut chunks = Vec::new();
                let mut reference = Vec::new();
                let mut elapsed_ns = 0_u64;
                for (index, samples) in source.chunks_exact(frames * 2).enumerate() {
                    let end = index * frames * 2;
                    let start = end.saturating_sub(history_frames * 2);
                    let mut history = vec![0_i16; (history_frames * 2).saturating_sub(end)];
                    history.extend_from_slice(&source[start..end]);
                    let started = Instant::now();
                    let (file, packets, preskip) = encoder.encode(samples, &history)?;
                    elapsed_ns += u64::try_from(started.elapsed().as_nanos())?;
                    reference.extend(decode(&packets, usize::from(preskip), frames)?);
                    chunks.push(file);
                }
                arms.push(json!({"signal": name, "mode": "independent-ogg", "frames": frames, "history_frames": history_frames, "chunks": chunks,
                "native_reference": reference, "encode_wall_ns": elapsed_ns}));
            }
        }
        // The released preferred codec uses one continuous VBR encoder. This
        // provides a scoped distortion comparison, not an identical bitrate.
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(96_000))?;
        encoder.set_complexity(4)?;
        let preskip = usize::try_from(encoder.get_lookahead()?)?;
        let mut chunks = Vec::new();
        let started = Instant::now();
        for samples in source.as_chunks::<1920>().0.iter() {
            chunks.push(encoder.encode_vec(samples, 1275)?);
        }
        chunks.push(encoder.encode_vec(&[0; 1920], 1275)?);
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos())?;
        arms.push(json!({"signal": name, "mode": "continuous-opus-reference", "frames": 960,
            "native_reference": decode(&chunks, preskip, 48_000)?, "chunks": chunks, "encode_wall_ns": elapsed_ns}));
    }
    println!(
        "{}",
        json!({"schema": if adpcm { "gliss-ogg-audio-component-v4" } else if rolling { "gliss-ogg-audio-component-v3" } else { "gliss-ogg-audio-component-v2" }, "rate": 48000, "channels": 2,
        "source_frames": 48000, "sources": sources, "arms": arms})
    );
    Ok(())
}
