use super::*;

#[path = "../../../test-support/rust/codec/workloads.rs"]
mod cached_workloads;

fn noise(width: u16, height: u16, seed: u64) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    let mut state = seed;
    for pixel in frame.pixels.as_chunks_mut::<4>().0.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        pixel[..3].copy_from_slice(&state.to_le_bytes()[..3]);
    }
    frame
}
fn roundtrip(mode: Mode, frames: &[Frame]) {
    let mut encoder = Encoder::new(mode, 32).unwrap();
    let mut decoder = Decoder::new();
    for (i, frame) in frames.iter().enumerate() {
        let packet = encoder.encode(frame, i as u32, 10 + i as u32).unwrap();
        assert_eq!(
            &decoder.decode(&packet).unwrap(),
            frame,
            "mode={mode:?}, frame={i}"
        );
        assert_eq!(decoder.sequence(), Some(i as u32));
        assert_eq!(decoder.input_id(), 10 + i as u32);
    }
}
#[test]
fn all_modes_handle_random_partial_tiles_static_and_resize() {
    let first = noise(73, 51, 7);
    let mut edit = first.clone();
    edit.pixels[100..104].copy_from_slice(&[1, 2, 3, 255]);
    let mut black = Frame::new(73, 51).unwrap();
    for (i, pixel) in black.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        pixel[0] = (i % 3) as u8 * 50;
    }
    let frames = [
        first.clone(),
        first,
        edit,
        black,
        noise(85, 19, 32),
        noise(1, 1, 2),
    ];
    for mode in [
        Mode::Raw,
        Mode::Deflate,
        Mode::Palette,
        Mode::Xor,
        Mode::Copy,
        Mode::Adaptive,
        Mode::Batch,
        Mode::Global,
        Mode::Sparse,
        Mode::GlobalCached,
    ] {
        roundtrip(mode, &frames);
    }
}
#[test]
fn static_delta_is_empty_and_sequence_wraps() {
    let frame = noise(19, 21, 1);
    let mut encoder = Encoder::new(Mode::Adaptive, 64).unwrap();
    let mut decoder = Decoder::new();
    let first = encoder.encode(&frame, u32::MAX, 0).unwrap();
    decoder.decode(&first).unwrap();
    let empty = encoder.encode(&frame, 0, 1).unwrap();
    assert_eq!(empty.len(), HEADER_BYTES);
    assert_eq!(get16(&empty, 16), 0);
    assert_eq!(get16(&empty, 18), 0);
    assert_eq!(decoder.decode(&empty).unwrap(), frame);
}
#[test]
fn dropped_and_duplicate_packets_recover_with_keyframe() {
    let frame = noise(65, 33, 3);
    let mut encoder = Encoder::new(Mode::Adaptive, 32).unwrap();
    let mut decoder = Decoder::new();
    let first = encoder.encode(&frame, 1, 0).unwrap();
    let dropped = encoder.encode(&frame, 2, 0).unwrap();
    let third = encoder.encode(&frame, 3, 0).unwrap();
    decoder.decode(&first).unwrap();
    assert!(decoder.decode(&third).is_err());
    assert_eq!(decoder.sequence(), Some(1));
    decoder.decode(&dropped).unwrap();
    assert!(decoder.decode(&dropped).is_err());
    decoder.decode(&third).unwrap();
    encoder.reset();
    let recovery = encoder.encode(&frame, 40, 0).unwrap();
    assert_eq!(decoder.decode(&recovery).unwrap(), frame);
    // A discontinuity at the encoder also forces an independent keyframe.
    assert_eq!(get16(&encoder.encode(&frame, 45, 0).unwrap(), 18), 1);
}
#[test]
fn copy_scroll_reads_previous_frame() {
    let old = noise(96, 96, 20);
    let mut next = old.clone();
    // Non-grid translation, including copy destinations that overlap sources.
    for row in 0..89 {
        next.pixels[row * 96 * 4..(row + 1) * 96 * 4]
            .copy_from_slice(&old.pixels[(row + 7) * 96 * 4..(row + 8) * 96 * 4]);
    }
    let mut encoder = Encoder::new(Mode::Copy, 32).unwrap();
    let mut decoder = Decoder::new();
    decoder
        .decode(&encoder.encode(&old, 0, 0).unwrap())
        .unwrap();
    let packet = encoder.encode(&next, 1, 0).unwrap();
    let mut offset = 24;
    let mut copies = 0;
    for _ in 0..get16(&packet, 16) {
        if packet[offset + 8] == 5 {
            copies += 1;
        }
        offset += 16 + get32(&packet, offset + 12) as usize;
    }
    assert!(copies > 0);
    assert_eq!(decoder.decode(&packet).unwrap(), next);
}
#[test]
fn malformed_packets_never_mutate_decoder() {
    let first = noise(17, 19, 91);
    let mut encoder = Encoder::new(Mode::Raw, 32).unwrap();
    let good = encoder.encode(&first, 0, 0).unwrap();
    let delta = encoder.encode(&first, 1, 0).unwrap();
    let mut decoder = Decoder::new();
    decoder.decode(&good).unwrap();
    for end in 0..good.len() {
        assert!(
            decoder.decode(&good[..end]).is_err(),
            "accepted truncation at {end}"
        );
    }
    for (offset, value) in [
        (0, 0),
        (18, 2),
        (20, 1),
        (24, 255),
        (28, 0),
        (32, 6),
        (33, 1),
        (36, 255),
        (39, 255),
    ] {
        let mut bad = good.clone();
        bad[offset] = value;
        assert!(
            decoder.decode(&bad).is_err(),
            "accepted malformed offset {offset}"
        );
        assert_eq!(decoder.sequence(), Some(0));
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(decoder.decode(&trailing).is_err());
    assert_eq!(decoder.decode(&delta).unwrap(), first);
}
#[test]
fn palette_indices_zlib_bombs_overlap_and_missing_coverage_are_rejected() {
    let frame = Frame::new(16, 16).unwrap();
    let mut encoder = Encoder::new(Mode::Raw, 16).unwrap();
    let raw = encoder.encode(&frame, 0, 0).unwrap();
    let mut palette = raw[..40].to_vec();
    palette[32] = 2;
    let mut body = vec![1, 0, 0, 0, 0, 255];
    body.extend(vec![1; 256]);
    palette[36..40].copy_from_slice(&(body.len() as u32).to_le_bytes());
    palette.extend(body);
    assert!(Decoder::new().decode(&palette).is_err());
    let mut bomb = raw[..40].to_vec();
    bomb[32] = 1;
    let zipped = compress(&vec![0; 1025]).unwrap();
    bomb[36..40].copy_from_slice(&(zipped.len() as u32).to_le_bytes());
    bomb.extend(zipped);
    assert!(Decoder::new().decode(&bomb).is_err());
    let mut overlap = raw.clone();
    overlap[16..18].copy_from_slice(&2u16.to_le_bytes());
    overlap.extend_from_slice(&raw[24..]);
    assert!(Decoder::new().decode(&overlap).is_err());
    let mut empty = raw[..24].to_vec();
    empty[16..18].copy_from_slice(&0u16.to_le_bytes());
    assert!(Decoder::new().decode(&empty).is_err());
    let mut extra_zlib = raw[..40].to_vec();
    extra_zlib[32] = 1;
    let mut zipped = compress(&frame.pixels).unwrap();
    zipped.push(0);
    extra_zlib[36..40].copy_from_slice(&(zipped.len() as u32).to_le_bytes());
    extra_zlib.extend(zipped);
    assert!(Decoder::new().decode(&extra_zlib).is_err());
}
#[test]
fn arbitrary_small_packets_do_not_panic() {
    let mut state = 18u64;
    for len in 0..256 {
        let mut packet = vec![0; len];
        for byte in &mut packet {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        assert!(Decoder::new().decode(&packet).is_err());
    }
}
#[test]
fn invalid_frames_and_tile_sizes_are_rejected() {
    assert!(Encoder::new(Mode::Raw, 0).is_err());
    assert!(Encoder::new(Mode::Raw, 300).is_err());
    let mut encoder = Encoder::new(Mode::Raw, 32).unwrap();
    assert!(
        encoder
            .encode(
                &Frame {
                    width: 16,
                    height: 16,
                    pixels: vec![]
                },
                0,
                0
            )
            .is_err()
    );
}

#[test]
fn incomplete_zlib_stream_cannot_pass_with_a_shorter_payload_length() {
    let frame = Frame::new(16, 16).unwrap();
    let mut encoder = Encoder::new(Mode::Deflate, 16).unwrap();
    let original = encoder.encode(&frame, 0, 0).unwrap();
    assert_eq!(original[32], 1);
    for removed in 1..=4 {
        let mut bad = original[..original.len() - removed].to_vec();
        let length = (bad.len() - 40) as u32;
        bad[36..40].copy_from_slice(&length.to_le_bytes());
        assert!(
            Decoder::new().decode(&bad).is_err(),
            "accepted zlib missing {removed} trailer bytes"
        );
    }
}
#[test]
fn previous_revision_vectors_and_current_encoder_are_pixel_identical() {
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/experiments/fixtures-v1.json"
    ))
    .unwrap();
    let mut decoder = Decoder::new();
    let mut encoder = Encoder::new(Mode::Adaptive, 16).unwrap();
    let mut current_decoder = Decoder::new();
    let mut codecs = [false; 6];
    for fixture in fixtures.as_array().unwrap() {
        let sequence = fixture["sequence"].as_u64().unwrap() as u32;
        if sequence == 0 {
            decoder.reset();
            encoder.reset();
            current_decoder.reset();
        }
        let packet = unhex(fixture["packet_hex"].as_str().unwrap());
        let expected = Frame {
            width: fixture["width"].as_u64().unwrap() as u16,
            height: fixture["height"].as_u64().unwrap() as u16,
            pixels: unhex(fixture["expected_pixels_hex"].as_str().unwrap()),
        };
        assert_eq!(decoder.decode(&packet).unwrap(), expected);
        let current_packet = encoder.encode(&expected, sequence, 0).unwrap();
        assert_eq!(current_decoder.decode(&current_packet).unwrap(), expected);
        let mut offset = 24;
        for _ in 0..get16(&packet, 16) {
            codecs[packet[offset + 8] as usize] = true;
            offset += 16 + get32(&packet, offset + 12) as usize;
        }
    }
    assert!(codecs.into_iter().all(|covered| covered));
}
fn unhex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn sixteen_pixel_tiles_cannot_wrap_the_rectangle_count() {
    let frame = Frame::new(4096, 4096).unwrap();
    let error = Encoder::new(Mode::Raw, 16)
        .unwrap()
        .encode(&frame, 0, 0)
        .unwrap_err();
    assert!(error.to_string().contains("too many tiles"));
}

