use super::*;
use crate::wire::{u32_at, wrap};

#[allow(dead_code)]
mod frozen_default {
    use crate::{TILE_SIZE, roi_for_masks};
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-support/rust/hybrid/frozen-default-hybrid.rs"
    ));
}

fn scene(width: u16, tick: u8, moving: &[usize]) -> Frame {
    let mut frame = Frame::new(width, 64).unwrap();
    for y in 0..64usize {
        for x in 0..usize::from(width) {
            let offset = (y * usize::from(width) + x) * 4;
            let rgb = if moving.contains(&(x / 64)) {
                [
                    (x * 3 + usize::from(tick) * 11) as u8,
                    (y * 5 + usize::from(tick) * 17) as u8,
                    (x ^ y ^ usize::from(tick)) as u8,
                ]
            } else {
                let level = if (x + y + usize::from(tick)) % 13 == 0 {
                    220
                } else {
                    24
                };
                [level, level, level]
            };
            frame.pixels[offset..offset + 3].copy_from_slice(&rgb);
        }
    }
    frame
}
fn assert_outside(actual: &Frame, source: &Frame, masks: &[u16]) {
    let columns = usize::from(source.width).div_ceil(64);
    for y in 0..usize::from(source.height) {
        for x in 0..usize::from(source.width) {
            let index = (y / 64) * columns + x / 64;
            let offset = (y * usize::from(source.width) + x) * 4;
            assert_eq!(actual.pixels[offset + 3], 255);
            if !masks.contains(&(index as u16)) {
                assert_eq!(
                    &actual.pixels[offset..offset + 4],
                    &source.pixels[offset..offset + 4]
                );
            }
        }
    }
}

#[test]
fn pure_lossless_and_invalid_alpha_preserve_exact_pixels() {
    let mut encoder = HybridEncoder::new(15, 3_000_000).unwrap();
    let mut decoder = Decoder::new();
    for sequence in 0..5 {
        let frame = scene(192, sequence as u8, &[]);
        let packet = encoder.encode(&frame, sequence, 42 + sequence).unwrap();
        let parsed = parse_packet(&packet).unwrap();
        assert!(parsed.video.is_none());
        assert_eq!(decoder.decode(&packet).unwrap(), frame);
    }
    let mut invalid = scene(192, 5, &[]);
    invalid.pixels[3] = 0;
    assert!(encoder.encode(&invalid, 5, 47).is_err());
    assert_eq!(
        decoder
            .decode(&encoder.encode(&scene(192, 5, &[]), 5, 47).unwrap())
            .unwrap(),
        scene(192, 5, &[])
    );
}

