//! Prospective CPU-only component source. Root owns validation and execution.
use anyhow::{Context, Result, ensure};
use gliss_core::Frame;
use gliss_video::{VideoEncoder, VideoProfile, VideoTiming, parse_packet};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use openh264::{OpenH264API, Timestamp, nal_units};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

const W: usize = 1280;
const H: usize = 800;
const COUNT: usize = 12;
const MAX_AU: usize = 16 * 1024 * 1024;
const MAX_ARM: usize = 64 * 1024 * 1024;
const MEMORY: usize = 1_610_612_736;

// Same constructor chain as native_encoder(Screen, 2M, 30/30/1); no qp setter.
fn configuration() -> EncoderConfig {
    EncoderConfig::new()
        .bitrate(BitRate::from_bps(2_000_000))
        .max_frame_rate(FrameRate::from_hz(30_f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .scene_change_detect(true)
        .profile(Profile::Baseline)
        .complexity(Complexity::Low)
        .adaptive_quantization(false)
        .background_detection(false)
        .num_threads(1)
        .skip_frames(false)
        .long_term_reference(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(1))
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .vui(VuiConfig::bt601())
}

fn kind(nal: &[u8]) -> Option<u8> {
    let prefix = if nal.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    nal.get(prefix).filter(|b| **b & 128 == 0).map(|b| b & 31)
}

// Exact production cache-and-prepend bytes, including pinned nal_units behavior.
// The runner separately parses SPS/PPS references and geometry without rewriting.
fn normalize(raw: Vec<u8>, sps: &mut Vec<u8>, pps: &mut Vec<u8>) -> Result<Vec<u8>> {
    let (mut has_sps, mut has_pps) = (false, false);
    for nal in nal_units(&raw) {
        match kind(nal) {
            Some(7) => {
                *sps = nal.to_vec();
                has_sps = true;
            }
            Some(8) => {
                *pps = nal.to_vec();
                has_pps = true;
            }
            Some(5 | 6 | 9..=12) => {}
            _ => anyhow::bail!("nal-type"),
        }
    }
    if !has_sps || !has_pps {
        ensure!(!sps.is_empty() && !pps.is_empty(), "parameter-sets");
        Ok([sps.as_slice(), pps.as_slice(), raw.as_slice()].concat())
    } else {
        Ok(raw)
    }
}

fn equality(actual: &[u8], reference: &[u8]) -> Result<()> {
    ensure!(actual == reference, "full-payload-equality");
    Ok(())
}

fn active_rows(data: &[u8], stride: usize, width: usize, height: usize) -> Result<Vec<u8>> {
    ensure!(width > 0 && height > 0 && stride >= width, "plane-layout");
    ensure!(
        stride.checked_mul(height).is_some_and(|n| n <= data.len()),
        "plane-length"
    );
    let mut active = Vec::with_capacity(width * height);
    for row in 0..height {
        active.extend_from_slice(&data[row * stride..row * stride + width]);
    }
    Ok(active)
}

fn active_i420(source: &impl YUVSource) -> Result<Vec<u8>> {
    ensure!(source.dimensions() == (W, H), "input-geometry");
    let (sy, su, sv) = source.strides();
    let mut bytes = active_rows(source.y(), sy, W, H)?;
    bytes.extend(active_rows(source.u(), su, W / 2, H / 2)?);
    bytes.extend(active_rows(source.v(), sv, W / 2, H / 2)?);
    ensure!(bytes.len() == W * H * 3 / 2, "i420-length");
    Ok(bytes)
}

fn create(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn store(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("parent")?)?.sync_all()?;
    Ok(())
}

fn record(file: &mut File, count: &mut usize, value: &str) -> Result<()> {
    ensure!(*count < 96 && value.len() < 1024, "journal-bound");
    file.write_all(value.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    *count += 1;
    Ok(())
}

fn private(path: &Path) -> Result<fs::Metadata> {
    let info = fs::symlink_metadata(path)?;
    ensure!(
        info.is_dir() && info.mode() & 0o077 == 0,
        "private-directory"
    );
    Ok(info)
}

// Read-only libc ABI, no new dependency. The supported experiment host is Linux64.
// Python also verifies an exec child's exact inherited soft/hard ceiling first.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn inherited_memory() -> Result<()> {
    #[repr(C)]
    struct Limit {
        soft: usize,
        hard: usize,
    }
    unsafe extern "C" {
        fn getrlimit(resource: i32, limit: *mut Limit) -> i32;
    }
    let mut value = Limit { soft: 0, hard: 0 };
    // Linux RLIMIT_AS=9; getrlimit only writes this correctly sized local value.
    let result = unsafe { getrlimit(9, &mut value) };
    ensure!(
        result == 0 && value.soft == MEMORY && value.hard == MEMORY,
        "inherited-memory"
    );
    Ok(())
}
#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn inherited_memory() -> Result<()> {
    anyhow::bail!("unsupported-host")
}

fn work() -> Result<()> {
    // No media constructor, input allocation or input open before this check.
    inherited_memory()?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2,
        "usage: all-idr-x264 REFERENCE_RGB NEW_PRIVATE_DIR"
    );
    let input = Path::new(&args[0]);
    let output = Path::new(&args[1]);
    let owner = private(input.parent().context("input-parent")?)?;
    let destination = private(output.parent().context("output-parent")?)?;
    let info = fs::symlink_metadata(input)?;
    ensure!(
        input.file_name().is_some_and(|v| v == "reference.rgb"),
        "input-name"
    );
    ensure!(
        info.is_file()
            && info.mode() & 0o077 == 0
            && info.uid() == owner.uid()
            && destination.uid() == owner.uid()
            && info.len() == (W * H * 3 * COUNT) as u64,
        "private-input"
    );
    DirBuilder::new().mode(0o700).create(output)?;
    let mut journal = create(&output.join("progress.jsonl"))?;
    File::open(output)?.sync_all()?;
    File::open(output.parent().context("parent")?)?.sync_all()?;
    let mut records = 0;
    record(
        &mut journal,
        &mut records,
        "{\"event\":\"start\",\"arm\":\"baseline2m\"}",
    )?;
    let mut stage = "constructor";
    let mut current: Option<usize> = None;
    let mut failed_wall: Option<u128> = None;
    let lifetime = Instant::now();
    let result = (|| -> Result<()> {
        let started = Instant::now();
        let created = Encoder::with_api_config(OpenH264API::from_source(), configuration());
        let elapsed = started.elapsed().as_nanos();
        failed_wall = Some(elapsed);
        let mut direct = created?;
        record(
            &mut journal,
            &mut records,
            &format!("{{\"event\":\"constructor\",\"wall_ns\":{elapsed},\"cpu_ns\":null}}"),
        )?;
        failed_wall = None;
        stage = "reference-constructor";
        let started = Instant::now();
        let created = VideoEncoder::with_timing(
            W as u16,
            H as u16,
            2_000_000,
            VideoProfile::Screen,
            VideoTiming::new(30, 30, 1)?,
        );
        let elapsed = started.elapsed().as_nanos();
        failed_wall = Some(elapsed);
        let mut reference = created?;
        record(
            &mut journal,
            &mut records,
            &format!(
                "{{\"event\":\"reference_constructor\",\"wall_ns\":{elapsed},\"cpu_ns\":null}}"
            ),
        )?;
        failed_wall = None;
        stage = "allocation";
        let mut source = File::open(input)?;
        let mut rgb = vec![0; W * H * 3];
        let mut rgba = Frame::new(W as u16, H as u16).context("rgba")?;
        let mut yuv = YUVBuffer::new(W, H);
        let (mut sps, mut pps) = (Vec::new(), Vec::new());
        let mut total = 0;
        for index in 0..COUNT {
            current = Some(index);
            stage = "deadline";
            ensure!(lifetime.elapsed() < Duration::from_secs(24), "deadline");
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"attempt\",\"frame\":{index}}}"),
            )?;
            stage = "conversion";
            source.read_exact(&mut rgb)?;
            for (out, input) in rgba
                .pixels
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(rgb.as_chunks::<3>().0)
            {
                out[..3].copy_from_slice(input);
                out[3] = 255;
            }
            // EXACT public conversion, excluded from component encode timing.
            yuv.read_rgba8(RgbaSliceU8::new(&rgba.pixels, (W, H)));
            let active = active_i420(&yuv)?;
            store(&output.join(format!("{index:02}.i420")), &active)?;
            let (sy, su, sv) = yuv.strides();
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"converted\",\"frame\":{index},\"source_bytes\":{},\"i420_bytes\":{},\"strides\":[{sy},{su},{sv}]}}",
                    rgb.len(),
                    active.len()
                ),
            )?;
            drop(active);
            stage = "encode";
            let started = Instant::now();
            let encoded = direct.encode_at(&yuv, Timestamp::from_millis(index as u64 * 1000 / 30));
            let elapsed = started.elapsed().as_nanos();
            failed_wall = Some(elapsed);
            let encoded = encoded?;
            // Borrowed-output copying and normalization are OUTSIDE this timer.
            let idr = encoded.frame_type() == FrameType::IDR;
            let raw = encoded.to_vec();
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"encoded\",\"frame\":{index},\"wall_ns\":{elapsed},\"cpu_ns\":null,\"elementary_bytes\":{}}}",
                    raw.len()
                ),
            )?;
            failed_wall = None;
            stage = "normalize";
            ensure!(idr && !raw.is_empty(), "immediate-idr");
            let payload = normalize(raw, &mut sps, &mut pps)?;
            stage = "bounds";
            total += payload.len();
            ensure!(payload.len() <= MAX_AU && total <= MAX_ARM, "coded-bound");
            stage = "store";
            store(&output.join(format!("{index:02}.au")), &payload)?;
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"stored\",\"frame\":{index},\"elementary_bytes\":{}}}",
                    payload.len()
                ),
            )?;
            stage = "reference-encode";
            let started = Instant::now();
            let packet = reference.encode(&rgba, index as u32, 0);
            let elapsed = started.elapsed().as_nanos();
            failed_wall = Some(elapsed);
            let packet = packet?;
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"reference_encoded\",\"frame\":{index},\"wall_ns\":{elapsed},\"cpu_ns\":null}}"
                ),
            )?;
            failed_wall = None;
            stage = "control";
            let parsed = parse_packet(&packet)?;
            ensure!(
                parsed.sequence == index as u32
                    && parsed.input_id == 0
                    && (parsed.width, parsed.height) == (W as u16, H as u16)
                    && parsed.keyframe,
                "reference-packet"
            );
            let identical = payload == parsed.payload;
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"control\",\"frame\":{index},\"byte_identical\":{identical}}}"
                ),
            )?;
            equality(&payload, parsed.payload)?;
        }
        stage = "complete";
        ensure!(lifetime.elapsed() < Duration::from_secs(24), "deadline");
        record(&mut journal, &mut records, "{\"event\":\"complete\"}")?;
        Ok(())
    })();
    if let Err(error) = &result {
        let frame = current.map_or_else(|| "null".to_owned(), |v| v.to_string());
        let wall = failed_wall.map_or_else(|| "null".to_owned(), |v| v.to_string());
        let code = error
            .chain()
            .find_map(|e| e.downcast_ref::<openh264::Error>().map(|e| e.native_code()))
            .map_or_else(|| "null".to_owned(), |v| v.to_string());
        let _ = record(
            &mut journal,
            &mut records,
            &format!(
                "{{\"event\":\"failure\",\"stage\":\"{stage}\",\"frame\":{frame},\"native_code\":{code},\"wall_ns\":{wall},\"cpu_ns\":null}}"
            ),
        );
    }
    result
}