#[test]
fn batch_envelope_uses_one_stream_and_no_nested_codecs() {
    let frame = noise(73, 51, 123);
    let mut encoder = Encoder::new(Mode::Batch, 32).unwrap();
    let packet = encoder.encode(&frame, 3, 9).unwrap();
    assert_eq!(&packet[..4], b"GLB1");
    let expanded = expand_batch_packet(&packet).unwrap();
    assert_eq!(&expanded[..4], b"GLS1");
    assert_eq!(get32(&expanded, 20), 0);
    assert_eq!(expanded.len() - HEADER_BYTES, get32(&packet, 20) as usize);
    let mut offset = HEADER_BYTES;
    for _ in 0..get16(&expanded, 16) {
        assert_eq!(expanded[offset + 8], 0);
        offset += RECT_HEADER_BYTES + get32(&expanded, offset + 12) as usize;
    }
    assert_eq!(offset, expanded.len());
    assert_eq!(Decoder::new().decode(&packet).unwrap(), frame);
}

#[test]
fn batch_empty_delta_sequence_wrap_and_mode_interoperability() {
    let frame = noise(33, 17, 4);
    let mut encoder = Encoder::new(Mode::Batch, 32).unwrap();
    let mut decoder = Decoder::new();
    decoder
        .decode(&encoder.encode(&frame, u32::MAX, 0).unwrap())
        .unwrap();
    let empty = encoder.encode(&frame, 0, 11).unwrap();
    assert_eq!(get16(&empty, 16), 0);
    assert_eq!(get32(&empty, 20), 0);
    assert!(empty.len() > HEADER_BYTES); // Empty body still has one complete zlib stream.
    assert_eq!(expand_batch_packet(&empty).unwrap().len(), HEADER_BYTES);
    assert_eq!(decoder.decode(&empty).unwrap(), frame);
    assert_eq!(decoder.input_id(), 11);
    let old = Encoder::new(Mode::Deflate, 32)
        .unwrap()
        .encode(&frame, 9, 0)
        .unwrap();
    assert_eq!(decoder.decode(&old).unwrap(), frame);
    encoder.reset();
    assert_eq!(
        decoder
            .decode(&encoder.encode(&frame, 10, 0).unwrap())
            .unwrap(),
        frame
    );
}

fn batch_wrap(packet: &[u8]) -> Vec<u8> {
    let mut batch = packet[..HEADER_BYTES].to_vec();
    batch[..4].copy_from_slice(b"GLB1");
    batch[20..24].copy_from_slice(&((packet.len() - HEADER_BYTES) as u32).to_le_bytes());
    batch.extend(compress(&packet[HEADER_BYTES..]).unwrap());
    batch
}

