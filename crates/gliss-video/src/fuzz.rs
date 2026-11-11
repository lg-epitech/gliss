//! Fuzzes the GLV1 packet parser and the packets the encoder writes.
//!
//! Packets are built from the grammar in the README, changed byte by byte and
//! NAL by NAL, and given to `parse_packet` next to a second reading of the
//! same rules. `tests/fuzz-video.test.mjs` gives the browser's parser the
//! same bytes from a vector file and compares its verdict with the one here.

use std::io::Write;

use super::*;

#[allow(dead_code)]
#[path = "../../../test-support/rust/fuzz.rs"]
mod support;

use support::Rng;

/// Every way `parse_packet` refuses a packet.
const REFUSALS: [&str; 9] = [
    "Invalid GLV1 packet size",
    "Unknown video wire version",
    "H.264 dimensions must be at least 16x16",
    "H.264 YUV420 requires even width and height; no implicit crop or padding",
    "OpenH264 dimensions exceed 3840x2160 / 2160x3840",
    "Invalid video flags/reserved field",
    "Video payload length mismatch",
    "Video payload is not Annex B",
    "Invalid IDR or parameter set declaration",
];

/// What a packet says about itself.
#[derive(Debug, PartialEq, Eq)]
struct Fields {
    sequence: u32,
    input_id: u32,
    width: u16,
    height: u16,
    keyframe: bool,
}

/// The type of every NAL unit in an Annex B stream: the low five bits of the
/// byte that follows each start code, which is a one after two zeros or more.
fn nal_types(payload: &[u8]) -> Vec<u8> {
    (2..payload.len().saturating_sub(1))
        .filter(|at| payload[*at] == 1 && payload[at - 1] == 0 && payload[at - 2] == 0)
        .map(|at| payload[at + 1] & 31)
        .collect()
}

/// The packet's fields if it is one the README's table and rules allow.
fn reference(packet: &[u8]) -> Option<Fields> {
    let field = |at: usize| u32::from(packet[at]) | u32::from(packet[at + 1]) << 8;
    let long = |at: usize| field(at) | field(at + 2) << 16;
    if packet.len() < 24 || packet.len() > 24 + 16 * 1024 * 1024 || &packet[..4] != b"GLV1" {
        return None;
    }
    let (width, height) = (field(12), field(14));
    let landscape = width.max(height) <= 3840 && width.min(height) <= 2160;
    if width < 16 || height < 16 || width % 2 != 0 || height % 2 != 0 || !landscape {
        return None;
    }
    let flags = field(18);
    let payload = &packet[24..];
    if field(16) != 0 || flags > 1 || long(20) as usize != payload.len() {
        return None;
    }
    // One access unit: it begins with a start code and a NAL unit.
    let begins = payload.starts_with(&[0, 0, 1]) && payload.len() > 3
        || payload.starts_with(&[0, 0, 0, 1]) && payload.len() > 4;
    let types = nal_types(payload);
    let has = |kind: u8| types.contains(&kind);
    // A keyframe is an IDR slice with its parameter sets; anything else is
    // a slice that is not one.
    let keyframe = flags == 1;
    if !begins || has(5) != keyframe || !(has(1) || has(5)) || keyframe && !(has(7) && has(8)) {
        return None;
    }
    Some(Fields {
        sequence: long(4),
        input_id: long(8),
        width: width as u16,
        height: height as u16,
        keyframe,
    })
}

/// A NAL unit of the given type behind a start code of either length.
fn nal(rng: &mut Rng, kind: u8) -> Vec<u8> {
    let mut unit = if rng.chance(2) {
        vec![0, 0, 1]
    } else {
        vec![0, 0, 0, 1]
    };
    // The two bits above the type say whether other pictures refer to this.
    unit.push(kind | (rng.below(4) as u8) << 5);
    if kind == 7 {
        // Baseline profile, as the browser's parser asks.
        unit.extend_from_slice(&[66, 0xc0, 0x1e]);
    }
    for _ in 0..rng.below(12) {
        // Zeros come in runs in real streams, with the byte that keeps them
        // from reading as a start code.
        match rng.below(8) {
            0 => unit.extend_from_slice(&[0, 0, 3]),
            1 => unit.push(0),
            _ => unit.push(rng.between(2, 255) as u8),
        }
    }
    // A unit ends in a byte that is not zero.
    unit.push(0x80);
    unit
}

/// A GLV1 packet around `payload`.
fn framed(rng: &mut Rng, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let side = |rng: &mut Rng, most: usize| 2 * rng.between(8, most / 2) as u16;
    let (width, height) = match rng.below(6) {
        0 => (3840, 2160),
        1 => (2160, 3840),
        2 => (16, 16),
        _ => (side(rng, 3840), side(rng, 2160)),
    };
    let mut packet = b"GLV1".to_vec();
    packet.extend_from_slice(&(rng.next() as u32).to_le_bytes());
    packet.extend_from_slice(&(rng.next() as u32).to_le_bytes());
    packet.extend_from_slice(&width.to_le_bytes());
    packet.extend_from_slice(&height.to_le_bytes());
    packet.extend_from_slice(&[0, 0, u8::from(keyframe), 0]);
    packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    packet.extend_from_slice(payload);
    packet
}

