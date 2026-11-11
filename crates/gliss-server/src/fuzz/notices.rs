//! Fuzzes the store of guest events: any events posted in any order, taken
//! at any moments, next to a model with one slot for each kind.

use futures_util::FutureExt;
use gliss_core::MAX_CLIPBOARD;
use gliss_rfb::{Cursor, MAX_CURSOR_SIDE};
use serde_json::{Value, json};

use super::Notices;
use crate::fuzz::support::{self, Rng};

/// The newest unsent event of each kind, in the order they are sent.
#[derive(Default)]
struct Model {
    failure: Option<&'static str>,
    display_error: Option<String>,
    input_error: Option<String>,
    resize_result: Option<(u16, u16, u16)>,
    bell: bool,
    clipboard: Option<String>,
    cursor: Option<Cursor>,
}

impl Model {
    /// The next event as JSON, and whether it is the last of the session.
    fn take(&mut self) -> Option<(Value, bool)> {
        if let Some(message) = self.failure.take() {
            return Some((json!({"type": "error", "message": message}), true));
        }
        let event = if let Some(message) = self.display_error.take() {
            json!({"type": "error", "message": message})
        } else if let Some(message) = self.input_error.take() {
            json!({"type": "input_error", "message": message})
        } else if let Some((width, height, status)) = self.resize_result.take() {
            json!({"type": "resize_result", "width": width, "height": height, "status": status})
        } else if std::mem::take(&mut self.bell) {
            json!({"type": "bell"})
        } else if let Some(text) = self.clipboard.take() {
            json!({"type": "clipboard", "text": text})
        } else {
            let cursor = self.cursor.take()?;
            json!({
                "type": "cursor",
                "width": cursor.width,
                "height": cursor.height,
                "hotspot_x": cursor.hotspot_x,
                "hotspot_y": cursor.hotspot_y,
                "pixels": cursor.pixels,
            })
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
