//! A second reading of the bytes an RFB server sends, for the fuzzers to
//! compare the decoder with.
//!
//! It is written from RFC 6143, the extension specification and the limits in
//! `docs/security.md`, not from the decoder: it parses a slice, keeps the
//! picture as plain rows, moves a CopyRect through a copy of its source and
//! states every limit as its own number. Whatever bytes it is given, it says
//! what a client must report, what it must send back, how the stream ends and
//! how much memory reading it may take at once.

use std::collections::VecDeque;

use flate2::{Decompress, FlushDecompress, Status};

/// Frame allocation limit, in pixels.
const MAX_PIXELS: usize = 16_777_216;
/// A frame's area, this many times, is what one update may write or move.
const UPDATE_AREA_BUDGET: usize = 4;
const MAX_WIRE_CURSOR_SIDE: u16 = 1024;
const MAX_REPORTED_CURSOR_SIDE: u16 = 128;
const MAX_SERVER_TEXT: usize = 65_536;
const MAX_AUDIO_CHUNK: usize = 65_536;
pub const MAX_CLIPBOARD: usize = 1_048_576;
/// Clipboard text on the wire: every line feed may arrive as two bytes, and a
/// terminator follows.
pub const MAX_WIRE_TEXT: usize = MAX_CLIPBOARD * 2 + 1;
/// An extended clipboard message: the text, and room for what zlib adds to it.
const MAX_CLIPBOARD_MESSAGE: usize = MAX_WIRE_TEXT + 4096;
/// What reading may allocate at once when no frame, clipboard or other large
/// item is in play: a cursor, an audio chunk and the server's name are 64 KiB
/// each, and a name that is not UTF-8 takes three bytes for each of its own.
const SMALL_ALLOCATION: usize = 4 * 65_536;

const RGBX: [u8; 13] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16];
const BGRX: [u8; 13] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0];
/// Raw, CopyRect, rich cursor, DesktopSize, ExtendedDesktopSize, extended
/// clipboard and QEMU audio, in the order the client lists them.
const ENCODINGS: [i32; 7] = [0, 1, -239, -223, -308, -1_063_131_698, -259];

pub const CLIPBOARD_TEXT: u32 = 1;
pub const CLIPBOARD_CAPS: u32 = 1 << 24;
pub const CLIPBOARD_REQUEST: u32 = 1 << 25;
pub const CLIPBOARD_PEEK: u32 = 1 << 26;
pub const CLIPBOARD_NOTIFY: u32 = 1 << 27;
pub const CLIPBOARD_PROVIDE: u32 = 1 << 28;
const CLIPBOARD_ACTIONS: u32 = 0xff00_0000;

/// How a stream stops being read. Every finite stream ends one of these ways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The bytes ran out.
    Truncated,
    /// The bytes break the protocol or one of its limits.
    Rejected,
}

type Parsed<T> = Result<T, Ending>;

struct Wire<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Wire<'a> {
    fn take(&mut self, length: usize) -> Parsed<&'a [u8]> {
        let rest = &self.bytes[self.at..];
        if rest.len() < length {
            // What is left is consumed: a reader takes it before it finds out.
            self.at = self.bytes.len();
            return Err(Ending::Truncated);
        }
        self.at += length;
        Ok(&rest[..length])
    }

    fn u8(&mut self) -> Parsed<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Parsed<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Parsed<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Parsed<i32> {
        Ok(self.u32()? as i32)
    }
}

fn allowed(condition: bool) -> Parsed<()> {
    if condition {
        Ok(())
    } else {
        Err(Ending::Rejected)
    }
}

