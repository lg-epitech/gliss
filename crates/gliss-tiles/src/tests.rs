use super::*;

fn scene(width: u16, height: u16, seed: u32) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    let mut rng = seed;
    for pixel in frame.pixels.as_chunks_mut::<4>().0 {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        pixel.copy_from_slice(&rng.to_le_bytes());
    }
    frame
}

fn apply(decoder: &mut Decoder, packets: &[Vec<u8>], step: u64) {
    for packet in packets {
        decoder.ingest(packet, step).unwrap();
    }
}

#[test]
fn exact_partial_edges_and_raw_rgba() {
    let mut encoder = Encoder::default();
    let source = scene(67, 35, 123);
    let packets = encoder.encode(&source, 1).unwrap();
    assert!(packets.iter().all(|p| p.len() <= DATAGRAM));
    let mut decoder = Decoder::new();
    for packet in packets.iter().rev() {
        decoder.ingest(packet, 0).unwrap();
    }
    assert_eq!(decoder.frame, Some(source));
    assert_eq!(decoder.applied_tiles, 6);
    assert_eq!(decoder.assembly_usage(), (0, 0));
}

#[test]
fn tile_loss_does_not_block_other_tiles_and_static_refresh_repairs_current_state() {
    let first = Frame::new(64, 64).unwrap();
    let mut second = first.clone();
    for row in 0..32 {
        second.pixels[row * 64 * 4..row * 64 * 4 + 128].fill(255);
    }
    let mut encoder = Encoder::default();
    let mut decoder = Decoder::new();
    apply(&mut decoder, &encoder.encode(&first, 0).unwrap(), 0);
    let packets = encoder.encode(&second, 1).unwrap();
    for packet in packets {
        if parse(&packet).unwrap().tile != 0 {
            decoder.ingest(&packet, 1).unwrap();
        }
    }
    assert_ne!(decoder.frame.as_ref().unwrap(), &second);
    for step in 2..=16 {
        apply(
            &mut decoder,
            &encoder.encode(&second, step).unwrap(),
            u64::from(step),
        );
    }
    assert_eq!(decoder.frame, Some(second));
}

#[test]
fn late_updates_sequence_wrap_epoch_wrap_and_resize() {
    assert!(newer(0, u32::MAX));
    assert!(!newer(u32::MAX, 0));
    assert!(!newer(0x8000_0000, 0));
    let mut encoder = Encoder::new(u32::MAX, 0).unwrap();
    let mut decoder = Decoder::new();
    let old = encoder.encode(&scene(32, 32, 1), u32::MAX).unwrap();
    let current = scene(32, 32, 2);
    apply(&mut decoder, &old, 0);
    apply(&mut decoder, &encoder.encode(&current, 0).unwrap(), 1);
    apply(&mut decoder, &old, 2);
    assert_eq!(decoder.frame.as_ref(), Some(&current));
    let resized = scene(48, 34, 3);
    apply(&mut decoder, &encoder.encode(&resized, 1).unwrap(), 3);
    apply(&mut decoder, &old, 4);
    assert_eq!(decoder.frame, Some(resized));
}

#[test]
fn parser_rejects_truncation_reserved_flags_and_noncanonical_overlap() {
    let packet = Encoder::default()
        .encode(&scene(32, 32, 1), 1)
        .unwrap()
        .remove(0);
    for length in [0, 3, 31, 32, packet.len() - 1] {
        assert!(parse(&packet[..length]).is_err());
    }
    for offset in [0, 18, 19, 22, 23, 24, 26, 28, 30] {
        let mut bad = packet.clone();
        bad[offset] ^= 0x80;
        assert!(parse(&bad).is_err(), "accepted offset {offset}");
    }
}

