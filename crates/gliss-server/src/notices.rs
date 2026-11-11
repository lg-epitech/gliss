//! Events for the browser that are not part of the video or audio stream.
//!
//! The guest decides how many of these there are: it can change its cursor,
//! copy text and ring the bell as fast as it likes. A browser reads them only
//! as fast as its connection allows. So nothing here is a queue: each kind of
//! event keeps its newest unsent value, a newer one replaces it, and posting
//! never waits and never fails. What is pending is bounded by one cursor of
//! [`MAX_CURSOR_SIDE`] squared and one clipboard of [`MAX_CLIPBOARD`] bytes.
//! The kinds are written in turn, so a kind the guest keeps pending does not
//! hold back the others.

use gliss_core::MAX_CLIPBOARD;
use gliss_rfb::{Cursor, MAX_CURSOR_SIDE};
use serde_json::json;
use std::{fmt::Write, sync::Mutex};
use tokio::sync::Notify;

/// The kinds of event a session goes on after.
#[derive(Clone, Copy)]
enum Kind {
    DisplayError,
    InputError,
    ResizeResult,
    Bell,
    Clipboard,
    Cursor,
}

/// The order the kinds are tried in: the one written longest ago first. At
/// the start that is small and urgent first, the largest payloads last.
struct Turns([Kind; 6]);

impl Default for Turns {
    fn default() -> Self {
        Self([
            Kind::DisplayError,
            Kind::InputError,
            Kind::ResizeResult,
            Kind::Bell,
            Kind::Clipboard,
            Kind::Cursor,
        ])
    }
}

#[derive(Default)]
struct Pending {
    failure: Option<&'static str>,
    display_error: Option<String>,
    input_error: Option<String>,
    resize_result: Option<(u16, u16, u16)>,
    bell: bool,
    clipboard: Option<String>,
    cursor: Option<Cursor>,
    turns: Turns,
}

impl Pending {
    /// Takes the event of `kind` that waits, as its text.
    fn take(&mut self, kind: Kind) -> Option<String> {
        Some(match kind {
            Kind::DisplayError => {
                json!({"type":"error","message":self.display_error.take()?}).to_string()
            }
            Kind::InputError => {
                json!({"type":"input_error","message":self.input_error.take()?}).to_string()
            }
            Kind::ResizeResult => {
                let (width, height, status) = self.resize_result.take()?;
                json!({"type":"resize_result","width":width,"height":height,"status":status})
                    .to_string()
            }
            Kind::Bell => {
                std::mem::take(&mut self.bell).then(|| json!({"type":"bell"}).to_string())?
            }
            Kind::Clipboard => {
                json!({"type":"clipboard","text":self.clipboard.take()?}).to_string()
            }
            Kind::Cursor => cursor_text(&self.cursor.take()?),
        })
    }
}

#[derive(Default)]
pub struct Notices {
    pending: Mutex<Pending>,
    posted: Notify,
}

/// One event as the text of a WebSocket message.
#[derive(Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    /// The display is gone: nothing follows this event.
    pub last: bool,
}

impl Notices {
    fn post(&self, change: impl FnOnce(&mut Pending)) {
        change(&mut self.pending.lock().unwrap_or_else(|lock| lock.into_inner()));
        // One reader: a stored permit covers a post that races its check.
        self.posted.notify_one();
    }

    pub fn cursor(&self, cursor: Cursor) {
        // The decoder already drops larger shapes. Checked again because the
        // bound on pending bytes is this module's promise.
        if cursor.width <= MAX_CURSOR_SIDE && cursor.height <= MAX_CURSOR_SIDE {
            self.post(|pending| pending.cursor = Some(cursor));
        }
    }

    pub fn clipboard(&self, text: String) {
        if text.len() <= MAX_CLIPBOARD {
            self.post(|pending| pending.clipboard = Some(text));
        }
    }

    pub fn bell(&self) {
        self.post(|pending| pending.bell = true);
    }

    pub fn resize_result(&self, width: u16, height: u16, status: u16) {
        self.post(|pending| pending.resize_result = Some((width, height, status)));
    }

    pub fn input_error(&self, message: String) {
        self.post(|pending| pending.input_error = Some(message));
    }

    /// The display cannot be shown for now; the session goes on.
    pub fn display_error(&self, message: String) {
        self.post(|pending| pending.display_error = Some(message));
    }