/// One event a client reports.
#[derive(Debug, PartialEq, Eq)]
pub enum Reported {
    /// The picture changed; it is [`Reference::pixels`] as long as nothing
    /// further has been asked of the reference.
    Frame,
    Unchanged,
    Clipboard(String),
    Cursor {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        pixels: Vec<u8>,
    },
    Bell,
    ResizeResult {
        width: u16,
        height: u16,
        status: u16,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Sound {
    Begin,
    End,
    Data(Vec<u8>),
}

/// Bytes a client sends.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    Exact(Vec<u8>),
    /// The answer to a VNC authentication challenge, which is not modelled.
    Any(usize),
    /// An extended clipboard message that provides this text, compressed
    /// however the client likes.
    Provide(Vec<u8>),
}

#[derive(Debug)]
pub enum Step {
    Report(Reported),
    End(Ending),
}

/// What ServerInit established.
#[derive(Clone, Debug)]
pub struct Desktop {
    pub minor: u8,
    pub name: String,
}

/// What one framebuffer update has done so far.
struct Update {
    /// Pixels its remaining rectangles may write or move.
    budget: usize,
    resized: bool,
    /// Whether a pixel or the size changed.
    changed: bool,
}

impl Update {
    fn draw(&mut self, area: usize) -> Parsed<()> {
        allowed(area <= self.budget)?;
        self.budget -= area;
        self.changed |= area > 0;
        Ok(())
    }
}

pub struct Reference<'a> {
    wire: Wire<'a>,
    /// The outcome of the handshake. Nothing is reported unless it succeeded.
    pub desktop: Parsed<Desktop>,
    pub width: u16,
    pub height: u16,
    /// Opaque RGBA rows of the desktop as the last complete rectangle left it.
    pub pixels: Vec<u8>,
    blue_first: bool,
    /// Whether the server has acknowledged QEMU audio.
    pub audio: bool,
    /// Whether the server has described its screen layout.
    pub resizable: bool,
    pub clipboard: ClipboardModel,
    reports: VecDeque<Reported>,
    ended: Option<Ending>,
    pub sounds: Vec<Sound>,
    pub written: Vec<Piece>,
    /// The most a client may allocate at once for what was read so far.
    pub largest_allocation: usize,
    /// Messages read to their end.
    pub messages: usize,
    /// Whether the stream ended between two messages, as a valid one does.
    pub ended_between_messages: bool,
    /// Where the message begins that was not read to its end.
    pub stopped_at: usize,
}

impl<'a> Reference<'a> {
    /// Reads the handshake. `password` is whether the client has one.
    pub fn new(bytes: &'a [u8], password: bool) -> Self {
        let mut reference = Self {
            wire: Wire { bytes, at: 0 },
            desktop: Err(Ending::Truncated),
            width: 0,
            height: 0,
            pixels: Vec::new(),
            blue_first: false,
            audio: false,
            resizable: false,
            clipboard: ClipboardModel::default(),
            reports: VecDeque::new(),
            ended: None,
            sounds: Vec::new(),
            written: Vec::new(),
            largest_allocation: SMALL_ALLOCATION,
            messages: 0,
            ended_between_messages: false,
            stopped_at: 0,
        };
        reference.desktop = reference.handshake(password);
        if let Ok(desktop) = &reference.desktop {
            reference.clipboard = ClipboardModel::new(&desktop.name);
        }
        reference
    }

    fn send(&mut self, bytes: &[u8]) {
        self.written.push(Piece::Exact(bytes.to_vec()));
    }

    /// The reason a server gives for refusing; the connection ends either way.
    fn refusal(&mut self) -> Ending {
        match self.text() {
            Ok(_) => Ending::Rejected,
            Err(ending) => ending,
        }
    }

    fn text(&mut self) -> Parsed<String> {
        let length = self.wire.u32()? as usize;
        allowed(length <= MAX_SERVER_TEXT)?;
        Ok(String::from_utf8_lossy(self.wire.take(length)?).into_owned())
    }

