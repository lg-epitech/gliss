//! Text-only RFB extended clipboard, including QEMU's notify/request exchange.
//! https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst#extended-clipboard-pseudo-encoding
use std::io::Write;

use anyhow::{Context, Result, ensure};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use gliss_core::MAX_CLIPBOARD;

pub const ENCODING: i32 = 0xc0a1_e5ceu32 as i32;
const TEXT: u32 = 1;
const CAPS: u32 = 1 << 24;
const REQUEST: u32 = 1 << 25;
const NOTIFY: u32 = 1 << 27;
const PROVIDE: u32 = 1 << 28;
const ACTIONS: u32 = 0xff00_0000;
const CLIENT_CAPS: u32 = CAPS | REQUEST | NOTIFY | PROVIDE | TEXT;
// Allow LF -> CRLF expansion, a terminator, and bounded zlib overhead.
const MAX_WIRE_TEXT: usize = MAX_CLIPBOARD * 2 + 1;
pub const MAX_MESSAGE: usize = MAX_WIRE_TEXT + 4096;

#[derive(Default)]
pub struct Clipboard {
    server_caps: u32,
    local: Option<Vec<u8>>,
    local_owned: bool,
}

impl Clipboard {
    pub fn input(&mut self, text: &str) -> Result<Vec<u8>> {
        ensure!(
            text.len() <= MAX_CLIPBOARD,
            "clipboard exceeds {MAX_CLIPBOARD} bytes"
        );
        if self.server_caps & (TEXT | NOTIFY | PROVIDE) == TEXT | NOTIFY | PROVIDE {
            let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
            // QEMU's inflater caps the entire decompressed payload at 1 MiB.
            ensure!(
                text.len() + 5 <= MAX_CLIPBOARD,
                "encoded clipboard exceeds {MAX_CLIPBOARD} bytes"
            );
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&((text.len() + 1) as u32).to_be_bytes())?;
            encoder.write_all(text.as_bytes())?;
            encoder.write_all(&[0])?;
            let provide = message(PROVIDE | TEXT, &encoder.finish()?);
            ensure!(
                provide.len() - 8 <= MAX_CLIPBOARD,
                "compressed clipboard exceeds {MAX_CLIPBOARD} bytes"
            );
            self.local = Some(provide);
            self.local_owned = true;
            // QEMU establishes ownership on notify and ignores unsolicited
            // provide messages before that. Its advertised size is zero.
            return Ok(message(NOTIFY | TEXT, &[]));
        }
        let mut bytes = vec![6, 0, 0, 0];
        let mut encoded = Vec::with_capacity(text.len());
        for character in text.chars() {
            ensure!(
                u32::from(character) <= 255,
                "base RFB clipboard supports only Latin-1 text"
            );
            encoded.push(character as u8);
        }
        bytes.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&encoded);
        Ok(bytes)
    }

    pub fn remote_copy(&mut self) {
        // Retain the latest browser payload: a remote notification can cross
        // its queued outgoing notification, which may still trigger a request.
        self.local_owned = false;
    }

    pub fn receive(&mut self, bytes: &[u8]) -> Result<(Option<Vec<u8>>, Option<String>)> {
        ensure!(bytes.len() >= 4, "invalid extended clipboard header");
        let flags = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        let data = &bytes[4..];
        if flags & CAPS != 0 {
            let formats = (flags & 0xffff).count_ones() as usize;
            ensure!(
                data.len() == formats * 4,
                "invalid extended clipboard capabilities"
            );
            self.server_caps = flags;
            // Request all remote text explicitly; never accept unbounded auto-pushes.
            return Ok((Some(message(CLIENT_CAPS, &0u32.to_be_bytes())), None));
        }
        match flags & ACTIONS {
            NOTIFY => {
                ensure!(data.is_empty(), "invalid extended clipboard notify");
                self.remote_copy();
                Ok((
                    (flags & TEXT != 0).then(|| message(REQUEST | TEXT, &[])),
                    None,
                ))
            }
            REQUEST => {
                ensure!(data.is_empty(), "invalid extended clipboard request");
                Ok((
                    if flags & TEXT != 0 {
                        self.local.clone()
                    } else {
                        None
                    },
                    None,
                ))
            }
            PROVIDE if flags & 0xffff == TEXT => {
                // A response to an earlier request can cross a newer browser
                // copy. Keep the newer local ownership and its pending data.
                if self.local_owned {
                    return Ok((None, None));
                }
                let mut decoded = Vec::with_capacity(MAX_WIRE_TEXT + 5);
                let mut inflater = Decompress::new(true);
                let status =
                    inflater.decompress_vec(data, &mut decoded, FlushDecompress::Finish)?;
                ensure!(
                    status == Status::StreamEnd && inflater.total_in() as usize == data.len(),
                    "invalid or excessive extended clipboard zlib stream"
                );
                ensure!(decoded.len() >= 4, "invalid extended clipboard text length");
                let length = u32::from_be_bytes(decoded[..4].try_into().unwrap()) as usize;
                ensure!(
                    length <= MAX_WIRE_TEXT && length == decoded.len() - 4,
                    "invalid or excessive extended clipboard text length"
                );
                let text = std::str::from_utf8(&decoded[4..])
                    .context("invalid extended clipboard UTF-8")?;
                // QEMU can omit the protocol's NUL terminator. Accept both.
                let text = text
                    .strip_suffix('\0')
                    .unwrap_or(text)
                    .replace("\r\n", "\n");
                ensure!(
                    text.len() <= MAX_CLIPBOARD,
                    "clipboard exceeds {MAX_CLIPBOARD} bytes"
                );
                self.remote_copy();
                Ok((None, Some(text)))
            }
            // Ignore formats and optional actions we did not advertise.
            _ => Ok((None, None)),
        }
    }
}