    /// The display can be shown again: what [`Notices::display_error`] said
    /// no longer holds, and is not sent if it has not been sent yet.
    pub fn display_resumed(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|lock| lock.into_inner());
        pending.display_error = None;
    }

    /// The display is gone for good.
    pub fn failed(&self, message: &'static str) {
        self.post(|pending| pending.failure = Some(message));
    }

    /// Waits for an event. Cancelling the wait loses nothing.
    pub async fn next(&self) -> Notice {
        loop {
            let posted = self.posted.notified();
            if let Some(notice) = self.take() {
                return notice;
            }
            posted.await;
        }
    }

    fn take(&self) -> Option<Notice> {
        let mut pending = self.pending.lock().unwrap_or_else(|lock| lock.into_inner());
        if let Some(message) = pending.failure.take() {
            return Some(Notice {
                text: json!({"type":"error","message":message}).to_string(),
                last: true,
            });
        }
        // A kind that was written goes behind the others. A guest can keep
        // one kind pending for ever, a bell that never stops ringing, and
        // then each other kind still waits for one event of it at most.
        for turn in 0..pending.turns.0.len() {
            let kind = pending.turns.0[turn];
            if let Some(text) = pending.take(kind) {
                pending.turns.0[turn..].rotate_left(1);
                return Some(Notice { text, last: false });
            }
        }
        None
    }
}

/// The `cursor` event, written directly as text.
///
/// Building a `serde_json::Value` first would hold a 32-byte number for each
/// pixel byte. The keys are in the order `serde_json` has always written them
/// for this event, so the text is the same as before, byte for byte.
fn cursor_text(cursor: &Cursor) -> String {
    // At most three digits and a comma per byte.
    let mut text = String::with_capacity(cursor.pixels.len() * 4 + 96);
    let _ = write!(
        text,
        r#"{{"height":{},"hotspot_x":{},"hotspot_y":{},"pixels":["#,
        cursor.height, cursor.hotspot_x, cursor.hotspot_y
    );
    for (index, byte) in cursor.pixels.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        let _ = write!(text, "{byte}");
    }
    let _ = write!(text, r#"],"type":"cursor","width":{}}}"#, cursor.width);
    text
}