    fn handshake(&mut self, password: bool) -> Parsed<Desktop> {
        let banner = self.wire.take(12)?;
        let digits = |text: &[u8]| text.iter().all(u8::is_ascii_digit);
        allowed(
            &banner[..4] == b"RFB "
                && digits(&banner[4..7])
                && banner[7] == b'.'
                && digits(&banner[8..11])
                && banner[11] == b'\n',
        )?;
        allowed(&banner[4..7] == b"003")?;
        // Any other minor version is answered as 3.3, as the RFC asks.
        let minor = match &banner[8..11] {
            b"008" => 8,
            b"007" => 7,
            _ => 3,
        };
        self.send(format!("RFB 003.00{minor}\n").as_bytes());
        let security = if minor == 3 {
            self.wire.u32()?
        } else {
            let count = self.wire.u8()?;
            if count == 0 {
                return Err(self.refusal());
            }
            let offered = self.wire.take(usize::from(count))?;
            let chosen = if password && offered.contains(&2) {
                2
            } else if offered.contains(&1) {
                1
            } else {
                return Err(Ending::Rejected);
            };
            self.send(&[chosen]);
            u32::from(chosen)
        };
        match security {
            0 => return Err(self.refusal()),
            1 => {}
            2 => {
                allowed(password)?;
                self.wire.take(16)?;
                self.written.push(Piece::Any(16));
            }
            _ => return Err(Ending::Rejected),
        }
        if (minor == 8 || security == 2) && self.wire.u32()? != 0 {
            // Only 3.8 says why.
            return Err(if minor == 8 {
                self.refusal()
            } else {
                Ending::Rejected
            });
        }
        // ClientInit: share the desktop.
        self.send(&[1]);
        let width = self.wire.u16()?;
        let height = self.wire.u16()?;
        let area = usize::from(width) * usize::from(height);
        allowed(area > 0 && area <= MAX_PIXELS)?;
        self.resize(width, height);
        let format = self.wire.take(16)?;
        // The last three bytes of a pixel format are padding.
        self.blue_first = format[..13] == BGRX;
        let name = self.text()?;
        let mut pixel_format = vec![0, 0, 0, 0];
        pixel_format.extend_from_slice(if self.blue_first { &BGRX } else { &RGBX });
        pixel_format.extend_from_slice(&[0, 0, 0]);
        self.send(&pixel_format);
        let mut encodings = vec![2, 0, 0, ENCODINGS.len() as u8];
        for encoding in ENCODINGS {
            encodings.extend_from_slice(&encoding.to_be_bytes());
        }
        self.send(&encodings);
        Ok(Desktop { minor, name })
    }