/// A valid packet: a keyframe with its parameter sets, or a delta frame, and
/// units a decoder skips in between.
fn generated(rng: &mut Rng) -> Vec<u8> {
    let keyframe = rng.chance(2);
    let mut kinds = if keyframe { vec![7, 8, 5] } else { vec![1] };
    for _ in 0..rng.below(3) {
        let at = rng.below(kinds.len() + 1);
        kinds.insert(at, rng.pick(&[6, 9, 12, 1, 0, 31]));
    }
    if keyframe && rng.chance(4) {
        kinds.push(5);
    }
    let payload: Vec<u8> = kinds.into_iter().flat_map(|kind| nal(rng, kind)).collect();
    framed(rng, keyframe, &payload)
}

/// Offsets of the fields of the header, and of the payload's first bytes.
const FIELDS: [usize; 11] = [0, 4, 8, 12, 14, 16, 18, 20, 24, 27, 28];

/// One change to a packet: to its bytes, or to the units of its payload.
fn mutate(rng: &mut Rng, packet: &mut Vec<u8>) {
    if packet.len() <= 24 || rng.chance(2) {
        support::mutate(rng, packet, &FIELDS, false);
        return;
    }
    if rng.chance(6) {
        // A size on one of the limits or just past it.
        let (width, height): (u16, u16) = rng.pick(&[
            (16, 16),
            (14, 16),
            (16, 14),
            (17, 16),
            (16, 17),
            (3840, 2160),
            (2160, 3840),
            (3842, 2160),
            (3840, 2162),
            (2162, 2162),
            (0, 0),
        ]);
        packet[12..14].copy_from_slice(&width.to_le_bytes());
        packet[14..16].copy_from_slice(&height.to_le_bytes());
        return;
    }
    let at = rng.between(24, packet.len());
    let kind = rng.pick(&[1, 5, 7, 8]);
    let unit = match rng.below(8) {
        // A start code with nothing behind it, at any place.
        0 => vec![0, 0, 1],
        1 => vec![0, 0, 0, 1],
        // A unit of a type the rules count.
        2..5 => nal(rng, kind),
        // A header with the bit set that must be zero.
        5 => vec![0, 0, 1, 0x80 | kind, 0x80],
        // A parameter set of another profile, and one cut short.
        6 => vec![0, 0, 1, 0x67, rng.pick(&[77, 100, 66]), 0x80],
        _ => vec![0, 0, 1, 0x67, 66],
    };
    packet.splice(at..at, unit);
    if !rng.chance(4) {
        // Most of the time the header still tells the truth about the length.
        let length = (packet.len() - 24) as u32;
        packet[20..24].copy_from_slice(&length.to_le_bytes());
    }
    if rng.chance(4) {
        packet[18] ^= 1;
    }
}

/// Parses a packet and checks the result against the reference. Returns the
/// line of a vector file: the packet, and its fields or that it is refused.
fn checked(packet: &[u8]) -> String {
    let expected = reference(packet);
    let verdict = match parse_packet(packet) {
        Ok(parsed) => {
            let fields = Fields {
                sequence: parsed.sequence,
                input_id: parsed.input_id,
                width: parsed.width,
                height: parsed.height,
                keyframe: parsed.keyframe,
            };
            assert_eq!(Some(&fields), expected.as_ref(), "{packet:?}");
            assert_eq!(parsed.payload, &packet[24..]);
            format!(
                "{} {} {} {} {}",
                fields.sequence,
                fields.input_id,
                fields.width,
                fields.height,
                u8::from(fields.keyframe)
            )
        }
        Err(error) => {
            assert_eq!(expected, None, "{error}: {packet:?}");
            assert!(REFUSALS.contains(&error.to_string().as_str()), "{error}");
            "refused".to_owned()
        }
    };
    format!("{} {verdict}\n", support::hex(packet))
}

const DEFAULT_ITERATIONS: u64 = 2000;

#[test]
fn fuzz_packet_parser() {
    let mut vectors = support::campaign_vectors("glv1-vectors.txt");
    support::run(
        "video-packet-parser",
        DEFAULT_ITERATIONS,
        |rng, iteration| {
            let mut packet = generated(rng);
            assert!(reference(&packet).is_some(), "{packet:?}");
            let mut lines = checked(&packet);
            for _ in 0..3 {
                for _ in 0..1 + rng.below(3) {
                    mutate(rng, &mut packet);
                }
                lines.push_str(&checked(&packet));
            }
            if let Some(file) = &mut vectors {
                file.write_all(lines.as_bytes()).unwrap();
            }
            if iteration == 0 {
                // A payload one byte over the limit, and one at it that is not
                // a picture.
                let mut largest = vec![0; 24 + MAX_PAYLOAD + 1];
                largest[..4].copy_from_slice(b"GLV1");
                largest[12..16].copy_from_slice(&[16, 0, 16, 0]);
                largest[20..24].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
                assert_eq!(
                    parse_packet(&largest).unwrap_err().to_string(),
                    "Invalid GLV1 packet size"
                );
                largest.pop();
                largest[20..24].copy_from_slice(&(MAX_PAYLOAD as u32).to_le_bytes());
                assert_eq!(
                    parse_packet(&largest).unwrap_err().to_string(),
                    "Video payload is not Annex B"
                );
            }
        },
    );
}