#[test]
fn duplicates_conflicts_newer_incomplete_and_expiry_are_bounded() {
    let mut encoder = Encoder::default();
    let old = encoder.encode(&scene(32, 32, 1), 1).unwrap();
    let new_source = scene(32, 32, 2);
    let new = encoder.encode(&new_source, 2).unwrap();
    let mut decoder = Decoder::new();
    assert!(!decoder.ingest(&old[0], 0).unwrap());
    assert!(!decoder.ingest(&old[0], 0).unwrap());
    let mut conflict = old[0].clone();
    conflict[HEADER] ^= 1;
    assert!(decoder.ingest(&conflict, 0).is_err());
    decoder.ingest(&new[0], 1).unwrap();
    for packet in &old[1..] {
        assert!(!decoder.ingest(packet, 1).unwrap());
    }
    apply(&mut decoder, &new[1..], 1);
    assert_eq!(decoder.frame, Some(new_source));
    let packets = encoder.encode(&scene(32, 32, 3), 3).unwrap();
    decoder.ingest(&packets[0], 2).unwrap();
    decoder.advance(5);
    assert_eq!(decoder.assembly_usage(), (0, 0));
}

#[test]
fn malformed_zlib_and_geometry_conflict_do_not_apply_pixels() {
    let mut encoder = Encoder::default();
    let source = Frame::new(32, 32).unwrap();
    let valid = encoder.encode(&source, 1).unwrap().remove(0);
    assert!(valid[19] == 1 || valid[19] == 2);
    let mut bad = valid.clone();
    bad[HEADER] = 0;
    let mut decoder = Decoder::new();
    assert!(decoder.ingest(&bad, 0).is_err());
    assert_eq!(decoder.applied_tiles, 0);
    decoder.ingest(&valid, 1).unwrap();
    let mut resized = encoder
        .encode(&Frame::new(32, 16).unwrap(), 2)
        .unwrap()
        .remove(0);
    resized[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert!(decoder.ingest(&resized, 2).is_err());
    assert_eq!(decoder.frame, Some(source));
}

#[test]
fn many_incomplete_tiles_respect_aggregate_limits() {
    let mut decoder = Decoder::new();
    let packets = Encoder::default().encode(&scene(512, 512, 1), 1).unwrap();
    for packet in &packets {
        if parse(packet).unwrap().index == 0 {
            decoder.ingest(packet, 0).unwrap();
        }
    }
    let (count, bytes) = decoder.assembly_usage();
    assert_eq!(count, MAX_ASSEMBLIES);
    assert!(bytes <= MAX_ASSEMBLY_BYTES);
    decoder.advance(3);
    assert_eq!(decoder.assembly_usage(), (0, 0));
}

#[test]
fn rejected_newer_epoch_preserves_committed_frame_then_valid_resize_recovers() {
    let mut encoder = Encoder::default();
    let mut decoder = Decoder::new();
    let first = scene(32, 32, 31);
    let old_packets = encoder.encode(&first, 1).unwrap();
    apply(&mut decoder, &old_packets, 0);
    let sequences = decoder.sequences().to_vec();
    let next = Frame::new(32, 16).unwrap();
    let new_packets = encoder.encode(&next, 2).unwrap();
    assert!(new_packets[0][19] == 1 || new_packets[0][19] == 2);
    let mut bad = new_packets[0].clone();
    bad[HEADER] = 0;
    assert!(decoder.ingest(&bad, 1).is_err());
    assert_eq!(decoder.frame.as_ref(), Some(&first));
    assert_eq!(decoder.epoch, Some(1));
    assert_eq!(decoder.sequences(), sequences);
    assert_eq!(decoder.assembly_usage(), (0, 0));
    let current_old_geometry = scene(32, 32, 32);
    let mut old_encoder = Encoder::new(1, 0).unwrap();
    apply(
        &mut decoder,
        &old_encoder.encode(&current_old_geometry, 3).unwrap(),
        2,
    );
    assert_eq!(decoder.frame.as_ref(), Some(&current_old_geometry));
    apply(&mut decoder, &new_packets, 3);
    assert_eq!(decoder.frame.as_ref(), Some(&next));
    assert_eq!(decoder.epoch, Some(2));
    apply(&mut decoder, &old_packets, 4);
    assert_eq!(decoder.frame.as_ref(), Some(&next));
    assert_eq!(decoder.assembly_usage(), (0, 0));
}

#[test]
fn incomplete_future_epoch_does_not_allocate_or_replace_committed_frame() {
    let mut encoder = Encoder::default();
    let mut decoder = Decoder::new();
    let current = scene(32, 32, 7);
    apply(&mut decoder, &encoder.encode(&current, 1).unwrap(), 0);
    let future = encoder.encode(&scene(64, 32, 8), 2).unwrap();
    decoder.ingest(&future[0], 1).unwrap();
    assert_eq!(decoder.frame.as_ref(), Some(&current));
    assert_eq!(decoder.epoch, Some(1));
    assert!(decoder.assembly_usage().1 <= 4096);
    decoder.advance(4);
    assert_eq!(decoder.assembly_usage(), (0, 0));
    assert_eq!(decoder.frame.as_ref(), Some(&current));
}

#[test]
fn reversible_planes_and_quantized_signed_inverse_have_declared_bounds() {
    let values = [
        0u8, 1, 7, 8, 15, 16, 17, 31, 32, 63, 127, 128, 191, 239, 247, 248, 254, 255,
    ];
    for r in values {
        for g in values {
            for b in values {
                let pixel = [r, g, b, 13];
                for policy in 0..=2 {
                    let predicted = forward(&pixel, 1, 1, policy).unwrap();
                    let reconstructed = inverse(&predicted, 1, 1, policy).unwrap();
                    let bound = [0, 8, 17][usize::from(policy)];
                    for channel in 0..3 {
                        assert!(
                            pixel[channel].abs_diff(reconstructed[channel]) <= bound,
                            "policy{policy} RGB{pixel:?}->{reconstructed:?}"
                        );
                    }
                    assert_eq!(reconstructed[3], 13);
                }
            }
        }
    }
    for policy in 0..=2 {
        let source = scene(32, 31, 0x243f6a88);
        let predicted = forward(&source.pixels, 32, 31, policy).unwrap();
        let decoded = inverse(&predicted, 32, 31, policy).unwrap();
        for (a, b) in source
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(decoded.as_chunks::<4>().0)
        {
            for channel in 0..3 {
                assert!(a[channel].abs_diff(b[channel]) <= [0, 8, 17][usize::from(policy)]);
            }
            assert_eq!(a[3], b[3]);
        }
    }
}

#[test]
fn every_low_palette_tile_remains_exact_for_all_requested_policies() {
    let mut source = Frame::new(67, 35).unwrap();
    for (index, pixel) in source.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let value = index % 64;
        pixel.copy_from_slice(&[value as u8, (value * 3) as u8, (value * 4) as u8, 255]);
    }
    for policy in 0..=2 {
        let mut encoder = Encoder::new(1, policy).unwrap();
        let packets = encoder.encode(&source, 1).unwrap();
        assert!(packets.iter().all(|p| p[30] == 0));
        let mut decoder = Decoder::new();
        apply(&mut decoder, &packets, 0);
        assert_eq!(decoder.frame.as_ref(), Some(&source));
        assert_eq!(encoder.stats.palette_protected_tiles, 6);
        assert_eq!(
            encoder.stats.palette_protected_pixels,
            u64::from(source.width) * u64::from(source.height)
        );
    }
}

