//! Shared capture and input contracts. Pixels are tightly packed, opaque RGBA8.
use serde::{Deserialize, Serialize};

pub const MAX_PIXELS: usize = 16_777_216;
pub const MAX_CLIPBOARD: usize = 1_048_576;

/// Text that came from outside the process, made safe for one line of a log.
///
/// Control characters and the Unicode line separators are written as escapes,
/// so the text cannot start a line of its own or drive a terminal, and at
/// most `limit` characters of it are kept.
pub fn printable(text: &str, limit: usize) -> String {
    let mut safe = String::new();
    for (index, character) in text.chars().enumerate() {
        if index == limit {
            safe.push_str("...");
            break;
        }
        if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn printable_text_stays_on_its_line_and_within_its_limit() {
        assert_eq!(printable("bad password", 256), "bad password");
        assert_eq!(
            printable("caf\u{e9} \u{65e5}\u{672c}", 256),
            "caf\u{e9} \u{65e5}\u{672c}"
        );
        assert_eq!(
            printable("a\r\nb\tc\u{0}\u{1b}[31m\u{7f}\u{85}\u{2028}\u{2029}", 256),
            r"a\r\nb\tc\u{0}\u{1b}[31m\u{7f}\u{85}\u{2028}\u{2029}"
        );
        // The limit counts characters of the input, and says when it cut.
        assert_eq!(printable("abcdef", 6), "abcdef");
        assert_eq!(printable("abcdefg", 6), "abcdef...");
        assert_eq!(printable(&"\n".repeat(1000), 4), r"\n\n\n\n...");
        assert_eq!(printable("", 0), "");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
}

impl Frame {
    pub fn new(width: u16, height: u16) -> Option<Self> {
        let count = usize::from(width) * usize::from(height);
        if count == 0 || count > MAX_PIXELS {
            return None;
        }
        let mut pixels = vec![0; count * 4];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Some(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn is_valid(&self) -> bool {
        let count = usize::from(self.width) * usize::from(self.height);
        count > 0 && count <= MAX_PIXELS && self.pixels.len() == count * 4
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Key {
        keysym: u32,
        down: bool,
        #[serde(default)]
        id: u32,
    },
    Pointer {
        x: u16,
        y: u16,
        buttons: u8,
        #[serde(default)]
        id: u32,
    },
    Clipboard {
        text: String,
        #[serde(default)]
        id: u32,
    },
    Resize {
        width: u16,
        height: u16,
        #[serde(default)]
        id: u32,
    },
}

impl Input {
    pub fn id(&self) -> u32 {
        match self {
            Self::Key { id, .. }
            | Self::Pointer { id, .. }
            | Self::Clipboard { id, .. }
            | Self::Resize { id, .. } => *id,
        }
    }
}
