use super::*;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

// An independent copy of the encoder at its fixed two-second keyframe interval, never used at runtime.
#[allow(dead_code)]
mod frozen_screen {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-support/rust/video/frozen-default-video.rs"
    ));
}

fn scene(width: u16, height: u16, step: usize) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    for (index, pixel) in frame.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = index % usize::from(width);
        let y = index / usize::from(width);
        let light = if (x + step) % 32 < 16 { 210 } else { 35 };
        pixel.copy_from_slice(&[light, (y * 160 / usize::from(height)) as u8, light, 255]);
    }
    frame
}

#[test]
fn software_encode_decode_metadata_and_quality() {
    let mut encoder = VideoEncoder::new(96, 64, 30).unwrap();
    let mut decoder = Decoder::new().unwrap();
    for step in 0..4 {
        let frame = scene(96, 64, step * 2);
        let bytes = encoder
            .encode(&frame, step as u32, 100 + step as u32)
            .unwrap();
        let packet = parse_packet(&bytes).unwrap();
        assert_eq!((packet.width, packet.height), (96, 64));
        assert_eq!(
            (packet.sequence, packet.input_id),
            (step as u32, 100 + step as u32)
        );
        assert_eq!(packet.keyframe, step == 0);
        let decoded = decoder
            .decode(packet.payload)
            .unwrap()
            .expect("each full AU yields a frame without flushing");
        assert_eq!(decoded.dimensions(), (96, 64));
        let mut pixels = vec![0; frame.pixels.len()];
        decoded.write_rgba8(&mut pixels);
        let error: u64 = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(frame.pixels.as_chunks::<4>().0.iter())
            .flat_map(|(actual, expected)| {
                (0..3).map(move |c| u64::from(actual[c].abs_diff(expected[c])))
            })
            .sum();
        let mean_error = error as f64 / (96.0 * 64.0 * 3.0);
        assert!(
            mean_error < 12.0,
            "decoded image mean RGB error: {mean_error}"
        );
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255)
        );
    }
}

#[test]
fn reset_keyframes_decode_without_previous_stream_state() {
    let mut encoder = VideoEncoder::new(64, 48, 30).unwrap();
    encoder.encode(&scene(64, 48, 0), 1, 0).unwrap();
    encoder.encode(&scene(64, 48, 2), 2, 0).unwrap();
    encoder.reset();
    let bytes = encoder.encode(&scene(64, 48, 4), 3, 12).unwrap();
    let packet = parse_packet(&bytes).unwrap();
    assert!(packet.keyframe);
    let mut decoder = Decoder::new().unwrap();
    let decoded = decoder.decode(packet.payload).unwrap().unwrap();
    assert_eq!(decoded.dimensions(), (64, 48));
}

#[test]
fn resize_rebuilds_with_new_independent_sps_pps() {
    let mut encoder = VideoEncoder::new(64, 48, 30).unwrap();
    let first = encoder.encode(&scene(64, 48, 0), 1, 0).unwrap();
    let second = encoder.encode(&scene(98, 66, 0), 2, 0).unwrap();
    assert!(parse_packet(&first).unwrap().keyframe);
    let packet = parse_packet(&second).unwrap();
    assert!(packet.keyframe);
    let mut decoder = Decoder::new().unwrap();
    assert_eq!(
        decoder
            .decode(packet.payload)
            .unwrap()
            .unwrap()
            .dimensions(),
        (98, 66)
    );
}

#[test]
fn invalid_frame_and_resize_do_not_corrupt_existing_encoder() {
    let mut encoder = VideoEncoder::new(64, 48, 30).unwrap();
    encoder.encode(&scene(64, 48, 0), 1, 0).unwrap();
    let mut truncated = scene(64, 48, 0);
    truncated.pixels.pop();
    assert!(encoder.encode(&truncated, 2, 0).is_err());
    assert!(encoder.encode(&scene(65, 49, 0), 2, 0).is_err());
    let packet = encoder.encode(&scene(64, 48, 2), 2, 0).unwrap();
    assert!(!parse_packet(&packet).unwrap().keyframe);
}

