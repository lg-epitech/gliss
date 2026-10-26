//! GLH2 fixed 32/16 masks; unchanged exact Global64 base and software video.
mod classifier;
mod decoder;
pub mod fixtures;
mod wire;
use anyhow::{Result, ensure};
pub use classifier::{
    ACTIVATE_FRAMES, BASE_TILE_SIZE, Classifier, MAX_SAMPLES, MAX_TILES, MIN_DISTINCT, QUIET_FRAMES,
};
pub use decoder::Decoder;
pub use decoder::Decoder as HybridGridDecoder;
use gliss_codec::{Encoder as BaseEncoder, Mode};
use gliss_core::Frame;
use gliss_video::VideoEncoder;
pub use gliss_video::VideoProfile;
pub use wire::{HEADER_BYTES, MAX_PACKET_BYTES, Packet, Roi, parse_packet, roi_for_masks};

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub active_tiles: usize,
    pub classified_tiles: usize,
    pub roi: Option<Roi>,
    pub roi_fallback: bool,
    pub base_bytes: usize,
    pub video_bytes: usize,
    pub mask_grid: u16,
    pub grid_fallback: bool,
}

/// Both exact bases emit GLB2; caching does not change its representation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseMode {
    #[default]
    Global,
    GlobalCached,
}
impl BaseMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::GlobalCached => "global-cached",
        }
    }
    fn mode(self) -> Mode {
        match self {
            Self::Global => Mode::Global,
            Self::GlobalCached => Mode::GlobalCached,
        }
    }
}

/// Classifier, ROI policy and wire stay fixed; only exact base/video profile vary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HybridGridConfig {
    pub base: BaseMode,
    pub video: VideoProfile,
}

pub struct HybridGridEncoder {
    mask_grid: u16,
    base: BaseEncoder,
    classifier: Classifier,
    video: Option<VideoEncoder>,
    masks: Vec<u16>,
    roi: Option<Roi>,
    dimensions: Option<(u16, u16)>,
    sequence: Option<u32>,
    fps: u32,
    bitrate: u32,
    config: HybridGridConfig,
    force_keyframe: bool,
    failed: bool,
    stats: Stats,
}
pub type Encoder = HybridGridEncoder;
impl HybridGridEncoder {
    pub fn new(mask_grid: u16, fps: u32, bitrate: u32) -> Result<Self> {
        Self::with_config(mask_grid, fps, bitrate, HybridGridConfig::default())
    }
    pub fn with_config(
        mask_grid: u16,
        fps: u32,
        bitrate: u32,
        config: HybridGridConfig,
    ) -> Result<Self> {
        ensure!(
            (1..=60).contains(&fps) && (1_000..=50_000_000).contains(&bitrate),
            "Invalid hybrid video fps/bitrate"
        );
        Ok(Self {
            mask_grid,
            base: BaseEncoder::new(config.base.mode(), BASE_TILE_SIZE)?,
            classifier: Classifier::new(mask_grid)?,
            video: None,
            masks: Vec::new(),
            roi: None,
            dimensions: None,
            sequence: None,
            fps,
            bitrate,
            config,
            force_keyframe: true,
            failed: false,
            stats: Stats {
                mask_grid,
                ..Stats::default()
            },
        })
    }
    pub fn config(&self) -> HybridGridConfig {
        self.config
    }
    pub fn mask_grid(&self) -> u16 {
        self.mask_grid
    }
    pub fn reset(&mut self) {
        self.base.reset();
        // Discard native video history, including after an encoder failure.
        self.video = None;
        self.force_keyframe = true;
        self.failed = false;
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn encode(&mut self, frame: &Frame, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        ensure!(!self.failed, "Hybrid encode failed; reset before reuse");
        ensure!(
            frame.is_valid() && frame.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
            "Hybrid source must be valid opaque RGBA"
        );
        let result = self.encode_inner(frame, sequence, input_id);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn encode_inner(&mut self, frame: &Frame, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        let dimensions = (frame.width, frame.height);
        let independent = self.force_keyframe
            || self.dimensions != Some(dimensions)
            || self
                .sequence
                .is_none_or(|old| old.wrapping_add(1) != sequence);
        let (columns, rows) = classifier::grid(frame.width, frame.height, self.mask_grid);
        // An oversized virtual grid must not clone/allocate the previous history.
        let mut classifier = if columns * rows > MAX_TILES {
            Classifier::new(self.mask_grid)?
        } else {
            self.classifier.clone()
        };
        let classification = classifier.update(frame)?;
        let classified = classification.masks;
        let roi = roi_for_masks(&classified, frame.width, frame.height, self.mask_grid);
        let masks = if roi.is_some() {
            classified.clone()
        } else {
            Vec::new()
        };
        let changed = masks != self.masks || roi != self.roi;
        let mut base = frame.clone();
        for &index in &masks {
            let (x, y, w, h) =
                classifier::tile_bounds(index, frame.width, frame.height, self.mask_grid);
            for row in y..y + h {
                for col in x..x + w {
                    let offset = (row * usize::from(frame.width) + col) * 4;
                    base.pixels[offset..offset + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
        let video = if let Some(roi) = roi {
            if self.video.is_none()
                || self.roi.map(|old| (old.width, old.height)) != Some((roi.width, roi.height))
            {
                self.video = Some(VideoEncoder::with_profile(
                    roi.width,
                    roi.height,
                    self.fps,
                    self.bitrate,
                    self.config.video,
                )?);
            }
            let video = self.video.as_mut().unwrap();
            if independent || changed {
                video.reset();
            }
            Some(video.encode(&crop(frame, roi), sequence, input_id)?)
        } else {
            None
        };
        if independent {
            self.base.reset();
        }
        let base = self.base.encode(&base, sequence, input_id)?;
        let packet = wire::wrap(&base, video.as_deref(), &masks, roi, self.mask_grid)?;
        if roi.is_none() {
            self.video = None;
        }
        self.stats = Stats {
            active_tiles: masks.len(),
            classified_tiles: classified.len(),
            roi,
            roi_fallback: !classified.is_empty() && roi.is_none(),
            base_bytes: base.len(),
            video_bytes: video.as_ref().map_or(0, Vec::len),
            mask_grid: self.mask_grid,
            grid_fallback: classification.grid_fallback,
        };
        self.classifier = classifier;
        self.masks = masks;
        self.roi = roi;
        self.dimensions = Some(dimensions);
        self.sequence = Some(sequence);
        self.force_keyframe = false;
        Ok(packet)
    }
}
fn crop(frame: &Frame, roi: Roi) -> Frame {
    let mut pixels = Vec::with_capacity(usize::from(roi.width) * usize::from(roi.height) * 4);
    for row in roi.y..roi.y + roi.height {
        let start = (usize::from(row) * usize::from(frame.width) + usize::from(roi.x)) * 4;
        pixels.extend_from_slice(&frame.pixels[start..start + usize::from(roi.width) * 4]);
    }
    Frame {
        width: roi.width,
        height: roi.height,
        pixels,
    }
}

#[cfg(test)]
mod tests;