#[test]
fn high_palette_quantized_packets_decode_with_bounds_and_keep_alpha() {
    let source = scene(67, 35, 0x6a09e667);
    for policy in 1..=2 {
        let mut encoder = Encoder::new(1, policy).unwrap();
        let packets = encoder.encode(&source, 1).unwrap();
        assert!(encoder.stats.candidate_trials > encoder.stats.tiles * 3);
        let mut decoder = Decoder::new();
        for packet in packets.iter().rev() {
            decoder.ingest(packet, 0).unwrap();
        }
        let actual = decoder.frame.unwrap();
        for (expected, observed) in source
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(actual.pixels.as_chunks::<4>().0)
        {
            for c in 0..3 {
                assert!(expected[c].abs_diff(observed[c]) <= if policy == 1 { 8 } else { 17 });
            }
            assert_eq!(expected[3], observed[3]);
        }
    }
}

#[test]
fn malformed_transforms_and_policy_flags_are_rejected() {
    assert!(Encoder::new(1, 3).is_err());
    let packet = Encoder::default()
        .encode(&scene(32, 32, 13), 1)
        .unwrap()
        .remove(0);
    for (offset, value) in [(19, 3), (30, 3), (31, 1)] {
        let mut bad = packet.clone();
        bad[offset] = value;
        assert!(parse(&bad).is_err());
    }
    let mut bad = packet.clone();
    bad[19] = 0;
    bad[30] = 1;
    assert!(parse(&bad).is_err());
    assert!(inverse(&[0, 0, 0, 255], 1, 1, 1).is_err());
    assert!(inverse(&[0, 255, 255, 255], 1, 1, 2).is_err());
    assert!(forward(&[0; 4], 33, 1, 0).is_err());
}

