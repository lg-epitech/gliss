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

/// Tile edge length in pixels for [`Damage`] tracking.
///
/// 64 is a multiple of both YUV420 2x2 chroma blocks and 16-pixel AVX2 rows,
/// and 60x34 = 2040 tiles fit in 32 u64 words (256 bytes) for both 3840x2160
/// and 2160x3840 displays with zero heap allocation.
pub const DAMAGE_TILE: usize = 64;
const DAMAGE_WORDS: usize = 32;
const DAMAGE_MAX_TILES: usize = DAMAGE_WORDS * 64;

/// Compact tile-level dirty region for a framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Damage {
    width: u16,
    height: u16,
    tiles_x: u16,
    tiles_y: u16,
    full: bool,
    bits: [u64; DAMAGE_WORDS],
}

impl Damage {
    pub fn empty(width: u16, height: u16) -> Self {
        let tiles_x = width.div_ceil(DAMAGE_TILE as u16);
        let tiles_y = height.div_ceil(DAMAGE_TILE as u16);
        Self {
            width,
            height,
            tiles_x,
            tiles_y,
            full: false,
            bits: [0; DAMAGE_WORDS],
        }
    }

    pub fn full(width: u16, height: u16) -> Self {
        let mut damage = Self::empty(width, height);
        damage.full = true;
        damage
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn mark_rect(&mut self, x: u16, y: u16, width: u16, height: u16) {
        if self.full || width == 0 || height == 0 {
            return;
        }
        let total = usize::from(self.tiles_x) * usize::from(self.tiles_y);
        if total == 0 || total > DAMAGE_MAX_TILES {
            self.full = true;
            return;
        }
        let x0 = usize::from(x) / DAMAGE_TILE;
        let y0 = usize::from(y) / DAMAGE_TILE;
        let x1 = (usize::from(x) + usize::from(width) - 1) / DAMAGE_TILE;
        let y1 = (usize::from(y) + usize::from(height) - 1) / DAMAGE_TILE;
        let max_tx = usize::from(self.tiles_x).saturating_sub(1);
        let max_ty = usize::from(self.tiles_y).saturating_sub(1);
        let x1 = x1.min(max_tx);
        let y1 = y1.min(max_ty);
        if x0 > x1 || y0 > y1 {
            return;
        }
        let stride = usize::from(self.tiles_x);
        for ty in y0..=y1 {
            let row_base = ty * stride;
            let mut idx = row_base + x0;
            let end = row_base + x1 + 1;
            while idx < end {
                let word = idx / 64;
                let bit = idx % 64;
                let count = (end - idx).min(64 - bit);
                let mask = if count == 64 {
                    u64::MAX
                } else {
                    ((1u64 << count) - 1) << bit
                };
                self.bits[word] |= mask;
                idx += count;
            }
        }
    }

    pub fn union_with(&mut self, other: &Self) {
        if (self.width, self.height) != (other.width, other.height) {
            *self = Self::full(other.width, other.height);
            return;
        }
        if self.full || other.full {
            self.full = true;
            return;
        }
        for (dst, src) in self.bits.iter_mut().zip(other.bits.iter()) {
            *dst |= *src;
        }
    }

    /// Calls `f(x, y, width, height)` for disjoint pixel spans covering every
    /// dirty tile. All coordinates are tile-aligned on the start and clamped
    /// to `(self.width, self.height)` on the end.
    pub fn for_each_span(&self, mut f: impl FnMut(usize, usize, usize, usize)) {
        let w = usize::from(self.width);
        let h = usize::from(self.height);
        if w == 0 || h == 0 {
            return;
        }
        if self.full || usize::from(self.tiles_x) * usize::from(self.tiles_y) > DAMAGE_MAX_TILES {
            f(0, 0, w, h);
            return;
        }
        let tx_count = usize::from(self.tiles_x);
        let ty_count = usize::from(self.tiles_y);
        for ty in 0..ty_count {
            let y0 = ty * DAMAGE_TILE;
            let y1 = (y0 + DAMAGE_TILE).min(h);
            let row_base = ty * tx_count;
            let mut tx = 0;
            while tx < tx_count {
                let idx = row_base + tx;
                if (self.bits[idx / 64] >> (idx % 64)) & 1 == 0 {
                    tx += 1;
                    continue;
                }
                let start_tx = tx;
                tx += 1;
                while tx < tx_count {
                    let next_idx = row_base + tx;
                    if (self.bits[next_idx / 64] >> (next_idx % 64)) & 1 == 0 {
                        break;
                    }
                    tx += 1;
                }
                let x0 = start_tx * DAMAGE_TILE;
                let x1 = (tx * DAMAGE_TILE).min(w);
                f(x0, y0, x1 - x0, y1 - y0);
            }
        }
    }

    /// Calls `f(y0, height)` for merged contiguous horizontal row bands that
    /// contain at least one dirty tile. Each band starts on a tile boundary
    /// and is clamped to `self.height`, so `y0` and `height` are even whenever
    /// `self.height` is even.
    pub fn for_each_row_band(&self, mut f: impl FnMut(usize, usize)) {
        let w = usize::from(self.width);
        let h = usize::from(self.height);
        if w == 0 || h == 0 {
            return;
        }
        if self.full || usize::from(self.tiles_x) * usize::from(self.tiles_y) > DAMAGE_MAX_TILES {
            f(0, h);
            return;
        }
        let tx_count = usize::from(self.tiles_x);
        let ty_count = usize::from(self.tiles_y);
        let row_dirty = |ty: usize| -> bool {
            let row_base = ty * tx_count;
            let mut idx = row_base;
            let end = row_base + tx_count;
            while idx < end {
                let word = idx / 64;
                let bit = idx % 64;
                let count = (end - idx).min(64 - bit);
                let mask = if count == 64 {
                    u64::MAX
                } else {
                    ((1u64 << count) - 1) << bit
                };
                if self.bits[word] & mask != 0 {
                    return true;
                }
                idx += count;
            }
            false
        };
        let mut ty = 0;
        while ty < ty_count {
            if !row_dirty(ty) {
                ty += 1;
                continue;
            }
            let start_ty = ty;
            ty += 1;
            while ty < ty_count && row_dirty(ty) {
                ty += 1;
            }
            let y0 = start_ty * DAMAGE_TILE;
            let y1 = (ty * DAMAGE_TILE).min(h);
            f(y0, y1 - y0);
        }
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