#[test]
fn constructor_rejects_unsupported_dimensions_and_rate() {
    for (width, height) in [
        (0, 48),
        (15, 48),
        (65, 48),
        (64, 49),
        (4000, 2160),
        (3840, 2162),
    ] {
        assert!(VideoEncoder::new(width, height, 30).is_err());
    }
    assert!(VideoEncoder::new(64, 48, 0).is_err());
    assert!(VideoEncoder::new(64, 48, 61).is_err());
    assert!(VideoEncoder::with_bitrate(64, 48, 30, 0).is_err());
}

#[test]
fn packet_rejects_truncation_bad_flags_lengths_and_keyframe_claims() {
    let mut encoder = VideoEncoder::new(64, 48, 30).unwrap();
    let valid = encoder.encode(&scene(64, 48, 0), 7, 0).unwrap();
    for length in [0, 3, 23, 24, valid.len() - 1] {
        assert!(parse_packet(&valid[..length]).is_err());
    }
    for (offset, value) in [(0, 0), (16, 1), (18, 2), (20, 0), (18, 0), (24, 9)] {
        let mut bad = valid.clone();
        bad[offset] = value;
        assert!(
            parse_packet(&bad).is_err(),
            "accepted mutation at byte {offset}"
        );
    }
}

#[test]
fn default_video_packets_match_frozen_screen_implementation_through_reset_and_resize() {
    let mut current = VideoEncoder::new(96, 64, 15).unwrap();
    let mut frozen = frozen_screen::VideoEncoder::new(96, 64, 15).unwrap();
    for index in 0..8 {
        let (width, height) = if index >= 6 { (98, 66) } else { (96, 64) };
        if index == 4 {
            current.reset();
            frozen.reset();
        }
        let source = scene(width, height, index * 2);
        let sequence = index as u32 + 10;
        let packet = current.encode(&source, sequence, sequence + 100).unwrap();
        assert_eq!(
            packet,
            frozen.encode(&source, sequence, sequence + 100).unwrap()
        );
    }
}

#[test]
fn custom_video_timing_rejects_each_unsupported_axis_and_accepts_boundaries() {
    for (fps, intra_period) in [
        (0, 60),
        (61, 60),
        (30, 0),
        (30, 3601),
        (u32::MAX, 60),
        (30, u32::MAX),
    ] {
        assert!(VideoTiming::new(fps, intra_period).is_err());
    }
    for (fps, intra_period) in [(1, 1), (60, 3600)] {
        let timing = VideoTiming::new(fps, intra_period).unwrap();
        assert_eq!(timing.fps(), fps);
        assert_eq!(timing.intra_period(), intra_period);
    }
    assert!(VideoEncoder::with_bitrate(64, 48, u32::MAX, 3_000_000).is_err());
}

#[test]
fn explicit_two_second_interval_matches_frozen_packets_at_both_nominal_rates() {
    for fps in [15, 60] {
        let timing = VideoTiming::new(fps, fps * 2).unwrap();
        let mut encoder = VideoEncoder::with_timing(96, 64, DEFAULT_BITRATE_BPS, timing).unwrap();
        let mut frozen = frozen_screen::VideoEncoder::new(96, 64, fps).unwrap();
        for index in 0..8 {
            if index == 4 {
                encoder.reset();
                frozen.reset();
            }
            let (width, height) = if index >= 6 { (98, 66) } else { (96, 64) };
            let source = scene(width, height, index * 2);
            assert_eq!(
                encoder.encode(&source, index as u32, 100).unwrap(),
                frozen.encode(&source, index as u32, 100).unwrap()
            );
            assert_eq!(encoder.timing(), timing);
        }
    }
}

#[test]
fn custom_gop_decodes_and_survives_resize_and_explicit_reset() {
    let timing = VideoTiming::new(15, 4).unwrap();
    let mut encoder = VideoEncoder::with_timing(96, 64, 3_000_000, timing).unwrap();
    for (width, height) in [(96, 64), (98, 66)] {
        let mut decoder = Decoder::new().unwrap();
        let source = scene(width, height, 0);
        for sequence in 0..10 {
            let bytes = encoder.encode(&source, sequence, 80).unwrap();
            let packet = parse_packet(&bytes).unwrap();
            assert_eq!(packet.keyframe, sequence % 4 == 0);
            assert_eq!(
                decoder
                    .decode(packet.payload)
                    .unwrap()
                    .unwrap()
                    .dimensions(),
                (usize::from(width), usize::from(height))
            );
            assert_eq!(encoder.timing(), timing);
        }
    }
    encoder.reset();
    let bytes = encoder.encode(&scene(98, 66, 2), 20, 81).unwrap();
    let packet = parse_packet(&bytes).unwrap();
    assert!(packet.keyframe);
    assert_eq!(
        Decoder::new()
            .unwrap()
            .decode(packet.payload)
            .unwrap()
            .unwrap()
            .dimensions(),
        (98, 66)
    );
    assert_eq!(encoder.timing(), timing);
}