#[test]
fn legacy_default_packets_match_the_actual_measured_codec_for_every_policy() {
    for policy in 0..=2 {
        let mut current = Encoder::new(u32::MAX, policy).unwrap();
        let mut measured = gliss_adaptive_tiles_experiment::Encoder::new(u32::MAX, policy).unwrap();
        let mut current_decoder = Decoder::new();
        let mut measured_decoder = gliss_adaptive_tiles_experiment::Decoder::new();
        for step in 0..9 {
            let (width, height) = if step < 5 { (67, 35) } else { (98, 66) };
            let source = scene(width, height, 41 + step);
            let sequence = u32::MAX.wrapping_sub(3).wrapping_add(step);
            let actual = current.encode(&source, sequence).unwrap();
            let expected = measured.encode(&source, sequence).unwrap();
            assert_eq!(actual, expected, "policy{policy} step{step}");
            for packet in &actual {
                current_decoder.ingest(packet, u64::from(step)).unwrap();
                measured_decoder.ingest(packet, u64::from(step)).unwrap();
            }
            assert_eq!(current_decoder.frame, measured_decoder.frame);
        }
    }
}

#[test]
fn all_twelve_archived_native_fixtures_keep_exact_packets_and_rgba_hashes() {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let compressed = include_bytes!("../../../test-support/rust/tiles/fixtures.json.gz");
    assert_eq!(
        format!("{:x}", Sha256::digest(compressed)),
        "53ff72be77e621e15281d099d985da5271a0d07f2f6a80e99c9edb366303d9b9"
    );
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(1_137_199)
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes.len(), 1_137_198);
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "68f46a1cca18b67dab63931ed48f6a293b8e378cdde37e0936a7927c2f318106"
    );
    let fixture: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let steps = fixture["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 12);
    for policy in 0..=2 {
        let mut encoder = Encoder::new(u32::MAX, policy).unwrap();
        let mut decoder = Decoder::new();
        let mut old_packets = Vec::new();
        for index in 0..4 {
            let step = &steps[usize::from(policy) * 4 + index];
            assert_eq!(step["policy"], policy);
            assert_eq!(step["step"], index);
            let expected: Vec<Vec<u8>> = serde_json::from_value(step["packets"].clone()).unwrap();
            let actual = if index < 3 {
                let (width, height, sequence) =
                    [(67, 35, u32::MAX), (67, 35, 0), (48, 34, 1)][index];
                let mut source = scene(width, height, 23 + index as u32);
                if index == 1 {
                    for (offset, pixel) in
                        source.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate()
                    {
                        let x = offset % usize::from(width);
                        let y = offset / usize::from(width);
                        pixel.copy_from_slice(&[
                            (x * 3) as u8,
                            (y * 7) as u8,
                            ((x + y) * 2) as u8,
                            255,
                        ]);
                    }
                }
                assert_eq!(
                    format!("{:x}", Sha256::digest(&source.pixels)),
                    step["source_rgba_sha256"].as_str().unwrap()
                );
                let mut packets = encoder.encode(&source, sequence).unwrap();
                if index == 0 {
                    old_packets = packets.clone();
                }
                packets.reverse();
                packets
            } else {
                old_packets.clone()
            };
            assert_eq!(actual, expected, "policy{policy} archived step{index}");
            apply(&mut decoder, &actual, index as u64);
            let frame = decoder.frame.as_ref().unwrap();
            assert_eq!(u64::from(frame.width), step["width"].as_u64().unwrap());
            assert_eq!(u64::from(frame.height), step["height"].as_u64().unwrap());
            assert_eq!(
                format!("{:x}", Sha256::digest(&frame.pixels)),
                step["rgba_sha256"].as_str().unwrap()
            );
        }
        assert!(encoder.stats.selected_encoding_policy[2][0] > 0);
        assert!(policy == 0 || encoder.stats.selected_encoding_policy[2][usize::from(policy)] > 0);
    }
}

