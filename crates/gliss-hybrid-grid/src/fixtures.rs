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