    /// A black, opaque desktop of the new size.
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.pixels = [0, 0, 0, 255].repeat(usize::from(width) * usize::from(height));
        self.largest_allocation = self.largest_allocation.max(self.pixels.len());
    }

    /// The next event, or how the stream ends once every event before the end
    /// has been taken.
    pub fn next(&mut self) -> Step {
        loop {
            if let Some(report) = self.reports.pop_front() {
                return Step::Report(report);
            }
            if self.desktop.is_err() {
                return Step::End(self.desktop.clone().unwrap_err());
            }
            if let Some(ending) = self.ended {
                return Step::End(ending);
            }
            let before = self.wire.at;
            match self.message() {
                Ok(()) => self.messages += 1,
                Err(ending) => {
                    self.stopped_at = before;
                    self.ended_between_messages =
                        ending == Ending::Truncated && before == self.wire.bytes.len();
                    self.ended = Some(ending);
                }
            }
        }
    }

    fn inside(&self, x: u16, y: u16, width: u16, height: u16) -> Parsed<()> {
        allowed(
            usize::from(x) + usize::from(width) <= usize::from(self.width)
                && usize::from(y) + usize::from(height) <= usize::from(self.height),
        )
    }

    fn rgba(&self, pixel: &[u8]) -> [u8; 4] {
        if self.blue_first {
            [pixel[2], pixel[1], pixel[0], 255]
        } else {
            [pixel[0], pixel[1], pixel[2], 255]
        }
    }

    fn offset(&self, x: usize, y: usize) -> usize {
        (y * usize::from(self.width) + x) * 4
    }

    fn message(&mut self) -> Parsed<()> {
        match self.wire.u8()? {
            0 => self.update(),
            2 => {
                self.reports.push_back(Reported::Bell);
                Ok(())
            }
            3 => self.cut_text(),
            255 => self.sound(),
            _ => Err(Ending::Rejected),
        }
    }

    fn update(&mut self) -> Parsed<()> {
        self.wire.u8()?;
        let count = self.wire.u16()?;
        let mut update = Update {
            budget: self.budget(),
            resized: false,
            changed: false,
        };
        for index in 0..count {
            let x = self.wire.u16()?;
            let y = self.wire.u16()?;
            let width = self.wire.u16()?;
            let height = self.wire.u16()?;
            let area = usize::from(width) * usize::from(height);
            match self.wire.i32()? {
                0 => {
                    self.inside(x, y, width, height)?;
                    update.draw(area)?;
                    for row in 0..usize::from(height) {
                        for column in 0..usize::from(width) {
                            let pixel = self.wire.take(4)?;
                            let pixel = self.rgba(pixel);
                            let at = self.offset(usize::from(x) + column, usize::from(y) + row);
                            self.pixels[at..at + 4].copy_from_slice(&pixel);
                        }
                    }
                }
                1 => {
                    self.inside(x, y, width, height)?;
                    let source_x = self.wire.u16()?;
                    let source_y = self.wire.u16()?;
                    self.inside(source_x, source_y, width, height)?;
                    update.draw(area)?;
                    // All of the source is read before any of it is written.
                    let row_bytes = usize::from(width) * 4;
                    let mut source = Vec::with_capacity(area * 4);
                    for row in 0..usize::from(height) {
                        let at = self.offset(usize::from(source_x), usize::from(source_y) + row);
                        source.extend_from_slice(&self.pixels[at..at + row_bytes]);
                    }
                    for row in 0..usize::from(height) {
                        let at = self.offset(usize::from(x), usize::from(y) + row);
                        self.pixels[at..at + row_bytes]
                            .copy_from_slice(&source[row * row_bytes..(row + 1) * row_bytes]);
                    }
                }
                -239 => {
                    allowed(width <= MAX_WIRE_CURSOR_SIDE && height <= MAX_WIRE_CURSOR_SIDE)?;
                    // No cursor at all, or a hotspot on the shape.
                    allowed((width == 0 && height == 0) || (area > 0 && x < width && y < height))?;
                    let mask_row = usize::from(width).div_ceil(8);
                    let colours = self.wire.take(area * 4)?;
                    let mask = self.wire.take(mask_row * usize::from(height))?;
                    if width <= MAX_REPORTED_CURSOR_SIDE && height <= MAX_REPORTED_CURSOR_SIDE {
                        let mut pixels = Vec::with_capacity(area * 4);
                        for (index, colour) in colours.as_chunks::<4>().0.iter().enumerate() {
                            let (row, column) =
                                (index / usize::from(width), index % usize::from(width));
                            let shown = mask[row * mask_row + column / 8] & (0x80 >> (column % 8));
                            let mut pixel = self.rgba(colour);
                            pixel[3] = if shown != 0 { 255 } else { 0 };
                            pixels.extend_from_slice(&pixel);
                        }
                        self.reports.push_back(Reported::Cursor {
                            width,
                            height,
                            hotspot_x: x,
                            hotspot_y: y,
                            pixels,
                        });
                    }
                }
                -223 => {
                    allowed(index + 1 == count)?;
                    allowed(x == 0 && y == 0)?;
                    allowed(area > 0 && area <= MAX_PIXELS)?;
                    self.set_size(width, height, &mut update)?;
                }
                -308 => {
                    let screens = self.wire.u8()?;
                    self.wire.take(3)?;
                    let layout = self.wire.take(usize::from(screens) * 16)?;
                    // Reason 1 is the answer to this client's request, and a
                    // refused request carries no size.
                    if x != 1 || y == 0 {
                        allowed(area > 0 && area <= MAX_PIXELS)?;
                        allowed(screens > 0)?;
                        let mut ids = Vec::new();
                        for screen in layout.as_chunks::<16>().0 {
                            let field = |at: usize| {
                                usize::from(u16::from_be_bytes([screen[at], screen[at + 1]]))
                            };
                            let (left, top, across, down) =
                                (field(4), field(6), field(8), field(10));
                            allowed(
                                across > 0
                                    && down > 0
                                    && left + across <= usize::from(width)
                                    && top + down <= usize::from(height),
                            )?;
                            allowed(!ids.contains(&&screen[..4]))?;
                            ids.push(&screen[..4]);
                        }
                        self.resizable = true;
                        self.set_size(width, height, &mut update)?;
                    }
                    if x == 1 {
                        self.reports.push_back(Reported::ResizeResult {
                            width,
                            height,
                            status: y,
                        });
                    }
                }
                -259 => self.audio = true,
                _ => return Err(Ending::Rejected),
            }
        }
        self.reports.push_back(if update.changed {
            Reported::Frame
        } else {
            Reported::Unchanged
        });
        Ok(())
    }

    /// What one update may write or move, in pixels.
    fn budget(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * UPDATE_AREA_BUDGET
    }

    /// An update may change the size once; the rectangles after that draw the
    /// new desktop, so they get its budget.
    fn set_size(&mut self, width: u16, height: u16, update: &mut Update) -> Parsed<()> {
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        allowed(!update.resized)?;
        self.resize(width, height);
        update.resized = true;
        update.changed = true;
        update.budget = self.budget();
        Ok(())
    }

    fn cut_text(&mut self) -> Parsed<()> {
        self.wire.take(3)?;
        let length = self.wire.i32()?;
        let size = length.unsigned_abs() as usize;
        // A negative length marks the extended clipboard.
        allowed(
            size <= if length < 0 {
                MAX_CLIPBOARD_MESSAGE
            } else {
                MAX_CLIPBOARD
            },
        )?;
        // Room for a message within the limit is made before its bytes arrive.
        self.largest_allocation = self.largest_allocation.max(size);
        let bytes = self.wire.take(size)?;
        let text = if length < 0 {
            let (allocation, received) = self.clipboard.receive(bytes);
            self.largest_allocation = self.largest_allocation.max(allocation);
            let (reply, text) = received.ok_or(Ending::Rejected)?;
            self.written.extend(reply);
            text
        } else {
            self.clipboard.remote_copy();
            // Twice the bytes: text that grows as it is converted may be
            // held in a buffer that doubled once.
            self.largest_allocation = self.largest_allocation.max(size * 2 + 16);
            latin1(bytes)
        };
        self.reports.extend(text.map(Reported::Clipboard));
        Ok(())
    }

    fn sound(&mut self) -> Parsed<()> {
        allowed(self.audio)?;
        allowed(self.wire.u8()? == 1)?;
        let sound = match self.wire.u16()? {
            0 => Sound::End,
            1 => Sound::Begin,
            2 => {
                let length = self.wire.u32()? as usize;
                // Stereo frames of two 16-bit samples.
                allowed(length <= MAX_AUDIO_CHUNK && length.is_multiple_of(4))?;
                Sound::Data(self.wire.take(length)?.to_vec())
            }
            _ => return Err(Ending::Rejected),
        };
        self.sounds.push(sound);
        Ok(())
    }
}

