use crate::classifier::{BASE_TILE_SIZE, MAX_TILES, grid, tile_bounds, valid_grid};
use anyhow::{Result, ensure};
use gliss_core::MAX_PIXELS;

pub const HEADER_BYTES: usize = 64;
pub const MAX_PACKET_BYTES: usize = HEADER_BYTES
    + MAX_TILES * 2
    + gliss_codec::MAX_PACKET_BYTES
    + gliss_video::HEADER_LEN
    + gliss_video::MAX_PAYLOAD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roi {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Canonical even/minimum-sized enclosing crop; unsupported geometry stays lossless.
pub fn roi_for_masks(masks: &[u16], width: u16, height: u16, mask_grid: u16) -> Option<Roi> {
    if masks.is_empty() || width == 0 || height == 0 || !valid_grid(mask_grid) {
        return None;
    }
    let mut left = usize::from(width);
    let mut top = usize::from(height);
    let mut right = 0;
    let mut bottom = 0;
    let (columns, rows) = grid(width, height, mask_grid);
    if columns * rows > MAX_TILES {
        return None;
    }
    for &index in masks {
        if usize::from(index) >= columns * rows {
            return None;
        }
        let (x, y, w, h) = tile_bounds(index, width, height, mask_grid);
        left = left.min(x);
        top = top.min(y);
        right = right.max(x + w);
        bottom = bottom.max(y + h);
    }
    fn axis(start: usize, end: usize, limit: usize) -> Option<(usize, usize)> {
        let mut start = start;
        let mut end = (end + 1) & !1;
        if end > limit {
            return None;
        }
        if end - start < 16 {
            let extra = 16 - (end - start);
            if end + extra <= limit {
                end += extra;
            } else {
                start = start.checked_sub(extra)?;
            }
        }
        Some((start, end - start))
    }
    let (x, w) = axis(left, right, usize::from(width))?;
    let (y, h) = axis(top, bottom, usize::from(height))?;
    if w.max(h) > 3840 || w.min(h) > 2160 {
        return None;
    }
    Some(Roi {
        x: x as u16,
        y: y as u16,
        width: w as u16,
        height: h as u16,
    })
}

pub struct Packet<'a> {
    pub sequence: u32,
    pub input_id: u32,
    pub width: u16,
    pub height: u16,
    pub mask_grid: u16,
    pub keyframe: bool,
    pub video_keyframe: bool,
    pub masks: Vec<u16>,
    pub roi: Option<Roi>,
    pub base: &'a [u8],
    pub video: Option<&'a [u8]>,
}
pub fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
pub fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Validate the complete wrapper and nested declarations before codec submission.
pub fn parse_packet(bytes: &[u8]) -> Result<Packet<'_>> {
    ensure!(
        (HEADER_BYTES..=MAX_PACKET_BYTES).contains(&bytes.len()),
        "Invalid GLH4 size"
    );
    ensure!(&bytes[..4] == b"GLH4", "Unknown hybrid version");
    let sequence = u32_at(bytes, 4);
    let input_id = u32_at(bytes, 8);
    let width = u16_at(bytes, 12);
    let height = u16_at(bytes, 14);
    let flags = u16_at(bytes, 18);
    let area = usize::from(width) * usize::from(height);
    let mask_grid = u16_at(bytes, 16);
    ensure!(valid_grid(mask_grid), "GLH4 mask grid must be 16 or 32");
    let (columns, rows) = grid(width, height, mask_grid);
    ensure!(area > 0 && area <= MAX_PIXELS, "Invalid hybrid geometry");
    ensure!(
        flags & !7 == 0 && u16_at(bytes, 22) == 0 && bytes[48..64].iter().all(|b| *b == 0),
        "Invalid hybrid flags/reserved/tile size"
    );
    ensure!(
        u32_at(bytes, 40) == sequence,
        "Nested base sequence declaration differs"
    );
    let count = usize::from(u16_at(bytes, 20));
    let base_len = u32_at(bytes, 32) as usize;
    let video_len = u32_at(bytes, 36) as usize;
    ensure!(
        count <= columns * rows
            && count <= MAX_TILES
            && (count == 0 || columns * rows <= MAX_TILES)
            && (24..=gliss_codec::MAX_PACKET_BYTES).contains(&base_len)
            && video_len <= gliss_video::HEADER_LEN + gliss_video::MAX_PAYLOAD,
        "Hybrid nested count/length exceeds limits"
    );
    ensure!(
        HEADER_BYTES
            .checked_add(count * 2)
            .and_then(|n| n.checked_add(base_len))
            .and_then(|n| n.checked_add(video_len))
            == Some(bytes.len()),
        "Hybrid nested lengths differ"
    );
    let mut masks = Vec::with_capacity(count);
    for index in 0..count {
        let mask = u16_at(bytes, HEADER_BYTES + index * 2);
        ensure!(
            usize::from(mask) < columns * rows && masks.last().is_none_or(|old| *old < mask),
            "Hybrid masks must be sorted unique and in range"
        );
        masks.push(mask);
    }
    let has_video = flags & 2 != 0;
    let keyframe = flags & 1 != 0;
    let video_keyframe = flags & 4 != 0;
    let declared = Roi {
        x: u16_at(bytes, 24),
        y: u16_at(bytes, 26),
        width: u16_at(bytes, 28),
        height: u16_at(bytes, 30),
    };
    let roi = if has_video {
        ensure!(
            count > 0 && video_len >= gliss_video::HEADER_LEN && u32_at(bytes, 44) == sequence,
            "Invalid video activation declaration"
        );
        let expected = roi_for_masks(&masks, width, height, mask_grid)
            .ok_or_else(|| anyhow::anyhow!("Unsupported hybrid ROI"))?;
        ensure!(
            declared == expected,
            "Hybrid ROI differs from canonical mask bounds"
        );
        Some(expected)
    } else {
        ensure!(
            count == 0
                && video_len == 0
                && !video_keyframe
                && u32_at(bytes, 44) == 0
                && declared
                    == (Roi {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0
                    }),
            "Video-free hybrid has masks/ROI/payload"
        );
        None
    };
    let base_start = HEADER_BYTES + count * 2;
    let base = &bytes[base_start..base_start + base_len];
    ensure!(
        &base[..4] == b"GLB2"
            && u32_at(base, 4) == sequence
            && u32_at(base, 8) == input_id
            && u16_at(base, 12) == width
            && u16_at(base, 14) == height
            && u16_at(base, 18) == u16::from(keyframe),
        "Nested Global declarations differ"
    );
    let base_count = usize::from(u16_at(base, 16));
    let (base_columns, base_rows) = grid(width, height, BASE_TILE_SIZE);
    let expanded = u32_at(base, 20) as usize;
    ensure!(
        base_count <= base_columns * base_rows
            && expanded >= base_count * 16
            && expanded <= area * 4 + base_count * 16,
        "Nested Global expansion exceeds bound"
    );
    let video = if has_video {
        let video = &bytes[base_start + base_len..];
        let parsed = gliss_video::parse_packet(video)?;
        // Match the existing browser adapter's bounded Baseline Annex B preflight.
        for (index, nal) in openh264::nal_units(parsed.payload).enumerate() {
            ensure!(index < 4096, "Too many hybrid video NAL units");
            let prefix = if nal.starts_with(&[0, 0, 0, 1]) {
                4
            } else if nal.starts_with(&[0, 0, 1]) {
                3
            } else {
                0
            };
            ensure!(
                prefix > 0 && nal.len() > prefix && nal[prefix] & 128 == 0,
                "Invalid hybrid Annex B NAL header"
            );
            if nal[prefix] & 31 == 7 {
                ensure!(
                    nal.len() >= prefix + 4 && nal[prefix + 1] == 66,
                    "Hybrid video requires Baseline SPS"
                );
            }
        }
        let roi = roi.unwrap();
        ensure!(
            parsed.sequence == sequence
                && parsed.input_id == input_id
                && parsed.width == roi.width
                && parsed.height == roi.height
                && parsed.keyframe == video_keyframe
                && (!keyframe || parsed.keyframe),
            "Nested video declarations/keyframe differ"
        );
        Some(video)
    } else {
        None
    };
    Ok(Packet {
        sequence,
        input_id,
        width,
        height,
        mask_grid,
        keyframe,
        video_keyframe,
        masks,
        roi,
        base,
        video,
    })
}

