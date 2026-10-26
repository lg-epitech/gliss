use super::*;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

// Independent pre-experiment production implementation, never used at runtime.
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
    let mut explicit =
        VideoEncoder::with_profile(96, 64, 15, DEFAULT_BITRATE_BPS, VideoProfile::Screen).unwrap();
    let mut frozen = frozen_screen::VideoEncoder::new(96, 64, 15).unwrap();
    for index in 0..8 {
        let (width, height) = if index >= 6 { (98, 66) } else { (96, 64) };
        if index == 4 {
            current.reset();
            explicit.reset();
            frozen.reset();
        }
        let source = scene(width, height, index * 2);
        let sequence = index as u32 + 10;
        let packet = current.encode(&source, sequence, sequence + 100).unwrap();
        assert_eq!(
            packet,
            explicit.encode(&source, sequence, sequence + 100).unwrap()
        );
        assert_eq!(
            packet,
            frozen.encode(&source, sequence, sequence + 100).unwrap()
        );
    }
}

#[test]
fn camera_profiles_native_decode_reset_and_resize_preserve_profile_and_opaque_output() {
    for profile in [VideoProfile::CameraSceneOn, VideoProfile::CameraSceneOff] {
        assert_eq!(profile.usage_name(), "camera-video-real-time");
        assert_eq!(
            profile.scene_change_detection(),
            profile == VideoProfile::CameraSceneOn
        );
        let mut encoder = VideoEncoder::with_profile(96, 64, 15, 3_000_000, profile).unwrap();
        let mut decoder = Decoder::new().unwrap();
        for index in 0..4 {
            let source = scene(96, 64, index * 11);
            let bytes = encoder
                .encode(&source, index as u32, index as u32 + 40)
                .unwrap();
            let packet = parse_packet(&bytes).unwrap();
            assert_eq!(packet.input_id, index as u32 + 40);
            if profile == VideoProfile::CameraSceneOff {
                assert_eq!(
                    packet.keyframe,
                    index == 0,
                    "no automatic scene IDR before periodic deadline"
                );
            }
            let decoded = decoder.decode(packet.payload).unwrap().unwrap();
            assert_eq!(decoded.dimensions(), (96, 64));
            let mut pixels = vec![0; source.pixels.len()];
            decoded.write_rgba8(&mut pixels);
            assert!(pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        }
        encoder.reset();
        let reset = encoder.encode(&scene(96, 64, 5), 10, 50).unwrap();
        let packet = parse_packet(&reset).unwrap();
        assert!(packet.keyframe);
        assert_eq!(encoder.profile(), profile);
        assert_eq!(
            Decoder::new()
                .unwrap()
                .decode(packet.payload)
                .unwrap()
                .unwrap()
                .dimensions(),
            (96, 64)
        );
        let resized = encoder.encode(&scene(98, 66, 6), 11, 51).unwrap();
        let packet = parse_packet(&resized).unwrap();
        assert!(packet.keyframe);
        assert_eq!(encoder.profile(), profile);
        assert_eq!(
            Decoder::new()
                .unwrap()
                .decode(packet.payload)
                .unwrap()
                .unwrap()
                .dimensions(),
            (98, 66)
        );
        let mut malformed = resized.clone();
        malformed[18] = 2;
        assert!(parse_packet(&malformed).is_err());
    }
}

#[test]
fn custom_video_timing_rejects_each_unsupported_axis_and_accepts_boundaries() {
    for (fps, timestamp_fps, intra_period) in [
        (0, 30, 60),
        (61, 30, 60),
        (30, 0, 60),
        (30, 61, 60),
        (30, 30, 0),
        (30, 30, 3601),
        (u32::MAX, 30, 60),
        (30, u32::MAX, 60),
        (30, 30, u32::MAX),
    ] {
        assert!(VideoTiming::new(fps, timestamp_fps, intra_period).is_err());
    }
    for (fps, timestamp_fps, intra_period) in [(1, 60, 1), (60, 1, 3600)] {
        let timing = VideoTiming::new(fps, timestamp_fps, intra_period).unwrap();
        assert_eq!(timing.fps(), fps);
        assert_eq!(timing.timestamp_fps(), timestamp_fps);
        assert_eq!(timing.intra_period(), intra_period);
    }
    assert!(VideoEncoder::with_profile(64, 48, u32::MAX, 3_000_000, VideoProfile::Screen).is_err());
}

#[test]
fn explicit_legacy_timing_matches_frozen_packets_at_both_nominal_rates() {
    for fps in [15, 60] {
        let timing = VideoTiming::new(fps, fps, fps * 2).unwrap();
        let mut encoder =
            VideoEncoder::with_timing(96, 64, DEFAULT_BITRATE_BPS, VideoProfile::Screen, timing)
                .unwrap();
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
    let timing = VideoTiming::new(15, 60, 4).unwrap();
    let mut encoder =
        VideoEncoder::with_timing(96, 64, 3_000_000, VideoProfile::CameraSceneOff, timing).unwrap();
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
            assert_eq!(encoder.profile(), VideoProfile::CameraSceneOff);
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