/// The text of a base-protocol clipboard, which is Latin-1. Text that is over
/// the limit as UTF-8 is dropped.
pub fn latin1(bytes: &[u8]) -> Option<String> {
    let text: String = bytes
        .iter()
        .map(|byte| char::from_u32(u32::from(*byte)).unwrap())
        .collect();
    (text.len() <= MAX_CLIPBOARD).then_some(text)
}

/// Each carriage return followed by a line feed becomes the line feed alone.
pub fn without_carriage_returns(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' && characters.peek() == Some(&'\n') {
            continue;
        }
        result.push(character);
    }
    result
}

/// Inflates `data` if it is exactly one zlib stream of at most `limit` bytes.
///
/// The inflater is the one the client uses: there is no other in the
/// dependency tree. It is driven differently here, a small block at a time
/// with no promise that the input is complete, so what is compared is the
/// handling around it: where the stream ends and how much it may produce.
fn inflated(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut inflater = Decompress::new(true);
    let mut output = Vec::new();
    let mut block = [0; 4096];
    loop {
        let (consumed, produced) = (inflater.total_in(), inflater.total_out());
        let status = inflater
            .decompress(
                &data[consumed as usize..],
                &mut block,
                FlushDecompress::None,
            )
            .ok()?;
        let made = (inflater.total_out() - produced) as usize;
        output.extend_from_slice(&block[..made]);
        if output.len() > limit {
            return None;
        }
        if status == Status::StreamEnd {
            return (inflater.total_in() as usize == data.len()).then_some(output);
        }
        if made == 0 && inflater.total_in() == consumed {
            // The stream wants bytes that are not there.
            return None;
        }
    }
}

