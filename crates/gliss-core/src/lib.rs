//! Shared capture and input contracts. Pixels are tightly packed, opaque RGBA8.
use serde::{Deserialize, Serialize};

pub const MAX_PIXELS: usize = 16_777_216;
pub const MAX_CLIPBOARD: usize = 1_048_576;

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