fn main() -> std::process::ExitCode {
    if work().is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_plane_exports_only_active_rows() {
        assert_eq!(
            active_rows(&[1, 2, 99, 3, 4, 88], 3, 2, 2).unwrap(),
            [1, 2, 3, 4]
        );
        assert!(active_rows(&[1, 2, 3], 1, 2, 1).is_err());
        assert!(active_rows(&[1, 2, 3], 2, 2, 2).is_err());
        assert!(active_rows(&[1], usize::MAX, 1, 2).is_err());
    }
    #[test]
    fn full_equality_denies_one_changed_byte_or_different_length() {
        assert!(equality(&[1, 2, 3], &[1, 2, 4]).is_err());
        assert!(equality(&[1, 2], &[1, 2, 0]).is_err());
        equality(&[0, 0, 1, 101], &[0, 0, 1, 101]).unwrap();
    }
    #[test]
    fn normalization_keeps_pinned_three_byte_cache_and_all_sei() {
        let s = &[0, 0, 0, 1, 0x67, 1][..];
        let p = &[0, 0, 0, 1, 0x68, 2][..];
        let i = &[0, 0, 1, 0x65, 3][..];
        let sei = &[0, 0, 1, 0x06, 4][..];
        let (mut sps, mut pps) = (Vec::new(), Vec::new());
        let full = [s, p, sei, i].concat();
        assert_eq!(normalize(full.clone(), &mut sps, &mut pps).unwrap(), full);
        // The pinned nal_units includes a trailing prefix zero in the first SPS.
        assert_eq!(sps, [0, 0, 1, 0x67, 1, 0]);
        assert_eq!(pps, [0, 0, 1, 0x68, 2]);
        let missing = [s, sei, i].concat();
        let actual = normalize(missing.clone(), &mut sps, &mut pps).unwrap();
        assert_eq!(sps, [0, 0, 1, 0x67, 1]);
        assert_eq!(
            actual,
            [sps.as_slice(), pps.as_slice(), missing.as_slice()].concat()
        );
    }
    #[test]
    fn reserved_forbidden_and_inter_slices_are_denied() {
        for header in [
            0x00, 0x61, 0x62, 0x63, 0x64, 0x6d, 0x6e, 0x6f, 0x70, 0x75, 0xff,
        ] {
            assert!(normalize(vec![0, 0, 1, header, 0x80], &mut vec![], &mut vec![]).is_err());
        }
        assert!(normalize(vec![0, 0, 1, 0x65, 0x80], &mut vec![], &mut vec![]).is_err());
    }
    #[test]
    fn constructor_is_exact_public_screen_chain_without_qp_override() {
        let public = include_str!("../src/lib.rs");
        let begin = public.find("let config = EncoderConfig::new()").unwrap();
        let end = public[begin..].find("Encoder::with_api_config").unwrap() + begin;
        let public = public[begin..end]
            .lines()
            .filter(|l| !l.trim().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .replace("let config = ", "")
            .replace("timing.fps as f32", "30_f32")
            .replace("bitrate)", "2_000_000)")
            .replace("profile.usage()", "UsageType::ScreenContentRealTime")
            .replace("profile.scene_change_detection()", "true")
            .replace("timing.intra_period", "1")
            .replace(';', "");
        let own = include_str!("all-idr-x264.rs");
        let begin = own.find("    EncoderConfig::new()").unwrap();
        let end = own[begin..].find("\n}").unwrap() + begin;
        let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(compact(&public), compact(&own[begin..end]));
    }
}