/// The extended clipboard as the specification and QEMU's use of it describe
/// it, for a client that holds text only and asks for every remote text.
#[derive(Default)]
pub struct ClipboardModel {
    /// QEMU passes text to the guest as it is: no CRLF and no terminator.
    pub qemu: bool,
    capabilities: u32,
    /// The text the client provides when asked, as it goes on the wire.
    pub local: Option<Vec<u8>>,
    /// Whether the newest copy was made on the client's side.
    owned: bool,
}

impl ClipboardModel {
    pub fn new(server_name: &str) -> Self {
        Self {
            qemu: server_name == "QEMU" || server_name.starts_with("QEMU ("),
            ..Self::default()
        }
    }

    /// Whether text goes out through the extended clipboard.
    pub fn extended(&self) -> bool {
        let needed = CLIPBOARD_TEXT | CLIPBOARD_NOTIFY | CLIPBOARD_PROVIDE;
        self.capabilities & needed == needed
    }

    pub fn remote_copy(&mut self) {
        self.owned = false;
    }

    /// The text as the peer is sent it once it asks: `None` if it is refused.
    pub fn wire_text(&self, text: &str) -> Option<Vec<u8>> {
        if text.len() > MAX_CLIPBOARD {
            return None;
        }
        let text = without_carriage_returns(text);
        if !self.extended() {
            // The base protocol carries Latin-1 only.
            return text
                .chars()
                .map(|character| u8::try_from(u32::from(character)).ok())
                .collect();
        }
        let mut wire = Vec::with_capacity(text.len() + 1);
        if self.qemu {
            wire.extend_from_slice(text.as_bytes());
        } else {
            for byte in text.bytes() {
                if byte == b'\n' {
                    wire.push(b'\r');
                }
                wire.push(byte);
            }
            wire.push(0);
        }
        // QEMU inflates at most 1 MiB, and that includes the length.
        (wire.len() + 4 <= MAX_CLIPBOARD).then_some(wire)
    }

    /// A copy made on the client's side: what it sends at once, if the text
    /// is accepted.
    pub fn input(&mut self, text: &str) -> Option<Piece> {
        let wire = self.wire_text(text)?;
        if !self.extended() {
            let mut message = vec![6, 0, 0, 0];
            message.extend_from_slice(&(wire.len() as u32).to_be_bytes());
            message.extend_from_slice(&wire);
            return Some(Piece::Exact(message));
        }
        self.local = Some(wire);
        self.owned = true;
        // The text itself waits for the peer's request.
        Some(Piece::Exact(client_message(
            CLIPBOARD_NOTIFY | CLIPBOARD_TEXT,
            &[],
        )))
    }

    /// An extended clipboard message from the server: the most the client may
    /// allocate to read it, then what it sends back and the text it reports,
    /// or `None` if the message breaks the protocol.
    pub fn receive(&mut self, bytes: &[u8]) -> (usize, Option<Received>) {
        let Some(flags) = bytes.get(..4) else {
            return (0, None);
        };
        let flags = u32::from_be_bytes(flags.try_into().unwrap());
        let data = &bytes[4..];
        if flags & CLIPBOARD_CAPS != 0 {
            // One size for each format the server names.
            if data.len() != (flags & 0xffff).count_ones() as usize * 4 {
                return (0, None);
            }
            self.capabilities = flags;
            // Text only, and a size of zero: nothing is to be sent unasked.
            let ours = CLIPBOARD_CAPS
                | CLIPBOARD_REQUEST
                | CLIPBOARD_NOTIFY
                | CLIPBOARD_PROVIDE
                | CLIPBOARD_TEXT;
            let reply = Piece::Exact(client_message(ours, &[0, 0, 0, 0]));
            return (0, Some((Some(reply), None)));
        }
        let actions = flags & CLIPBOARD_ACTIONS;
        if actions == CLIPBOARD_NOTIFY {
            if !data.is_empty() {
                return (0, None);
            }
            self.owned = false;
            let request = (flags & CLIPBOARD_TEXT != 0)
                .then(|| Piece::Exact(client_message(CLIPBOARD_REQUEST | CLIPBOARD_TEXT, &[])));
            return (0, Some((request, None)));
        }
        if actions == CLIPBOARD_REQUEST {
            if !data.is_empty() {
                return (0, None);
            }
            let provided = self
                .local
                .clone()
                .filter(|_| flags & CLIPBOARD_TEXT != 0)
                .map(Piece::Provide);
            // The text is kept compressed, which can add a little to it.
            let allocation = self.local.as_ref().map_or(0, |text| text.len() + 4096);
            return (allocation, Some((provided, None)));
        }
        if actions == CLIPBOARD_PROVIDE && flags & 0xffff == CLIPBOARD_TEXT {
            if self.owned {
                // An answer to an earlier request that crossed a newer copy
                // made on this side.
                return (0, Some((None, None)));
            }
            // Room for the inflated text and its length field, and one byte
            // to tell text at the limit from text over it. It is made before
            // the stream shows how much it holds.
            (MAX_WIRE_TEXT + 5, self.provided(data))
        } else {
            // Formats and actions the client did not ask for.
            (0, Some((None, None)))
        }
    }

