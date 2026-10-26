//! Conventional CPU-only H.264 baseline for comparison with lossless Gliss.
//!
//! GLV1 packets contain one Annex B access unit. RGBA8 is converted to limited
//! range BT.601 YUV420, so this baseline is lossy, including chroma subsampling.

use anyhow::{Context, Result, ensure};
use gliss_core::Frame;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp, nal_units};

pub const HEADER_LEN: usize = 24;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
pub const DEFAULT_BITRATE_BPS: u32 = 4_000_000;

/// Native rate-control, synthetic timestamp and IDR cadence settings.
/// These rates describe encoder policy, not the cadence of frame capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTiming {
    fps: u32,
    timestamp_fps: u32,
    intra_period: u32,
}

impl VideoTiming {
    pub fn new(fps: u32, timestamp_fps: u32, intra_period: u32) -> Result<Self> {
        ensure!(
            (1..=60).contains(&fps),
            "OpenH264 fps must be within 1..=60"
        );
        ensure!(
            (1..=60).contains(&timestamp_fps),
            "H.264 timestamp fps must be within 1..=60"
        );
        ensure!(
            (1..=3600).contains(&intra_period),
            "H.264 intra period must be within 1..=3600 frames"
        );
        Ok(Self {
            fps,
            timestamp_fps,
            intra_period,
        })
    }

    pub fn fps(self) -> u32 {
        self.fps
    }

    pub fn timestamp_fps(self) -> u32 {
        self.timestamp_fps
    }

    pub fn intra_period(self) -> u32 {
        self.intra_period
    }
}

/// Fixed software video profiles; screen remains the existing default.
/// OpenH264 forces scene detection on for screen content, so no screen-off
/// profile is exposed. Camera on/off varies that knob within camera usage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoProfile {
    #[default]
    Screen,
    CameraSceneOn,
    CameraSceneOff,
}
impl VideoProfile {
    pub fn name(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::CameraSceneOn => "camera-scene-on",
            Self::CameraSceneOff => "camera-scene-off",
        }
    }
    pub fn usage_name(self) -> &'static str {
        match self {
            Self::Screen => "screen-content-real-time",
            Self::CameraSceneOn | Self::CameraSceneOff => "camera-video-real-time",
        }
    }
    pub fn scene_change_detection(self) -> bool {
        !matches!(self, Self::CameraSceneOff)
    }
    fn usage(self) -> UsageType {
        match self {
            Self::Screen => UsageType::ScreenContentRealTime,
            Self::CameraSceneOn | Self::CameraSceneOff => UsageType::CameraVideoRealTime,
        }
    }
}

/// One-thread software encoder; run encode on a bounded blocking worker.
pub struct VideoEncoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    width: u16,
    height: u16,
    timing: VideoTiming,
    bitrate: u32,
    profile: VideoProfile,
    encoded_frames: u64,
    force_keyframe: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

fn validate_dimensions(width: u16, height: u16) -> Result<()> {
    ensure!(
        width >= 16 && height >= 16,
        "H.264 dimensions must be at least 16x16"
    );
    ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "H.264 YUV420 requires even width and height; no implicit crop or padding"
    );
    ensure!(
        width.max(height) <= 3840 && width.min(height) <= 2160,
        "OpenH264 dimensions exceed 3840x2160 / 2160x3840"
    );
    Ok(())
}

fn native_encoder(timing: VideoTiming, bitrate: u32, profile: VideoProfile) -> Result<Encoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(timing.fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(profile.usage())
        .scene_change_detect(profile.scene_change_detection())
        .profile(Profile::Baseline)
        .complexity(Complexity::Low)
        .adaptive_quantization(false)
        .background_detection(false)
        .num_threads(1)
        .skip_frames(false)
        .long_term_reference(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(timing.intra_period))
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        // The crate's RGB conversion uses BT.601 limited-range coefficients.
        .vui(VuiConfig::bt601());
    Encoder::with_api_config(OpenH264API::from_source(), config)
        .context("Create software OpenH264 encoder")
}

impl VideoEncoder {
    pub fn new(width: u16, height: u16, fps: u32) -> Result<Self> {
        Self::with_bitrate(width, height, fps, DEFAULT_BITRATE_BPS)
    }

    /// Explicit bitrate points make quality/bandwidth comparisons reproducible.
    pub fn with_bitrate(width: u16, height: u16, fps: u32, bitrate: u32) -> Result<Self> {
        Self::with_profile(width, height, fps, bitrate, VideoProfile::Screen)
    }

    /// Explicit usage/scene experiment; all other native settings are unchanged.
    pub fn with_profile(
        width: u16,
        height: u16,
        fps: u32,
        bitrate: u32,
        profile: VideoProfile,
    ) -> Result<Self> {
        ensure!(
            (1..=60).contains(&fps),
            "OpenH264 fps must be within 1..=60"
        );
        Self::with_timing(
            width,
            height,
            bitrate,
            profile,
            VideoTiming::new(fps, fps, fps * 2)?,
        )
    }

    /// Change native timing without changing capture or input scheduling.
    pub fn with_timing(
        width: u16,
        height: u16,
        bitrate: u32,
        profile: VideoProfile,
        timing: VideoTiming,
    ) -> Result<Self> {
        validate_dimensions(width, height)?;
        ensure!(
            (1_000..=50_000_000).contains(&bitrate),
            "H.264 bitrate outside 1 kbit/s..50 Mbit/s"
        );
        Ok(Self {
            encoder: native_encoder(timing, bitrate, profile)?,
            yuv: YUVBuffer::new(usize::from(width), usize::from(height)),
            width,
            height,
            timing,
            bitrate,
            profile,
            encoded_frames: 0,
            force_keyframe: true,
            sps: Vec::new(),
            pps: Vec::new(),
        })
    }

