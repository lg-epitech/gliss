use anyhow::{Context, Result};
use gliss_core::Frame;
use gliss_video::VideoEncoder;
use std::{fs, path::PathBuf};

fn main() -> Result<()> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: fixtures <output-directory>")?,
    );
    fs::create_dir_all(&dir)?;
    let mut encoder = VideoEncoder::new(96, 64, 30)?;
    for index in 0..4 {
        let (width, height) = if index == 3 { (98, 66) } else { (96, 64) };
        let mut frame = Frame::new(width, height).unwrap();
        for (offset, pixel) in frame.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = offset % usize::from(width);
            let y = offset / usize::from(width);
            let light = if (x + index * 2) % 32 < 16 { 210 } else { 35 };
            pixel.copy_from_slice(&[light, (y * 160 / usize::from(height)) as u8, light, 255]);
        }
        if index == 2 {
            encoder.reset();
        }
        fs::write(
            dir.join(format!("{index}.glv")),
            encoder.encode(&frame, 10 + index as u32, 100 + index as u32)?,
        )?;
        fs::write(dir.join(format!("{index}.rgba")), &frame.pixels)?;
    }
    Ok(())
}
