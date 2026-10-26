use anyhow::{Context, Result, ensure};
use flate2::{Compression, write::ZlibEncoder};
use gliss_codec::{Decoder, Encoder, Mode, expand_batch_packet};
use gliss_core::Frame;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{io::Write, path::PathBuf, time::Instant};
#[path = "../../../../test-support/rust/codec/workloads.rs"]
mod workloads;

#[derive(Clone, Copy)]
enum Variant {
    FullRaw,
    FullDeflate,
    Tiled(Mode),
}
impl Variant {
    fn name(self) -> &'static str {
        match self {
            Self::FullRaw => "full_raw",
            Self::FullDeflate => "full_deflate",
            Self::Tiled(mode) => mode.name(),
        }
    }
}
#[derive(Serialize)]
struct Report {
    schema_version: u32,
    scope: &'static str,
    evaluation_version: &'static str,
    encoder_revision: &'static str,
    seed: String,
    tile_size: u16,
    repetitions: usize,
    nominal_fps: f64,
    clock: &'static str,
    platform: String,
    provenance: Provenance,
    workloads: Vec<WorkloadReport>,
}
#[derive(Serialize)]
struct Provenance {
    binary_sha256: String,
    codec_source_sha256: String,
    benchmark_source_sha256: String,
    workloads_source_sha256: String,
}
#[derive(Serialize)]
struct WorkloadReport {
    name: &'static str,
    held_out: bool,
    fingerprint: String,
    frames: usize,
    raw_frame_bytes: usize,
    results: Vec<Measurement>,
}
#[derive(Serialize)]
struct Measurement {
    mode: &'static str,
    exact_pixels: bool,
    bytes_per_run: usize,
    bytes_per_frame: f64,
    nominal_mbps: f64,
    byte_ratio_full_raw: f64,
    byte_ratio_full_deflate: f64,
    rectangles_by_codec: [usize; 9],
    keyframes_per_run: usize,
    encode_cpu_ms_total: f64,
    decode_cpu_ms_total: f64,
    encode_cpu_ms_per_frame: f64,
    decode_cpu_ms_per_frame: f64,
    encode_wall_ms_p50: f64,
    encode_wall_ms_p95: f64,
    decode_wall_ms_p50: f64,
    decode_wall_ms_p95: f64,
}
fn main() -> Result<()> {
    let mut repetitions = 3usize;
    let mut output = PathBuf::from("benchmarks/output.json");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repetitions" => repetitions = args.next().context("missing repetitions")?.parse()?,
            "--output" => output = PathBuf::from(args.next().context("missing output path")?),
            _ => anyhow::bail!("usage: codec-bench [--repetitions 1..10] [--output path]"),
        }
    }
    ensure!((1..=10).contains(&repetitions), "repetitions must be 1..10");
    let variants = [
        Variant::FullRaw,
        Variant::FullDeflate,
        Variant::Tiled(Mode::Raw),
        Variant::Tiled(Mode::Deflate),
        Variant::Tiled(Mode::Palette),
        Variant::Tiled(Mode::Xor),
        Variant::Tiled(Mode::Copy),
        Variant::Tiled(Mode::Adaptive),
        Variant::Tiled(Mode::Batch),
        Variant::Tiled(Mode::Global),
        Variant::Tiled(Mode::Sparse),
    ];
    let mut reports = Vec::new();
    for workload in workloads::suite() {
        let mut results = Vec::new();
        for variant in variants {
            eprintln!("{} / {}", workload.name, variant.name());
            results.push(measure(variant, &workload.frames, repetitions)?);
        }
        let raw = results[0].bytes_per_run as f64;
        let zipped = results[1].bytes_per_run as f64;
        for result in &mut results {
            result.byte_ratio_full_raw = result.bytes_per_run as f64 / raw;
            result.byte_ratio_full_deflate = result.bytes_per_run as f64 / zipped;
        }
        reports.push(WorkloadReport {
            name: workload.name,
            held_out: workload.held_out,
            fingerprint: workloads::fingerprint(&workload.frames),
            frames: workload.frames.len(),
            raw_frame_bytes: workload.frames.iter().map(|f| f.pixels.len()).sum(),
            results,
        });
    }
    let report = Report {
        schema_version: 3,
        scope: "synthetic CPU codec component workloads; no live network or desktop qualification",
        evaluation_version: workloads::VERSION,
        encoder_revision: "global-sparse-patches-v5",
        seed: format!("0x{:016x}", workloads::SEED),
        tile_size: 64,
        repetitions,
        nominal_fps: workloads::FPS,
        clock: "CLOCK_THREAD_CPUTIME_ID; Instant for wall percentiles",
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        provenance: Provenance {
            binary_sha256: sha256(&std::fs::read(std::env::current_exe()?)?)?,
            codec_source_sha256: sha256(include_bytes!("../lib.rs"))?,
            benchmark_source_sha256: sha256(include_bytes!("codec-bench.rs"))?,
            workloads_source_sha256: sha256(include_bytes!(
                "../../../../test-support/rust/codec/workloads.rs"
            ))?,
        },
        workloads: reports,
    };
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, format!("{json}\n"))?;
    Ok(())
}
fn measure(variant: Variant, frames: &[Frame], repetitions: usize) -> Result<Measurement> {
    let mut encode_cpu = 0.0;
    let mut decode_cpu = 0.0;
    let mut encode_wall = Vec::new();
    let mut decode_wall = Vec::new();
    let mut bytes_per_run = 0;
    let mut codecs = [0; 9];
    let mut keyframes = 0;
    // One untimed first-frame warmup, with fresh state for every measured run.
    let mut warm_encoder = Encoder::new(Mode::Adaptive, 64)?;
    let mut warm_decoder = Decoder::new();
    warm_decoder.decode(&warm_encoder.encode(&frames[0], 0, 0)?)?;
    for repetition in 0..repetitions {
        let mode = match variant {
            Variant::Tiled(mode) => mode,
            _ => Mode::Raw,
        };
        let mut encoder = Encoder::new(mode, 64)?;
        let mut decoder = Decoder::new();
        let mut bytes = 0;
        for (index, frame) in frames.iter().enumerate() {
            let wall = Instant::now();
            let cpu = cpu_seconds()?;
            let packet = match variant {
                Variant::FullRaw => full_packet(frame, index as u32, false)?,
                Variant::FullDeflate => full_packet(frame, index as u32, true)?,
                Variant::Tiled(_) => encoder.encode(frame, index as u32, index as u32)?,
            };
            encode_cpu += (cpu_seconds()? - cpu) * 1000.0;
            encode_wall.push(wall.elapsed().as_secs_f64() * 1000.0);
            let wall = Instant::now();
            let cpu = cpu_seconds()?;
            let decoded = decoder.decode(&packet)?;
            decode_cpu += (cpu_seconds()? - cpu) * 1000.0;
            decode_wall.push(wall.elapsed().as_secs_f64() * 1000.0);
            ensure!(
                decoded == *frame,
                "pixel fidelity failure: {}, frame {index}",
                variant.name()
            );
            bytes += packet.len();
            if repetition == 0 {
                keyframes += usize::from(packet[18] & 1 != 0);
                // Untimed metadata extraction; GLB1/GLB2/GLB3 have one compressed body.
                let expanded = expand_batch_packet(&packet)?;
                let packet = expanded.as_ref();
                let count = u16::from_le_bytes(packet[16..18].try_into().unwrap());
                let mut offset = 24;
                for _ in 0..count {
                    codecs[packet[offset + 8] as usize] += 1;
                    offset += 16
                        + u32::from_le_bytes(packet[offset + 12..offset + 16].try_into().unwrap())
                            as usize;
                }
            }
        }
        if repetition == 0 {
            bytes_per_run = bytes;
        } else {
            ensure!(bytes_per_run == bytes, "non-deterministic byte count");
        }
    }
    let n = (frames.len() * repetitions) as f64;
    let bytes_per_frame = bytes_per_run as f64 / frames.len() as f64;
    Ok(Measurement {
        mode: variant.name(),
        exact_pixels: true,
        bytes_per_run,
        bytes_per_frame,
        nominal_mbps: bytes_per_frame * 8.0 * workloads::FPS / 1_000_000.0,
        byte_ratio_full_raw: 0.0,
        byte_ratio_full_deflate: 0.0,
        rectangles_by_codec: codecs,
        keyframes_per_run: keyframes,
        encode_cpu_ms_total: encode_cpu / repetitions as f64,
        decode_cpu_ms_total: decode_cpu / repetitions as f64,
        encode_cpu_ms_per_frame: encode_cpu / n,
        decode_cpu_ms_per_frame: decode_cpu / n,
        encode_wall_ms_p50: percentile(&encode_wall, 0.50),
        encode_wall_ms_p95: percentile(&encode_wall, 0.95),
        decode_wall_ms_p50: percentile(&decode_wall, 0.50),
        decode_wall_ms_p95: percentile(&decode_wall, 0.95),
    })
}
fn sha256(bytes: &[u8]) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn full_packet(frame: &Frame, sequence: u32, zipped: bool) -> Result<Vec<u8>> {
    let body = if zipped {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&frame.pixels)?;
        encoder.finish()?
    } else {
        frame.pixels.clone()
    };
    let mut packet = Vec::with_capacity(40 + body.len());
    packet.extend_from_slice(b"GLS1");
    packet.extend_from_slice(&sequence.to_le_bytes());
    packet.extend_from_slice(&sequence.to_le_bytes());
    packet.extend_from_slice(&frame.width.to_le_bytes());
    packet.extend_from_slice(&frame.height.to_le_bytes());
    packet.extend_from_slice(&1u16.to_le_bytes());
    packet.extend_from_slice(&1u16.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet.extend_from_slice(&frame.width.to_le_bytes());
    packet.extend_from_slice(&frame.height.to_le_bytes());
    packet.extend_from_slice(&[u8::from(zipped), 0, 0, 0]);
    packet.extend_from_slice(&(body.len() as u32).to_le_bytes());
    packet.extend(body);
    Ok(packet)
}
fn percentile(values: &[f64], q: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by(f64::total_cmp);
    sorted[((sorted.len() as f64 * q).ceil() as usize).saturating_sub(1)]
}
#[cfg(target_os = "linux")]
fn cpu_seconds() -> Result<f64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // A valid stack timespec and supported Linux clock ID; no pointers escape this call.
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) } == 0,
        "CPU clock failed"
    );
    Ok(value.tv_sec as f64 + value.tv_nsec as f64 / 1_000_000_000.0)
}
#[cfg(not(target_os = "linux"))]
fn cpu_seconds() -> Result<f64> {
    anyhow::bail!("codec-bench currently requires Linux thread CPU clock")
}
