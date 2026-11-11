//! Fuzzes the clipboard as a state machine: capability, notify, request,
//! provide and base-protocol messages from the server in any order and with
//! any bytes, between copies made on the client's side.
//!
//! The real [`Clipboard`] and the model in [`super::model`] are given the
//! same steps and must agree on every reply, every text and every refusal.

use super::{
    model::{
        self, CLIPBOARD_CAPS, CLIPBOARD_NOTIFY, CLIPBOARD_PEEK, CLIPBOARD_PROVIDE,
        CLIPBOARD_REQUEST, CLIPBOARD_TEXT, ClipboardModel, MAX_CLIPBOARD, MAX_WIRE_TEXT, Piece,
    },
    stream::{deflated, text},
    support::{self, Rng},
};
use crate::clipboard::{Clipboard, latin1_text};

/// What handling a message may allocate at once when it carries no text: the
/// inflater's own state is the largest of it.
const SMALL_ALLOCATION: usize = 131_072;

/// A zlib stream that inflates to `count` zero bytes, written by hand: one
/// literal, then copies of the byte before at 258 bytes for 13 bits. It is
/// over a hundred times smaller than what it claims to hold.
fn zeros(count: usize) -> Vec<u8> {
    assert!(count > 0);
    struct Bits(Vec<u8>, u32);
    impl Bits {
        /// A Huffman code, which is written from its highest bit.
        fn code(&mut self, code: u32, length: u32) {
            for bit in (0..length).rev() {
                if self.1 == 0 {
                    self.0.push(0);
                }
                *self.0.last_mut().unwrap() |= ((code >> bit & 1) as u8) << self.1;
                self.1 = (self.1 + 1) % 8;
            }
        }
    }
    let mut bits = Bits(vec![0x78, 0x01], 0);
    // The last block, with the fixed codes. Header bits go lowest first.
    bits.code(0b110, 3);
    let literal = |bits: &mut Bits| bits.code(0b0011_0000, 8);
    literal(&mut bits);
    for _ in 0..(count - 1) / 258 {
        // Length 258 is code 285, and distance 1 is code 0.
        bits.code(0b1100_0101, 8);
        bits.code(0, 5);
    }
    for _ in 0..(count - 1) % 258 {
        literal(&mut bits);
    }
    // End of block.
    bits.code(0, 7);
    let mut stream = bits.0;
    // Adler-32 of zeros: the low sum stays 1 and the high sum counts bytes.
    let checksum = ((count % 65_521) as u32) << 16 | 1;
    stream.extend_from_slice(&checksum.to_be_bytes());
    stream
}

#[test]
fn the_hand_written_stream_inflates_to_zeros() {
    for count in [1, 2, 258, 259, 260, 1000, 70_000] {
        use std::io::Read;
        let mut inflated = Vec::new();
        flate2::read::ZlibDecoder::new(zeros(count).as_slice())
            .read_to_end(&mut inflated)
            .unwrap();
        assert_eq!(inflated, vec![0; count]);
    }
    assert!(zeros(8 * 1024 * 1024).len() < 65_536);
}

fn extended(flags: u32, data: &[u8]) -> Vec<u8> {
    [flags.to_be_bytes().as_slice(), data].concat()
}