fn message(flags: u32, data: &[u8]) -> Vec<u8> {
    let mut bytes = vec![6, 0, 0, 0];
    bytes.extend_from_slice(&(-((4 + data.len()) as i32)).to_be_bytes());
    bytes.extend_from_slice(&flags.to_be_bytes());
    bytes.extend_from_slice(data);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provide(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let mut bytes = (PROVIDE | TEXT).to_be_bytes().to_vec();
        bytes.extend_from_slice(&encoder.finish().unwrap());
        bytes
    }

    #[test]
    fn malformed_and_excessive_payloads_are_rejected() {
        let mut clipboard = Clipboard::default();
        assert!(clipboard.receive(&[0; 3]).is_err());
        assert!(clipboard.receive(&(CAPS | TEXT).to_be_bytes()).is_err());
        assert!(clipboard.receive(&provide(&[])).is_err());
        assert!(clipboard.receive(&provide(&[0, 0, 0, 2, b'a'])).is_err());
        assert!(clipboard.receive(&provide(&[0, 0, 0, 1, 0xff])).is_err());
        let valid = provide(&[0, 0, 0, 2, b'a', 0]);
        assert!(clipboard.receive(&valid[..valid.len() - 1]).is_err());
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(clipboard.receive(&trailing).is_err());
        let bomb = provide(&vec![0; MAX_WIRE_TEXT + 6]);
        assert!(clipboard.receive(&bomb).is_err());
        let mut too_big = ((MAX_CLIPBOARD + 1) as u32).to_be_bytes().to_vec();
        too_big.extend_from_slice(&vec![b'a'; MAX_CLIPBOARD + 1]);
        assert!(clipboard.receive(&provide(&too_big)).is_err());
    }

    #[test]
    fn qemu_text_without_nul_and_empty_text_are_accepted() {
        let mut clipboard = Clipboard::default();
        assert_eq!(
            clipboard
                .receive(&provide(&[0, 0, 0, 1, b'a']))
                .unwrap()
                .1
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            clipboard
                .receive(&provide(&[0, 0, 0, 1, 0]))
                .unwrap()
                .1
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn crossed_remote_response_preserves_newer_browser_clipboard() {
        let mut clipboard = Clipboard::default();
        let caps = [CLIENT_CAPS.to_be_bytes(), 0u32.to_be_bytes()].concat();
        clipboard.receive(&caps).unwrap();
        clipboard.receive(&(NOTIFY | TEXT).to_be_bytes()).unwrap();
        clipboard.input("new browser copy").unwrap();
        assert!(
            clipboard
                .receive(&provide(&[0, 0, 0, 2, b'a', 0]))
                .unwrap()
                .1
                .is_none()
        );
        let request = (REQUEST | TEXT).to_be_bytes();
        let reply = clipboard.receive(&request).unwrap().0.unwrap();
        let mut remote = Clipboard::default();
        assert_eq!(
            remote.receive(&reply[8..]).unwrap().1.as_deref(),
            Some("new browser copy")
        );
        clipboard.receive(&(NOTIFY | TEXT).to_be_bytes()).unwrap();
        // The remote notification may have crossed the browser notification.
        // Its subsequent request must still have an answer.
        assert_eq!(
            clipboard.receive(&request).unwrap().0.as_ref(),
            Some(&reply)
        );
        assert_eq!(
            clipboard
                .receive(&provide(&[0, 0, 0, 2, b'a', 0]))
                .unwrap()
                .1
                .as_deref(),
            Some("a")
        );
    }

    #[test]
    fn encoded_limit_includes_crlf_and_framing() {
        let mut clipboard = Clipboard::default();
        clipboard
            .receive(&[CLIENT_CAPS.to_be_bytes(), 0u32.to_be_bytes()].concat())
            .unwrap();
        assert!(clipboard.input(&"a".repeat(MAX_CLIPBOARD - 5)).is_ok());
        assert!(clipboard.input(&"a".repeat(MAX_CLIPBOARD - 4)).is_err());
        assert!(clipboard.input(&"\n".repeat(MAX_CLIPBOARD / 2)).is_err());
    }
}