#[test]
fn batch_corruption_bombs_trailers_and_nested_streams_preserve_state() {
    let first = noise(17, 19, 123);
    let mut encoder = Encoder::new(Mode::Batch, 32).unwrap();
    let first_packet = encoder.encode(&first, 0, 10).unwrap();
    let valid_delta = encoder.encode(&first, 1, 11).unwrap();
    let mut decoder = Decoder::new();
    decoder.decode(&first_packet).unwrap();
    let mut bad_packets = Vec::new();
    for removed in 1..=4 {
        bad_packets.push(first_packet[..first_packet.len() - removed].to_vec());
    }
    for field in [16, 18, 20, 23] {
        let mut bad = first_packet.clone();
        bad[field] ^= 255;
        bad_packets.push(bad);
    }
    let mut trailing = first_packet.clone();
    trailing.push(0);
    bad_packets.push(trailing);
    let mut concat = first_packet.clone();
    concat.extend(compress(b"second stream").unwrap());
    bad_packets.push(concat);
    let mut bomb = valid_delta[..HEADER_BYTES].to_vec();
    bomb.extend(compress(&vec![0; 1024 * 1024]).unwrap()); // Declares zero expanded bytes.
    bad_packets.push(bomb);
    for codec in [1, 2, 3, 4, 6] {
        let mut expanded = expand_batch_packet(&first_packet).unwrap().into_owned();
        expanded[32] = codec;
        bad_packets.push(batch_wrap(&expanded));
    }
    for bad in bad_packets {
        assert!(decoder.decode(&bad).is_err());
        assert_eq!(decoder.sequence(), Some(0));
        assert_eq!(decoder.input_id(), 10);
    }
    assert_eq!(decoder.decode(&valid_delta).unwrap(), first);
}

#[test]
fn batch_copy_overlap_resize_and_missing_sequence_recover() {
    let old = noise(96, 96, 20);
    let mut next = old.clone();
    for row in 0..89 {
        next.pixels[row * 96 * 4..(row + 1) * 96 * 4]
            .copy_from_slice(&old.pixels[(row + 7) * 96 * 4..(row + 8) * 96 * 4]);
    }
    let mut encoder = Encoder::new(Mode::Batch, 32).unwrap();
    let mut decoder = Decoder::new();
    decoder
        .decode(&encoder.encode(&old, 0, 0).unwrap())
        .unwrap();
    // Explicit non-grid, overlapping copy proves the batch envelope's codec5
    // semantics independently of the encoder's conservative copy search gate.
    let mut raw_delta = Vec::new();
    raw_delta.extend_from_slice(b"GLS1");
    put32(&mut raw_delta, 1);
    put32(&mut raw_delta, 1);
    put16(&mut raw_delta, 96);
    put16(&mut raw_delta, 96);
    put16(&mut raw_delta, 1);
    put16(&mut raw_delta, 0);
    put32(&mut raw_delta, 0);
    put16(&mut raw_delta, 0);
    put16(&mut raw_delta, 0);
    put16(&mut raw_delta, 96);
    put16(&mut raw_delta, 89);
    raw_delta.extend_from_slice(&[5, 0, 0, 0]);
    put32(&mut raw_delta, 4);
    put16(&mut raw_delta, 0);
    put16(&mut raw_delta, 7);
    let delta = batch_wrap(&raw_delta);
    let expanded = expand_batch_packet(&delta).unwrap();
    let mut offset = HEADER_BYTES;
    let mut copied = 0;
    for _ in 0..get16(&expanded, 16) {
        copied += usize::from(expanded[offset + 8] == 5);
        offset += RECT_HEADER_BYTES + get32(&expanded, offset + 12) as usize;
    }
    assert!(copied > 0);
    let mut raw_empty = raw_delta[..HEADER_BYTES].to_vec();
    raw_empty[4..8].copy_from_slice(&2u32.to_le_bytes());
    raw_empty[16..18].copy_from_slice(&0u16.to_le_bytes());
    let third = batch_wrap(&raw_empty);
    assert!(decoder.decode(&third).is_err());
    assert_eq!(decoder.decode(&delta).unwrap(), next);
    assert!(decoder.decode(&delta).is_err());
    assert_eq!(decoder.decode(&third).unwrap(), next);
    let resized = noise(1, 3, 6);
    assert_eq!(
        decoder
            .decode(&encoder.encode(&resized, 3, 3).unwrap())
            .unwrap(),
        resized
    );
}

#[test]
fn batch_inner_overlap_and_missing_keyframe_coverage_are_rejected() {
    let frame = Frame::new(16, 16).unwrap();
    let raw = Encoder::new(Mode::Raw, 16)
        .unwrap()
        .encode(&frame, 0, 0)
        .unwrap();
    let mut overlapping = raw.clone();
    overlapping[16..18].copy_from_slice(&2u16.to_le_bytes());
    overlapping.extend_from_slice(&raw[HEADER_BYTES..]);
    assert!(Decoder::new().decode(&batch_wrap(&overlapping)).is_err());
    let mut missing = raw.clone();
    missing[30..32].copy_from_slice(&15u16.to_le_bytes());
    missing.truncate(40 + 16 * 15 * 4);
    missing[36..40].copy_from_slice(&(16_u32 * 15 * 4).to_le_bytes());
    assert!(Decoder::new().decode(&batch_wrap(&missing)).is_err());
}

#[test]
fn batch_vectors_append_without_changing_prior_interoperability_records() {
    let previous: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/experiments/pre-batch-fixtures-v2.json"
    ))
    .unwrap();
    let current: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/fixtures.json"
    ))
    .unwrap();
    let previous = previous.as_array().unwrap();
    let current = current.as_array().unwrap();
    assert_eq!(&current[..previous.len()], previous);
    assert!(current.len() >= previous.len() + 7);
    assert!(
        current[previous.len()..previous.len() + 7]
            .iter()
            .all(|record| { record["mode"].as_str().unwrap().contains("batch") })
    );
    let mut decoder = Decoder::new();
    for record in current {
        if record["sequence"].as_u64().unwrap() == 0 {
            decoder.reset();
        }
        let packet = unhex(record["packet_hex"].as_str().unwrap());
        let expected = Frame {
            width: record["width"].as_u64().unwrap() as u16,
            height: record["height"].as_u64().unwrap() as u16,
            pixels: unhex(record["expected_pixels_hex"].as_str().unwrap()),
        };
        assert_eq!(decoder.decode(&packet).unwrap(), expected);
    }
}