#[test]
fn geometry_extremes_have_bounded_partial_tiles_before_allocation() {
    for (width, height) in [(1, 1), (1, 2160), (3840, 1), (3840, 2160)] {
        let tile = tile_count(width, height) - 1;
        let (_, _, w, h) = rect(width, height, tile);
        let encoded = EncodedTile {
            epoch: 1,
            sequence: 1,
            width,
            height,
            tile,
            kind: 0,
            policy: 0,
            raw: w * h * 4,
            body: vec![137; w * h * 4],
        };
        for stride in [MIN_PAYLOAD, PAYLOAD] {
            for packet in encoded.fragments(stride).unwrap() {
                let header = parse_with_payload(&packet, stride).unwrap();
                assert_eq!(header.tile, tile);
                assert_eq!(header.raw, w * h * 4);
                assert!(header.raw <= TILE * TILE * 4);
            }
        }
    }
    for (width, height) in [(0, 1), (1, 0), (3841, 1), (1, 2161)] {
        let encoded = EncodedTile {
            epoch: 1,
            sequence: 1,
            width,
            height,
            tile: 0,
            kind: 0,
            policy: 0,
            raw: 4,
            body: vec![0; 4],
        };
        assert!(encoded.fragments(PAYLOAD).is_err());
    }
}

#[test]
fn negotiated_stride_is_canonical_at_minimum_midpoint_and_legacy_maximum() {
    let source = scene(67, 35, 41);
    for payload in [MIN_PAYLOAD, 1100, PAYLOAD] {
        let mut encoder = Encoder::with_payload(1, 0, payload).unwrap();
        let mut decoder = Decoder::with_payload(payload).unwrap();
        let packets = encoder.encode(&source, 1).unwrap();
        for packet in packets.iter().rev() {
            assert!(packet.len() <= HEADER + payload);
            let header = parse_with_payload(packet, payload).unwrap();
            assert_eq!(header.offset, header.index * payload);
            assert_eq!(header.count, header.total.div_ceil(payload));
            decoder.ingest(packet, 0).unwrap();
        }
        assert_eq!(decoder.frame.as_ref(), Some(&source));
    }
    for payload in [0, MIN_PAYLOAD - 1, PAYLOAD + 1, usize::MAX] {
        assert!(Encoder::with_payload(1, 0, payload).is_err());
        assert!(Decoder::with_payload(payload).is_err());
        assert!(parse_with_payload(&[], payload).is_err());
    }
    let packets = Encoder::new(1, 0)
        .unwrap()
        .encode(&scene(32, 32, 3), 1)
        .unwrap();
    assert!(parse_with_payload(&packets[0], MIN_PAYLOAD).is_err());
}

fn live(
    encoder: &mut Encoder,
    frame: &Frame,
    sequence: u32,
    new_source: bool,
    quota: usize,
    force: bool,
) -> (EncodeSummary, Vec<EncodedTile>) {
    let mut tiles = Vec::new();
    let summary = encoder
        .encode_with_refresh_budget(frame, sequence, new_source, quota, force, |tile| {
            tiles.push(tile);
            Ok(())
        })
        .unwrap();
    (summary, tiles)
}