pub fn wrap(
    base: &[u8],
    video: Option<&[u8]>,
    masks: &[u16],
    roi: Option<Roi>,
    mask_grid: u16,
) -> Result<Vec<u8>> {
    ensure!(
        (24..=gliss_codec::MAX_PACKET_BYTES).contains(&base.len()) && masks.len() <= MAX_TILES,
        "Invalid base/mask bound"
    );
    ensure!(
        video.is_none_or(
            |packet| (24..=gliss_video::HEADER_LEN + gliss_video::MAX_PAYLOAD)
                .contains(&packet.len())
        ),
        "Invalid nested video size"
    );
    let mut bytes = vec![0; HEADER_BYTES];
    bytes[..4].copy_from_slice(b"GLH4");
    bytes[4..16].copy_from_slice(&base[4..16]);
    bytes[16..18].copy_from_slice(&mask_grid.to_le_bytes());
    let flags = u16_at(base, 18)
        | (u16::from(video.is_some()) * 2)
        | (u16::from(video.is_some_and(|p| u16_at(p, 18) == 1)) * 4);
    bytes[18..20].copy_from_slice(&flags.to_le_bytes());
    bytes[20..22].copy_from_slice(&(masks.len() as u16).to_le_bytes());
    if let Some(roi) = roi {
        for (offset, value) in [(24, roi.x), (26, roi.y), (28, roi.width), (30, roi.height)] {
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
    }
    bytes[32..36].copy_from_slice(&(base.len() as u32).to_le_bytes());
    bytes[36..40].copy_from_slice(&(video.map_or(0, |p| p.len()) as u32).to_le_bytes());
    bytes[40..44].copy_from_slice(&base[4..8]);
    if video.is_some() {
        bytes[44..48].copy_from_slice(&base[4..8]);
    }
    for index in masks {
        bytes.extend_from_slice(&index.to_le_bytes());
    }
    bytes.extend_from_slice(base);
    if let Some(video) = video {
        bytes.extend_from_slice(video);
    }
    parse_packet(&bytes)?;
    Ok(bytes)
}