#[test]
fn encodable_sizes_are_exactly_the_ones_the_encoder_accepts() {
    for (width, height) in [
        (16, 16),
        (64, 48),
        (3840, 2160),
        (2160, 3840),
        (15, 16),
        (16, 14),
        (65, 48),
        (64, 47),
        (3842, 2160),
        (3840, 2162),
        (2162, 2162),
        (0, 0),
    ] {
        assert_eq!(
            encodable(width, height),
            VideoEncoder::new(width, height, 30).is_ok(),
            "{width}x{height}"
        );
    }
    assert!(encodable(3840, 2160) && !encodable(3842, 2160) && !encodable(65, 48));
}

#[test]
fn a_lower_bitrate_makes_the_frames_that_follow_smaller() {
    // Fine grain over a quarter of the picture, new in every frame: more than
    // the lower target pays for, less than the higher one does.
    let noise = |step: usize| {
        let mut frame = scene(640, 360, 0);
        let mut state = (step as u32).wrapping_mul(2654435761) + 1;
        for y in 90..270 {
            for byte in &mut frame.pixels[(y * 640 + 160) * 4..(y * 640 + 480) * 4] {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = 112 + (state >> 27) as u8;
            }
        }
        frame
    };
    let bytes_after = |lowered: Option<u32>| {
        let mut encoder = VideoEncoder::with_bitrate(640, 360, 30, 2_000_000).unwrap();
        let mut total = 0;
        for step in 0..60 {
            if step == 20
                && let Some(bitrate) = lowered
            {
                encoder.set_bitrate(bitrate);
            }
            let packet = encoder.encode(&noise(step), step as u32 + 1, 0).unwrap();
            assert!(parse_packet(&packet).is_ok());
            if step >= 30 {
                total += packet.len();
            }
        }
        total
    };
    let (kept, lowered) = (bytes_after(None), bytes_after(Some(400_000)));
    assert!(lowered * 10 < kept * 8, "{lowered} bytes against {kept}");
    // More than the encoder was made for is no change.
    assert_eq!(bytes_after(Some(50_000_000)), kept);
}

#[test]
fn bitrate_can_be_lowered_and_raised_without_error() {
    let mut encoder = VideoEncoder::with_bitrate(128, 96, 30, 4_000_000).unwrap();
    let frame = scene(128, 96, 0);
    assert!(parse_packet(&encoder.encode(&frame, 1, 0).unwrap()).is_ok());
    encoder.set_bitrate(400_000);
    assert!(parse_packet(&encoder.encode(&scene(128, 96, 2), 2, 0).unwrap()).is_ok());
    encoder.set_bitrate(4_000_000);
    assert!(parse_packet(&encoder.encode(&scene(128, 96, 4), 3, 0).unwrap()).is_ok());
}

