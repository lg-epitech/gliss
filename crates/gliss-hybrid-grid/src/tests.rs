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
        let mut encoder = HybridGridEncoder::new(size, 15, 3_000_000).unwrap();
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
        let mut encoder = HybridGridEncoder::new(size, 15, 3_000_000).unwrap();
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
        let mut encoder = HybridGridEncoder::new(size, 15, 3_000_000).unwrap();
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
    let mut encoder = HybridGridEncoder::new(16, 15, 3_000_000).unwrap();
    let bytes = encoder.encode(&source, 0, 1).unwrap();
    assert!(encoder.stats().grid_fallback);
    assert!(parse_packet(&bytes).unwrap().masks.is_empty());
    assert_eq!(Decoder::new().decode(&bytes).unwrap(), source);
}
#[test]
fn malformed_headers_and_grid_delta_preserve_retained_pixels() {
    let mut encoder = HybridGridEncoder::new(32, 15, 3_000_000).unwrap();
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
        let mut encoder = HybridGridEncoder::new(size, 15, 3_000_000).unwrap();
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
            let mut global = HybridGridEncoder::with_config(
                size,
                15,
                3_000_000,
                HybridGridConfig {
                    base: BaseMode::Global,
                    video,
                },
            )
            .unwrap();
            let mut cached = HybridGridEncoder::with_config(
                size,
                15,
                3_000_000,
                HybridGridConfig {
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
    let mut wide = HybridGridEncoder::new(32, 15, 3_000_000).unwrap();
    let mut fine = HybridGridEncoder::new(16, 15, 3_000_000).unwrap();
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
        let mut a = HybridGridEncoder::new(size, 15, 3_000_000).unwrap();
        let mut b = HybridGridEncoder::new(size, 15, 3_000_000).unwrap();
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
        let mut encoder = HybridGridEncoder::new(size, 15, 3_000_000).unwrap();
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