#[test]
fn idle_quota_has_no_full_scan_or_clone_and_coalesces_with_dirty_work() {
    let mut encoder = Encoder::new(1, 0).unwrap();
    let mut source = scene(320, 160, 123);
    let (first, tiles) = live(&mut encoder, &source, 1, true, 0, false);
    assert_eq!(tiles.len(), 50);
    assert_eq!(first.reference_cloned_bytes, source.pixels.len());
    let (same, tiles) = live(&mut encoder, &source, 2, true, 0, false);
    assert!(tiles.is_empty());
    assert_eq!(same.source_tiles_compared, 50);
    assert_eq!(encoder.refresh_cursor(), 0);
    for sequence in 3..23 {
        let (idle, tiles) = live(&mut encoder, &source, sequence, false, 0, false);
        assert!(tiles.is_empty());
        assert_eq!(
            (idle.source_tiles_compared, idle.reference_cloned_bytes),
            (0, 0)
        );
        assert_eq!(encoder.refresh_cursor(), 0);
    }
    let (idle, tiles) = live(&mut encoder, &source, 23, false, 4, false);
    assert_eq!(
        tiles.iter().map(|tile| tile.tile).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(
        (idle.source_tiles_compared, idle.reference_cloned_bytes),
        (0, 0)
    );
    source.pixels[5 * 32 * 4] ^= 1;
    let (coalesced, tiles) = live(&mut encoder, &source, 24, true, 4, false);
    assert_eq!(coalesced.dirty_tiles, 1);
    assert_eq!(coalesced.refresh_tiles, 4);
    assert_eq!(coalesced.selected_tiles, 4);
    assert_eq!(
        tiles.iter().map(|tile| tile.tile).collect::<Vec<_>>(),
        [4, 5, 6, 7]
    );
    assert_eq!(
        (
            coalesced.refresh_cursor_before,
            coalesced.refresh_cursor_after
        ),
        (4, 8)
    );
}

#[test]
fn static_final_change_repairs_with_only_current_state_quota_ticks() {
    let mut encoder = Encoder::with_payload(1, 0, MIN_PAYLOAD).unwrap();
    let mut decoder = Decoder::with_payload(MIN_PAYLOAD).unwrap();
    let mut source = Frame::new(128, 128).unwrap();
    let (_, tiles) = live(&mut encoder, &source, 1, true, 0, false);
    for tile in tiles {
        for packet in tile.fragments(MIN_PAYLOAD).unwrap() {
            decoder.ingest(&packet, 0).unwrap();
        }
    }
    for y in 96..128 {
        for x in 96..128 {
            source.pixels[(y * 128 + x) * 4..(y * 128 + x) * 4 + 4].fill(255);
        }
    }
    let (_, changed) = live(&mut encoder, &source, 2, true, 0, false);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].tile, 15);
    // Deliberately discard the final changed update; no later capture occurs.
    assert_ne!(decoder.frame.as_ref(), Some(&source));
    for step in 1..=8 {
        let (summary, tiles) = live(&mut encoder, &source, 2 + step, false, 2, false);
        assert_eq!(
            (
                summary.source_tiles_compared,
                summary.reference_cloned_bytes
            ),
            (0, 0)
        );
        for tile in tiles {
            for packet in tile.fragments(MIN_PAYLOAD).unwrap() {
                decoder.ingest(&packet, u64::from(step)).unwrap();
            }
        }
    }
    assert_eq!(decoder.frame.as_ref(), Some(&source));
}

#[test]
fn forced_current_state_resync_wraps_epoch_and_resize_only_bumps_once() {
    let mut encoder = Encoder::new(u32::MAX, 0).unwrap();
    let mut decoder = Decoder::new();
    let initial = scene(32, 32, 2);
    let (_, tiles) = live(&mut encoder, &initial, u32::MAX, true, 0, false);
    let mut old = Vec::new();
    for tile in tiles {
        old.extend(tile.fragments(PAYLOAD).unwrap());
    }
    apply(&mut decoder, &old, 0);
    let (resync, tiles) = live(&mut encoder, &initial, 0, false, 0, true);
    assert_eq!(resync.epoch, 0);
    assert_eq!(encoder.epoch(), 0);
    for tile in tiles {
        apply(&mut decoder, &tile.fragments(PAYLOAD).unwrap(), 1);
    }
    apply(&mut decoder, &old, 2);
    assert_eq!(decoder.epoch(), Some(0));
    let resized = scene(64, 32, 3);
    let (changed, tiles) = live(&mut encoder, &resized, 1, true, 0, true);
    assert_eq!(changed.epoch, 1);
    for tile in tiles {
        apply(&mut decoder, &tile.fragments(PAYLOAD).unwrap(), 3);
    }
    apply(&mut decoder, &old, 4);
    assert_eq!(decoder.frame, Some(resized));
    assert_eq!(encoder.dimensions(), Some((64, 32)));
}

