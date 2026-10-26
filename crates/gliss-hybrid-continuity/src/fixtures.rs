//! Opaque synthetic fixture pixels; no captured frames, semantic image hints or IDs.
use crate::classifier::{grid, valid_grid};
use anyhow::{Result, ensure};
use gliss_core::Frame;

pub fn scene(width: u16, height: u16, tick: u32, mask_grid: u16, moving: &[u16]) -> Result<Frame> {
    ensure!(valid_grid(mask_grid), "Fixture grid must be 16 or 32");
    let mut frame =
        Frame::new(width, height).ok_or_else(|| anyhow::anyhow!("Invalid fixture dimensions"))?;
    let (columns, rows) = grid(width, height, mask_grid);
    ensure!(
        moving
            .iter()
            .all(|index| usize::from(*index) < columns * rows),
        "Fixture cell outside frame"
    );
    let glyph = [
        0b01110u8, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
    ];
    for y in 0..usize::from(height) {
        for x in 0..usize::from(width) {
            let index = (y / usize::from(mask_grid)) * columns + x / usize::from(mask_grid);
            let rgb = if moving.contains(&(index as u16)) {
                [
                    32 + ((2 * x + tick as usize * 5) % 192) as u8,
                    48 + ((2 * y + tick as usize * 7) % 176) as u8,
                    64 + ((x + y + tick as usize * 3) % 160) as u8,
                ]
            } else {
                let local = (x + tick as usize) % 16;
                let ink = local < 5 && glyph[y % 7] & (1 << (4 - local)) != 0;
                let shade = if ink { 232 } else { 24 };
                [shade, shade, shade]
            };
            let offset = (y * usize::from(width) + x) * 4;
            frame.pixels[offset..offset + 3].copy_from_slice(&rgb);
        }
    }
    Ok(frame)
}

pub fn text_overlay(frame: &mut Frame) {
    let glyph = [
        0b01110u8, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
    ];
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..5 {
            let x = 4 + col;
            let y = 4 + row;
            if x < usize::from(frame.width)
                && y < usize::from(frame.height)
                && bits & (1 << (4 - col)) != 0
            {
                let offset = (y * usize::from(frame.width) + x) * 4;
                frame.pixels[offset..offset + 4].copy_from_slice(&[240, 240, 240, 255]);
            }
        }
    }
}

/// Stable enclosing anchors with internal image exposure/removal. Coordinates
/// belong to this synthetic fixture; the encoder only receives opaque pixels.
pub fn continuity_scene(mask_grid: u16, tick: u32) -> Result<Frame> {
    let moving: &[u16] = match tick {
        4..=8 | 27..=29 => &[0, 1, 3],
        15..=20 => &[0, 2, 3],
        _ => &[0, 3],
    };
    let mut frame = scene(128, 64, tick, mask_grid, moving)?;
    if (6..=8).contains(&tick) {
        // Make newly exposed pixels visibly different from the preceding crop,
        // while retaining opaque/high-diversity synthetic texture.
        for y in 0..usize::from(mask_grid) {
            for x in usize::from(mask_grid)..usize::from(mask_grid) * 2 {
                let offset = (y * 128 + x) * 4;
                for channel in 0..3 {
                    frame.pixels[offset + channel] ^= 128;
                }
            }
        }
    }
    Ok(frame)
}

/// Fixture-only evidence that an actual Annex-B access unit contains a P slice.
/// OpenH264 0.9.8 upstream common/inc/wels_common_defs.h declares P_SLICE=0;
/// the decoder reads first_mb_in_slice and slice_type as unsigned Exp-Golomb.
/// Inspect at most 32 RBSP bytes per type-1 NAL, never alter native payloads.
pub fn contains_p_slice(payload: &[u8]) -> bool {
    if payload.len() > gliss_video::MAX_PAYLOAD {
        return false;
    }
    fn unsigned(bytes: &[u8], bit: &mut usize) -> Option<u32> {
        let read = |position: &mut usize| -> Option<u32> {
            let byte = bytes.get(*position / 8)?;
            let value = u32::from((byte >> (7 - *position % 8)) & 1);
            *position += 1;
            Some(value)
        };
        let mut zeros = 0;
        while read(bit)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut value = 0u32;
        for _ in 0..zeros {
            value = (value << 1) | read(bit)?;
        }
        (1u32.checked_shl(zeros)? - 1).checked_add(value)
    }
    for nal in openh264::nal_units(payload) {
        let start = if nal.starts_with(&[0, 0, 0, 1]) {
            4
        } else if nal.starts_with(&[0, 0, 1]) {
            3
        } else {
            continue;
        };
        if nal.get(start).is_none_or(|header| header & 31 != 1) {
            continue;
        }
        let mut rbsp = Vec::with_capacity(32);
        let mut zeros = 0;
        for &byte in nal[start + 1..].iter().take(32) {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            rbsp.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        let mut bit = 0;
        if unsigned(&rbsp, &mut bit).is_some()
            && unsigned(&rbsp, &mut bit).is_some_and(|slice| slice <= 9 && slice % 5 == 0)
        {
            return true;
        }
    }
    false
}