#[cfg(test)]
#[path = "fuzz/notices.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    fn cursor(width: u16, height: u16, shade: u8) -> Cursor {
        Cursor {
            width,
            height,
            hotspot_x: width.saturating_sub(1),
            hotspot_y: height / 2,
            pixels: (0..usize::from(width) * usize::from(height) * 4)
                .map(|index| shade.wrapping_add(index as u8))
                .collect(),
        }
    }

    fn pending_bytes(notices: &Notices) -> usize {
        let pending = notices.pending.lock().unwrap();
        pending
            .cursor
            .as_ref()
            .map_or(0, |cursor| cursor.pixels.len())
            + pending.clipboard.as_ref().map_or(0, String::len)
    }

    #[test]
    fn the_cursor_event_is_the_text_serde_json_wrote_for_it() {
        for cursor in [
            cursor(0, 0, 0),
            cursor(1, 1, 255),
            cursor(3, 2, 250),
            cursor(128, 128, 7),
        ] {
            let before = json!({"type":"cursor","width":cursor.width,"height":cursor.height,"hotspot_x":cursor.hotspot_x,"hotspot_y":cursor.hotspot_y,"pixels":cursor.pixels});
            let text = cursor_text(&cursor);
            assert_eq!(text, before.to_string());
            assert!(text.len() <= text.capacity());
            assert!(text.capacity() <= cursor.pixels.len() * 4 + 96);
        }
    }

    #[test]
    fn a_newer_event_replaces_an_unsent_one_of_its_kind() {
        let notices = Notices::default();
        for round in 0..1000u16 {
            notices.cursor(cursor(128, 128, round as u8));
            notices.clipboard(format!("copy {round}"));
            notices.bell();
            notices.resize_result(640, 480, round);
            notices.input_error(format!("input {round}"));
            notices.display_error(format!("display {round}"));
        }
        assert_eq!(pending_bytes(&notices), 128 * 128 * 4 + "copy 999".len());
        let mut sent = Vec::new();
        while let Some(notice) = notices.next().now_or_never() {
            assert!(!notice.last);
            sent.push(notice.text);
        }
        assert_eq!(
            sent,
            [
                r#"{"message":"display 999","type":"error"}"#.to_owned(),
                r#"{"message":"input 999","type":"input_error"}"#.to_owned(),
                r#"{"height":480,"status":999,"type":"resize_result","width":640}"#.to_owned(),
                r#"{"type":"bell"}"#.to_owned(),
                r#"{"text":"copy 999","type":"clipboard"}"#.to_owned(),
                cursor_text(&cursor(128, 128, 231)),
            ]
        );
        assert_eq!(pending_bytes(&notices), 0);
    }

    /// The `type` of the next event.
    fn next_kind(notices: &Notices) -> String {
        let text = notices.next().now_or_never().unwrap().text;
        let event: serde_json::Value = serde_json::from_str(&text).unwrap();
        event["type"].as_str().unwrap().to_owned()
    }

    #[test]
    fn a_kind_that_is_always_pending_does_not_hold_back_the_others() {
        let notices = Notices::default();
        notices.clipboard("copy".into());
        notices.cursor(cursor(1, 1, 0));
        // The guest rings again before every write.
        let sent: Vec<String> = (0..5)
            .map(|_| {
                notices.bell();
                next_kind(&notices)
            })
            .collect();
        assert_eq!(sent, ["bell", "clipboard", "cursor", "bell", "bell"]);
        // The same for any kind, also when all of them are posted without end:
        // each is sent once in every six writes.
        let mut sent = Vec::new();
        for round in 0..12u16 {
            notices.display_error(format!("display {round}"));
            notices.input_error(format!("input {round}"));
            notices.resize_result(640, 480, round);
            notices.bell();
            notices.clipboard(format!("copy {round}"));
            notices.cursor(cursor(1, 1, round as u8));
            sent.push(next_kind(&notices));
        }
        for turn in sent.chunks(6) {
            let mut kinds = turn.to_vec();
            kinds.sort();
            assert_eq!(
                kinds,
                [
                    "bell",
                    "clipboard",
                    "cursor",
                    "error",
                    "input_error",
                    "resize_result"
                ],
                "{sent:?}"
            );
        }
    }

    #[test]
    fn an_unsent_display_error_is_dropped_when_the_display_resumes() {
        let notices = Notices::default();
        notices.display_error("paused".into());
        notices.bell();
        notices.display_resumed();
        assert_eq!(
            notices.next().now_or_never().unwrap().text,
            r#"{"type":"bell"}"#
        );
        assert!(notices.next().now_or_never().is_none());
        // A later error is sent as any other.
        notices.display_error("paused again".into());
        assert_eq!(
            notices.next().now_or_never().unwrap().text,
            r#"{"message":"paused again","type":"error"}"#
        );
    }

    #[test]
    fn pending_bytes_are_bounded_by_one_cursor_and_one_clipboard() {
        let notices = Notices::default();
        notices.cursor(cursor(128, 128, 1));
        notices.clipboard("x".repeat(MAX_CLIPBOARD));
        let full = pending_bytes(&notices);
        assert_eq!(full, 128 * 128 * 4 + MAX_CLIPBOARD);
        // Larger values are never held, and do not displace what is pending.
        notices.cursor(cursor(129, 1, 2));
        notices.cursor(cursor(1, 129, 2));
        notices.cursor(cursor(1024, 1024, 2));
        notices.clipboard("x".repeat(MAX_CLIPBOARD + 1));
        assert_eq!(pending_bytes(&notices), full);
    }

    #[test]
    fn the_end_of_the_display_is_sent_first_and_marked_last() {
        let notices = Notices::default();
        notices.bell();
        notices.failed("The VM display disconnected.");
        assert_eq!(
            notices.next().now_or_never().unwrap(),
            Notice {
                text: r#"{"message":"The VM display disconnected.","type":"error"}"#.to_owned(),
                last: true
            }
        );
    }

    #[tokio::test]
    async fn a_waiting_reader_is_woken_by_a_post() {
        let notices = std::sync::Arc::new(Notices::default());
        let reader = tokio::spawn({
            let notices = notices.clone();
            async move { notices.next().await }
        });
        tokio::task::yield_now().await;
        assert!(!reader.is_finished());
        notices.bell();
        assert_eq!(reader.await.unwrap().text, r#"{"type":"bell"}"#);
    }
}
