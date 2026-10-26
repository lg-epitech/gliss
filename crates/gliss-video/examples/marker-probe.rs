//! Diagnostic fixture only: exact copied rect/render functions from demo.rs.
//! Source SHA-256: 024e5e1da0c99dfb5d5eb78ad0bd895811d678824c078d47c48127f858cdfab3.
//! Uses the unchanged public VideoEncoder and native decoder; no configuration changes.
//! Run: CARGO_BUILD_JOBS=2 cargo run --release -p gliss-video --example marker-probe -- 150

use anyhow::{Result, ensure};
use gliss_core::Frame;
use gliss_video::{VideoEncoder, parse_packet};
use openh264::decoder::Decoder;
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use std::io::{self, Write};

fn rect(frame: &mut Frame, x: usize, y: usize, w: usize, h: usize, color: [u8; 4]) {
    for py in y..(y + h).min(usize::from(frame.height)) {
        for px in x..(x + w).min(usize::from(frame.width)) {
            let pos = (py * usize::from(frame.width) + px) * 4;
            frame.pixels[pos..pos + 4].copy_from_slice(&color);
        }
    }
}

fn render(
    width: u16,
    height: u16,
    tick: u32,
    id: u32,
    workload: &str,
    pointer: (u16, u16),
) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    let w = usize::from(width);
    let h = usize::from(height);
    rect(&mut frame, 0, 0, w, h, [34, 48, 61, 255]);
    rect(
        &mut frame,
        20,
        50,
        w.saturating_sub(40),
        h.saturating_sub(80),
        [242, 244, 246, 255],
    );
    rect(
        &mut frame,
        20,
        50,
        w.saturating_sub(40),
        25,
        [80, 97, 116, 255],
    );
    let offset = if workload == "scroll" {
        tick as usize * 3
    } else {
        0
    };
    for row in 0..h.saturating_sub(110) / 16 {
        for col in 0..w.saturating_sub(80) / 8 {
            let seed = (row * 27 + col * 13 + offset / 16) % 97;
            if seed % 5 != 0 {
                rect(
                    &mut frame,
                    38 + col * 8,
                    85 + row * 16,
                    3 + seed % 4,
                    8,
                    [30 + (seed % 60) as u8, 65, 98, 255],
                );
            }
        }
    }
    if workload == "drag" {
        let x = 30 + (tick as usize * 7) % w.saturating_sub(230).max(1);
        let y = 80 + (tick as usize * 3) % h.saturating_sub(230).max(1);
        rect(&mut frame, x, y, 180, 130, [109, 157, 194, 255]);
    }
    if workload == "video" {
        for y in 80..h.saturating_sub(30) {
            for x in 30..w.saturating_sub(30) {
                let p = (y * w + x) * 4;
                frame.pixels[p] = (x as u32 + tick * 5) as u8;
                frame.pixels[p + 1] = (y as u32 + tick * 3) as u8;
                frame.pixels[p + 2] = ((x ^ y) as u32 + tick * 11) as u8;
            }
        }
    }
    rect(
        &mut frame,
        usize::from(pointer.0),
        usize::from(pointer.1),
        8,
        8,
        [230, 91, 56, 255],
    );
    // The reserved probe remains visible after pointer movement, blur and release.
    rect(
        &mut frame,
        0,
        0,
        24,
        24,
        [id as u8, (id >> 8) as u8, (id >> 16) as u8, 255],
    );
    let probe_id = id & 0x00ff_ffff;
    let checksum = (probe_id as u8) ^ ((probe_id >> 8) as u8) ^ ((probe_id >> 16) as u8) ^ 0xa5;
    let probe = probe_id | (u32::from(checksum) << 24);
    for bit in 0..32 {
        let value = if probe & (1 << bit) != 0 { 235 } else { 20 };
        rect(
            &mut frame,
            32 + (bit % 8) * 8,
            (bit / 8) * 8,
            8,
            8,
            [value, value, value, 255],
        );
    }
    frame
}

