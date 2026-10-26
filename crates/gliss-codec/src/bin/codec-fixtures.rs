//! Browser interoperability vectors. Expected pixels come from source frames, never a decoder.
use anyhow::{Context, Result};
use flate2::{Compression, write::ZlibEncoder};
use gliss_codec::{Encoder, Mode};
use gliss_core::Frame;
use serde::Serialize;
use std::io::Write;

#[derive(Serialize)]
struct Fixture {
    mode: String,
    sequence: u32,
    packet_hex: String,
    width: u16,
    height: u16,
    expected_pixels_hex: String,
}
fn main() -> Result<()> {
    let mut output = Vec::new();
    for mode in [
        Mode::Raw,
        Mode::Deflate,
        Mode::Palette,
        Mode::Xor,
        Mode::Copy,
        Mode::Adaptive,
    ] {
        let mut encoder = Encoder::new(mode, 16)?;
        let first = noise(67, 35);
        let mut edit = first.clone();
        edit.pixels[100..104].copy_from_slice(&[13, 22, 39, 255]);
        let mut scroll = edit.clone();
        for y in 0..28 {
            scroll.pixels[y * 67 * 4..(y + 1) * 67 * 4]
                .copy_from_slice(&edit.pixels[(y + 7) * 67 * 4..(y + 8) * 67 * 4]);
        }
        let mut palette = Frame::new(67, 35).unwrap();
        for (index, pixel) in palette.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel[..3].copy_from_slice(if index % 3 == 0 {
                &[16, 64, 196]
            } else {
                &[240, 244, 248]
            });
        }
        for (sequence, frame) in [first.clone(), first, edit, scroll, palette, noise(19, 17)]
            .into_iter()
            .enumerate()
        {
            let packet = encoder.encode(&frame, sequence as u32, sequence as u32 + 10)?;
            output.push(fixture(mode.name(), sequence as u32, &frame, packet));
        }
    }
    // Explicit palette vectors guarantee codec2 and codec3 even when byte-cost selection prefers deflate.
    let mut tiny = Frame::new(3, 2).unwrap();
    for (i, p) in tiny.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        if i % 2 == 0 {
            p[..3].copy_from_slice(&[5, 17, 231]);
        }
    }
    let mut palette = vec![2, 0, 5, 17, 231, 255, 0, 0, 0, 255];
    palette.extend([0, 1, 0, 1, 0, 1]);
    output.push(fixture(
        "forced_palette",
        0,
        &tiny,
        packet(&tiny, 0, true, 2, &palette),
    ));
    output.push(fixture(
        "forced_palette_zlib",
        0,
        &tiny,
        packet(&tiny, 0, true, 3, &compress(&palette)?),
    ));
    // Both temporal vectors deliberately have source/destination overlap.
    let previous = noise(19, 17);
    let mut next = previous.clone();
    next.pixels[4..8].copy_from_slice(&[1, 2, 3, 255]);
    output.push(fixture(
        "forced_xor",
        0,
        &previous,
        packet(&previous, 0, true, 0, &previous.pixels),
    ));
    let xor: Vec<_> = previous
        .pixels
        .iter()
        .zip(&next.pixels)
        .map(|(a, b)| a ^ b)
        .collect();
    output.push(fixture(
        "forced_xor",
        1,
        &next,
        packet(&next, 1, false, 4, &compress(&xor)?),
    ));
    output.push(fixture(
        "forced_copy",
        0,
        &previous,
        packet(&previous, 0, true, 0, &previous.pixels),
    ));
    let mut copied = previous.clone();
    copied.pixels[..16 * 19 * 4].copy_from_slice(&previous.pixels[19 * 4..]);
    let mut copy_packet = packet(&copied, 1, false, 5, &[0, 0, 1, 0]);
    copy_packet[30..32].copy_from_slice(&16u16.to_le_bytes()); // destination is top16 rows, source starts at row1.
    output.push(fixture("forced_copy", 1, &copied, copy_packet));
    // Append the new mode after every old vector; old objects and order remain
    // unchanged and are checked against the preserved pre-batch fixture file.
    let mut batch = Encoder::new(Mode::Batch, 16)?;
    let first = noise(67, 35);
    let mut edited = first.clone();
    edited.pixels[100..104].copy_from_slice(&[13, 22, 39, 255]);
    let mut scrolled = edited.clone();
    for y in 0..28 {
        scrolled.pixels[y * 67 * 4..(y + 1) * 67 * 4]
            .copy_from_slice(&edited.pixels[(y + 7) * 67 * 4..(y + 8) * 67 * 4]);
    }
    for (sequence, frame) in [first.clone(), first, edited, scrolled, noise(19, 17)]
        .into_iter()
        .enumerate()
    {
        let encoded = batch.encode(&frame, sequence as u32, sequence as u32 + 10)?;
        output.push(fixture("batch", sequence as u32, &frame, encoded));
    }
    output.push(fixture(
        "forced_batch_copy",
        0,
        &previous,
        batch_wrap(packet(&previous, 0, true, 0, &previous.pixels))?,
    ));
    let mut copied_packet = packet(&copied, 1, false, 5, &[0, 0, 1, 0]);
    copied_packet[30..32].copy_from_slice(&16u16.to_le_bytes());
    output.push(fixture(
        "forced_batch_copy",
        1,
        &copied,
        batch_wrap(copied_packet)?,
    ));
    // GLB2 vectors append after all49 earlier records, which remain byte exact.
    let mut global = Encoder::new(Mode::Global, 16)?;
    let first = noise(67, 35);
    let mut edit = first.clone();
    edit.pixels[100..104].copy_from_slice(&[13, 22, 39, 255]);
    let mut low_color = Frame::new(67, 35).unwrap();
    for (index, pixel) in low_color
        .pixels
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .enumerate()
    {
        pixel[..3].copy_from_slice(if index % 3 == 0 {
            &[16, 64, 196]
        } else {
            &[240, 244, 248]
        });
    }
    for (sequence, frame) in [
        first.clone(),
        first,
        edit,
        low_color,
        Frame::new(67, 35).unwrap(),
        noise(19, 17),
    ]
    .into_iter()
    .enumerate()
    {
        let encoded = global.encode(&frame, sequence as u32, sequence as u32 + 10)?;
        output.push(fixture("global", sequence as u32, &frame, encoded));
    }
    output.push(fixture(
        "forced_global_palette",
        0,
        &tiny,
        global_wrap(packet(&tiny, 0, true, 2, &palette))?,
    ));
    let mut solid = Frame::new(3, 2).unwrap();
    for pixel in solid.pixels.as_chunks_mut::<4>().0.iter_mut() {
        *pixel = [7, 21, 231, 255];
    }
    output.push(fixture(
        "forced_global_solid",
        0,
        &solid,
        global_wrap(packet(&solid, 0, true, 6, &[7, 21, 231, 255]))?,
    ));
    output.push(fixture(
        "forced_global_xor",
        0,
        &previous,
        global_wrap(packet(&previous, 0, true, 0, &previous.pixels))?,
    ));
    output.push(fixture(
        "forced_global_xor",
        1,
        &next,
        global_wrap(packet(&next, 1, false, 7, &xor))?,
    ));
    output.push(fixture(
        "forced_global_copy",
        0,
        &previous,
        global_wrap(packet(&previous, 0, true, 0, &previous.pixels))?,
    ));
    let mut global_copy = packet(&copied, 1, false, 5, &[0, 0, 1, 0]);
    global_copy[30..32].copy_from_slice(&16u16.to_le_bytes());
    output.push(fixture(
        "forced_global_copy",
        1,
        &copied,
        global_wrap(global_copy)?,
    ));
    // GLB3 appends after all61 earlier records. Expectations remain source bytes.
    let mut sparse = Encoder::new(Mode::Sparse, 16)?;
    let first = noise(67, 35);
    let mut edit = first.clone();
    edit.pixels[100..104].copy_from_slice(&[13, 22, 39, 255]);
    let mut low_color = Frame::new(67, 35).unwrap();
    for (index, pixel) in low_color
        .pixels
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .enumerate()
    {
        pixel[..3].copy_from_slice(if index % 3 == 0 {
            &[16, 64, 196]
        } else {
            &[240, 244, 248]
        });
    }
    for (sequence, frame) in [
        first.clone(),
        first,
        edit,
        low_color,
        Frame::new(67, 35).unwrap(),
        noise(19, 17),
    ]
    .into_iter()
    .enumerate()
    {
        let encoded = sparse.encode(&frame, sequence as u32, sequence as u32 + 10)?;
        output.push(fixture("sparse", sequence as u32, &frame, encoded));
    }
    output.push(fixture(
        "forced_sparse_xor",
        0,
        &previous,
        sparse_wrap(packet(&previous, 0, true, 0, &previous.pixels))?,
    ));
    output.push(fixture(
        "forced_sparse_xor",
        1,
        &next,
        sparse_wrap(packet(&next, 1, false, 7, &xor))?,
    ));
    output.push(fixture(
        "forced_sparse_copy",
        0,
        &previous,
        sparse_wrap(packet(&previous, 0, true, 0, &previous.pixels))?,
    ));
    let mut sparse_copy = packet(&copied, 1, false, 5, &[0, 0, 1, 0]);
    sparse_copy[30..32].copy_from_slice(&16u16.to_le_bytes());
    output.push(fixture(
        "forced_sparse_copy",
        1,
        &copied,
        sparse_wrap(sparse_copy)?,
    ));
    output.push(fixture(
        "forced_sparse_patch",
        0,
        &previous,
        sparse_wrap(packet(&previous, 0, true, 0, &previous.pixels))?,
    ));
    let mut patched = previous.clone();
    let mut patch = 3u16.to_le_bytes().to_vec();
    for (index, color) in [
        (0u16, [1, 2, 3, 255]),
        (63, [31, 17, 91, 255]),
        (322, [191, 73, 29, 255]),
    ] {
        patch.extend_from_slice(&index.to_le_bytes());
        patch.extend_from_slice(&color);
        patched.pixels[usize::from(index) * 4..usize::from(index) * 4 + 4].copy_from_slice(&color);
    }
    output.push(fixture(
        "forced_sparse_patch",
        1,
        &patched,
        sparse_wrap(packet(&patched, 1, false, 8, &patch))?,
    ));
    let json = serde_json::to_string_pretty(&output)?;
    match std::env::args().nth(1) {
        Some(path) => std::fs::write(path, format!("{json}\n"))?,
        None => println!("{json}"),
    }
    Ok(())
}
fn batch_wrap(mut packet: Vec<u8>) -> Result<Vec<u8>> {
    let body_length = packet.len() - 24;
    let body = compress(&packet[24..])?;
    packet.truncate(24);
    packet[..4].copy_from_slice(b"GLB1");
    packet[20..24].copy_from_slice(&(body_length as u32).to_le_bytes());
    packet.extend_from_slice(&body);
    Ok(packet)
}
fn global_wrap(mut packet: Vec<u8>) -> Result<Vec<u8>> {
    let body_length = packet.len() - 24;
    let body = compress(&packet[24..])?;
    packet.truncate(24);
    packet[..4].copy_from_slice(b"GLB2");
    packet[20..24].copy_from_slice(&(body_length as u32).to_le_bytes());
    packet.extend_from_slice(&body);
    Ok(packet)
}
fn sparse_wrap(mut packet: Vec<u8>) -> Result<Vec<u8>> {
    let body_length = packet.len() - 24;
    let body = compress(&packet[24..])?;
    packet.truncate(24);
    packet[..4].copy_from_slice(b"GLB3");
    packet[20..24].copy_from_slice(&(body_length as u32).to_le_bytes());
    packet.extend_from_slice(&body);
    Ok(packet)
}
fn fixture(mode: &str, sequence: u32, frame: &Frame, packet: Vec<u8>) -> Fixture {
    Fixture {
        mode: mode.into(),
        sequence,
        packet_hex: hex(&packet),
        width: frame.width,
        height: frame.height,
        expected_pixels_hex: hex(&frame.pixels),
    }
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(value, "{byte:02x}").unwrap();
    }
    value
}
fn noise(width: u16, height: u16) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    let mut state = 0x1234_abcd_9987_6543u64;
    for pixel in frame.pixels.as_chunks_mut::<4>().0.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        pixel[..3].copy_from_slice(&state.to_le_bytes()[..3]);
    }
    frame
}
fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
    z.write_all(bytes)?;
    z.finish().context("zlib")
}
fn packet(frame: &Frame, sequence: u32, keyframe: bool, codec: u8, body: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"GLS1");
    p.extend_from_slice(&sequence.to_le_bytes());
    p.extend_from_slice(&10u32.to_le_bytes());
    p.extend_from_slice(&frame.width.to_le_bytes());
    p.extend_from_slice(&frame.height.to_le_bytes());
    p.extend_from_slice(&1u16.to_le_bytes());
    p.extend_from_slice(&u16::from(keyframe).to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(&frame.width.to_le_bytes());
    p.extend_from_slice(&frame.height.to_le_bytes());
    p.extend_from_slice(&[codec, 0, 0, 0]);
    p.extend_from_slice(&(body.len() as u32).to_le_bytes());
    p.extend_from_slice(body);
    p
}