fn provide(rng: &mut Rng, text: &[u8]) -> Vec<u8> {
    let mut payload = (text.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(text);
    extended(CLIPBOARD_PROVIDE | CLIPBOARD_TEXT, &deflated(rng, &payload))
}

/// Text of megabytes that a server provides: at the limit, a byte over it,
/// and at the limit only once its line ends are converted.
fn large_provide(rng: &mut Rng) -> Vec<u8> {
    let text = match rng.below(5) {
        0 => "a".repeat(MAX_CLIPBOARD),
        1 => "a".repeat(MAX_CLIPBOARD + 1),
        2 => "a".repeat(MAX_CLIPBOARD) + "\0",
        3 => "\r\n".repeat(MAX_CLIPBOARD) + "\0",
        _ => "\r\n".repeat(MAX_CLIPBOARD) + "a\0",
    };
    provide(rng, text.as_bytes())
}

/// An extended clipboard message a server may send, valid most of the time.
/// One time in `large` it provides megabytes of text.
fn message(rng: &mut Rng, large: usize) -> Vec<u8> {
    if rng.chance(large) {
        return large_provide(rng);
    }
    let formats = rng.next() as u32 & 0xffff;
    let mut message = match rng.below(17) {
        // Capabilities that allow the extended clipboard, and some that do not.
        0..3 => {
            let flags = CLIPBOARD_CAPS | (rng.next() as u32 & 0x1e00_0000) | formats;
            extended(flags, &rng.bytes(formats.count_ones() as usize * 4))
        }
        3 => extended(
            CLIPBOARD_CAPS
                | CLIPBOARD_NOTIFY
                | CLIPBOARD_PROVIDE
                | CLIPBOARD_REQUEST
                | CLIPBOARD_TEXT,
            &[0, 0, 0, 0],
        ),
        4 | 5 => extended(CLIPBOARD_NOTIFY | CLIPBOARD_TEXT, &[]),
        6 => extended(CLIPBOARD_NOTIFY | formats, &[]),
        7 | 8 => extended(CLIPBOARD_REQUEST | CLIPBOARD_TEXT, &[]),
        9 => extended(CLIPBOARD_REQUEST | formats, &[]),
        10..14 => {
            let text = text(rng);
            provide(rng, text.as_bytes())
        }
        // Text that is not text: any bytes behind a true length.
        14 => {
            let length = rng.below(12);
            let bytes = rng.bytes(length);
            provide(rng, &bytes)
        }
        // Text behind a length that is one too short or too long.
        15 => {
            let text = text(rng) + "a";
            let length = (text.len() as u32).wrapping_add(rng.pick(&[1, u32::MAX]));
            let mut payload = length.to_be_bytes().to_vec();
            payload.extend_from_slice(text.as_bytes());
            extended(CLIPBOARD_PROVIDE | CLIPBOARD_TEXT, &deflated(rng, &payload))
        }
        _ => {
            let length = rng.below(24);
            extended(
                rng.pick(&[
                    CLIPBOARD_PEEK,
                    CLIPBOARD_PROVIDE | 2,
                    CLIPBOARD_PROVIDE | CLIPBOARD_TEXT | 2,
                    CLIPBOARD_PROVIDE,
                    CLIPBOARD_NOTIFY | CLIPBOARD_REQUEST | CLIPBOARD_TEXT,
                    1 << 29,
                    0,
                ]),
                &rng.bytes(length),
            )
        }
    };
    if rng.chance(4) {
        for _ in 0..1 + rng.below(3) {
            support::mutate(rng, &mut message, &[0, 4, 6], true);
        }
    }
    message
}

/// A message whose compressed text claims megabytes: over the limit, at it,
/// and far beyond it.
fn bomb(rng: &mut Rng) -> Vec<u8> {
    let count = rng.pick(&[
        MAX_WIRE_TEXT + 4,
        MAX_WIRE_TEXT + 5,
        MAX_WIRE_TEXT + 6,
        MAX_WIRE_TEXT * 2,
        256 * 1024 * 1024,
    ]);
    extended(CLIPBOARD_PROVIDE | CLIPBOARD_TEXT, &zeros(count))
}

/// Text copied on the client's side. One time in `large` it is megabytes
/// long, at one of the limits.
fn copied(rng: &mut Rng, large: usize) -> String {
    if !rng.chance(large) {
        return text(rng);
    }
    let unit = rng.pick(&["a", "\n", "\u{e9}", "\r\n"]);
    let length = rng.pick(&[
        MAX_CLIPBOARD - 5,
        MAX_CLIPBOARD - 4,
        MAX_CLIPBOARD - 3,
        MAX_CLIPBOARD,
        MAX_CLIPBOARD + 1,
        MAX_CLIPBOARD / 2 - 2,
        MAX_CLIPBOARD / 2,
    ]);
    let mut text = unit.repeat(length / unit.len());
    text.push_str(&"a".repeat(length - text.len()));
    text
}

/// Checks that `reply` is one whole ClientCutText message of a form this
/// client has a reason to send.
fn assert_well_formed(reply: &[u8], qemu: bool) {
    assert_eq!(reply[..4], [6, 0, 0, 0], "not a ClientCutText message");
    let length = i32::from_be_bytes(reply[4..8].try_into().unwrap());
    assert_eq!(reply.len() - 8, length.unsigned_abs() as usize);
    if length >= 0 {
        // The base protocol: Latin-1, one byte for each character.
        assert!(reply.len() - 8 <= MAX_CLIPBOARD);
        return;
    }
    let flags = u32::from_be_bytes(reply[8..12].try_into().unwrap());
    let data = &reply[12..];
    let all =
        CLIPBOARD_CAPS | CLIPBOARD_REQUEST | CLIPBOARD_NOTIFY | CLIPBOARD_PROVIDE | CLIPBOARD_TEXT;
    if flags == all {
        assert_eq!(data, [0, 0, 0, 0]);
    } else if flags == CLIPBOARD_REQUEST | CLIPBOARD_TEXT
        || flags == CLIPBOARD_NOTIFY | CLIPBOARD_TEXT
    {
        assert!(data.is_empty());
    } else {
        assert_eq!(flags, CLIPBOARD_PROVIDE | CLIPBOARD_TEXT);
        // Both the compressed message and the text with its length are
        // within what QEMU takes.
        assert!(reply.len() - 8 <= MAX_CLIPBOARD);
        let (text, rest) = model::provided_text(reply).expect("a stream QEMU can inflate");
        assert!(rest.is_empty());
        let text = std::str::from_utf8(&text).expect("UTF-8 text");
        if !qemu {
            // The specification's form: a terminator, and CRLF line ends.
            let text = text.strip_suffix('\0').expect("a terminator");
            assert!(
                text.split('\n')
                    .rev()
                    .skip(1)
                    .all(|line| line.ends_with('\r')),
                "a bare line feed in {text:?}"
            );
        }
    }
}

/// What a second client makes of the text this one provides for `copied`.
fn round_trip(clipboard: &mut Clipboard, model: &ClipboardModel, copied: &str) {
    let normalised = model::without_carriage_returns(copied);
    let request = extended(CLIPBOARD_REQUEST | CLIPBOARD_TEXT, &[]);
    let reply = clipboard
        .receive(&request)
        .unwrap()
        .0
        .expect("the copied text");
    assert_well_formed(&reply, model.qemu);
    let received = Clipboard::default()
        .receive(&reply[8..])
        .unwrap()
        .1
        .unwrap();
    if !model.qemu {
        assert_eq!(received, normalised);
        return;
    }
    // QEMU is sent the text as it is, so a reader cannot tell a NUL that ends
    // the text from the protocol's terminator, nor a carriage return before a
    // line feed from a CRLF line end.
    let plain = normalised.strip_suffix('\0').unwrap_or(&normalised);
    assert_eq!(received, model::without_carriage_returns(plain));
    if !normalised.ends_with('\0') && !normalised.contains("\r\n") {
        assert_eq!(received, normalised);
    }
}

/// One session: a clipboard and its model taken through the same steps.
fn session(rng: &mut Rng, large: usize, with_bomb: bool) {
    let name = rng.pick(&["QEMU", "QEMU (guest)", "test", "", "QEMUlator"]);
    let mut clipboard = Clipboard::new(name);
    let mut model = ClipboardModel::new(name);
    let bomb_at = with_bomb.then(|| rng.below(8));
    for step in 0..rng.between(1, 24) {
        if bomb_at == Some(step) {
            // Far more than fits is refused without room being made for it.
            if rng.chance(2) {
                model.remote_copy();
                clipboard.remote_copy();
            }
            let message = bomb(rng);
            let (allowed, expected) = model.receive(&message);
            support::reset_largest_allocation();
            let actual = clipboard.receive(&message);
            let largest = support::largest_allocation();
            assert!(largest <= allowed.max(SMALL_ALLOCATION), "{largest} bytes");
            assert_eq!(actual.is_ok(), expected.is_some());
            continue;
        }
        match rng.below(10) {
            0..3 => {
                let copied = copied(rng, large);
                let expected = model.input(&copied);
                match (clipboard.input(&copied), expected) {
                    (Ok(sent), Some(piece)) => {
                        assert_well_formed(&sent, model.qemu);
                        model::assert_written(&sent, &[piece]);
                        if model.extended() {
                            round_trip(&mut clipboard, &model, &copied);
                        } else {
                            // Latin-1 on the wire is the text again.
                            assert_eq!(
                                latin1_text(&sent[8..]).unwrap(),
                                model::without_carriage_returns(&copied)
                            );
                        }
                    }
                    (Err(_), None) => {}
                    (actual, expected) => panic!(
                        "a copy of {} bytes gave {:?}, not {expected:?}",
                        copied.len(),
                        actual.map(|sent| sent.len())
                    ),
                }
            }
            3 | 4 => {
                // The base protocol: any bytes are Latin-1 text.
                let length = if rng.chance(large) {
                    rng.pick(&[MAX_CLIPBOARD / 2, MAX_CLIPBOARD / 2 + 1, MAX_CLIPBOARD])
                } else {
                    rng.below(40)
                };
                let high = rng.pick(&[0x7f, 0xff, 0xff]);
                let bytes: Vec<u8> = (0..length).map(|_| rng.byte() & high).collect();
                model.remote_copy();
                clipboard.remote_copy();
                let text = latin1_text(&bytes);
                assert_eq!(text, model::latin1(&bytes));
                assert!(text.is_none_or(|text| text.len() <= MAX_CLIPBOARD));
            }
            _ => {
                let message = message(rng, large * 4);
                let (allowed, expected) = model.receive(&message);
                support::reset_largest_allocation();
                let actual = clipboard.receive(&message);
                let largest = support::largest_allocation();
                // Never more than the text limit, whatever the message claims.
                assert!(
                    largest <= allowed.max(SMALL_ALLOCATION) && largest <= MAX_WIRE_TEXT + 5,
                    "one allocation of {largest} bytes for a message of {}",
                    message.len()
                );
                match (actual, expected) {
                    (Ok((reply, text)), Some((expected_reply, expected_text))) => {
                        assert_eq!(text, expected_text);
                        assert!(text.is_none_or(|text| text.len() <= MAX_CLIPBOARD));
                        if let Some(reply) = &reply {
                            assert_well_formed(reply, model.qemu);
                        }
                        let expected: Vec<Piece> = expected_reply.into_iter().collect();
                        model::assert_written(&reply.unwrap_or_default(), &expected);
                    }
                    (Err(_), None) => {}
                    (actual, expected) => panic!(
                        "{:?} gave {:?}, not {:?}",
                        &message[..message.len().min(64)],
                        actual.map(|(reply, text)| (
                            reply.map(|reply| reply.len()),
                            text.map(|text| text.len())
                        )),
                        expected.map(|(_, text)| text.map(|text| text.len()))
                    ),
                }
            }
        }
    }
}

/// Sessions on every run; the first one is sent a compressed stream that
/// claims megabytes.
const DEFAULT_ITERATIONS: u64 = 300;

#[test]
fn fuzz_clipboard() {
    support::run("rfb-clipboard", DEFAULT_ITERATIONS, |rng, iteration| {
        // Megabytes of text take a while to compress in a debug build, so
        // they are left to campaigns: one copy in four hundred.
        session(rng, 400, iteration % 256 == 0);
    });
}