#[test]
fn callback_failure_then_forced_resync_never_reuses_emitted_epoch() {
    let mut encoder = Encoder::new(u32::MAX, 0).unwrap();
    let first = scene(32, 32, 7);
    live(&mut encoder, &first, 1, true, 0, false);
    let resized = scene(64, 32, 8);
    let mut attempted_epoch = None;
    let error = encoder.encode_with_refresh_budget(&resized, 2, true, 0, true, |tile| {
        attempted_epoch = Some(tile.epoch);
        anyhow::bail!("closed pending queue")
    });
    assert!(error.is_err());
    assert_eq!(attempted_epoch, Some(0));
    assert_eq!(encoder.epoch(), 0);
    assert_eq!(encoder.dimensions(), Some((32, 32)));
    let (summary, tiles) = live(&mut encoder, &resized, 3, true, 0, true);
    assert_eq!(summary.epoch, 1);
    assert!(tiles.iter().all(|tile| tile.epoch == 1));
    assert_eq!(encoder.dimensions(), Some((64, 32)));
}

#[test]
fn memory_validation_precedes_frame_copies_output_and_epoch_changes() {
    let source = scene(64, 32, 2);
    let estimate = memory_estimate(source.width, source.height, 0).unwrap();
    let mut encoder =
        Encoder::with_memory_limit(1, 0, PAYLOAD, estimate.total_codec_bytes - 1).unwrap();
    let mut emitted = false;
    assert!(
        encoder
            .encode_with_refresh_budget(&source, 1, true, 1, true, |_| {
                emitted = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!emitted);
    assert_eq!(encoder.dimensions(), None);
    assert_eq!(encoder.epoch(), 1);
    assert_eq!(encoder.stats.tiles, 0);
    assert!(memory_estimate(3840, 2160, 0).unwrap().total_codec_bytes < MAX_CODEC_BYTES);
    assert_eq!(tile_count(3840, 2160), 8160);
    assert!(memory_estimate(3841, 2160, 0).is_err());
    assert!(memory_estimate(64, 32, usize::MAX).is_err());
    let valid = Encoder::default().encode(&source, 1).unwrap();
    let mut decoder = Decoder::with_memory_limit(PAYLOAD, 128 * 1024).unwrap();
    assert!(decoder.ingest(&valid[0], 0).is_err());
    assert_eq!(decoder.frame, None);
    assert_eq!(decoder.epoch(), None);
    assert_eq!(decoder.assembly_usage(), (0, 0));
    let mut invalid = source.clone();
    invalid.pixels.pop();
    assert!(Encoder::default().encode(&invalid, 1).is_err());
}

#[test]
fn complete_tile_validation_and_transport_admission_charge_are_bounded() {
    let mut encoder = Encoder::new(1, 0).unwrap();
    let (_, mut tiles) = live(&mut encoder, &scene(32, 32, 15), 1, true, 0, false);
    let tile = tiles.pop().unwrap();
    assert_eq!(tile.body.capacity(), tile.body.len());
    let count = tile.fragment_count(MIN_PAYLOAD).unwrap();
    assert_eq!(
        tile.wire_bytes(MIN_PAYLOAD).unwrap(),
        tile.body.len() + count * 32
    );
    assert_eq!(
        tile.admission_bytes(MIN_PAYLOAD).unwrap(),
        tile.body.len() + count * (32 + 8 + 128)
    );
    assert_eq!(
        tile.fragments(MIN_PAYLOAD)
            .unwrap()
            .iter()
            .map(Vec::len)
            .sum::<usize>(),
        tile.wire_bytes(MIN_PAYLOAD).unwrap()
    );
    let mut bad = tile.clone();
    bad.tile = usize::MAX;
    assert!(bad.fragments(PAYLOAD).is_err());
    let mut bad = tile.clone();
    bad.raw = 1;
    assert!(bad.fragments(PAYLOAD).is_err());
    let mut bad = tile;
    bad.kind = 0;
    bad.policy = 1;
    assert!(bad.fragments(PAYLOAD).is_err());
}