#[test]
fn yuv_planes_match_openh264_yuv_buffer_bit_for_bit_across_full_and_partial_damage() {
    use openh264::formats::{RgbaSliceU8, YUVBuffer};

    // Include a non-16-aligned width (142) and multi-tile height (260) to cover SIMD + scalar tails.
    let (w, h) = (142u16, 260u16);
    let mut frame = Frame::new(w, h).unwrap();
    let mut state = 0x1234_5678_u32;
    for px in frame.pixels.as_chunks_mut::<4>().0 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        px[0] = (state >> 24) as u8;
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        px[1] = (state >> 24) as u8;
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        px[2] = (state >> 24) as u8;
        px[3] = 255;
    }

    let mut reference = YUVBuffer::new(usize::from(w), usize::from(h));
    reference.read_rgba8(RgbaSliceU8::new(
        &frame.pixels,
        (usize::from(w), usize::from(h)),
    ));

    let mut cache = YuvPlanes::new(usize::from(w), usize::from(h));
    cache.update_rgba8(&frame.pixels, &Damage::full(w, h));
    assert_eq!(cache.y(), reference.y(), "full Y mismatch");
    assert_eq!(cache.u(), reference.u(), "full U mismatch");
    assert_eq!(cache.v(), reference.v(), "full V mismatch");

    // Mutate a sparse band (<50% of rows) crossing a tile boundary.
    for y in 60..110 {
        for x in 10..138 {
            let off = (y * usize::from(w) + x) * 4;
            frame.pixels[off] = ((x * 3 + y) & 0xff) as u8;
            frame.pixels[off + 1] = ((x + y * 5) & 0xff) as u8;
            frame.pixels[off + 2] = ((x ^ y) & 0xff) as u8;
        }
    }
    let mut partial = Damage::empty(w, h);
    partial.mark_rect(10, 60, 128, 50);
    cache.update_rgba8(&frame.pixels, &partial);
    reference.read_rgba8(RgbaSliceU8::new(
        &frame.pixels,
        (usize::from(w), usize::from(h)),
    ));
    assert_eq!(cache.y(), reference.y(), "partial Y mismatch");
    assert_eq!(cache.u(), reference.u(), "partial U mismatch");
    assert_eq!(cache.v(), reference.v(), "partial V mismatch");

    // Mutate a second sparse band including non-16-aligned right edge.
    for y in 200..250 {
        for x in 0..142 {
            let off = (y * usize::from(w) + x) * 4;
            frame.pixels[off] = ((x * 7) & 0xff) as u8;
            frame.pixels[off + 1] = ((y * 3) & 0xff) as u8;
            frame.pixels[off + 2] = 180;
        }
    }
    let mut partial2 = Damage::empty(w, h);
    partial2.mark_rect(0, 200, 142, 50);
    cache.update_rgba8(&frame.pixels, &partial2);
    reference.read_rgba8(RgbaSliceU8::new(
        &frame.pixels,
        (usize::from(w), usize::from(h)),
    ));
    assert_eq!(cache.y(), reference.y(), "second partial Y mismatch");
    assert_eq!(cache.u(), reference.u(), "second partial U mismatch");
    assert_eq!(cache.v(), reference.v(), "second partial V mismatch");
}

#[test]
fn dynamic_bitrates_above_24_bit_float_precision_are_accepted() {
    let mut encoder = VideoEncoder::with_bitrate(128, 96, 30, 50_000_000).unwrap();
    let frame = scene(128, 96, 0);
    assert!(parse_packet(&encoder.encode(&frame, 1, 0).unwrap()).is_ok());
    // These bitrates round upward when OpenH264 rescales layer bitrates via f32.
    for (seq, target) in [
        (2u32, 16_777_219),
        (3, 33_554_435),
        (4, 49_999_999),
        (5, 20_000_003),
        (6, 400_000),
        (7, 33_554_435),
    ] {
        encoder.set_bitrate(target);
        let packet = encoder
            .encode(&scene(128, 96, seq as usize), seq, 0)
            .unwrap();
        assert!(parse_packet(&packet).is_ok(), "failed at target={target}");
    }
}

#[test]
fn prepared_frame_can_be_reencoded_as_idr_after_reset_and_rejects_unprepared() {
    let mut encoder = VideoEncoder::new(96, 64, 30).unwrap();
    assert!(!encoder.has_prepared());
    assert!(encoder.encode_prepared(1, 0).is_err());

    let frame = scene(96, 64, 0);
    encoder
        .prepare_damaged(&frame, &Damage::full(96, 64))
        .unwrap();
    assert!(encoder.has_prepared());
    let first = encoder.encode_prepared(1, 10).unwrap();
    let parsed_first = parse_packet(&first).unwrap();
    assert!(parsed_first.keyframe);
    assert_eq!(parsed_first.input_id, 10);

    let second = encoder.encode_prepared(2, 10).unwrap();
    assert!(!parse_packet(&second).unwrap().keyframe);

    encoder.reset();
    let resync = encoder.encode_prepared(3, 10).unwrap();
    let parsed_resync = parse_packet(&resync).unwrap();
    assert!(parsed_resync.keyframe);
    assert_eq!(parsed_resync.sequence, 3);
    assert_eq!(parsed_resync.input_id, 10);
}