    pub fn profile(&self) -> VideoProfile {
        self.profile
    }

    pub fn timing(&self) -> VideoTiming {
        self.timing
    }

    /// Request an independently decodable IDR access unit on the next encode.
    pub fn reset(&mut self) {
        self.force_keyframe = true;
    }

    /// Encode one complete RGBA frame; size changes rebuild native state.
    pub fn encode(&mut self, frame: &Frame, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        ensure!(frame.is_valid(), "Invalid RGBA frame length or dimensions");
        validate_dimensions(frame.width, frame.height)?;
        if (frame.width, frame.height) != (self.width, self.height) {
            // Construct first so a rejected resize leaves the old encoder intact.
            *self = Self::with_timing(
                frame.width,
                frame.height,
                self.bitrate,
                self.profile,
                self.timing,
            )?;
        }
        self.yuv.read_rgba8(RgbaSliceU8::new(
            &frame.pixels,
            (usize::from(frame.width), usize::from(frame.height)),
        ));
        if self.force_keyframe && self.encoded_frames > 0 {
            self.encoder.force_intra_frame();
        }
        let timestamp = Timestamp::from_millis(
            self.encoded_frames.saturating_mul(1000) / u64::from(self.timing.timestamp_fps),
        );
        let encoded = self
            .encoder
            .encode_at(&self.yuv, timestamp)
            .context("Encode H.264 frame")?;
        let keyframe = encoded.frame_type() == FrameType::IDR;
        ensure!(
            !self.force_keyframe || keyframe,
            "Encoder did not provide the requested IDR frame"
        );
        let mut payload = encoded.to_vec();
        ensure!(!payload.is_empty(), "Encoder unexpectedly skipped a frame");
        let mut has_sps = false;
        let mut has_pps = false;
        for nal in nal_units(&payload) {
            match nal_type(nal) {
                Some(7) => {
                    self.sps = nal.to_vec();
                    has_sps = true;
                }
                Some(8) => {
                    self.pps = nal.to_vec();
                    has_pps = true;
                }
                _ => {}
            }
        }
        if keyframe && (!has_sps || !has_pps) {
            ensure!(
                !self.sps.is_empty() && !self.pps.is_empty(),
                "IDR lacks SPS/PPS"
            );
            let mut complete = Vec::with_capacity(self.sps.len() + self.pps.len() + payload.len());
            complete.extend_from_slice(&self.sps);
            complete.extend_from_slice(&self.pps);
            complete.extend_from_slice(&payload);
            payload = complete;
        }
        ensure!(
            payload.len() <= MAX_PAYLOAD,
            "Encoded access unit exceeds packet limit"
        );
        let mut packet = Vec::with_capacity(HEADER_LEN + payload.len());
        packet.extend_from_slice(b"GLV1");
        packet.extend_from_slice(&sequence.to_le_bytes());
        packet.extend_from_slice(&input_id.to_le_bytes());
        packet.extend_from_slice(&self.width.to_le_bytes());
        packet.extend_from_slice(&self.height.to_le_bytes());
        packet.extend_from_slice(&0_u16.to_le_bytes());
        packet.extend_from_slice(&u16::from(keyframe).to_le_bytes());
        packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        packet.extend_from_slice(&payload);
        self.encoded_frames += 1;
        self.force_keyframe = false;
        Ok(packet)
    }
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    let prefix = if nal.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    nal.get(prefix).map(|byte| byte & 31)
}

/// Validated packet framing, with borrowed native H.264 bytes.
#[derive(Debug)]
pub struct VideoPacket<'a> {
    pub sequence: u32,
    pub input_id: u32,
    pub width: u16,
    pub height: u16,
    pub keyframe: bool,
    pub payload: &'a [u8],
}

/// Checks framing and IDR/header declarations; not a full H.264 validator.
pub fn parse_packet(packet: &[u8]) -> Result<VideoPacket<'_>> {
    ensure!(
        packet.len() >= HEADER_LEN && packet.len() <= HEADER_LEN + MAX_PAYLOAD,
        "Invalid GLV1 packet size"
    );
    ensure!(&packet[..4] == b"GLV1", "Unknown video wire version");
    let u16_at = |offset| u16::from_le_bytes([packet[offset], packet[offset + 1]]);
    let u32_at = |offset| u32::from_le_bytes(packet[offset..offset + 4].try_into().unwrap());
    let width = u16_at(12);
    let height = u16_at(14);
    validate_dimensions(width, height)?;
    let flags = u16_at(18);
    ensure!(
        u16_at(16) == 0 && flags <= 1,
        "Invalid video flags/reserved field"
    );
    ensure!(
        u32_at(20) as usize == packet.len() - HEADER_LEN,
        "Video payload length mismatch"
    );
    let payload = &packet[HEADER_LEN..];
    ensure!(nal_type(payload).is_some(), "Video payload is not Annex B");
    let mut idr = false;
    let mut sps = false;
    let mut pps = false;
    let mut slice = false;
    for nal in nal_units(payload) {
        match nal_type(nal) {
            Some(5) => {
                idr = true;
                slice = true;
            }
            Some(1) => slice = true,
            Some(7) => sps = true,
            Some(8) => pps = true,
            _ => {}
        }
    }
    ensure!(
        slice && idr == (flags == 1) && (!idr || (sps && pps)),
        "Invalid IDR or parameter set declaration"
    );
    Ok(VideoPacket {
        sequence: u32_at(4),
        input_id: u32_at(8),
        width,
        height,
        keyframe: flags == 1,
        payload,
    })
}

#[cfg(test)]
mod tests;
