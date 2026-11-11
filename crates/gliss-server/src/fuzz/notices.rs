//! Fuzzes the store of guest events: any events posted in any order, taken
//! at any moments, next to a model with one slot for each kind.

use futures_util::FutureExt;
use gliss_core::MAX_CLIPBOARD;
use gliss_rfb::{Cursor, MAX_CURSOR_SIDE};
use serde_json::{Value, json};

use super::Notices;
use crate::fuzz::support::{self, Rng};

/// The kinds of event a session goes on after, by the `type` each is sent
/// with, in the order they are sent when none has been sent before.
const KINDS: [&str; 6] = [
    "error",
    "input_error",
    "resize_result",
    "bell",
    "clipboard",
    "cursor",
];

/// The newest unsent event of each kind, and when each kind was last sent.
#[derive(Default)]
struct Model {
    failure: Option<&'static str>,
    display_error: Option<String>,
    input_error: Option<String>,
    resize_result: Option<(u16, u16, u16)>,
    bell: bool,
    clipboard: Option<String>,
    cursor: Option<Cursor>,
    /// For each of [`KINDS`], the number of the write that last sent it:
    /// zero if none has.
    sent: [u64; 6],
    writes: u64,
}

impl Model {
    /// Whether an event of each of [`KINDS`] waits.
    fn waiting(&self) -> [bool; 6] {
        [
            self.display_error.is_some(),
            self.input_error.is_some(),
            self.resize_result.is_some(),
            self.bell,
            self.clipboard.is_some(),
            self.cursor.is_some(),
        ]
    }

    /// The next event as JSON, and whether it is the last of the session.
    fn take(&mut self) -> Option<(Value, bool)> {
        if let Some(message) = self.failure.take() {
            return Some((json!({"type": "error", "message": message}), true));
        }
        // Of the kinds that wait, the one sent longest ago, and the first in
        // the list of those never sent.
        let waiting = self.waiting();
        let kind = (0..KINDS.len())
            .filter(|kind| waiting[*kind])
            .min_by_key(|kind| self.sent[*kind])?;
        self.writes += 1;
        self.sent[kind] = self.writes;
        let event = match KINDS[kind] {
            "error" => json!({"type": "error", "message": self.display_error.take()}),
            "input_error" => json!({"type": "input_error", "message": self.input_error.take()}),
            "resize_result" => {
                let (width, height, status) = self.resize_result.take().unwrap();
                json!({"type": "resize_result", "width": width, "height": height, "status": status})
            }
            "bell" => {
                self.bell = false;
                json!({"type": "bell"})
            }
            "clipboard" => json!({"type": "clipboard", "text": self.clipboard.take()}),
            _ => {
                let cursor = self.cursor.take().unwrap();
                json!({
                    "type": "cursor",
                    "width": cursor.width,
                    "height": cursor.height,
                    "hotspot_x": cursor.hotspot_x,
                    "hotspot_y": cursor.hotspot_y,
                    "pixels": cursor.pixels,
                })
            }
        };
        Some((event, false))
    }

    /// Bytes of guest data that wait to be sent.
    fn pending(&self) -> usize {
        self.cursor.as_ref().map_or(0, |cursor| cursor.pixels.len())
            + self.clipboard.as_ref().map_or(0, String::len)
    }
}

/// Text with everything JSON has to escape, and now and then megabytes of it.
fn text(rng: &mut Rng) -> String {
    if rng.chance(300) {
        return "x".repeat(rng.pick(&[MAX_CLIPBOARD - 1, MAX_CLIPBOARD, MAX_CLIPBOARD + 1]));
    }
    let pieces = [
        "a",
        "\"",
        "\\",
        "\n",
        "\r",
        "\0",
        "\u{1b}",
        "\u{7f}",
        "\u{e9}",
        "\u{2028}",
        "\u{1f680}",
        "</script>",
        "{",
        "}",
        ",",
        " ",
    ];
    (0..rng.below(10)).map(|_| rng.pick(&pieces)).collect()
}

fn cursor(rng: &mut Rng) -> Cursor {
    let (width, height) = match rng.below(12) {
        0 => (0, 0),
        1 => (MAX_CURSOR_SIDE, MAX_CURSOR_SIDE),
        2 => (MAX_CURSOR_SIDE + 1, 1),
        3 => (1, MAX_CURSOR_SIDE + 1),
        4 => (MAX_CURSOR_SIDE, 1),
        _ => (rng.between(1, 6) as u16, rng.between(1, 6) as u16),
    };
    Cursor {
        width,
        height,
        hotspot_x: rng.below(usize::from(width.max(1))) as u16,
        hotspot_y: rng.below(usize::from(height.max(1))) as u16,
        pixels: rng.bytes(usize::from(width) * usize::from(height) * 4),
    }
}