fn global_packet(
    width: u16,
    height: u16,
    sequence: u32,
    keyframe: bool,
    codec: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut header = b"GLB2".to_vec();
    put32(&mut header, sequence);
    put32(&mut header, 10);
    put16(&mut header, width);
    put16(&mut header, height);
    put16(&mut header, 1);
    put16(&mut header, u16::from(keyframe));
    put32(&mut header, (RECT_HEADER_BYTES + payload.len()) as u32);
    let mut body = vec![0; 4];
    put16(&mut body, width);
    put16(&mut body, height);
    body.extend_from_slice(&[codec, 0, 0, 0]);
    put32(&mut body, payload.len() as u32);
    body.extend_from_slice(payload);
    header.extend(compress(&body).unwrap());
    header
}
fn global_rewrap(expanded: &[u8]) -> Vec<u8> {
    let mut packet = expanded[..HEADER_BYTES].to_vec();
    packet[..4].copy_from_slice(b"GLB2");
    packet[20..24].copy_from_slice(&((expanded.len() - HEADER_BYTES) as u32).to_le_bytes());
    packet.extend(compress(&expanded[HEADER_BYTES..]).unwrap());
    packet
}

#[test]
fn global_independent_fill_palette_xor_copy_and_sequence_recovery() {
    let mut decoder = Decoder::new();
    let solid = [13, 27, 249, 255];
    let first = Frame {
        width: 2,
        height: 3,
        pixels: solid.repeat(6),
    };
    assert_eq!(
        decoder
            .decode(&global_packet(2, 3, u32::MAX, true, 6, &solid))
            .unwrap(),
        first
    );
    let colors = [[11, 31, 191, 255], [231, 219, 217, 255]];
    let mut palette = vec![2, 0];
    palette.extend(colors.concat());
    palette.extend([0, 1, 0, 1, 0, 1]);
    let patterned = Frame {
        width: 2,
        height: 3,
        pixels: [
            colors[0], colors[1], colors[0], colors[1], colors[0], colors[1],
        ]
        .concat(),
    };
    assert_eq!(
        decoder
            .decode(&global_packet(2, 3, 0, false, 2, &palette))
            .unwrap(),
        patterned
    );
    let mut next = patterned.clone();
    next.pixels[0] ^= 5;
    let xor: Vec<_> = next
        .pixels
        .iter()
        .zip(&patterned.pixels)
        .map(|(a, b)| a ^ b)
        .collect();
    let temporal = global_packet(2, 3, 1, false, 7, &xor);
    let empty = Encoder::new(Mode::Global, 16)
        .unwrap()
        .encode(&next, 10, 0)
        .unwrap();
    assert!(
        decoder
            .decode(&global_packet(2, 3, 2, false, 7, &xor))
            .is_err()
    );
    assert_eq!(decoder.decode(&temporal).unwrap(), next);
    assert!(decoder.decode(&temporal).is_err());
    // Source overlaps destination and must be read from the immutable old frame.
    let copy = global_packet(2, 3, 2, false, 5, &[0, 0, 1, 0]);
    let mut expanded = expand_batch_packet(&copy).unwrap().into_owned();
    expanded[30..32].copy_from_slice(&2u16.to_le_bytes());
    let mut copied = next.clone();
    copied.pixels[..16].copy_from_slice(&next.pixels[8..24]);
    assert_eq!(decoder.decode(&global_rewrap(&expanded)).unwrap(), copied);
    assert_eq!(decoder.decode(&empty).unwrap(), next); // Independent keyframe recovers.
    let resized = global_packet(1, 1, 11, true, 6, &solid);
    assert_eq!(decoder.decode(&resized).unwrap().pixels, solid);
}

#[test]
fn global_malformed_transforms_palettes_and_envelopes_are_atomic() {
    let first = noise(2, 2, 9);
    let good = global_packet(2, 2, 0, true, 0, &first.pixels);
    let mut decoder = Decoder::new();
    decoder.decode(&good).unwrap();
    let mut invalid = Vec::new();
    for codec in [1, 3, 4, 8, 255] {
        invalid.push(global_packet(2, 2, 1, true, codec, &first.pixels));
    }
    invalid.push(global_packet(2, 2, 1, true, 7, &first.pixels));
    for size in [0, 3, 5] {
        invalid.push(global_packet(2, 2, 1, false, 6, &vec![0; size]));
    }
    invalid.push(global_packet(2, 2, 1, false, 7, &[0; 4]));
    let mut palette = vec![2, 0, 1, 2, 3, 255, 9, 8, 7, 255];
    for indices in [[0, 0, 0, 0], [0, 1, 0, 2]] {
        let mut body = palette.clone();
        body.extend(indices);
        invalid.push(global_packet(2, 2, 1, true, 2, &body));
    }
    palette.extend([0, 1, 0, 1]);
    let mut extra = palette.clone();
    extra.push(0);
    invalid.push(global_packet(2, 2, 1, true, 2, &extra));
    for count in [0u16, 1, 257] {
        let mut body = palette.clone();
        body[..2].copy_from_slice(&count.to_le_bytes());
        invalid.push(global_packet(2, 2, 1, true, 2, &body));
    }
    let bounded = global_packet(2, 2, 1, true, 2, &palette);
    let mut narrow = expand_batch_packet(&bounded).unwrap().into_owned();
    narrow[28..32].copy_from_slice(&[1, 0, 1, 0]);
    let error = decoder.decode(&global_rewrap(&narrow)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("global transform exceeds raw tile size")
    );
    for removed in 1..=4 {
        invalid.push(good[..good.len() - removed].to_vec());
    }
    let mut trailing = good.clone();
    trailing.push(0);
    invalid.push(trailing);
    let mut concatenated = good.clone();
    concatenated.extend(compress(b"second").unwrap());
    invalid.push(concatenated);
    for field in [16, 18, 20, 23] {
        let mut bad = good.clone();
        bad[field] ^= 255;
        invalid.push(bad);
    }
    let mut bomb = good[..HEADER_BYTES].to_vec();
    bomb[16..18].fill(0);
    bomb[20..24].fill(0);
    bomb.extend(compress(&vec![0; 10000]).unwrap());
    invalid.push(bomb);
    for bad in invalid {
        assert!(decoder.decode(&bad).is_err());
        assert_eq!(decoder.sequence(), Some(0));
        assert_eq!(decoder.input_id(), 10);
        assert_eq!(decoder.previous.as_ref(), Some(&first));
    }
}

#[test]
fn global_transform_ids_cannot_escape_to_prior_wire_versions() {
    for codec in [6, 7] {
        let global = global_packet(2, 2, 0, true, codec, &[0, 0, 0, 255]);
        let mut expanded = expand_batch_packet(&global).unwrap().into_owned();
        expanded[..4].copy_from_slice(b"GLS1");
        assert!(Decoder::new().decode(&expanded).is_err());
        assert!(Decoder::new().decode(&batch_wrap(&expanded)).is_err());
    }
}

