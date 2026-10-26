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
