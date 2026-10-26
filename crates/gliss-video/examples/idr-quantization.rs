//! Offline QP-bound experiment. No production changes, resets, or forced frames.
use anyhow::{Context, Result, ensure};
use gliss_core::Frame;
use gliss_video::{HEADER_LEN, MAX_PAYLOAD, VideoEncoder, VideoProfile, VideoTiming, parse_packet};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    QpRange, RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp, nal_units};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const WIDTH: u16 = 1280;
const HEIGHT: u16 = 800;
const COUNT: usize = 12;
const SOURCE_BYTES: usize = WIDTH as usize * HEIGHT as usize * 3;
const MAX_CODED: usize = 64 * 1024 * 1024;

// Derived line-for-line from native_encoder in the pinned gliss-video/src/lib.rs:
// bitrate=2M, timing=(30,30,1), profile=Screen. The only added setter is qp.
// QpRange::default() REQUESTS [0,51], also EncoderConfig::new()'s wrapper default.
// Native Screen validation replaces a nonpositive bound with defaults 26..35;
// CAVLC macroblock overflow retries can exceed requested bounds. No coded-QP
// attestation is made. The persistent byte-equality control remains mandatory.
fn direct_configuration(qp: QpRange) -> EncoderConfig {
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
        .qp(qp)
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    let prefix = if nal.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    nal.get(prefix).filter(|b| **b & 128 == 0).map(|b| b & 31)
}

// Exactly the public library's SPS/PPS cache-and-prepend rule. In particular,
// when either is absent, prepend BOTH cached NALs, without removing any NAL.
fn normalize(
    payload: Vec<u8>,
    keyframe: bool,
    sps: &mut Vec<u8>,
    pps: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let (mut has_sps, mut has_pps) = (false, false);
    for nal in nal_units(&payload) {
        match nal_type(nal) {
            Some(7) => {
                *sps = nal.to_vec();
                has_sps = true;
            }
            Some(8) => {
                *pps = nal.to_vec();
                has_pps = true;
            }
            _ => {}
        }
    }
    if keyframe && (!has_sps || !has_pps) {
        ensure!(!sps.is_empty() && !pps.is_empty(), "parameter-sets");
        let mut complete = Vec::with_capacity(sps.len() + pps.len() + payload.len());
        complete.extend_from_slice(sps);
        complete.extend_from_slice(pps);
        complete.extend_from_slice(&payload);
        Ok(complete)
    } else {
        Ok(payload)
    }
}

