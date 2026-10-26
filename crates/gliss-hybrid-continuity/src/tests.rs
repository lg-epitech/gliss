use super::*;
use crate::{
    classifier::{grid, tile_bounds},
    fixtures::scene,
    wire::wrap,
};

fn outside(actual: &Frame, source: &Frame, masks: &[u16], size: u16) {
    let (columns, _) = grid(source.width, source.height, size);
    for y in 0..usize::from(source.height) {
        for x in 0..usize::from(source.width) {
            let index = (y / usize::from(size)) * columns + x / usize::from(size);
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
fn classifier_fixed_temporal_gates_and_partial_sample_floor() {
    for size in [32, 16] {
        let mut classifier = Classifier::new(size).unwrap();
        for tick in 0..4 {
            let class = classifier
                .update(&scene(128, 64, tick, size, &[0, 2]).unwrap())
                .unwrap();
            assert_eq!(class.masks, if tick == 3 { vec![0, 2] } else { vec![] });
        }
        for tick in 4..10 {
            let class = classifier
                .update(&scene(128, 64, tick, size, &[]).unwrap())
                .unwrap();
            assert_eq!(class.masks, if tick == 9 { vec![] } else { vec![0, 2] });
        }
        let mut edge = Classifier::new(size).unwrap();
        // Five columns provide only 40 sample positions, so even moving unique
        // RGB can never pass the unchanged absolute 48-color gate.
        for tick in 0..5 {
            assert!(
                edge.update(&scene(size + 5, size, tick, size, &[1]).unwrap())
                    .unwrap()
                    .masks
                    .is_empty()
            );
        }
    }
    assert!(Classifier::new(64).is_err());
}
#[test]
fn classifier_grid_cap_clears_state_without_large_grid_allocation() {
    for (size, width, height) in [(16, 2048, 1024), (32, 4096, 2048)] {
        let mut classifier = Classifier::new(size).unwrap();
        let limit = Frame::new(width, height).unwrap();
        assert!(!classifier.update(&limit).unwrap().grid_fallback);
        assert_eq!(classifier.state_tiles(), 8192);
        let oversized = Frame::new(width + 1, height).unwrap();
        let result = classifier.update(&oversized).unwrap();
        assert!(result.grid_fallback && result.masks.is_empty());
        assert_eq!(classifier.state_tiles(), 0);
    }
}
#[test]
fn software_roundtrip_activation_removal_fine_gaps_and_global64_reference() {
    for size in [32, 16] {
        let mut encoder = HybridContinuityEncoder::new(size, 15, 3_000_000).unwrap();
        let mut decoder = Decoder::new();
        for tick in 0..10 {
            let source = scene(128, 64, tick, size, if tick < 4 { &[0, 2] } else { &[] }).unwrap();
            let bytes = encoder.encode(&source, tick, tick + 20).unwrap();
            let packet = parse_packet(&bytes).unwrap();
            assert_eq!(packet.mask_grid, size);
            assert_eq!(&packet.base[..4], b"GLB2");
            if tick == 3 {
                assert_eq!(packet.masks, vec![0, 2]);
                assert!(packet.video_keyframe);
            }
            let decoded = decoder.decode(&bytes).unwrap();
            outside(&decoded, &source, &packet.masks, size);
            if tick == 9 {
                assert!(packet.masks.is_empty());
                assert_eq!(decoded, source);
            }
            if let Some(base) = decoder.base_frame() {
                for index in packet.masks {
                    let (x, y, w, h) = tile_bounds(index, source.width, source.height, size);
                    for row in y..y + h {
                        for col in x..x + w {
                            let offset = (row * usize::from(source.width) + col) * 4;
                            assert_eq!(&base.pixels[offset..offset + 4], &[0, 0, 0, 255]);
                        }
                    }
                }
            }
        }
    }
}
#[test]
fn changed_roi_resize_sequence_gap_and_explicit_reset_are_independent() {
    for size in [32, 16] {
        let mut encoder = HybridContinuityEncoder::new(size, 15, 3_000_000).unwrap();
        let mut decoder = Decoder::new();
        for tick in 0..8 {
            let source = scene(128, 64, tick, size, if tick < 4 { &[0, 2] } else { &[3] }).unwrap();
            let bytes = encoder.encode(&source, tick, 0).unwrap();
            let packet = parse_packet(&bytes).unwrap();
            if tick == 6 {
                assert!(packet.video_keyframe);
                assert!(packet.masks.contains(&3));
            }
            outside(
                &decoder.decode(&bytes).unwrap(),
                &source,
                &packet.masks,
                size,
            );
        }
        encoder.reset();
        let source = scene(128, 64, 8, size, &[3]).unwrap();
        let key = encoder.encode(&source, 20, 77).unwrap();
        assert!(parse_packet(&key).unwrap().keyframe);
        outside(
            &decoder.decode(&key).unwrap(),
            &source,
            &parse_packet(&key).unwrap().masks,
            size,
        );
        let resized = scene(130, 66, 9, size, &[0]).unwrap();
        let key = encoder.encode(&resized, 22, 78).unwrap();
        assert!(parse_packet(&key).unwrap().keyframe);
        outside(
            &decoder.decode(&key).unwrap(),
            &resized,
            &parse_packet(&key).unwrap().masks,
            size,
        );
    }
}
#[test]
fn unsupported_odd_crop_and_oversized_grid_are_lossless_not_partial_video() {
    for size in [32, 16] {
        let mut encoder = HybridContinuityEncoder::new(size, 15, 3_000_000).unwrap();
        let mut decoder = Decoder::new();
        for tick in 0..4 {
            let source = scene(size + 9, size, tick, size, &[1]).unwrap();
            let bytes = encoder.encode(&source, tick, 0).unwrap();
            assert_eq!(decoder.decode(&bytes).unwrap(), source);
            if tick == 3 {
                assert!(encoder.stats().roi_fallback);
                assert!(parse_packet(&bytes).unwrap().masks.is_empty());
            }
        }
    }
    // Lowest-cost oversized 16-grid case; classifier-only tests cover 32 cap.
    let source = Frame::new(2049, 1024).unwrap();
    let mut encoder = HybridContinuityEncoder::new(16, 15, 3_000_000).unwrap();
    let bytes = encoder.encode(&source, 0, 1).unwrap();
    assert!(encoder.stats().grid_fallback);
    assert!(parse_packet(&bytes).unwrap().masks.is_empty());
    assert_eq!(Decoder::new().decode(&bytes).unwrap(), source);
}
#[test]
fn malformed_headers_and_grid_delta_preserve_retained_pixels() {
    let mut encoder = HybridContinuityEncoder::new(32, 15, 3_000_000).unwrap();
    let mut decoder = Decoder::new();
    for tick in 0..4 {
        let bytes = encoder
            .encode(&scene(128, 64, tick, 32, &[0, 2]).unwrap(), tick, 0)
            .unwrap();
        decoder.decode(&bytes).unwrap();
    }
    let before = decoder.frame().unwrap().clone();
    let base = decoder.base_frame().unwrap().clone();
    let good = encoder
        .encode(&scene(128, 64, 4, 32, &[0, 2]).unwrap(), 4, 0)
        .unwrap();
    for offset in [3, 16, 18, 22, 24, 32, 40, 48, 66, 72] {
        let mut bad = good.clone();
        bad[offset] ^= 1;
        assert!(decoder.decode(&bad).is_err());
        assert_eq!(decoder.frame().unwrap(), &before);
        assert_eq!(decoder.base_frame().unwrap(), &base);
        assert_eq!(decoder.sequence(), Some(3));
    }
    let mut changed = good.clone();
    changed[16..18].copy_from_slice(&16u16.to_le_bytes());
    assert!(decoder.decode(&changed).is_err());
    let mut old_version = good.clone();
    old_version[..4].copy_from_slice(b"GLH1");
    assert!(parse_packet(&old_version).is_err());
}
#[test]
fn native_video_failure_rolls_back_and_requires_whole_grid_key() {
    for size in [32, 16] {
        let mut encoder = HybridContinuityEncoder::new(size, 15, 3_000_000).unwrap();
        let mut decoder = Decoder::new();
        for tick in 0..3 {
            decoder
                .decode(
                    &encoder
                        .encode(&scene(128, 64, tick, size, &[0, 2]).unwrap(), tick, 0)
                        .unwrap(),
                )
                .unwrap();
        }
        let before = decoder.frame().unwrap().clone();
        let old_base = decoder.base_frame().unwrap().clone();
        let good = encoder
            .encode(&scene(128, 64, 3, size, &[0, 2]).unwrap(), 3, 0)
            .unwrap();
        let parsed = parse_packet(&good).unwrap();
        let payload = [0, 0, 1, 0x67, 66, 0, 31, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0];
        let mut broken = parsed.video.unwrap()[..24].to_vec();
        broken[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        broken.extend_from_slice(&payload);
        let bad = wrap(parsed.base, Some(&broken), &parsed.masks, parsed.roi, size).unwrap();
        assert!(decoder.decode(&bad).is_err());
        assert!(decoder.needs_keyframe());
        assert_eq!(decoder.frame().unwrap(), &before);
        assert_eq!(decoder.base_frame().unwrap(), &old_base);
        assert!(decoder.decode(&good).is_err());
        encoder.reset();
        let source = scene(128, 64, 4, size, &[0, 2]).unwrap();
        let recovery = encoder.encode(&source, 4, 0).unwrap();
        outside(
            &decoder.decode(&recovery).unwrap(),
            &source,
            &parse_packet(&recovery).unwrap().masks,
            size,
        );
    }
}
#[test]
fn cached_base_matches_default_packets_for_explicit_video_profiles() {
    for size in [32, 16] {
        for video in [VideoProfile::Screen, VideoProfile::CameraSceneOff] {
            let mut global = HybridContinuityEncoder::with_config(
                size,
                15,
                3_000_000,
                HybridContinuityConfig {
                    base: BaseMode::Global,
                    video,
                },
            )
            .unwrap();
            let mut cached = HybridContinuityEncoder::with_config(
                size,
                15,
                3_000_000,
                HybridContinuityConfig {
                    base: BaseMode::GlobalCached,
                    video,
                },
            )
            .unwrap();
            for tick in 0..10 {
                let source =
                    scene(128, 64, tick, size, if tick < 4 { &[0, 2] } else { &[] }).unwrap();
                assert_eq!(
                    global.encode(&source, tick, tick + 100).unwrap(),
                    cached.encode(&source, tick, tick + 100).unwrap()
                );
            }
        }
    }
}

#[test]
fn grid_change_delta_rejects_before_commit_and_independent_key_establishes_grid() {
    let source = Frame::new(128, 64).unwrap();
    let mut wide = HybridContinuityEncoder::new(32, 15, 3_000_000).unwrap();
    let mut fine = HybridContinuityEncoder::new(16, 15, 3_000_000).unwrap();
    let mut decoder = Decoder::new();
    let first = wide.encode(&source, 0, 0).unwrap();
    decoder.decode(&first).unwrap();
    let before = decoder.frame().unwrap().clone();
    let _ = fine.encode(&source, 0, 0).unwrap();
    let delta = fine.encode(&source, 1, 0).unwrap();
    assert!(!parse_packet(&delta).unwrap().keyframe);
    assert!(decoder.decode(&delta).is_err());
    assert_eq!(decoder.frame().unwrap(), &before);
    assert_eq!(decoder.mask_grid(), Some(32));
    fine.reset();
    let key = fine.encode(&source, 10, 0).unwrap();
    assert_eq!(decoder.decode(&key).unwrap(), source);
    assert_eq!(decoder.mask_grid(), Some(16));
}

#[test]
fn input_metadata_has_no_effect_on_classifier_masks_or_roi() {
    for size in [32, 16] {
        let mut a = HybridContinuityEncoder::new(size, 15, 3_000_000).unwrap();
        let mut b = HybridContinuityEncoder::new(size, 15, 3_000_000).unwrap();
        for tick in 0..5 {
            let source = scene(128, 64, tick, size, &[0, 2]).unwrap();
            let left = a.encode(&source, tick, 0).unwrap();
            let right = b.encode(&source, tick, u32::MAX - tick).unwrap();
            let left = parse_packet(&left).unwrap();
            let right = parse_packet(&right).unwrap();
            assert_eq!(left.masks, right.masks);
            assert_eq!(left.roi, right.roi);
        }
    }
}

#[test]
fn text_inside_moving_pixels_does_not_receive_an_oracle_exemption() {
    for size in [32, 16] {
        let mut encoder = HybridContinuityEncoder::new(size, 15, 3_000_000).unwrap();
        let mut decoder = Decoder::new();
        for tick in 0..4 {
            let mut source = scene(128, 64, tick, size, &[0]).unwrap();
            crate::fixtures::text_overlay(&mut source);
            let bytes = encoder.encode(&source, tick, 1000 + tick).unwrap();
            let parsed = parse_packet(&bytes).unwrap();
            if tick == 3 {
                assert_eq!(parsed.masks, vec![0]);
            }
            outside(
                &decoder.decode(&bytes).unwrap(),
                &source,
                &parsed.masks,
                size,
            );
            // No requirement that text inside the lossy mask is exact.
        }
    }
}

fn camera(size: u16) -> HybridContinuityEncoder {
    HybridContinuityEncoder::with_config(
        size,
        15,
        3_000_000,
        HybridContinuityConfig {
            base: BaseMode::GlobalCached,
            video: VideoProfile::CameraSceneOff,
        },
    )
    .unwrap()
}
fn tile_mae(actual: &Frame, expected: &Frame, tile: u16, size: u16) -> f64 {
    let (x, y, w, h) = tile_bounds(tile, expected.width, expected.height, size);
    let mut error = 0u64;
    for row in y..y + h {
        for col in x..x + w {
            let offset = (row * usize::from(expected.width) + col) * 4;
            for channel in 0..3 {
                error += u64::from(
                    actual.pixels[offset + channel].abs_diff(expected.pixels[offset + channel]),
                );
            }
        }
    }
    error as f64 / (w * h * 3) as f64
}
#[test]
fn actual_camera_p_frames_expose_prior_unmasked_crop_and_restore_removed_tiles() {
    for size in [32, 16] {
        let mut encoder = camera(size);
        let mut decoder = Decoder::new();
        let previous = crate::fixtures::continuity_scene(size, 5).unwrap();
        let mut roi = None;
        let mut before = Vec::new();
        let mut mask_changes = Vec::new();
        for tick in 0..30 {
            let source = crate::fixtures::continuity_scene(size, tick).unwrap();
            let bytes = encoder.encode(&source, tick, 900 + tick).unwrap();
            let packet = parse_packet(&bytes).unwrap();
            let changed = packet.masks != before;
            if tick == 3 {
                roi = packet.roi;
                assert_eq!(packet.masks, vec![0, 3]);
                assert!(packet.video_keyframe);
            }
            if tick > 3 {
                assert_eq!(
                    packet.roi, roi,
                    "Only mask membership changes in this fixture"
                );
                if changed {
                    mask_changes.push(tick);
                    assert!(
                        !packet.video_keyframe,
                        "Mask-only changes do not force native IDR"
                    );
                    let video = gliss_video::parse_packet(packet.video.unwrap()).unwrap();
                    assert!(
                        crate::fixtures::contains_p_slice(video.payload),
                        "Actual native packet lacks a P slice"
                    );
                }
            }
            let actual = decoder.decode(&bytes).unwrap();
            assert_eq!(decoder.input_id(), 900 + tick);
            outside(&actual, &source, &packet.masks, size);
            if tick == 6 {
                assert!(packet.masks.contains(&1) && !before.contains(&1));
                assert!(tile_mae(&actual, &source, 1, size) <= 40.0);
                assert!(
                    tile_mae(&actual, &source, 1, size) < tile_mae(&actual, &previous, 1, size),
                    "Newly exposed crop pixels are fresh, not preceding video"
                );
                let (x, y, _, _) = tile_bounds(1, source.width, source.height, size);
                let offset = (y * usize::from(source.width) + x) * 4;
                assert_eq!(
                    &decoder.base_frame().unwrap().pixels[offset..offset + 4],
                    &[0, 0, 0, 255]
                );
            }
            if tick == 14 {
                assert!(!packet.masks.contains(&1));
            }
            before = packet.masks;
        }
        assert_eq!(mask_changes, vec![6, 14, 17, 26, 29]);
    }
}
#[test]
fn missing_actual_continuity_p_delta_is_atomic_and_only_whole_key_recovers() {
    for size in [32, 16] {
        let mut encoder = camera(size);
        let mut decoder = Decoder::new();
        for tick in 0..6 {
            let source = crate::fixtures::continuity_scene(size, tick).unwrap();
            decoder
                .decode(&encoder.encode(&source, tick, tick).unwrap())
                .unwrap();
        }
        let _skipped = encoder
            .encode(&crate::fixtures::continuity_scene(size, 6).unwrap(), 6, 6)
            .unwrap();
        let next = encoder
            .encode(&crate::fixtures::continuity_scene(size, 7).unwrap(), 7, 7)
            .unwrap();
        assert!(!parse_packet(&next).unwrap().video_keyframe);
        let before = decoder.frame().unwrap().clone();
        let base = decoder.base_frame().unwrap().clone();
        assert!(decoder.decode(&next).is_err());
        assert_eq!(decoder.frame().unwrap(), &before);
        assert_eq!(decoder.base_frame().unwrap(), &base);
        assert_eq!(decoder.sequence(), Some(5));
        decoder.reset();
        assert!(decoder.decode(&next).is_err());
        encoder.reset();
        let source = crate::fixtures::continuity_scene(size, 8).unwrap();
        let key = encoder.encode(&source, 20, 123).unwrap();
        let packet = parse_packet(&key).unwrap();
        assert!(packet.keyframe && packet.video_keyframe);
        outside(&decoder.decode(&key).unwrap(), &source, &packet.masks, size);
        assert_eq!(decoder.sequence(), Some(20));
        assert_eq!(decoder.input_id(), 123);
    }
}
#[test]
fn old_versions_and_mismatched_nested_input_identity_reject_before_commit() {
    let mut encoder = camera(32);
    let mut decoder = Decoder::new();
    for tick in 0..4 {
        decoder
            .decode(
                &encoder
                    .encode(
                        &crate::fixtures::continuity_scene(32, tick).unwrap(),
                        tick,
                        10 + tick,
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    let good = encoder
        .encode(&crate::fixtures::continuity_scene(32, 4).unwrap(), 4, 14)
        .unwrap();
    let parsed = parse_packet(&good).unwrap();
    let base_start = 64 + parsed.masks.len() * 2;
    let video_start = base_start + parsed.base.len();
    let before = decoder.frame().unwrap().clone();
    for magic in [b"GLH1", b"GLH2", b"GLH3"] {
        let mut old = good.clone();
        old[..4].copy_from_slice(magic);
        assert!(decoder.decode(&old).is_err());
    }
    for offset in [base_start + 8, video_start + 8] {
        let mut bad = good.clone();
        bad[offset] ^= 1;
        assert!(decoder.decode(&bad).is_err());
        assert_eq!(decoder.frame().unwrap(), &before);
        assert_eq!(decoder.input_id(), 13);
    }
}
#[test]
fn p_slice_evidence_is_bounded_and_distinguishes_i_idr_and_truncation() {
    // first_mb=0: '1'; P slice_type=0: '1'; non-IDR I slice_type=2: '011'.
    assert!(crate::fixtures::contains_p_slice(&[0, 0, 1, 0x61, 0xc0]));
    assert!(!crate::fixtures::contains_p_slice(&[0, 0, 1, 0x61, 0xb0]));
    assert!(!crate::fixtures::contains_p_slice(&[0, 0, 1, 0x65, 0xc0]));
    assert!(!crate::fixtures::contains_p_slice(&[0, 0, 1, 0x61]));
    assert!(!crate::fixtures::contains_p_slice(&[0, 0, 1, 0x61, 0]));
}

#[test]
fn roi_translation_with_identical_crop_size_rejects_p_and_accepts_actual_idr() {
    for size in [32, 16] {
        let old_masks = vec![0, 2];
        let new_masks = vec![1, 3];
        let old_roi = roi_for_masks(&old_masks, 128, 64, size).unwrap();
        let new_roi = roi_for_masks(&new_masks, 128, 64, size).unwrap();
        assert_eq!(
            (old_roi.width, old_roi.height),
            (new_roi.width, new_roi.height)
        );
        assert_ne!(old_roi.x, new_roi.x);
        let mut video = VideoEncoder::with_profile(
            old_roi.width,
            old_roi.height,
            15,
            3_000_000,
            VideoProfile::CameraSceneOff,
        )
        .unwrap();
        let mut base = BaseEncoder::new(Mode::Global, 64).unwrap();
        let mut decoder = Decoder::new();
        let black = |source: &Frame, masks: &[u16]| {
            let mut black = source.clone();
            for &tile in masks {
                let (x, y, w, h) = tile_bounds(tile, source.width, source.height, size);
                for row in y..y + h {
                    for col in x..x + w {
                        let offset = (row * usize::from(source.width) + col) * 4;
                        black.pixels[offset..offset + 4].copy_from_slice(&[0, 0, 0, 255]);
                    }
                }
            }
            black
        };
        let first = scene(128, 64, 0, size, &old_masks).unwrap();
        let first_base = base.encode(&black(&first, &old_masks), 0, 500).unwrap();
        let first_video = video.encode(&crop(&first, old_roi), 0, 500).unwrap();
        let first_packet = wrap(
            &first_base,
            Some(&first_video),
            &old_masks,
            Some(old_roi),
            size,
        )
        .unwrap();
        decoder.decode(&first_packet).unwrap();
        let source = scene(128, 64, 1, size, &new_masks).unwrap();
        let next_base = base.encode(&black(&source, &new_masks), 1, 501).unwrap();
        let next_video = video.encode(&crop(&source, new_roi), 1, 501).unwrap();
        assert!(crate::fixtures::contains_p_slice(
            gliss_video::parse_packet(&next_video).unwrap().payload
        ));
        let delta = wrap(
            &next_base,
            Some(&next_video),
            &new_masks,
            Some(new_roi),
            size,
        )
        .unwrap();
        let before = decoder.frame().unwrap().clone();
        let exact = decoder.base_frame().unwrap().clone();
        assert!(decoder.decode(&delta).is_err());
        assert_eq!(decoder.frame().unwrap(), &before);
        assert_eq!(decoder.base_frame().unwrap(), &exact);
        assert_eq!(decoder.input_id(), 500);
        video.reset();
        let independent = video.encode(&crop(&source, new_roi), 1, 501).unwrap();
        assert!(gliss_video::parse_packet(&independent).unwrap().keyframe);
        let correct = wrap(
            &next_base,
            Some(&independent),
            &new_masks,
            Some(new_roi),
            size,
        )
        .unwrap();
        outside(
            &decoder.decode(&correct).unwrap(),
            &source,
            &new_masks,
            size,
        );
        assert_eq!(decoder.input_id(), 501);
    }
}
#[test]
fn native_empty_base_delta_removes_previous_p_video_without_stale_pixels() {
    for size in [32, 16] {
        let mut encoder = camera(size);
        let mut decoder = Decoder::new();
        for tick in 0..7 {
            let source = crate::fixtures::continuity_scene(size, tick).unwrap();
            let bytes = encoder.encode(&source, tick, tick).unwrap();
            decoder.decode(&bytes).unwrap();
        }
        let exact = decoder.base_frame().unwrap().clone();
        assert_ne!(decoder.frame().unwrap(), &exact);
        let mut base = BaseEncoder::new(Mode::Global, 64).unwrap();
        let _seed = base.encode(&exact, 6, 6).unwrap();
        let empty = base.encode(&exact, 7, 777).unwrap();
        assert_eq!(u16::from_le_bytes(empty[16..18].try_into().unwrap()), 0);
        let packet = wrap(&empty, None, &[], None, size).unwrap();
        assert_eq!(decoder.decode(&packet).unwrap(), exact);
        assert_eq!(decoder.masks(), &[]);
        assert_eq!(decoder.input_id(), 777);
    }
}