/// A frame of noise, of flat colour or of stripes.
fn picture(rng: &mut Rng, width: u16, height: u16) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    let kind = rng.below(3);
    let colour = [rng.byte(), rng.byte(), rng.byte(), 255];
    for (index, pixel) in frame.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *pixel = match kind {
            0 => [rng.byte(), rng.byte(), rng.byte(), 255],
            1 => colour,
            _ if index % 7 < 3 => colour,
            _ => [0, 0, 0, 255],
        };
    }
    frame
}

/// Encodes a few frames with resets and resizes in between and checks what
/// each packet says. Returns the packets as lines of a vector file.
fn encoded(rng: &mut Rng, frames: usize) -> String {
    let sizes = [(16, 16), (32, 16), (16, 34), (48, 32), (64, 48)];
    let (mut width, mut height) = rng.pick(&sizes);
    // A keyframe interval the run does not reach, or one it reaches often.
    // The encoder may start a keyframe sooner on its own: in screen-content
    // mode it does at a change of scene, which noise after stripes is.
    let period = rng.pick(&[3600, 3600, 1, 2, 3]);
    let timing = VideoTiming::new(rng.between(1, 60) as u32, period).unwrap();
    let mut encoder = VideoEncoder::with_timing(width, height, 1_000_000, timing).unwrap();
    let mut scene = picture(rng, width, height);
    // Whether the next packet has to stand on its own.
    let mut fresh = true;
    let mut since_keyframe = 0;
    let mut lines = String::new();
    for _ in 0..frames {
        match rng.below(8) {
            0 => {
                encoder.reset();
                fresh = true;
            }
            1 => {
                let size = rng.pick(&sizes);
                fresh |= size != (width, height);
                (width, height) = size;
            }
            2 => {
                // A size that cannot be encoded leaves the encoder as it was.
                let odd = Frame::new(width + 1, height).unwrap();
                assert!(encoder.encode(&odd, 0, 0).is_err());
            }
            _ => {}
        }
        if (scene.width, scene.height) != (width, height) || rng.chance(5) {
            scene = picture(rng, width, height);
        } else {
            // The same scene with a few pixels changed, which the encoder
            // answers with a delta frame unless it was told otherwise.
            for _ in 0..rng.below(4) {
                let at = rng.below(scene.pixels.len() / 4) * 4;
                scene.pixels[at] = rng.byte();
            }
        }
        let (sequence, input_id) = (rng.next() as u32, rng.next() as u32);
        let packet = encoder.encode(&scene, sequence, input_id).unwrap();
        let line = checked(&packet);
        let parsed = parse_packet(&packet).unwrap();
        assert_eq!(
            (
                parsed.sequence,
                parsed.input_id,
                parsed.width,
                parsed.height
            ),
            (sequence, input_id, width, height)
        );
        let types = nal_types(parsed.payload);
        if parsed.keyframe {
            assert!(types.contains(&7) && types.contains(&8) && types.contains(&5));
            since_keyframe = 0;
        } else {
            assert!(
                !fresh,
                "the first packet after a reset or resize is not a keyframe"
            );
            assert!(types.contains(&1) && !types.contains(&5));
            since_keyframe += 1;
            assert!(since_keyframe < period, "no keyframe in {period} frames");
        }
        fresh = false;
        lines.push_str(&line);
    }
    lines
}

/// Encoder runs on every run. Each builds a native encoder and encodes up
/// to eight small frames.
const DEFAULT_ENCODER_ITERATIONS: u64 = 40;

#[test]
fn fuzz_encoded_packets() {
    let mut vectors = support::campaign_vectors("glv1-encoded-vectors.txt");
    support::run(
        "video-encoded-packets",
        DEFAULT_ENCODER_ITERATIONS,
        |rng, _| {
            let frames = rng.between(1, 8);
            let lines = encoded(rng, frames);
            if let Some(file) = &mut vectors {
                file.write_all(lines.as_bytes()).unwrap();
            }
        },
    );
}

#[test]
fn the_vectors_for_the_browser_parser_are_current() {
    // The same packets on every run: the cases the Node suite checks without
    // a campaign. Real packets first, then generated and changed ones.
    let mut rng = Rng::new(0x67_6c_76_31);
    let mut vectors = encoded(&mut rng, 4);
    for _ in 0..40 {
        let mut packet = generated(&mut rng);
        vectors.push_str(&checked(&packet));
        for _ in 0..3 {
            mutate(&mut rng, &mut packet);
            vectors.push_str(&checked(&packet));
        }
    }
    support::assert_vectors("glv1-vectors.txt", &vectors);
}