    /// Text the server provides in answer to a request.
    fn provided(&mut self, data: &[u8]) -> Option<Received> {
        let payload = inflated(data, MAX_WIRE_TEXT + 4)?;
        let length = u32::from_be_bytes(payload.get(..4)?.try_into().unwrap()) as usize;
        if length != payload.len() - 4 {
            return None;
        }
        let text = std::str::from_utf8(&payload[4..]).ok()?;
        // The terminator the specification asks for, which QEMU leaves out.
        let text = without_carriage_returns(text.strip_suffix('\0').unwrap_or(text));
        if text.len() > MAX_CLIPBOARD {
            return None;
        }
        self.owned = false;
        Some((None, Some(text)))
    }
}

/// What the client sends back for a clipboard message, and the text it reports.
pub type Received = (Option<Piece>, Option<String>);

/// A ClientCutText message in its extended form.
fn client_message(flags: u32, data: &[u8]) -> Vec<u8> {
    let mut message = vec![6, 0, 0, 0];
    message.extend_from_slice(&(-(4 + data.len() as i32)).to_be_bytes());
    message.extend_from_slice(&flags.to_be_bytes());
    message.extend_from_slice(data);
    message
}

/// Checks that `written` is the `expected` pieces, one after another.
pub fn assert_written(written: &[u8], expected: &[Piece]) {
    let mut rest = written;
    for (index, piece) in expected.iter().enumerate() {
        match piece {
            Piece::Exact(bytes) => {
                assert!(
                    rest.starts_with(bytes),
                    "client message {index} is not {bytes:?}: {:?}",
                    &rest[..rest.len().min(bytes.len() + 16)]
                );
                rest = &rest[bytes.len()..];
            }
            Piece::Any(length) => {
                assert!(rest.len() >= *length, "client message {index} is cut short");
                rest = &rest[*length..];
            }
            Piece::Provide(text) => {
                let (provided, after) = provided_text(rest)
                    .unwrap_or_else(|| panic!("client message {index} is not a provide message"));
                assert_eq!(
                    &provided, text,
                    "client message {index} provides other text"
                );
                rest = after;
            }
        }
    }
    assert!(rest.is_empty(), "the client sent more: {rest:?}");
}

/// Takes one extended clipboard message that provides text off the front of
/// `bytes` and returns the text with what follows the message.
pub fn provided_text(bytes: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    if bytes.get(..4)? != [6, 0, 0, 0] {
        return None;
    }
    let length = i32::from_be_bytes(bytes.get(4..8)?.try_into().unwrap());
    let size = usize::try_from(length.checked_neg()?).ok()?;
    let message = bytes.get(8..8 + size)?;
    if message.get(..4)? != (CLIPBOARD_PROVIDE | CLIPBOARD_TEXT).to_be_bytes() {
        return None;
    }
    // What QEMU accepts: at most 1 MiB once inflated, the length included.
    let payload = inflated(&message[4..], MAX_CLIPBOARD)?;
    let text = payload.get(4..)?;
    if u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize != text.len() {
        return None;
    }
    Some((text.to_vec(), &bytes[8 + size..]))
}