fn probe(pixels: &[[u8; 3]]) -> Option<u32> {
    let mut bits = 0_u32;
    for (bit, rgb) in pixels.iter().enumerate() {
        let low = *rgb.iter().min().unwrap();
        let high = *rgb.iter().max().unwrap();
        if high - low > 24 || !(high < 80 || low > 176) {
            return None;
        }
        if low > 176 {
            bits |= 1 << bit;
        }
    }
    let id = bits & 0xffffff;
    let check = (bits >> 24) as u8;
    let expected = (id as u8) ^ ((id >> 8) as u8) ^ ((id >> 16) as u8) ^ 0xa5;
    (check == expected).then_some(id)
}

fn yuv_cells(source: &impl YUVSource) -> Vec<[u8; 3]> {
    let (sy, su, sv) = source.strides();
    (0..32)
        .map(|bit| {
            let x = 36 + (bit % 8) * 8;
            let y = 4 + (bit / 8) * 8;
            [
                source.y()[y * sy + x],
                source.u()[(y / 2) * su + x / 2],
                source.v()[(y / 2) * sv + x / 2],
            ]
        })
        .collect()
}

fn main() -> Result<()> {
    let count = std::env::args()
        .nth(1)
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(150);
    ensure!(
        (1..=180).contains(&count),
        "frame count must be within 1..=180"
    );
    let mut encoder = VideoEncoder::new(960, 600, 60)?;
    let mut decoder = Decoder::new()?;
    let mut source_yuv = YUVBuffer::new(960, 600);
    let mut output = io::BufWriter::new(io::stdout().lock());
    for tick in 0..count {
        let id = if tick < 5 {
            0
        } else if tick < 21 {
            1
        } else {
            2
        };
        let frame = render(960, 600, tick, id, "video", (480, 300));
        source_yuv.read_rgba8(RgbaSliceU8::new(&frame.pixels, (960, 600)));
        let encoded = encoder.encode(&frame, tick + 1, id)?;
        let packet = parse_packet(&encoded)?;
        let decoded = decoder
            .decode(packet.payload)?
            .expect("one complete access unit must decode a frame");
        let mut rgba = vec![0; frame.pixels.len()];
        decoded.write_rgba8(&mut rgba);
        let cells: Vec<[u8; 3]> = (0..32)
            .map(|bit| {
                let x = 36 + (bit % 8) * 8;
                let y = 4 + (bit / 8) * 8;
                rgba[(y * 960 + x) * 4..][..3].try_into().unwrap()
            })
            .collect();
        let uncertain: Vec<usize> = cells
            .iter()
            .enumerate()
            .filter_map(|(bit, rgb)| {
                let low = *rgb.iter().min().unwrap();
                let high = *rgb.iter().max().unwrap();
                (high - low > 24 || !(high < 80 || low > 176)).then_some(bit)
            })
            .collect();
        let nearest = cells.iter().enumerate().fold(0_u32, |word, (bit, rgb)| {
            if rgb.iter().map(|&value| u32::from(value)).sum::<u32>() > 128 * 3 {
                word | (1 << bit)
            } else {
                word
            }
        });
        let read_probe = probe(&cells).map_or_else(|| "null".to_owned(), |value| value.to_string());
        writeln!(
            output,
            "{{\"tick\":{tick},\"sequence\":{},\"forwarded_input_id\":{},\"keyframe\":{},\"packet_bytes\":{},\"read_probe\":{read_probe},\"nearest_word\":{nearest},\"uncertain_bits\":{uncertain:?},\"cells_rgb\":{cells:?},\"source_cells_yuv\":{:?},\"decoded_cells_yuv\":{:?}}}",
            packet.sequence,
            packet.input_id,
            packet.keyframe,
            encoded.len(),
            yuv_cells(&source_yuv),
            yuv_cells(&decoded)
        )?;
    }
    Ok(())
}
