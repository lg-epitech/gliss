//! Fixed shared-input component: requested QP default vs26..51, no frame skips.
use anyhow::{Context, Result, ensure};
use gliss_core::Frame;
use gliss_video::{QpBounds, VideoEncoder, VideoOptions, VideoProfile, VideoTiming, parse_packet};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    time::Instant,
};

fn error(reference: &Frame, decoded: &[u8], area: (usize, usize, usize, usize)) -> (f64, f64) {
    let (x0, y0, width, height) = area;
    let (mut abs, mut square) = (0u64, 0u64);
    for y in y0..y0 + height {
        for x in x0..x0 + width {
            for c in 0..3 {
                let i = (y * 1280 + x) * 4 + c;
                let e = i32::from(reference.pixels[i]) - i32::from(decoded[i]);
                abs += u64::from(e.unsigned_abs());
                square += (e * e) as u64;
            }
        }
    }
    let n = (width * height * 3) as f64;
    (abs as f64 / n, square as f64 / n)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2,
        "usage: video-qp OWNED_INPUT.rgba NEW_REPORT.json"
    );
    let mut input = File::open(&args[0])?;
    ensure!(
        input.metadata()?.len() == 1280 * 800 * 4 * 12,
        "fixed input length required"
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args[1])?;
    let mut journal = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(std::path::PathBuf::from(&args[1]).with_extension("progress.jsonl"))?;
    writeln!(
        journal,
        "{{\"event\":\"start\",\"order\":[false,true,true,false],\"frames_per_arm\":12}}"
    )?;
    journal.sync_all()?;
    let started = Instant::now();
    let mut all = String::from(
        "{\"schema\":\"gliss-video-qp-component-v1\",\"frames_per_arm\":12,\"arms\":[",
    );
    for (arm, wide) in [false, true, true, false].into_iter().enumerate() {
        writeln!(
            journal,
            "{{\"event\":\"arm-start\",\"arm\":{arm},\"wide\":{wide}}}"
        )?;
        journal.sync_all()?;
        let options = VideoOptions {
            frame_skip: false,
            qp_bounds: if wide {
                Some(QpBounds::new(26, 51)?)
            } else {
                None
            },
        };
        let mut encoder = VideoEncoder::with_options(
            1280,
            800,
            500_000,
            VideoProfile::Screen,
            VideoTiming::new(30, 30, 60)?,
            options,
        )?;
        let mut decoder = Decoder::new()?;
        let mut frame = Frame::new(1280, 800).context("frame allocation")?;
        let mut decoded = vec![0u8; 1280 * 800 * 4];
        use std::io::{Seek, SeekFrom};
        input.seek(SeekFrom::Start(0))?;
        if arm > 0 {
            all.push(',');
        }
        all.push_str(&format!("{{\"wide\":{wide},\"frames\":["));
        for index in 0..12u32 {
            ensure!(started.elapsed().as_secs() < 45, "component deadline");
            input.read_exact(&mut frame.pixels)?;
            writeln!(
                journal,
                "{{\"event\":\"attempt\",\"arm\":{arm},\"index\":{index}}}"
            )?;
            journal.sync_all()?;
            let begin = Instant::now();
            let packet = encoder.encode(&frame, index + 1, 0)?;
            let encode_ns = begin.elapsed().as_nanos();
            let parsed = parse_packet(&packet)?;
            ensure!(parsed.sequence == index + 1, "sequence changed");
            let image = decoder
                .decode(parsed.payload)?
                .context("missing native output")?;
            ensure!(image.dimensions() == (1280, 800), "geometry changed");
            image.write_rgba8(&mut decoded);
            let (mae, mse) = error(&frame, &decoded, (0, 0, 1280, 800));
            let (document_mae, document_mse) = error(&frame, &decoded, (40, 40, 950, 460));
            let (texture_mae, texture_mse) = error(&frame, &decoded, (600, 540, 360, 180));
            if index > 0 {
                all.push(',');
            }
            let row = format!(
                "{{\"index\":{index},\"source_counter\":{},\"bytes\":{},\"keyframe\":{},\"encode_wall_ns\":{encode_ns},\"rgb_mae\":{mae},\"rgb_mse\":{mse},\"document_mae\":{document_mae},\"document_mse\":{document_mse},\"texture_mae\":{texture_mae},\"texture_mse\":{texture_mse}}}",
                index * 3,
                packet.len(),
                parsed.keyframe
            );
            writeln!(
                journal,
                "{{\"event\":\"frame\",\"arm\":{arm},\"data\":{row}}}"
            )?;
            journal.sync_all()?;
            all.push_str(&row);
        }
        all.push_str("]}");
    }
    all.push_str("]}\n");
    output.write_all(all.as_bytes())?;
    output.sync_all()?;
    writeln!(journal, "{{\"event\":\"complete\"}}")?;
    journal.sync_all()?;
    Ok(())
}