#[test]
fn global_vectors_preserve_all_prior_packets_and_cover_every_transform() {
    let previous: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/experiments/pre-global-fixtures-v3.json"
    ))
    .unwrap();
    let current: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/fixtures.json"
    ))
    .unwrap();
    let previous = previous.as_array().unwrap();
    let current = current.as_array().unwrap();
    assert_eq!(previous.len(), 49);
    assert_eq!(&current[..previous.len()], previous);
    assert!(current.len() >= previous.len() + 12);
    let mut seen = [false; 8];
    let mut decoder = Decoder::new();
    for record in &current[previous.len()..previous.len() + 12] {
        if record["sequence"].as_u64().unwrap() == 0 {
            decoder.reset();
        }
        let packet = unhex(record["packet_hex"].as_str().unwrap());
        assert_eq!(&packet[..4], b"GLB2");
        let expected = Frame {
            width: record["width"].as_u64().unwrap() as u16,
            height: record["height"].as_u64().unwrap() as u16,
            pixels: unhex(record["expected_pixels_hex"].as_str().unwrap()),
        };
        assert_eq!(decoder.decode(&packet).unwrap(), expected);
        let expanded = expand_batch_packet(&packet).unwrap();
        let mut offset = HEADER_BYTES;
        for _ in 0..get16(&expanded, 16) {
            seen[expanded[offset + 8] as usize] = true;
            offset += RECT_HEADER_BYTES + get32(&expanded, offset + 12) as usize;
        }
    }
    for codec in [0, 2, 5, 6, 7] {
        assert!(seen[codec]);
    }
    for codec in [1, 3, 4] {
        assert!(!seen[codec]);
    }
}

fn sparse_packet(
    width: u16,
    height: u16,
    sequence: u32,
    keyframe: bool,
    codec: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut packet = global_packet(width, height, sequence, keyframe, codec, payload);
    packet[..4].copy_from_slice(b"GLB3");
    packet
}
fn sparse_body(entries: &[(u16, [u8; 4])]) -> Vec<u8> {
    let mut body = Vec::new();
    put16(&mut body, entries.len() as u16);
    for (index, color) in entries {
        put16(&mut body, *index);
        body.extend_from_slice(color);
    }
    body
}

fn independent_sparse_rectangles(
    width: u16,
    height: u16,
    sequence: u32,
    input_id: u32,
    keyframe: bool,
    rectangles: &[(Rect, u8, Vec<u8>)],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (rect, codec, payload) in rectangles {
        for value in [rect.x, rect.y, rect.w, rect.h] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(&[*codec, 0, 0, 0]);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
    }
    let mut packet = b"GLB3".to_vec();
    packet.extend_from_slice(&sequence.to_le_bytes());
    packet.extend_from_slice(&input_id.to_le_bytes());
    for value in [width, height, rectangles.len() as u16, u16::from(keyframe)] {
        packet.extend_from_slice(&value.to_le_bytes());
    }
    packet.extend_from_slice(&(body.len() as u32).to_le_bytes());
    packet.extend(compress(&body).unwrap());
    packet
}

#[test]
fn sparse_mixed_temporal_rectangles_read_immutable_previous_pixels() {
    let first = Frame {
        width: 8,
        height: 2,
        pixels: (0u8..16)
            .flat_map(|index| [index * 11 + 3, index * 7 + 13, index * 5 + 29, 255])
            .collect(),
    };
    let full = Rect {
        x: 0,
        y: 0,
        w: 8,
        h: 2,
    };
    let patch_rect = Rect { w: 2, ..full };
    let xor_rect = Rect { x: 2, ..patch_rect };
    let copy_rect = Rect { x: 4, w: 4, ..full };
    let replacements = [(0, [231, 17, 93, 255]), (3, [87, 211, 43, 255])];
    let xor_colors = [
        [11, 71, 193, 255],
        [47, 223, 101, 255],
        [181, 19, 37, 255],
        [29, 109, 229, 255],
    ];
    let mut expected = first.clone();
    expected.pixels[..4].copy_from_slice(&replacements[0].1);
    expected.pixels[9 * 4..10 * 4].copy_from_slice(&replacements[1].1);
    let mut xor = Vec::new();
    for (local, color) in xor_colors.iter().enumerate() {
        let index = (local / 2) * 8 + 2 + local % 2;
        xor.extend(
            first.pixels[index * 4..index * 4 + 4]
                .iter()
                .zip(color)
                .map(|(old, current)| old ^ current),
        );
        expected.pixels[index * 4..index * 4 + 4].copy_from_slice(color);
    }
    for row in 0..2 {
        expected.pixels[(row * 8 + 4) * 4..(row * 8 + 8) * 4]
            .copy_from_slice(&first.pixels[row * 8 * 4..(row * 8 + 4) * 4]);
    }
    // Copy is deliberately last and reads both previously patched/XORed regions.
    let rectangles = vec![
        (patch_rect, 8, sparse_body(&replacements)),
        (xor_rect, 7, xor),
        (copy_rect, 5, vec![0, 0, 0, 0]),
    ];
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder
            .decode(&independent_sparse_rectangles(
                8,
                2,
                0,
                40,
                true,
                &[(full, 0, first.pixels.clone())],
            ))
            .unwrap(),
        first
    );
    assert_eq!(
        decoder
            .decode(&independent_sparse_rectangles(
                8,
                2,
                1,
                41,
                false,
                &rectangles,
            ))
            .unwrap(),
        expected
    );
    let mut malformed = rectangles.clone();
    malformed[2].2[0] = 5; // Source x + width exceeds the frame after other tiles.
    assert!(
        decoder
            .decode(&independent_sparse_rectangles(
                8, 2, 2, 99, false, &malformed,
            ))
            .is_err()
    );
    assert_eq!(decoder.previous.as_ref(), Some(&expected));
    assert_eq!(decoder.sequence(), Some(1));
    assert_eq!(decoder.input_id(), 41);
    assert_eq!(
        decoder
            .decode(&independent_sparse_rectangles(8, 2, 2, 42, false, &[]))
            .unwrap(),
        expected
    );
    assert_eq!(decoder.input_id(), 42);
}