#[test]
fn classifier_activates_after_three_changes_and_removes_after_six_quiet_frames() {
    let mut classifier = Classifier::default();
    for tick in 0..3 {
        assert!(
            classifier
                .update(&scene(192, tick, &[0, 2]))
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        classifier.update(&scene(192, 3, &[0, 2])).unwrap(),
        vec![0, 2]
    );
    let quiet = scene(192, 4, &[]);
    for _ in 0..5 {
        assert_eq!(classifier.update(&quiet).unwrap(), vec![0, 2]);
    }
    assert!(classifier.update(&quiet).unwrap().is_empty());
    assert!(classifier.update(&scene(128, 1, &[0])).unwrap().is_empty());
}

#[test]
fn activation_masks_gaps_and_removal_keep_the_exact_base_separate() {
    let mut encoder = HybridEncoder::new(15, 3_000_000).unwrap();
    let mut decoder = Decoder::new();
    for tick in 0..4 {
        let source = scene(192, tick, &[0, 2]);
        let bytes = encoder
            .encode(&source, u32::from(tick), 10 + u32::from(tick))
            .unwrap();
        let packet = parse_packet(&bytes).unwrap();
        let display = decoder.decode(&bytes).unwrap();
        assert_outside(&display, &source, &packet.masks);
        if tick == 3 {
            assert_eq!(packet.masks, vec![0, 2]);
            assert_eq!(packet.roi.unwrap().width, 192);
            assert!(packet.video_keyframe);
            assert!(!packet.keyframe);
            let base = decoder.base_frame().unwrap();
            assert_eq!(&base.pixels[..4], &[0, 0, 0, 255]);
            assert_ne!(&display.pixels[..3], &base.pixels[..3]);
        }
    }
    let quiet = scene(192, 4, &[]);
    for tick in 4..10 {
        let bytes = encoder.encode(&quiet, tick, tick + 10).unwrap();
        let packet = parse_packet(&bytes).unwrap();
        let display = decoder.decode(&bytes).unwrap();
        assert_outside(&display, &quiet, &packet.masks);
        if tick == 9 {
            assert!(packet.masks.is_empty());
            assert!(!packet.keyframe);
            assert_eq!(display, quiet);
        }
    }
}

#[test]
fn moving_masks_and_roi_force_idr_and_resize_resync_are_independent() {
    let mut encoder = HybridEncoder::new(15, 3_000_000).unwrap();
    let mut decoder = Decoder::new();
    let mut old_masks = Vec::new();
    let mut old_roi = None;
    for tick in 0..11 {
        let source = scene(256, tick, if tick < 4 { &[0] } else { &[2] });
        let bytes = encoder
            .encode(&source, u32::from(tick), u32::from(tick))
            .unwrap();
        let packet = parse_packet(&bytes).unwrap();
        if packet.video.is_some() && (packet.masks != old_masks || packet.roi != old_roi) {
            assert!(packet.video_keyframe);
        }
        assert_outside(&decoder.decode(&bytes).unwrap(), &source, &packet.masks);
        old_masks = packet.masks;
        old_roi = packet.roi;
    }
    encoder.reset();
    let source = scene(256, 12, &[2]);
    let bytes = encoder.encode(&source, 25, 90).unwrap();
    let packet = parse_packet(&bytes).unwrap();
    assert!(packet.keyframe && packet.video_keyframe);
    assert_outside(
        &Decoder::new().decode(&bytes).unwrap(),
        &source,
        &packet.masks,
    );
    let resized = Frame::new(130, 98).unwrap();
    let bytes = encoder.encode(&resized, 26, 91).unwrap();
    assert!(parse_packet(&bytes).unwrap().keyframe);
    assert_eq!(decoder.decode(&bytes).unwrap(), resized);
}

#[test]
fn unsupported_roi_falls_back_and_edge_crop_is_canonical() {
    assert_eq!(
        roi_for_masks(&[1], 72, 64),
        Some(Roi {
            x: 56,
            y: 0,
            width: 16,
            height: 64
        })
    );
    assert_eq!(roi_for_masks(&[1], 73, 64), None);
    assert_eq!(roi_for_masks(&[0, 64], 4160, 64), None);
    let mut encoder = HybridEncoder::new(15, 3_000_000).unwrap();
    for tick in 0..4 {
        let source = scene(4160, tick, &[0, 64]);
        let bytes = encoder.encode(&source, u32::from(tick), 0).unwrap();
        if tick == 3 {
            assert!(encoder.stats().roi_fallback);
            assert!(parse_packet(&bytes).unwrap().video.is_none());
        }
    }
}

#[test]
fn malformed_wrapper_and_out_of_order_delta_do_not_change_native_state() {
    let mut encoder = HybridEncoder::new(15, 3_000_000).unwrap();
    let mut decoder = Decoder::new();
    let mut active = Vec::new();
    for tick in 0..4 {
        active = encoder
            .encode(&scene(192, tick, &[0, 2]), u32::from(tick), u32::from(tick))
            .unwrap();
        decoder.decode(&active).unwrap();
    }
    let before = decoder.frame().unwrap().clone();
    let base = decoder.base_frame().unwrap().clone();
    for (offset, byte) in [
        (18, 128),
        (22, 1),
        (24, 1),
        (32, 0),
        (40, 9),
        (48, 1),
        (64, 9),
        (66, 0),
    ] {
        let mut bad = active.clone();
        if offset == 32 {
            bad[offset] ^= 1;
        } else {
            bad[offset] = byte;
        }
        assert!(decoder.decode(&bad).is_err());
        assert_eq!(decoder.sequence(), Some(3));
        assert_eq!(decoder.frame().unwrap(), &before);
        assert_eq!(decoder.base_frame().unwrap(), &base);
    }
    let mut trailing = active.clone();
    trailing.push(0);
    assert!(parse_packet(&trailing).is_err());
    assert!(decoder.decode(&active).is_err());
    assert_eq!(decoder.frame().unwrap(), &before);
}

#[test]
fn video_failure_rolls_back_base_and_requires_whole_hybrid_recovery() {
    let mut encoder = HybridEncoder::new(15, 3_000_000).unwrap();
    let mut decoder = Decoder::new();
    for tick in 0..3 {
        decoder
            .decode(
                &encoder
                    .encode(&scene(192, tick, &[0, 2]), u32::from(tick), u32::from(tick))
                    .unwrap(),
            )
            .unwrap();
    }
    let before = decoder.frame().unwrap().clone();
    let old_base = decoder.base_frame().unwrap().clone();
    let good = encoder.encode(&scene(192, 3, &[0, 2]), 3, 3).unwrap();
    let packet = parse_packet(&good).unwrap();
    let payload = [0, 0, 1, 0x67, 66, 0, 31, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0];
    let mut broken = packet.video.unwrap()[..24].to_vec();
    broken[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    broken.extend_from_slice(&payload);
    let bad = wrap(packet.base, Some(&broken), &packet.masks, packet.roi).unwrap();
    assert_eq!(u32_at(&bad, 4), 3);
    assert!(decoder.decode(&bad).is_err());
    assert!(decoder.needs_keyframe());
    assert_eq!(decoder.sequence(), Some(2));
    assert_eq!(decoder.frame().unwrap(), &before);
    assert_eq!(decoder.base_frame().unwrap(), &old_base);
    assert!(decoder.decode(&good).is_err());
    encoder.reset();
    let next = encoder.encode(&scene(192, 4, &[0, 2]), 4, 4).unwrap();
    assert!(parse_packet(&next).unwrap().keyframe);
    decoder.decode(&next).unwrap();
    assert!(!decoder.needs_keyframe());
}

#[test]
fn default_hybrid_packets_match_frozen_implementation_with_active_video_and_recovery() {
    let mut current = HybridEncoder::new(15, 3_000_000).unwrap();
    let mut explicit = HybridEncoder::with_config(15, 3_000_000, HybridConfig::default()).unwrap();
    let mut frozen = frozen_default::HybridEncoder::new(15, 3_000_000).unwrap();
    for index in 0..15 {
        let (width, moving): (u16, &[usize]) = if index >= 9 {
            (256, &[1, 3])
        } else {
            (192, &[0, 2])
        };
        if index == 6 {
            current.reset();
            explicit.reset();
            frozen.reset();
        }
        // A gap exercises whole-key recovery independently of explicit reset.
        let sequence = index as u32 + if index >= 8 { 10 } else { 0 };
        let source = scene(width, index as u8, moving);
        let packet = current.encode(&source, sequence, sequence + 50).unwrap();
        assert_eq!(
            packet,
            explicit.encode(&source, sequence, sequence + 50).unwrap()
        );
        assert_eq!(
            packet,
            frozen.encode(&source, sequence, sequence + 50).unwrap()
        );
        assert_eq!(current.config(), HybridConfig::default());
    }
}

#[test]
fn cached_hybrid_base_preserves_all_profile_stream_bytes_on_masks_gaps_and_resize() {
    for video in [
        VideoProfile::Screen,
        VideoProfile::CameraSceneOn,
        VideoProfile::CameraSceneOff,
    ] {
        let mut normal = HybridEncoder::with_config(
            15,
            3_000_000,
            HybridConfig {
                base: BaseMode::Global,
                video,
            },
        )
        .unwrap();
        let mut cached = HybridEncoder::with_config(
            15,
            3_000_000,
            HybridConfig {
                base: BaseMode::GlobalCached,
                video,
            },
        )
        .unwrap();
        for index in 0..18 {
            if index == 5 {
                normal.reset();
                cached.reset();
            }
            let (width, moving): (u16, &[usize]) = if index >= 10 {
                (256, &[1, 3])
            } else if index >= 6 {
                (192, &[])
            } else {
                (192, &[0, 2])
            };
            let source = scene(width, index as u8, moving);
            let sequence = index as u32 + if index >= 8 { 4 } else { 0 };
            let packet = normal.encode(&source, sequence, sequence + 90).unwrap();
            assert_eq!(
                packet,
                cached.encode(&source, sequence, sequence + 90).unwrap(),
                "{video:?} sequence {sequence}"
            );
        }
    }
}

#[test]
fn camera_hybrid_profiles_preserve_exact_unmasked_pixels_rollback_and_recovery() {
    for base in [BaseMode::Global, BaseMode::GlobalCached] {
        for video in [VideoProfile::CameraSceneOn, VideoProfile::CameraSceneOff] {
            let config = HybridConfig { base, video };
            let mut encoder = HybridEncoder::with_config(15, 3_000_000, config).unwrap();
            let mut decoder = Decoder::new();
            for tick in 0..4 {
                let source = scene(192, tick, &[0, 2]);
                let bytes = encoder
                    .encode(&source, u32::from(tick), 100 + u32::from(tick))
                    .unwrap();
                let parsed = parse_packet(&bytes).unwrap();
                assert_outside(&decoder.decode(&bytes).unwrap(), &source, &parsed.masks);
                if tick == 3 {
                    assert_eq!(parsed.masks, vec![0, 2]);
                    assert!(parsed.video_keyframe);
                }
            }
            let before = decoder.frame().unwrap().clone();
            let old_base = decoder.base_frame().unwrap().clone();
            let _dropped = encoder.encode(&scene(192, 4, &[0, 2]), 4, 104).unwrap();
            let delta = encoder.encode(&scene(192, 5, &[0, 2]), 5, 105).unwrap();
            assert!(!parse_packet(&delta).unwrap().keyframe);
            assert!(decoder.decode(&delta).is_err());
            assert_eq!(decoder.frame().unwrap(), &before);
            assert_eq!(decoder.base_frame().unwrap(), &old_base);
            encoder.reset();
            let source = scene(192, 6, &[0, 2]);
            let recovery = encoder.encode(&source, 6, 106).unwrap();
            let parsed = parse_packet(&recovery).unwrap();
            assert!(parsed.keyframe && parsed.video_keyframe);
            assert_outside(&decoder.decode(&recovery).unwrap(), &source, &parsed.masks);
            assert_outside(
                &Decoder::new().decode(&recovery).unwrap(),
                &source,
                &parsed.masks,
            );
            let good_display = decoder.frame().unwrap().clone();
            let good_base = decoder.base_frame().unwrap().clone();
            let mut malformed = recovery.clone();
            malformed[48] = 1;
            assert!(decoder.decode(&malformed).is_err());
            assert_eq!(decoder.frame().unwrap(), &good_display);
            assert_eq!(decoder.base_frame().unwrap(), &good_base);
            // Framing accepts these independent SPS/PPS/IDR declarations, but
            // native decoding fails after staging the base; public pixels roll back.
            let payload = [0, 0, 1, 0x67, 66, 0, 31, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0];
            let mut broken = parsed.video.unwrap()[..24].to_vec();
            broken[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            broken.extend_from_slice(&payload);
            let malformed_video =
                wrap(parsed.base, Some(&broken), &parsed.masks, parsed.roi).unwrap();
            assert!(decoder.decode(&malformed_video).is_err());
            assert!(decoder.needs_keyframe());
            assert_eq!(decoder.frame().unwrap(), &good_display);
            assert_eq!(decoder.base_frame().unwrap(), &good_base);
            for tick in 7..11 {
                let source = scene(256, tick, &[1, 3]);
                let bytes = encoder
                    .encode(&source, u32::from(tick), 100 + u32::from(tick))
                    .unwrap();
                let parsed = parse_packet(&bytes).unwrap();
                assert_outside(&decoder.decode(&bytes).unwrap(), &source, &parsed.masks);
                if tick == 7 {
                    assert!(parsed.keyframe);
                }
                if tick == 10 {
                    assert_eq!(parsed.masks, vec![1, 3]);
                    assert!(parsed.video_keyframe);
                }
                assert_eq!(encoder.config(), config);
            }
        }
    }
}
