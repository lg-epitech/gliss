//! Offline owned RGB input only; unchanged public software encoder configuration.
use anyhow::{Context, Result, ensure};
use gliss_core::Frame;
use gliss_video::{MAX_PAYLOAD, VideoEncoder, VideoProfile, VideoTiming, parse_packet};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn create(path: PathBuf) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn record(journal: &mut File, records: &mut usize, event: &str) -> Result<()> {
    ensure!(*records < 64 && event.len() < 1024, "native journal bound");
    journal.write_all(event.as_bytes())?;
    journal.write_all(b"\n")?;
    journal.sync_all()?;
    *records += 1;
    Ok(())
}

fn work() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 6,
        "usage: desktop-fidelity RGB WIDTH HEIGHT FRAMES BITRATE NEW_PRIVATE_DIR"
    );
    let input = PathBuf::from(&args[0]);
    let width: u16 = args[1].to_str().context("width")?.parse()?;
    let height: u16 = args[2].to_str().context("height")?.parse()?;
    let count: usize = args[3].to_str().context("count")?.parse()?;
    let bitrate: u32 = args[4].to_str().context("bitrate")?.parse()?;
    let output = PathBuf::from(&args[5]);
    ensure!(
        (width, height, count) == (1280, 800, 12),
        "fixed fixture geometry/count required"
    );
    ensure!(
        [1_000_000, 2_000_000, 4_000_000].contains(&bitrate),
        "undeclared bitrate"
    );
    let parent = fs::canonicalize(input.parent().context("input parent")?)?;
    ensure!(
        fs::canonicalize(output.parent().context("output parent")?)? == parent,
        "output must be a new child of the private input root"
    );
    ensure!(
        input
            .file_name()
            .is_some_and(|name| name == "reference.rgb"),
        "owned fixture filename required"
    );
    let owner = fs::metadata(&parent)?;
    let metadata = fs::symlink_metadata(&input)?;
    let frame_bytes = usize::from(width) * usize::from(height) * 3;
    ensure!(
        owner.is_dir() && owner.mode() & 0o077 == 0,
        "private root required"
    );
    ensure!(
        metadata.is_file() && metadata.uid() == owner.uid() && metadata.mode() & 0o077 == 0,
        "private owned regular RGB file required"
    );
    ensure!(
        metadata.len() == (frame_bytes * count) as u64,
        "exact raw RGB length required"
    );
    DirBuilder::new().mode(0o700).create(&output)?;
    let mut journal = create(output.join("progress.jsonl"))?;
    File::open(&output)?.sync_all()?;
    File::open(&parent)?.sync_all()?;
    let mut records = 0;
    record(&mut journal, &mut records, "{\"event\":\"start\"}")?;
    let mut stage = "constructor";
    let mut current = None;
    let outcome = (|| -> Result<()> {
        let lifetime = Instant::now();
        let started = Instant::now();
        let mut encoder = VideoEncoder::with_timing(
            width,
            height,
            bitrate,
            VideoProfile::Screen,
            VideoTiming::new(30, 30, 1)?,
        )?;
        let constructor_wall_ns = started.elapsed().as_nanos();
        let timing = encoder.timing();
        record(
            &mut journal,
            &mut records,
            &format!(
                "{{\"event\":\"constructor\",\"wall_ns\":{constructor_wall_ns},\"timing\":{{\"fps\":{},\"timestamp_fps\":{},\"intra_period\":{}}}}}",
                timing.fps(),
                timing.timestamp_fps(),
                timing.intra_period(),
            ),
        )?;
        stage = "allocation";
        let mut source = File::open(input)?;
        let mut rgb = vec![0; frame_bytes];
        let mut frame = Frame::new(width, height).context("RGBA frame allocation")?;
        let mut total = 0;
        let mut manifest = format!(
            "{{\"bitrate\":{bitrate},\"constructor_wall_ns\":{constructor_wall_ns},\"timing\":{{\"fps\":{},\"timestamp_fps\":{},\"intra_period\":{}}},\"frames\":[",
            timing.fps(),
            timing.timestamp_fps(),
            timing.intra_period(),
        );
        for index in 0..count {
            current = Some(index);
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"attempt\",\"frame\":{index}}}"),
            )?;
            stage = "deadline";
            ensure!(
                lifetime.elapsed() < Duration::from_secs(24),
                "native work deadline"
            );
            stage = "frame-input";
            source.read_exact(&mut rgb)?;
            for (pixel, color) in frame
                .pixels
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(rgb.as_chunks::<3>().0)
            {
                pixel[..3].copy_from_slice(color);
                pixel[3] = 255;
            }
            stage = "encoder-call";
            let started = Instant::now();
            let packet = encoder.encode(&frame, index as u32, 0)?;
            let encode_wall_ns = started.elapsed().as_nanos();
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"encoded\",\"frame\":{index},\"encode_wall_ns\":{encode_wall_ns}}}"
                ),
            )?;
            stage = "packet-parse";
            let parsed = parse_packet(&packet)?;
            let bytes = parsed.payload.len();
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"parsed\",\"frame\":{index},\"elementary_bytes\":{bytes},\"observed_width\":{},\"observed_height\":{},\"observed_sequence\":{},\"keyframe\":{}}}",
                    parsed.width, parsed.height, parsed.sequence, parsed.keyframe,
                ),
            )?;
            stage = "all-idr-contract";
            ensure!(
                parsed.keyframe
                    && parsed.sequence == index as u32
                    && (parsed.width, parsed.height) == (width, height),
                "all-IDR contract"
            );
            total += bytes;
            stage = "coded-byte-bound";
            ensure!(
                bytes > 0 && bytes <= MAX_PAYLOAD && total <= 64 * 1024 * 1024,
                "coded byte bound"
            );
            stage = "au-write";
            let mut access_unit = create(output.join(format!("{index:02}.au")))?;
            access_unit.write_all(parsed.payload)?;
            access_unit.sync_all()?;
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"stored\",\"frame\":{index}}}"),
            )?;
            if index > 0 {
                manifest.push(',');
            }
            manifest.push_str(&format!(
                "{{\"frame\":{index},\"elementary_bytes\":{bytes},\"encode_wall_ns\":{encode_wall_ns}}}"
            ));
        }
        manifest.push_str("]}\n");
        stage = "manifest-write";
        let mut final_manifest = create(output.join("manifest.json"))?;
        final_manifest.write_all(manifest.as_bytes())?;
        final_manifest.sync_all()?;
        File::open(&output)?.sync_all()?;
        record(&mut journal, &mut records, "{\"event\":\"complete\"}")?;
        Ok(())
    })();
    if outcome.is_err() {
        let index = current.map_or_else(|| "null".to_owned(), |value| value.to_string());
        let _ = record(
            &mut journal,
            &mut records,
            &format!("{{\"event\":\"failure\",\"reason\":\"{stage}\",\"frame\":{index}}}"),
        );
    }
    outcome
}

fn main() -> std::process::ExitCode {
    if work().is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("desktop-fidelity: failed");
        std::process::ExitCode::FAILURE
    }
}