#[test]
fn sparse_maximum_raw_bounded_count_and_late_errors_are_atomic() {
    let base = [13, 27, 249, 255];
    let entries: Vec<_> = (0..43_690u16)
        .map(|index| (index, [index as u8, (index >> 8) as u8, 199, 255]))
        .collect();
    let legal = sparse_body(&entries);
    assert_eq!(legal.len(), 256 * 256 * 4 - 2);
    let mut excessive_entries = entries.clone();
    excessive_entries.push((43_690, [41, 109, 17, 255]));
    let excessive = sparse_body(&excessive_entries);
    assert_eq!(excessive.len(), 256 * 256 * 4 + 4);
    let mut late_duplicate = legal.clone();
    for entry in late_duplicate[2..].as_chunks_mut::<6>().0 {
        entry[2..].copy_from_slice(&[77, 101, 231, 255]);
    }
    let final_offset = 2 + (entries.len() - 1) * 6;
    late_duplicate[final_offset..final_offset + 2].copy_from_slice(&43_688u16.to_le_bytes());
    let tile = Rect {
        x: 0,
        y: 0,
        w: 256,
        h: 256,
    };
    // Width 257 leaves room in the outer expansion bound, exercising the
    // rectangle's raw-size limit independently of the whole-body size guard.
    for width in [256u16, 257] {
        let first = Frame {
            width,
            height: 256,
            pixels: base.repeat(usize::from(width) * 256),
        };
        let mut expected = first.clone();
        for (index, color) in &entries {
            let index = usize::from(*index);
            let destination = ((index / 256) * usize::from(width) + index % 256) * 4;
            expected.pixels[destination..destination + 4].copy_from_slice(color);
        }
        let mut decoder = Decoder::new();
        decoder
            .decode(&independent_sparse_rectangles(
                width,
                256,
                0,
                50,
                true,
                &[(Rect { w: width, ..tile }, 6, base.to_vec())],
            ))
            .unwrap();
        assert_eq!(
            decoder
                .decode(&independent_sparse_rectangles(
                    width,
                    256,
                    1,
                    51,
                    false,
                    &[(tile, 8, legal.clone())],
                ))
                .unwrap(),
            expected
        );
        for payload in [&excessive, &late_duplicate] {
            assert!(
                decoder
                    .decode(&independent_sparse_rectangles(
                        width,
                        256,
                        2,
                        99,
                        false,
                        &[(tile, 8, payload.clone())],
                    ))
                    .is_err()
            );
            assert_eq!(decoder.previous.as_ref(), Some(&expected));
            assert_eq!(decoder.sequence(), Some(1));
            assert_eq!(decoder.input_id(), 51);
        }
        assert_eq!(
            decoder
                .decode(&independent_sparse_rectangles(
                    width,
                    256,
                    2,
                    52,
                    false,
                    &[],
                ))
                .unwrap(),
            expected
        );
        assert_eq!(decoder.input_id(), 52);
    }
}

#[test]
fn sparse_encoder_collection_limit_does_not_omit_unsampled_edits() {
    let first = noise(64, 64, 21);
    let positions: Vec<_> = (0..4096)
        .filter(|index| index % 64 != 0)
        .take(129)
        .collect();
    for count in [128, 129] {
        let mut next = first.clone();
        for &index in &positions[..count] {
            next.pixels[index * 4] ^= 1;
        }
        let mut encoder = Encoder::new(Mode::Sparse, 64).unwrap();
        let mut decoder = Decoder::new();
        decoder
            .decode(&encoder.encode(&first, 0, 0).unwrap())
            .unwrap();
        let packet = encoder.encode(&next, 1, 1).unwrap();
        let expanded = expand_batch_packet(&packet).unwrap();
        if count == 128 {
            assert_eq!(expanded[32], 8);
            assert_eq!(get16(&expanded, 40), 128);
        } else {
            assert_ne!(expanded[32], 8); // Every eligibility sample missed these129 edits.
        }
        assert_eq!(decoder.decode(&packet).unwrap(), next);
    }
    let first = noise(256, 256, 22);
    let mut last = first.clone();
    last.pixels[65535 * 4] ^= 1;
    let mut encoder = Encoder::new(Mode::Sparse, 256).unwrap();
    let mut decoder = Decoder::new();
    decoder
        .decode(&encoder.encode(&first, 0, 0).unwrap())
        .unwrap();
    let packet = encoder.encode(&last, 1, 0).unwrap();
    let expanded = expand_batch_packet(&packet).unwrap();
    assert_eq!(expanded[32], 8);
    assert_eq!(get16(&expanded, 42), 65535);
    assert_eq!(decoder.decode(&packet).unwrap(), last);
}
#[test]
fn sparse_independent_packets_errors_and_recovery_are_atomic() {
    let base = [13, 27, 249, 255];
    let first = Frame {
        width: 4,
        height: 3,
        pixels: base.repeat(12),
    };
    let initial = sparse_packet(4, 3, 0, true, 6, &base);
    let valid_body = sparse_body(&[(0, [1, 2, 3, 255]), (11, [19, 31, 73, 255])]);
    let mut next = first.clone();
    next.pixels[..4].copy_from_slice(&[1, 2, 3, 255]);
    next.pixels[44..48].copy_from_slice(&[19, 31, 73, 255]);
    let valid = sparse_packet(4, 3, 1, false, 8, &valid_body);
    let mut decoder = Decoder::new();
    decoder.decode(&initial).unwrap();
    let mut bad_bodies = vec![vec![0, 0], valid_body[..8].to_vec()];
    let mut extra = valid_body.clone();
    extra.push(0);
    bad_bodies.push(extra);
    for count in [0u16, 3, 65535] {
        let mut body = valid_body.clone();
        body[..2].copy_from_slice(&count.to_le_bytes());
        bad_bodies.push(body);
    }
    for indices in [[0u16, 0], [3, 2], [0, 12], [0, 65535]] {
        bad_bodies.push(sparse_body(&[(indices[0], base), (indices[1], base)]));
    }
    let mut invalid: Vec<_> = bad_bodies
        .into_iter()
        .map(|body| sparse_packet(4, 3, 1, false, 8, &body))
        .collect();
    invalid.push(sparse_packet(4, 3, 1, true, 8, &valid_body));
    for codec in [1, 3, 4, 9, 255] {
        invalid.push(sparse_packet(4, 3, 1, false, codec, &first.pixels));
    }
    let oversized = sparse_packet(4, 3, 1, false, 8, &valid_body);
    let mut expanded = expand_batch_packet(&oversized).unwrap().into_owned();
    expanded[28..32].copy_from_slice(&[2, 0, 1, 0]);
    let mut bounded = global_rewrap(&expanded);
    bounded[..4].copy_from_slice(b"GLB3");
    invalid.push(bounded);
    for bad in invalid {
        assert!(decoder.decode(&bad).is_err());
        assert_eq!(decoder.sequence(), Some(0));
        assert_eq!(decoder.input_id(), 10);
        assert_eq!(decoder.previous.as_ref(), Some(&first));
    }
    assert!(
        decoder
            .decode(&sparse_packet(4, 3, 2, false, 8, &valid_body))
            .is_err()
    );
    assert_eq!(decoder.decode(&valid).unwrap(), next);
    assert!(decoder.decode(&valid).is_err());
    assert_eq!(
        decoder
            .decode(&sparse_packet(4, 3, 10, true, 6, &base))
            .unwrap(),
        first
    );
    assert_eq!(
        decoder
            .decode(&sparse_packet(1, 1, 11, true, 6, &base))
            .unwrap()
            .pixels,
        base
    );
    let large = sparse_packet(257, 256, 0, true, 6, &base);
    decoder.reset();
    decoder.decode(&large).unwrap();
    assert!(
        decoder
            .decode(&sparse_packet(
                257,
                256,
                1,
                false,
                8,
                &sparse_body(&[(0, base)])
            ))
            .is_err()
    );
    assert_eq!(decoder.sequence(), Some(0));
}
#[test]
fn sparse_patch_id_is_rejected_in_every_prior_context() {
    let base = [13, 27, 249, 255];
    let body = sparse_body(&[(0, [1, 2, 3, 255])]);
    let patch = sparse_packet(4, 3, 1, false, 8, &body);
    let mut expanded = expand_batch_packet(&patch).unwrap().into_owned();
    expanded[..4].copy_from_slice(b"GLS1");
    for prior in [
        expanded.clone(),
        batch_wrap(&expanded),
        global_rewrap(&expanded),
    ] {
        let mut decoder = Decoder::new();
        decoder
            .decode(&global_packet(4, 3, 0, true, 6, &base))
            .unwrap();
        assert!(decoder.decode(&prior).is_err());
        assert_eq!(decoder.sequence(), Some(0));
    }
}
#[test]
fn sparse_vectors_append_without_changing_the_first_sixty_one_records() {
    let previous: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/experiments/pre-sparse-fixtures-v4.json"
    ))
    .unwrap();
    let current: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/fixtures.json"
    ))
    .unwrap();
    let previous = previous.as_array().unwrap();
    let current = current.as_array().unwrap();
    assert_eq!(previous.len(), 61);
    assert_eq!(&current[..previous.len()], previous);
    assert_eq!(current.len() - previous.len(), 12);
    let mut seen = [false; 9];
    let mut decoder = Decoder::new();
    for record in &current[previous.len()..] {
        if record["sequence"].as_u64().unwrap() == 0 {
            decoder.reset();
        }
        let packet = unhex(record["packet_hex"].as_str().unwrap());
        assert_eq!(&packet[..4], b"GLB3");
        let expected = Frame {
            width: record["width"].as_u64().unwrap() as u16,
            height: record["height"].as_u64().unwrap() as u16,
            pixels: unhex(record["expected_pixels_hex"].as_str().unwrap()),
        };
        assert_eq!(decoder.decode(&packet).unwrap(), expected);
        let expanded = expand_batch_packet(&packet).unwrap();
        let mut offset = HEADER_BYTES;
        for _ in 0..get16(&expanded, 16) {
            seen[expanded[offset + 8] as usize] = true;
            offset += RECT_HEADER_BYTES + get32(&expanded, offset + 12) as usize;
        }
    }
    for codec in [0, 2, 5, 6, 7, 8] {
        assert!(seen[codec]);
    }
    for codec in [1, 3, 4] {
        assert!(!seen[codec]);
    }
}

