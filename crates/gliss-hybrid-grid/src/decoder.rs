use crate::{Roi, classifier::tile_bounds, parse_packet};
use anyhow::{Context, Result, ensure};
use gliss_codec::Decoder as BaseDecoder;
use gliss_core::Frame;
use openh264::{
    OpenH264API,
    decoder::{Decoder as NativeVideo, DecoderConfig},
    formats::YUVSource,
};

pub struct Decoder {
    base: BaseDecoder,
    base_frame: Option<Frame>,
    display: Option<Frame>,
    video: Option<NativeVideo>,
    sequence: Option<u32>,
    input_id: u32,
    roi: Option<Roi>,
    masks: Vec<u16>,
    needs_keyframe: bool,
    mask_grid: Option<u16>,
}
impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}
impl Decoder {
    pub fn new() -> Self {
        Self {
            base: BaseDecoder::new(),
            base_frame: None,
            display: None,
            video: None,
            sequence: None,
            input_id: 0,
            roi: None,
            masks: Vec::new(),
            needs_keyframe: true,
            mask_grid: None,
        }
    }
    pub fn reset(&mut self) {
        *self = Self::new();
    }
    pub fn sequence(&self) -> Option<u32> {
        self.sequence
    }
    pub fn input_id(&self) -> u32 {
        self.input_id
    }
    pub fn mask_grid(&self) -> Option<u16> {
        self.mask_grid
    }
    pub fn base_frame(&self) -> Option<&Frame> {
        self.base_frame.as_ref()
    }
    pub fn frame(&self) -> Option<&Frame> {
        self.display.as_ref()
    }
    pub fn needs_keyframe(&self) -> bool {
        self.needs_keyframe
    }
    pub fn decode(&mut self, bytes: &[u8]) -> Result<Frame> {
        let packet = parse_packet(bytes)?;
        if !packet.keyframe {
            ensure!(
                !self.needs_keyframe
                    && self.mask_grid == Some(packet.mask_grid)
                    && self
                        .sequence
                        .is_some_and(|old| old.wrapping_add(1) == packet.sequence),
                "Hybrid delta requires ordered reference/recovery keyframe"
            );
            ensure!(
                self.base_frame
                    .as_ref()
                    .is_some_and(|old| old.width == packet.width && old.height == packet.height),
                "Hybrid delta changed dimensions"
            );
        }
        ensure!(
            packet.video.is_none()
                || packet.video_keyframe
                || (self.video.is_some()
                    && self.mask_grid == Some(packet.mask_grid)
                    && self.roi == packet.roi
                    && self.masks == packet.masks),
            "Video activation/mask/ROI change requires IDR"
        );
        let staged = match self.base.decode(packet.base) {
            Ok(frame) => frame,
            Err(error) => {
                self.needs_keyframe = true;
                return Err(error);
            }
        };
        let result = (|| -> Result<(Frame, Option<NativeVideo>)> {
            ensure!(
                staged.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
                "Hybrid base is not opaque"
            );
            for &index in &packet.masks {
                let (x, y, w, h) =
                    tile_bounds(index, packet.width, packet.height, packet.mask_grid);
                for row in y..y + h {
                    for col in x..x + w {
                        let offset = (row * usize::from(packet.width) + col) * 4;
                        ensure!(
                            staged.pixels[offset..offset + 4] == [0, 0, 0, 255],
                            "Active hybrid base tile is not canonical black"
                        );
                    }
                }
            }
            let mut display = staged.clone();
            let mut fresh = None;
            if let (Some(video), Some(roi)) = (packet.video, packet.roi) {
                if packet.keyframe
                    || self.video.is_none()
                    || self.mask_grid != Some(packet.mask_grid)
                    || self.roi != packet.roi
                    || self.masks != packet.masks
                {
                    // Reuse the existing baseline's default synchronous decoder.
                    // Its experimental unsafe threading option is deliberately unused.
                    let config = DecoderConfig::new();
                    fresh = Some(NativeVideo::with_api_config(
                        OpenH264API::from_source(),
                        config,
                    )?);
                }
                let decoder = if let Some(fresh) = fresh.as_mut() {
                    fresh
                } else {
                    self.video.as_mut().unwrap()
                };
                let decoded = decoder
                    .decode(gliss_video::parse_packet(video)?.payload)?
                    .context("Hybrid video produced no frame")?;
                ensure!(
                    decoded.dimensions() == (usize::from(roi.width), usize::from(roi.height)),
                    "Hybrid decoded ROI geometry differs"
                );
                let mut rgba = vec![0; usize::from(roi.width) * usize::from(roi.height) * 4];
                decoded.write_rgba8(&mut rgba);
                for &index in &packet.masks {
                    let (x, y, w, h) =
                        tile_bounds(index, packet.width, packet.height, packet.mask_grid);
                    for row in y..y + h {
                        for col in x..x + w {
                            let source = ((row - usize::from(roi.y)) * usize::from(roi.width)
                                + col
                                - usize::from(roi.x))
                                * 4;
                            let destination = (row * usize::from(packet.width) + col) * 4;
                            display.pixels[destination..destination + 3]
                                .copy_from_slice(&rgba[source..source + 3]);
                            display.pixels[destination + 3] = 255;
                        }
                    }
                }
            }
            Ok((display, fresh))
        })();
        let (display, fresh) = match result {
            Ok(value) => value,
            Err(error) => {
                self.needs_keyframe = true;
                self.video = None;
                // The unchanged base decoder commits synchronously; reconstruct only on
                // a later video/canonical-base failure, keeping public state unchanged.
                self.base.reset();
                if let Some(old) = &self.base_frame {
                    self.base
                        .decode(&seed(old, self.sequence.unwrap(), self.input_id))?;
                }
                return Err(error);
            }
        };
        if packet.video.is_none() {
            self.video = None;
        } else if fresh.is_some() {
            self.video = fresh;
        }
        self.base_frame = Some(staged);
        self.display = Some(display.clone());
        self.sequence = Some(packet.sequence);
        self.input_id = packet.input_id;
        self.mask_grid = Some(packet.mask_grid);
        self.roi = packet.roi;
        self.masks = packet.masks;
        self.needs_keyframe = false;
        Ok(display)
    }
}
fn seed(frame: &Frame, sequence: u32, input_id: u32) -> Vec<u8> {
    let mut bytes = vec![0; 40];
    bytes[..4].copy_from_slice(b"GLS1");
    bytes[4..8].copy_from_slice(&sequence.to_le_bytes());
    bytes[8..12].copy_from_slice(&input_id.to_le_bytes());
    for (offset, value) in [
        (12, frame.width),
        (14, frame.height),
        (16, 1),
        (18, 1),
        (28, frame.width),
        (30, frame.height),
    ] {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    bytes[36..40].copy_from_slice(&(frame.pixels.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&frame.pixels);
    bytes
}