const FAILURES: [&str; 2] = ["The VM display disconnected.", "gone"];
const DEFAULT_ITERATIONS: u64 = 150;

#[test]
fn fuzz_guest_events() {
    support::run(
        "server-guest-events",
        DEFAULT_ITERATIONS,
        |rng, iteration| {
            let notices = Notices::default();
            let mut model = Model::default();
            // For each kind, how often each other kind was sent while an
            // event of the first has waited.
            let mut overtaken = [[0u8; 6]; 6];
            if iteration % 128 == 0 {
                // The clipboard limit itself, which random text rarely meets: a
                // byte over it is not kept, and text at it is.
                notices.clipboard("x".repeat(MAX_CLIPBOARD + 1));
                assert!(notices.pending.lock().unwrap().clipboard.is_none());
                notices.clipboard("x".repeat(MAX_CLIPBOARD));
                model.clipboard = Some("x".repeat(MAX_CLIPBOARD));
            }
            for _ in 0..rng.between(1, 40) {
                match rng.below(12) {
                    0 | 1 => {
                        let cursor = cursor(rng);
                        notices.cursor(cursor.clone());
                        // A shape the browser would not use is not kept.
                        if cursor.width <= MAX_CURSOR_SIDE && cursor.height <= MAX_CURSOR_SIDE {
                            model.cursor = Some(cursor);
                        }
                    }
                    2 | 3 => {
                        let text = text(rng);
                        notices.clipboard(text.clone());
                        if text.len() <= MAX_CLIPBOARD {
                            model.clipboard = Some(text);
                        }
                    }
                    4 => {
                        notices.bell();
                        model.bell = true;
                    }
                    5 => {
                        let result = (rng.next() as u16, rng.next() as u16, rng.next() as u16);
                        notices.resize_result(result.0, result.1, result.2);
                        model.resize_result = Some(result);
                    }
                    6 => {
                        // The server's own sentences; a guest does not write these.
                        let message: String = text(rng).chars().take(200).collect();
                        notices.input_error(message.clone());
                        model.input_error = Some(message);
                    }
                    7 => {
                        let message: String = text(rng).chars().take(200).collect();
                        notices.display_error(message.clone());
                        model.display_error = Some(message);
                    }
                    8 if rng.chance(2) => {
                        // The display fits again: an unsent error about it goes.
                        notices.display_resumed();
                        model.display_error = None;
                        overtaken[0] = [0; 6];
                    }
                    9 if rng.chance(4) => {
                        let message = rng.pick(&FAILURES);
                        notices.failed(message);
                        model.failure = Some(message);
                    }
                    _ => {
                        // The writer takes one event when it can send one.
                        let sent = notices.next().now_or_never();
                        let expected = model.take();
                        assert_eq!(sent.is_some(), expected.is_some());
                        if let (Some(sent), Some((event, last))) = (sent, expected) {
                            let parsed: Value =
                                serde_json::from_str(&sent.text).expect("an event is JSON");
                            assert_eq!(parsed, event);
                            assert_eq!(sent.last, last);
                            let kind = KINDS.iter().position(|kind| parsed["type"] == *kind);
                            if let (Some(kind), false) = (kind, last) {
                                // No kind waits behind two events of another.
                                overtaken[kind] = [0; 6];
                                let waiting = model.waiting();
                                for other in (0..KINDS.len()).filter(|other| waiting[*other]) {
                                    overtaken[other][kind] += 1;
                                    assert!(
                                        overtaken[other][kind] <= 1,
                                        "{} was sent twice while {} waited",
                                        KINDS[kind],
                                        KINDS[other]
                                    );
                                }
                            }
                        }
                    }
                }
                // One cursor and one clipboard at most, however many were posted.
                let pending = notices.pending.lock().unwrap();
                let held = pending
                    .cursor
                    .as_ref()
                    .map_or(0, |cursor| cursor.pixels.len())
                    + pending.clipboard.as_ref().map_or(0, String::len);
                assert_eq!(held, model.pending());
                assert!(held <= usize::from(MAX_CURSOR_SIDE).pow(2) * 4 + MAX_CLIPBOARD);
            }
        },
    );
}