struct GlobalPair {
    ordinary: Encoder,
    cached: Encoder,
    decoder: Decoder,
}
impl GlobalPair {
    fn new(tile_size: u16) -> Self {
        Self {
            ordinary: Encoder::new(Mode::Global, tile_size).unwrap(),
            cached: Encoder::new("global-cached".parse().unwrap(), tile_size).unwrap(),
            decoder: Decoder::new(),
        }
    }
    fn compare(&mut self, frame: &Frame, sequence: u32, context: &str) -> Vec<u8> {
        let input_id = sequence.wrapping_mul(17);
        let expected = self.ordinary.encode(frame, sequence, input_id).unwrap();
        let actual = self.cached.encode(frame, sequence, input_id).unwrap();
        assert_eq!(
            actual, expected,
            "Global packet changed: {context}, sequence={sequence}"
        );
        assert_eq!(&actual[..4], b"GLB2");
        assert_eq!(
            self.decoder.decode(&actual).unwrap(),
            *frame,
            "pixels changed: {context}"
        );
        assert_eq!(self.decoder.input_id(), input_id);
        actual
    }
    fn reset(&mut self) {
        self.ordinary.reset();
        self.cached.reset();
    }
}
fn cache_pattern(width: u16, height: u16, seed: u64) -> Frame {
    let mut frame = noise(width, height, seed);
    // Shared background keeps the unchanged temporal gate eligible, while
    // independently textured tile halves distinguish exact copy sources.
    for y in 0..usize::from(height) {
        if y % 32 < 16 {
            for pixel in frame.pixels[y * usize::from(width) * 4..(y + 1) * usize::from(width) * 4]
                .as_chunks_mut::<4>()
                .0
            {
                *pixel = [23, 29, 31, 255];
            }
        }
    }
    frame
}
fn cycle_columns(frame: &Frame, step: usize) -> Frame {
    assert!(frame.width >= 128);
    let mut next = frame.clone();
    for y in 0..usize::from(frame.height) {
        for x in 0..128 {
            let source_x = ((x / 32 + step) % 4) * 32 + x % 32;
            let destination = (y * usize::from(frame.width) + x) * 4;
            let source = (y * usize::from(frame.width) + source_x) * 4;
            next.pixels[destination..destination + 4]
                .copy_from_slice(&frame.pixels[source..source + 4]);
        }
    }
    next
}
fn warm_global_pair() -> (GlobalPair, Frame) {
    let first = cache_pattern(160, 97, 0x68d3);
    let next = cycle_columns(&first, 1);
    let mut pair = GlobalPair::new(32);
    pair.compare(&first, 10, "cold reference");
    let packet = pair.compare(&next, 11, "warm horizontal copies");
    let expanded = expand_batch_packet(&packet).unwrap();
    let mut offset = HEADER_BYTES;
    let mut copies = 0;
    for _ in 0..get16(&expanded, 16) {
        copies += usize::from(expanded[offset + 8] == 5);
        offset += RECT_HEADER_BYTES + get32(&expanded, offset + 12) as usize;
    }
    assert!(
        copies > 0,
        "cache invalidation tests must exercise copy lookup"
    );
    let cache = pair.cached.copy_hashes.as_ref().unwrap();
    assert!(cache.rows.is_some());
    assert!(cache.tiles.iter().any(Option::is_some));
    (pair, next)
}
fn poison_hash_values(encoder: &mut Encoder) {
    let cache = encoder.copy_hashes.as_mut().unwrap();
    cache.tiles.fill(Some(0));
    cache.rows.as_mut().unwrap().fill(0);
}
fn assert_cold_hashes(encoder: &Encoder, sequence: u32) {
    let cache = encoder.copy_hashes.as_ref().unwrap();
    assert_eq!(cache.sequence, sequence);
    assert!(cache.rows.is_none());
    assert!(cache.tiles.iter().all(Option::is_none));
}

#[test]
fn global_cached_packets_match_global_for_all_frozen_desktop_workloads() {
    assert_eq!(Mode::GlobalCached.name(), "global-cached");
    for workload in cached_workloads::suite() {
        let context = format!(
            "{} version={} seed={:#x} fps={} held_out={} fingerprint={}",
            workload.name,
            cached_workloads::VERSION,
            cached_workloads::SEED,
            cached_workloads::FPS,
            workload.held_out,
            cached_workloads::fingerprint(&workload.frames)
        );
        assert_eq!(workload.frames.len(), cached_workloads::FRAME_COUNT);
        let mut pair = GlobalPair::new(64);
        for (sequence, frame) in workload.frames.iter().enumerate() {
            pair.compare(frame, sequence as u32, &context);
        }
    }
}