struct Direct {
    encoder: Encoder,
    yuv: YUVBuffer,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl Direct {
    fn new(qp: QpRange) -> Result<Self> {
        Ok(Self {
            encoder: Encoder::with_api_config(
                OpenH264API::from_source(),
                direct_configuration(qp),
            )?,
            yuv: YUVBuffer::new(usize::from(WIDTH), usize::from(HEIGHT)),
            sps: Vec::new(),
            pps: Vec::new(),
        })
    }

    // Same timed work as VideoEncoder::encode: RGBA->YUV, native encode,
    // normalization, and GLV1 construction. RGB->RGBA happens in work().
    fn encode(&mut self, frame: &Frame, index: usize) -> Result<Vec<u8>> {
        self.yuv.read_rgba8(RgbaSliceU8::new(
            &frame.pixels,
            (usize::from(WIDTH), usize::from(HEIGHT)),
        ));
        let encoded = self
            .encoder
            .encode_at(&self.yuv, Timestamp::from_millis(index as u64 * 1000 / 30))?;
        let keyframe = encoded.frame_type() == FrameType::IDR;
        let raw = encoded.to_vec();
        ensure!(!raw.is_empty(), "skipped-frame");
        let payload = normalize(raw, keyframe, &mut self.sps, &mut self.pps)?;
        ensure!(payload.len() <= MAX_PAYLOAD, "access-unit-bound");
        let mut packet = Vec::with_capacity(HEADER_LEN + payload.len());
        packet.extend_from_slice(b"GLV1");
        packet.extend_from_slice(&(index as u32).to_le_bytes());
        packet.extend_from_slice(&0_u32.to_le_bytes());
        packet.extend_from_slice(&WIDTH.to_le_bytes());
        packet.extend_from_slice(&HEIGHT.to_le_bytes());
        packet.extend_from_slice(&0_u16.to_le_bytes());
        packet.extend_from_slice(&u16::from(keyframe).to_le_bytes());
        packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        packet.extend_from_slice(&payload);
        Ok(packet)
    }
}

fn independent(payload: &[u8]) -> Result<()> {
    let (mut idr, mut sps, mut pps) = (false, false, false);
    for nal in nal_units(payload) {
        match nal_type(nal).context("nal-header")? {
            5 => idr = true,
            7 => sps = true,
            8 => pps = true,
            // Baseline-compatible non-VCL metadata: SEI, AUD, end markers, filler.
            6 | 9..=12 => {}
            _ => anyhow::bail!("disallowed-nal"),
        }
    }
    ensure!(idr && sps && pps, "independent-idr");
    Ok(())
}

fn equal_control(actual: &[u8], duplicate: &[u8]) -> Result<()> {
    ensure!(actual == duplicate, "control-byte-equality");
    Ok(())
}

fn create(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn record(journal: &mut File, records: &mut usize, event: &str) -> Result<()> {
    ensure!(*records < 96 && event.len() < 1024, "native-journal-bound");
    journal.write_all(event.as_bytes())?;
    journal.write_all(b"\n")?;
    journal.sync_all()?;
    *records += 1;
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

fn work() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 3,
        "usage: idr-quantization REFERENCE_RGB ARM NEW_PRIVATE_DIR"
    );
    let input = PathBuf::from(&args[0]);
    let arm = args[1].to_str().context("arm")?;
    let qp = match arm {
        "baseline2m" => None,
        "qp28" => Some(28),
        "qp32" => Some(32),
        "qp36" => Some(36),
        "qp40" => Some(40),
        _ => anyhow::bail!("undeclared-arm"),
    };
    let output = PathBuf::from(&args[2]);
    let owner = private(input.parent().context("input-parent")?)?;
    let destination = private(output.parent().context("output-parent")?)?;
    let info = fs::symlink_metadata(&input)?;
    ensure!(
        input.file_name().is_some_and(|n| n == "reference.rgb"),
        "input-name"
    );
    ensure!(
        info.is_file()
            && info.uid() == owner.uid()
            && info.mode() & 0o077 == 0
            && destination.uid() == owner.uid()
            && info.len() == (SOURCE_BYTES * COUNT) as u64,
        "private-input"
    );
    DirBuilder::new().mode(0o700).create(&output)?;
    let mut journal = create(&output.join("progress.jsonl"))?;
    File::open(&output)?.sync_all()?;
    File::open(output.parent().context("output-parent")?)?.sync_all()?;
    let mut records = 0;
    record(
        &mut journal,
        &mut records,
        &format!("{{\"event\":\"start\",\"arm\":\"{arm}\"}}"),
    )?;
    let (mut stage, mut current) = ("constructor", None);
    let (mut failed_encode_wall_ns, mut failed_control_wall_ns) = (None, None);
    let lifetime = Instant::now();
    let outcome = (|| -> Result<()> {
        let started = Instant::now();
        let mut public = if qp.is_none() {
            Some(VideoEncoder::with_timing(
                WIDTH,
                HEIGHT,
                2_000_000,
                VideoProfile::Screen,
                VideoTiming::new(30, 30, 1)?,
            )?)
        } else {
            None
        };
        let mut direct;
        let constructor_wall_ns;
        let mut control_constructor_wall_ns = "null".to_owned();
        if public.is_some() {
            constructor_wall_ns = started.elapsed().as_nanos();
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"constructor\",\"wall_ns\":{constructor_wall_ns}}}"),
            )?;
            stage = "control-constructor";
            let started = Instant::now();
            direct = Direct::new(QpRange::default())?;
            control_constructor_wall_ns = started.elapsed().as_nanos().to_string();
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"control_constructor\",\"wall_ns\":{control_constructor_wall_ns}}}"
                ),
            )?;
        } else {
            let q = qp.context("candidate-qp")?;
            direct = Direct::new(QpRange::new(q, q))?;
            constructor_wall_ns = started.elapsed().as_nanos();
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"constructor\",\"wall_ns\":{constructor_wall_ns}}}"),
            )?;
        }
        stage = "allocation";
        let mut source = File::open(&input)?;
        let mut rgb = vec![0; SOURCE_BYTES];
        let mut frame = Frame::new(WIDTH, HEIGHT).context("rgba-allocation")?;
        let mut total = 0;
        let mut manifest = format!(
            "{{\"arm\":\"{arm}\",\"constructor_wall_ns\":{constructor_wall_ns},\"control_constructor_wall_ns\":{control_constructor_wall_ns},\"frames\":["
        );
        for index in 0..COUNT {
            current = Some(index);
            stage = "deadline";
            ensure!(
                lifetime.elapsed() < Duration::from_secs(24),
                "native-deadline"
            );
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"attempt\",\"frame\":{index}}}"),
            )?;
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
            let encoded = if let Some(encoder) = public.as_mut() {
                encoder.encode(&frame, index as u32, 0)
            } else {
                direct.encode(&frame, index)
            };
            let encode_wall_ns = started.elapsed().as_nanos();
            if encoded.is_err() {
                failed_encode_wall_ns = Some(encode_wall_ns);
            }
            let packet = encoded?;
            let bytes = packet.len().saturating_sub(HEADER_LEN);
            // Retain time and source/output byte observations BEFORE payload I/O,
            // packet validation, duplicate control encode, or a later failure.
            record(
                &mut journal,
                &mut records,
                &format!(
                    "{{\"event\":\"encoded\",\"frame\":{index},\"encode_wall_ns\":{encode_wall_ns},\"source_bytes\":{SOURCE_BYTES},\"elementary_bytes\":{bytes}}}"
                ),
            )?;
            stage = "packet-parse";
            let parsed = parse_packet(&packet)?;
            ensure!(
                parsed.sequence == index as u32
                    && parsed.input_id == 0
                    && (parsed.width, parsed.height) == (WIDTH, HEIGHT)
                    && parsed.keyframe,
                "packet-contract"
            );
            stage = "independent-idr";
            independent(parsed.payload)?;
            let (mut control_wall_ns, mut control_bytes, mut identical) =
                ("null".to_owned(), "null".to_owned(), "null");
            if public.is_some() {
                stage = "control-encode";
                let started = Instant::now();
                let encoded = direct.encode(&frame, index);
                let elapsed = started.elapsed().as_nanos();
                if encoded.is_err() {
                    failed_control_wall_ns = Some(elapsed);
                }
                control_wall_ns = elapsed.to_string();
                let duplicate = encoded?;
                control_bytes = duplicate.len().saturating_sub(HEADER_LEN).to_string();
                record(
                    &mut journal,
                    &mut records,
                    &format!(
                        "{{\"event\":\"control_encoded\",\"frame\":{index},\"wall_ns\":{control_wall_ns},\"elementary_bytes\":{control_bytes}}}"
                    ),
                )?;
                stage = "control-equality";
                let proof = parse_packet(&duplicate)?;
                independent(proof.payload)?;
                let equal = parsed.payload == proof.payload;
                record(
                    &mut journal,
                    &mut records,
                    &format!(
                        "{{\"event\":\"control\",\"frame\":{index},\"byte_identical\":{equal}}}"
                    ),
                )?;
                equal_control(parsed.payload, proof.payload)?;
                identical = "true";
            }
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"validated\",\"frame\":{index}}}"),
            )?;
            stage = "coded-byte-bound";
            total += bytes;
            ensure!(
                bytes > 0 && bytes <= MAX_PAYLOAD && total <= MAX_CODED,
                "coded-byte-bound"
            );
            stage = "au-write";
            let mut au = create(&output.join(format!("{index:02}.au")))?;
            au.write_all(parsed.payload)?;
            au.sync_all()?;
            record(
                &mut journal,
                &mut records,
                &format!("{{\"event\":\"stored\",\"frame\":{index}}}"),
            )?;
            if index > 0 {
                manifest.push(',');
            }
            manifest.push_str(&format!("{{\"frame\":{index},\"source_bytes\":{SOURCE_BYTES},\"elementary_bytes\":{bytes},\"encode_wall_ns\":{encode_wall_ns},\"control_wall_ns\":{control_wall_ns},\"control_bytes\":{control_bytes},\"byte_identical\":{identical}}}"));
        }
        manifest.push_str("]}\n");
        stage = "manifest-write";
        ensure!(manifest.len() <= 16384, "manifest-bound");
        let mut file = create(&output.join("manifest.json"))?;
        file.write_all(manifest.as_bytes())?;
        file.sync_all()?;
        File::open(&output)?.sync_all()?;
        record(&mut journal, &mut records, "{\"event\":\"complete\"}")?;
        Ok(())
    })();
    if let Err(error) = &outcome {
        let index = current.map_or_else(|| "null".to_owned(), |v| v.to_string());
        let native_code = error
            .chain()
            .find_map(|e| e.downcast_ref::<openh264::Error>().map(|e| e.native_code()))
            .map_or_else(|| "null".to_owned(), |v| v.to_string());
        let failed_encode =
            failed_encode_wall_ns.map_or_else(|| "null".to_owned(), |v| v.to_string());
        let failed_control =
            failed_control_wall_ns.map_or_else(|| "null".to_owned(), |v| v.to_string());
        let source_bytes = current.map_or_else(|| "null".to_owned(), |_| SOURCE_BYTES.to_string());
        let _ = record(
            &mut journal,
            &mut records,
            &format!(
                "{{\"event\":\"failure\",\"reason\":\"{stage}\",\"frame\":{index},\"native_code\":{native_code},\"encode_wall_ns\":{failed_encode},\"control_wall_ns\":{failed_control},\"source_bytes\":{source_bytes}}}"
            ),
        );
    }
    outcome
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
    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 1];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 2];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 3];

    #[test]
    fn normalization_matches_library_even_when_only_one_set_is_missing() {
        let first = [SPS, PPS, IDR].concat();
        // Pinned 0.9.8 utils.rs returns i-2 at each prefix terminator. Thus
        // four-byte prefixes start at their second zero, and the first SPS
        // includes the first zero of the following four-byte PPS prefix.
        let first_sps: &[u8] = &[0, 0, 1, 0x67, 1, 0];
        let cached_sps: &[u8] = &[0, 0, 1, 0x67, 1];
        let cached_pps: &[u8] = &[0, 0, 1, 0x68, 2];
        assert_eq!(
            nal_units(&first).collect::<Vec<_>>(),
            vec![first_sps, cached_pps, IDR]
        );
        let (mut sps, mut pps) = (Vec::new(), Vec::new());
        assert_eq!(
            normalize(first.clone(), true, &mut sps, &mut pps).unwrap(),
            first
        );
        assert_eq!(sps, first_sps);
        assert_eq!(pps, cached_pps);
        let missing_pps = [SPS, IDR].concat();
        assert_eq!(
            nal_units(&missing_pps).collect::<Vec<_>>(),
            vec![cached_sps, IDR]
        );
        let normalized = normalize(missing_pps.clone(), true, &mut sps, &mut pps).unwrap();
        assert_eq!(sps, cached_sps);
        assert_eq!(pps, cached_pps);
        assert_eq!(
            normalized,
            [cached_sps, cached_pps, missing_pps.as_slice()].concat()
        );
        assert_eq!(normalized.len(), 21);
        assert!(normalized.ends_with(&missing_pps));
        let missing_sps = [PPS, IDR].concat();
        let normalized = normalize(missing_sps.clone(), true, &mut sps, &mut pps).unwrap();
        assert_eq!(sps, cached_sps);
        assert_eq!(pps, cached_pps);
        assert_eq!(
            normalized,
            [cached_sps, cached_pps, missing_sps.as_slice()].concat()
        );
        assert!(normalized.ends_with(&missing_sps));
        assert_eq!(
            normalize(IDR.to_vec(), true, &mut sps, &mut pps).unwrap(),
            [cached_sps, cached_pps, IDR].concat()
        );
        assert!(normalize(IDR.to_vec(), true, &mut Vec::new(), &mut Vec::new()).is_err());
    }

    #[test]
    fn three_byte_prefixes_prepend_both_caches_and_preserve_input() {
        let sps3: &[u8] = &[0, 0, 1, 0x67, 1];
        let pps3: &[u8] = &[0, 0, 1, 0x68, 2];
        let first = [sps3, pps3, IDR].concat();
        let (mut sps, mut pps) = (Vec::new(), Vec::new());
        assert_eq!(nal_units(&first).collect::<Vec<_>>(), vec![sps3, pps3, IDR]);
        assert_eq!(
            normalize(first.clone(), true, &mut sps, &mut pps).unwrap(),
            first
        );
        for payload in [[sps3, IDR].concat(), [pps3, IDR].concat(), IDR.to_vec()] {
            let normalized = normalize(payload.clone(), true, &mut sps, &mut pps).unwrap();
            assert_eq!(sps, sps3);
            assert_eq!(pps, pps3);
            assert_eq!(normalized, [sps3, pps3, payload.as_slice()].concat());
            assert!(normalized.ends_with(&payload));
        }
        for payload in [IDR.to_vec(), [sps3, IDR].concat(), [pps3, IDR].concat()] {
            assert!(normalize(payload, true, &mut Vec::new(), &mut Vec::new()).is_err());
        }
    }

    #[test]
    fn only_baseline_idr_sets_and_non_vcl_metadata_are_allowed() {
        let first = [SPS, PPS, IDR].concat();
        for kind in 0_u8..=31 {
            let extra = [0, 0, 1, kind, 0x80];
            let result = independent(&[first.as_slice(), &extra].concat());
            assert_eq!(result.is_ok(), matches!(kind, 5..=12), "NAL type {kind}");
        }
        for extra in [&[0, 0, 1][..], &[0, 0, 1, 0xe5, 0x80][..]] {
            assert!(independent(&[first.as_slice(), extra].concat()).is_err());
        }
    }

    #[test]
    fn equality_is_payload_exact_and_mixed_idr_delta_is_rejected() {
        let first = [SPS, PPS, IDR].concat();
        independent(&first).unwrap();
        equal_control(&first, &first).unwrap();
        let mut changed = first.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(equal_control(&first, &changed).is_err());
        assert!(independent(&[first.as_slice(), &[0, 0, 1, 0x61, 4]].concat()).is_err());
        assert!(independent(&[SPS, IDR].concat()).is_err());
    }
}
