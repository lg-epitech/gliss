use anyhow::{Result, ensure};
use gliss_codec::{Encoder as BaseEncoder, Mode};
use gliss_core::Frame;
use gliss_video::VideoEncoder;
use crate::{Classifier, Roi, classifier, wire};

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub active_tiles: usize,
    pub classified_tiles: usize,
    pub roi: Option<Roi>,
    pub roi_fallback: bool,
    pub base_bytes: usize,
    pub video_bytes: usize,
}

pub struct HybridEncoder {
    base: BaseEncoder,
    classifier: Classifier,
    video: Option<VideoEncoder>,
    masks: Vec<u16>,
    roi: Option<Roi>,
    dimensions: Option<(u16, u16)>,
    sequence: Option<u32>,
    fps: u32,
    bitrate: u32,
    force_keyframe: bool,
    failed: bool,
    stats: Stats,
}
pub type Encoder = HybridEncoder;
impl HybridEncoder {
    pub fn new(fps: u32, bitrate: u32) -> Result<Self> {
        ensure!(
            (1..=60).contains(&fps) && (1_000..=50_000_000).contains(&bitrate),
            "Invalid hybrid video fps/bitrate"
        );
        Ok(Self {
            base: BaseEncoder::new(Mode::Global, TILE_SIZE)?,
            classifier: Classifier::default(),
            video: None,
            masks: Vec::new(),
            roi: None,
            dimensions: None,
            sequence: None,
            fps,
            bitrate,
            force_keyframe: true,
            failed: false,
            stats: Stats::default(),
        })
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
        let mut classifier = self.classifier.clone();
        let classified = classifier.update(frame)?;
        let roi = roi_for_masks(&classified, frame.width, frame.height);
        let masks = if roi.is_some() {
            classified.clone()
        } else {
            Vec::new()
        };
        let changed = masks != self.masks || roi != self.roi;
        let mut base = frame.clone();
        for &index in &masks {
            let (x, y, w, h) = classifier::tile_bounds(index, frame.width, frame.height);
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
                self.video = Some(VideoEncoder::with_bitrate(
                    roi.width,
                    roi.height,
                    self.fps,
                    self.bitrate,
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
        let packet = wire::wrap(&base, video.as_deref(), &masks, roi)?;
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