#[test]
fn global_cached_matches_preexisting_global_vector_bytes() {
    let records: serde_json::Value = serde_json::from_str(include_str!(
        "../../../test-support/rust/codec/fixtures.json"
    ))
    .unwrap();
    let mut ordinary = Encoder::new(Mode::Global, 16).unwrap();
    let mut cached = Encoder::new(Mode::GlobalCached, 16).unwrap();
    let mut decoder = Decoder::new();
    let mut count = 0;
    for record in records
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["mode"] == "global")
    {
        let sequence = record["sequence"].as_u64().unwrap() as u32;
        if sequence == 0 {
            ordinary.reset();
            cached.reset();
            decoder.reset();
        }
        let expected = unhex(record["packet_hex"].as_str().unwrap());
        let input_id = get32(&expected, 8);
        let frame = Frame {
            width: record["width"].as_u64().unwrap() as u16,
            height: record["height"].as_u64().unwrap() as u16,
            pixels: unhex(record["expected_pixels_hex"].as_str().unwrap()),
        };
        assert_eq!(
            ordinary.encode(&frame, sequence, input_id).unwrap(),
            expected
        );
        assert_eq!(cached.encode(&frame, sequence, input_id).unwrap(), expected);
        assert_eq!(decoder.decode(&expected).unwrap(), frame);
        count += 1;
    }
    // The other six Global fixtures force decoder transforms independently of
    // normal encoder selection; the existing transform tests cover those.
    assert_eq!(count, 6);
}

#[test]
fn global_cached_packets_match_random_partial_tiles_scrolling_and_static_traces() {
    for tile_size in [16, 32, 64, 127, 256] {
        let mut pair = GlobalPair::new(tile_size);
        let mut frame = cache_pattern(193, 133, u64::from(tile_size) + 0x8ab1);
        for sequence in 0..40u32 {
            match sequence % 8 {
                0 => frame = cache_pattern(frame.width, frame.height, u64::from(sequence) + 0xad47),
                1 | 2 => frame = cycle_columns(&frame, 1),
                3 => {
                    let width = usize::from(frame.width);
                    for (x, y) in [
                        (0, 0),
                        (63, 31),
                        (128, 64),
                        (width - 1, usize::from(frame.height) - 1),
                    ] {
                        let offset = (y * width + x) * 4;
                        frame.pixels[offset..offset + 4].copy_from_slice(&[
                            sequence as u8,
                            7,
                            13,
                            255,
                        ]);
                    }
                }
                4 => {
                    let old = frame.clone();
                    let row_bytes = usize::from(frame.width) * 4;
                    let height = usize::from(frame.height);
                    frame.pixels[..(height - 7) * row_bytes]
                        .copy_from_slice(&old.pixels[7 * row_bytes..]);
                }
                5 => {}
                6 => frame = noise(frame.width, frame.height, u64::from(sequence) + 0x711b),
                _ => frame = cache_pattern(197, 135, u64::from(sequence) + 0x42a9),
            }
            pair.compare(&frame, sequence, &format!("random trace tile={tile_size}"));
        }
    }
}

#[test]
fn global_cached_reset_gap_and_resize_discard_every_prior_hash() {
    for trigger in ["reset", "gap", "resize"] {
        let (mut pair, old) = warm_global_pair();
        poison_hash_values(&mut pair.cached);
        let (next, sequence) = match trigger {
            "reset" => {
                pair.reset();
                assert!(pair.cached.copy_hashes.is_none());
                (cycle_columns(&old, 1), 90)
            }
            "gap" => (cycle_columns(&old, 1), 90),
            _ => (cache_pattern(161, 99, 0x971e), 12),
        };
        let packet = pair.compare(&next, sequence, trigger);
        assert_eq!(get16(&packet, 18), 1);
        assert_cold_hashes(&pair.cached, sequence);
        pair.compare(
            &cycle_columns(&next, 1),
            sequence + 1,
            "first delta after cache invalidation",
        );
    }
}

#[test]
fn global_cached_sequence_wrap_discards_hashes_without_changing_delta_semantics() {
    let mut pair = GlobalPair::new(32);
    let mut frame = cache_pattern(160, 97, 0x683d);
    for sequence in [u32::MAX - 2, u32::MAX - 1, u32::MAX] {
        pair.compare(&frame, sequence, "before sequence wrap");
        frame = cycle_columns(&frame, 1);
    }
    poison_hash_values(&mut pair.cached);
    let packet = pair.compare(&frame, 0, "wrap remains a delta with cold hash discovery");
    assert_eq!(get16(&packet, 18), 0);
    assert_eq!(pair.cached.copy_hashes.as_ref().unwrap().sequence, 0);
    pair.compare(&cycle_columns(&frame, 1), 1, "after sequence wrap");
}

#[test]
fn global_cached_mismatched_reference_metadata_cannot_reuse_hash_values() {
    for field in 0..6 {
        let (mut pair, frame) = warm_global_pair();
        poison_hash_values(&mut pair.cached);
        let cache = pair.cached.copy_hashes.as_mut().unwrap();
        match field {
            0 => cache.sequence = 99,
            1 => cache.width = 1,
            2 => cache.height = 1,
            3 => cache.tile_size = 16,
            4 => {
                cache.tiles.pop();
            }
            _ => {
                cache.rows.as_mut().unwrap().pop();
            }
        }
        pair.compare(
            &cycle_columns(&frame, 1),
            12,
            &format!("mismatched cache metadata field={field}"),
        );
    }
}

#[test]
fn global_cached_owns_its_reference_and_failed_encode_preserves_cache_and_sequence() {
    let (mut pair, mut caller_buffer) = warm_global_pair();
    let owned = pair.cached.previous.clone().unwrap();
    let cache = pair.cached.copy_hashes.clone();
    caller_buffer.pixels[..4].copy_from_slice(&[241, 239, 233, 255]);
    assert_eq!(pair.cached.previous.as_ref().unwrap(), &owned);
    assert_eq!(pair.cached.copy_hashes, cache);
    let mut malformed = caller_buffer.clone();
    malformed.pixels.pop();
    assert!(pair.cached.encode(&malformed, 12, 44).is_err());
    assert_eq!(pair.cached.sequence, Some(11));
    assert_eq!(pair.cached.previous.as_ref().unwrap(), &owned);
    assert_eq!(pair.cached.copy_hashes, cache);
    pair.compare(
        &caller_buffer,
        12,
        "caller mutation affects only the next source frame",
    );
    pair.compare(
        &cycle_columns(&caller_buffer, 1),
        13,
        "lookup after a gate-rejected sparse edit",
    );
}
