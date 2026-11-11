//! Fuzzes everything an RFB server can send: the handshake, then any sequence
//! of server messages.
//!
//! A generator writes valid streams from the grammar of the protocol. Each is
//! decoded by the real code from memory and compared, event for event and
//! pixel for pixel, with the reference in [`super::model`]. Then bytes of the
//! stream are changed and the comparison is made again: the reference reads
//! any bytes, so a damaged stream has an expected outcome as well.

use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::broadcast::error::TryRecvError,
};

use super::{
    model::{
        self, CLIPBOARD_CAPS, CLIPBOARD_NOTIFY, CLIPBOARD_PEEK, CLIPBOARD_PROVIDE,
        CLIPBOARD_REQUEST, CLIPBOARD_TEXT, Ending, Reference, Reported, Sound, Step,
    },
    support::{self, Rng},
};
use crate::{Event, audio, handshake, start_tasks};

/// The server's bytes, handed over the way a socket does: in pieces of any
/// size, and sometimes not yet.
struct Arriving {
    bytes: Arc<[u8]>,
    at: usize,
    rng: Rng,
    /// The most bytes one read returns; zero for all there is.
    most: usize,
}

impl AsyncRead for Arriving {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let mut count = (this.bytes.len() - this.at).min(buffer.remaining());
        if this.most > 0 && count > 0 {
            if this.rng.chance(8) {
                context.waker().wake_by_ref();
                return Poll::Pending;
            }
            count = count.min(1 + this.rng.below(this.most));
        }
        buffer.put_slice(&this.bytes[this.at..this.at + count]);
        this.at += count;
        Poll::Ready(Ok(()))
    }
}

/// Keeps what the client sends, taking it in pieces as well.
struct Sent {
    bytes: Arc<Mutex<Vec<u8>>>,
    rng: Rng,
    most: usize,
}

impl AsyncWrite for Sent {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let mut count = buffer.len();
        if this.most > 0 && count > 0 {
            count = count.min(1 + this.rng.below(this.most));
        }
        this.bytes
            .lock()
            .unwrap()
            .extend_from_slice(&buffer[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Both directions, for the handshake.
struct Connection {
    arriving: Arriving,
    sent: Sent,
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.arriving).poll_read(context, buffer)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.sent).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Whether the error says the bytes ran out.
fn truncated(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof)
    })
}

fn assert_ending(expected: Ending, error: &anyhow::Error) {
    let actual = if truncated(error) {
        Ending::Truncated
    } else {
        Ending::Rejected
    };
    assert_eq!(actual, expected, "the stream ended with: {error:#}");
}

/// What a stream turned out to be.
struct Outcome {
    messages: usize,
    /// Whether it ended between two messages after a complete handshake.
    whole: bool,
    /// Where the message begins that was not read to its end.
    stopped_at: usize,
}

/// Decodes `bytes` with the real code and checks everything it does against
/// the reference: the handshake, each event, each frame's pixels, the audio,
/// the bytes sent back, how the stream ends and the largest allocation.
/// `delivery` seeds how the bytes arrive.
async fn check(bytes: &[u8], password: Option<&str>, delivery: u64) -> Outcome {
    let mut reference = Reference::new(bytes, password.is_some());
    let mut rng = Rng::new(delivery);
    // Some streams arrive whole, which is fast; the rest in pieces.
    let most = rng.pick(&[0, 0, 0, 1, 5, 64, 2000]);
    // Room for every answer, so that keeping them is not counted as the
    // client's allocation.
    let sent = Arc::new(Mutex::new(Vec::with_capacity(bytes.len() * 2 + 4096)));
    let mut connection = Connection {
        arriving: Arriving {
            bytes: bytes.into(),
            at: 0,
            rng: Rng::new(rng.next()),
            most,
        },
        sent: Sent {
            bytes: sent.clone(),
            rng: Rng::new(rng.next()),
            most: most.min(7),
        },
    };
    support::reset_largest_allocation();
    let (frame, info, layout) = match (
        handshake(&mut connection, password).await,
        &reference.desktop,
    ) {
        (Ok(started), Ok(_)) => started,
        (Err(error), Err(ending)) => {
            assert_ending(*ending, &error);
            model::assert_written(&sent.lock().unwrap(), &reference.written);
            assert_allocation(&reference);
            return Outcome {
                messages: 0,
                whole: false,
                stopped_at: 0,
            };
        }
        (actual, expected) => panic!(
            "the handshake gave {:?}, not {expected:?}",
            actual.map(|(_, info, _)| info)
        ),
    };
    let desktop = reference.desktop.clone().unwrap();
    assert_eq!(
        (info.width, info.height, &info.name, info.protocol_version),
        (
            reference.width,
            reference.height,
            &desktop.name,
            (3, desktop.minor)
        )
    );
    assert_eq!((frame.width, frame.height), (info.width, info.height));
    assert_eq!(
        frame.pixels, reference.pixels,
        "the first frame is not black"
    );
    let Connection { arriving, sent: to } = connection;
    let (mut reader, writer, _) = start_tasks(arriving, to, frame, info, layout);
    let mut sounds = reader.audio_events();
    let mut heard = Vec::new();
    // What the audio channel dropped because nothing was listening, by the
    // number of sounds heard before the drop.
    let mut missed = Vec::new();
    let mut listen = |heard: &mut Vec<audio::Event>, missed: &mut Vec<(usize, u64)>| loop {
        match sounds.try_recv() {
            Ok(sound) => heard.push(sound),
            Err(TryRecvError::Lagged(count)) => missed.push((heard.len(), count)),
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    };
    loop {
        let event = reader.read_event().await;
        // The reference and this bookkeeping allocate too, on the same thread.
        let expected = support::unmeasured(|| {
            listen(&mut heard, &mut missed);
            reference.next()
        });
        match (expected, event) {
            (Step::Report(Reported::Frame), Ok(Event::Frame(frame))) => {
                assert!(frame.is_valid());
                assert_eq!(
                    (frame.width, frame.height),
                    (reference.width, reference.height),
                    "a frame of another size than the server announced"
                );
                // Compared outside the assertion: megabytes make a poor message.
                assert!(frame.pixels == reference.pixels, "a frame's pixels differ");
            }
            (Step::Report(Reported::Unchanged), Ok(Event::Unchanged))
            | (Step::Report(Reported::Bell), Ok(Event::Bell)) => {}
            (Step::Report(Reported::Clipboard(expected)), Ok(Event::Clipboard(text))) => {
                assert_eq!(text, expected);
                assert!(text.len() <= model::MAX_CLIPBOARD);
            }
            (
                Step::Report(Reported::Cursor {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    pixels,
                }),
                Ok(Event::Cursor(cursor)),
            ) => {
                assert_eq!(
                    (
                        cursor.width,
                        cursor.height,
                        cursor.hotspot_x,
                        cursor.hotspot_y
                    ),
                    (width, height, hotspot_x, hotspot_y)
                );
                assert!(cursor.pixels == pixels, "a cursor's pixels differ");
                assert!(cursor.width <= 128 && cursor.height <= 128);
                assert_eq!(
                    cursor.pixels.len(),
                    usize::from(cursor.width) * usize::from(cursor.height) * 4
                );
                assert!(
                    cursor.pixels.is_empty()
                        || (cursor.hotspot_x < cursor.width && cursor.hotspot_y < cursor.height),
                    "a cursor's hotspot is off its shape"
                );
                assert!(
                    cursor
                        .pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| pixel[3] == 0 || pixel[3] == 255)
                );
            }
            (
                Step::Report(Reported::ResizeResult {
                    width,
                    height,
                    status,
                }),
                Ok(Event::ResizeResult {
                    width: actual_width,
                    height: actual_height,
                    status: actual_status,
                }),
            ) => assert_eq!(
                (actual_width, actual_height, actual_status),
                (width, height, status)
            ),
            (Step::End(ending), Err(error)) => {
                assert_ending(ending, &error);
                break;
            }
            (expected, actual) => panic!(
                "after {} messages the client reported {}, not {expected:?}",
                reference.messages,
                match actual {
                    Ok(Event::Frame(_)) => "a frame".to_owned(),
                    Ok(Event::Cursor(_)) => "a cursor".to_owned(),
                    other => format!("{other:?}"),
                }
            ),
        }
    }
    // Nothing is reported after an error, and the decoder ended by returning.
    let after = reader.read_event().await.unwrap_err();
    assert_eq!(after.to_string(), "RFB reader closed");
    (&mut reader.task).await.expect("the decoder panicked");
    assert!(
        !writer.task.is_finished(),
        "the writer ended while its handle was held"
    );
    listen(&mut heard, &mut missed);
    let mut expected = reference.sounds.iter();
    let mut missed = missed.into_iter().peekable();
    for (index, sound) in heard.iter().enumerate() {
        while let Some((_, count)) = missed.next_if(|(at, _)| *at == index) {
            // The channel keeps the newest sounds: the oldest are the ones lost.
            expected.nth(count as usize - 1).expect("lost sounds");
        }
        match (sound, expected.next().expect("a sound that was not sent")) {
            (audio::Event::Begin, Sound::Begin) | (audio::Event::End, Sound::End) => {}
            (audio::Event::Data { bytes, .. }, Sound::Data(sent)) => {
                assert!(bytes.len() <= 65_536 && bytes.len() % 4 == 0);
                assert!(**bytes == **sent, "audio samples differ");
            }
            (actual, expected) => panic!("heard {actual:?}, not {expected:?}"),
        }
    }
    let lost: u64 = missed.map(|(_, count)| count).sum();
    assert_eq!(expected.count() as u64, lost, "sounds went missing");
    assert_eq!(*writer.audio_available().borrow(), reference.audio);
    assert_eq!(writer.supports_resize(), reference.resizable);
    model::assert_written(&sent.lock().unwrap(), &reference.written);
    assert_allocation(&reference);
    Outcome {
        messages: reference.messages,
        whole: reference.ended_between_messages,
        stopped_at: reference.stopped_at,
    }
}

fn assert_allocation(reference: &Reference<'_>) {
    let largest = support::largest_allocation();
    assert!(
        largest <= reference.largest_allocation,
        "one allocation of {largest} bytes where the stream allows {}",
        reference.largest_allocation
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        // With the clock paused, a decoder that waits for something that
        // never comes runs into the timeout at once instead of hanging.
        .start_paused(true)
        .build()
        .unwrap()
}

/// Runs [`check`] to its end: a finite stream is read in finite time.
fn checked(
    runtime: &tokio::runtime::Runtime,
    bytes: &[u8],
    password: Option<&str>,
    delivery: u64,
) -> Outcome {
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3600), check(bytes, password, delivery))
            .await
            .expect("the stream was not read to an end")
    })
}

/// Writes a valid stream and notes where its fields are.
struct Generator<'a> {
    rng: &'a mut Rng,
    bytes: Vec<u8>,
    /// Offsets of counts, lengths, types and coordinates.
    fields: Vec<usize>,
    width: u16,
    height: u16,
    audio: bool,
}

/// A pixel format that is neither of the two the client keeps: 16 bits.
const OTHER_FORMAT: [u8; 16] = [16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0];
const RGBX: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];
const BGRX: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];

impl Generator<'_> {
    fn field(&mut self) {
        self.fields.push(self.bytes.len());
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn random(&mut self, length: usize) {
        let bytes = self.rng.bytes(length);
        self.bytes.extend_from_slice(&bytes);
    }

    /// A side of a desktop. Small, so that rectangles overlap and reach the
    /// edges often, and now and then one pixel or a few hundred.
    fn side(&mut self) -> u16 {
        match self.rng.below(20) {
            0 => 1,
            1 => self.rng.between(100, 300) as u16,
            _ => self.rng.between(1, 40) as u16,
        }
    }

    /// The handshake, and the password the client needs for it.
    fn handshake(&mut self) -> Option<&'static str> {
        // An unknown minor version is spoken to as 3.3.
        let minor = self.rng.pick(&[3, 3, 7, 7, 8, 8, 8, 5, 889]);
        self.field();
        self.bytes
            .extend_from_slice(format!("RFB 003.{minor:03}\n").as_bytes());
        let password = self.rng.pick(&[
            None,
            None,
            Some("secret"),
            Some(""),
            Some("longer than eight"),
        ]);
        let authenticated = password.is_some() && self.rng.chance(2);
        let security = if authenticated { 2 } else { 1 };
        self.field();
        if matches!(minor, 7 | 8) {
            // Types the client does not know, around the one it is to choose.
            // With a password it prefers VNC authentication when offered.
            let mut offered: Vec<u8> = (0..self.rng.below(4))
                .map(|_| self.rng.between(3, 255) as u8)
                .collect();
            offered.insert(self.rng.below(offered.len() + 1), security);
            if password.is_none() && self.rng.chance(2) {
                offered.push(2);
            } else if authenticated && self.rng.chance(2) {
                offered.push(1);
            }
            self.bytes.push(offered.len() as u8);
            self.bytes.extend_from_slice(&offered);
        } else {
            self.u32(u32::from(security));
        }
        if authenticated {
            self.random(16);
        }
        if minor == 8 || authenticated {
            self.field();
            self.u32(0);
        }
        self.field();
        (self.width, self.height) = (self.side(), self.side());
        let (width, height) = (self.width, self.height);
        self.u16(width);
        self.u16(height);
        self.field();
        let mut format = self.rng.pick(&[RGBX, BGRX, BGRX, OTHER_FORMAT]);
        if self.rng.chance(2) {
            // Padding, which a server may leave as it finds it.
            format[13 + self.rng.below(3)] = self.rng.byte();
        }
        self.bytes.extend_from_slice(&format);
        let name: &[u8] = self.rng.pick(&[
            b"QEMU",
            b"QEMU (guest)",
            b"QEMU (caf\xc3\xa9)",
            b"test",
            b"",
            b"QEMUlator",
            b"caf\xe9 \xff\xfe",
        ]);
        self.field();
        self.u32(name.len() as u32);
        self.bytes.extend_from_slice(name);
        password
    }

    fn rectangle(&mut self, x: u16, y: u16, width: u16, height: u16, encoding: i32) {
        self.field();
        for value in [x, y, width, height] {
            self.u16(value);
        }
        self.field();
        self.bytes.extend_from_slice(&encoding.to_be_bytes());
    }

    /// A rectangle on the desktop of at most `budget` pixels.
    fn area(&mut self, budget: usize) -> (u16, u16, u16, u16) {
        let (across, down) = (usize::from(self.width), usize::from(self.height));
        let mut width = match self.rng.below(8) {
            // The whole width takes its own path through the decoder.
            0 | 1 => across,
            2 => 0,
            _ => self.rng.between(0, across),
        };
        let mut height = match self.rng.below(8) {
            0 => down,
            1 => 0,
            _ => self.rng.between(0, down),
        };
        if width * height > budget {
            (width, height) = (0, self.rng.between(0, down));
        }
        let x = self.rng.between(0, across - width);
        let y = self.rng.between(0, down - height);
        (x as u16, y as u16, width as u16, height as u16)
    }

    /// A place for a `width`x`height` rectangle, near `x`,`y` half the time so
    /// that a copy overlaps its source from every side.
    fn place(&mut self, x: u16, y: u16, width: u16, height: u16) -> (u16, u16) {
        let (free_x, free_y) = (
            usize::from(self.width - width),
            usize::from(self.height - height),
        );
        if self.rng.chance(2) {
            let near = |rng: &mut Rng, at: u16, free: usize| {
                (usize::from(at) + rng.below(7)).saturating_sub(3).min(free) as u16
            };
            (near(self.rng, x, free_x), near(self.rng, y, free_y))
        } else {
            (
                self.rng.between(0, free_x) as u16,
                self.rng.between(0, free_y) as u16,
            )
        }
    }

    fn update(&mut self) {
        self.field();
        let padding = self.rng.byte();
        self.bytes.extend_from_slice(&[0, padding]);
        let count = match self.rng.below(12) {
            0 => 0,
            1 => self.rng.between(6, 40),
            _ => self.rng.between(1, 5),
        };
        self.field();
        self.u16(count as u16);
        let mut budget = usize::from(self.width) * usize::from(self.height) * 4;
        let mut resized = false;
        for index in 0..count {
            match self.rng.below(100) {
                0..35 => {
                    let (x, y, width, height) = self.area(budget);
                    budget -= usize::from(width) * usize::from(height);
                    self.rectangle(x, y, width, height, 0);
                    self.random(usize::from(width) * usize::from(height) * 4);
                }
                35..65 => {
                    let (x, y, width, height) = self.area(budget);
                    budget -= usize::from(width) * usize::from(height);
                    let (source_x, source_y) = self.place(x, y, width, height);
                    self.rectangle(x, y, width, height, 1);
                    self.u16(source_x);
                    self.u16(source_y);
                }
                65..80 => self.cursor(),
                80..90 => self.extended_size(&mut resized, &mut budget),
                90..95 => {
                    let (x, y, width, height) = (
                        self.rng.next() as u16,
                        self.rng.next() as u16,
                        self.rng.next() as u16,
                        self.rng.next() as u16,
                    );
                    self.rectangle(x, y, width, height, -259);
                    self.audio = true;
                }
                // DesktopSize ends its update.
                _ if index + 1 == count => {
                    let (width, height) = self.new_size(&mut resized, &mut budget);
                    self.rectangle(0, 0, width, height, -223);
                }
                // A rectangle of nothing, at the far corner.
                _ => self.rectangle(self.width, self.height, 0, 0, 0),
            }
        }
    }

    /// A size for the desktop: another one if this update may still change it.
    fn new_size(&mut self, resized: &mut bool, budget: &mut usize) -> (u16, u16) {
        if !*resized && !self.rng.chance(3) {
            let size = (self.side(), self.side());
            if size != (self.width, self.height) {
                // What was drawn on the old desktop counts on the new one.
                let drawn = usize::from(self.width) * usize::from(self.height) * 4 - *budget;
                (self.width, self.height) = size;
                *resized = true;
                *budget = (usize::from(size.0) * usize::from(size.1) * 4).saturating_sub(drawn);
            }
        }
        (self.width, self.height)
    }

    fn cursor(&mut self) {
        let (width, height) = match self.rng.below(20) {
            0 => (0, 0),
            1 => (128, self.rng.between(1, 128)),
            2 => (self.rng.between(1, 128), 128),
            // Too large to report, and read off the wire all the same.
            3 => (self.rng.between(129, 160), self.rng.between(1, 40)),
            4 => (self.rng.between(1, 4), 1024),
            5 => (1024, self.rng.between(1, 4)),
            _ => (self.rng.between(1, 24), self.rng.between(1, 24)),
        };
        let (x, y) = if width == 0 {
            // Without a shape the hotspot means nothing and may be anything.
            (self.rng.next() as u16, self.rng.next() as u16)
        } else {
            (self.rng.below(width) as u16, self.rng.below(height) as u16)
        };
        self.rectangle(x, y, width as u16, height as u16, -239);
        self.random(width * height * 4 + width.div_ceil(8) * height);
    }

    fn extended_size(&mut self, resized: &mut bool, budget: &mut usize) {
        // Reason 1 answers the client's own request, and a status other than
        // zero refuses it: the rest of such a rectangle means nothing.
        let (reason, status) = match self.rng.below(6) {
            0 => (1, self.rng.between(1, 3) as u16),
            1 => (1, 0),
            _ => (self.rng.pick(&[0, 0, 2, 7]), self.rng.next() as u16),
        };
        if reason == 1 && status != 0 {
            let (width, height) = (self.rng.next() as u16, self.rng.next() as u16);
            self.rectangle(reason, status, width, height, -308);
            let screens = self.rng.below(3);
            self.field();
            self.bytes.push(screens as u8);
            self.random(3 + screens * 16);
            return;
        }
        let (width, height) = self.new_size(resized, budget);
        self.rectangle(reason, status, width, height, -308);
        let screens = self.rng.between(1, 3);
        self.field();
        self.bytes.push(screens as u8);
        self.random(3);
        for id in 0..screens {
            // Screens may overlap; their identifiers differ.
            let across = self.rng.between(1, usize::from(width)) as u16;
            let down = self.rng.between(1, usize::from(height)) as u16;
            let left = self.rng.between(0, usize::from(width - across)) as u16;
            let top = self.rng.between(0, usize::from(height - down)) as u16;
            self.field();
            let identifier = self.rng.next() as u32 & !3 | id as u32;
            self.u32(identifier);
            for value in [left, top, across, down] {
                self.u16(value);
            }
            self.random(4);
        }
    }

    fn cut_text(&mut self, length: i32, bytes: &[u8]) {
        self.field();
        self.bytes.push(3);
        self.random(3);
        self.field();
        self.bytes.extend_from_slice(&length.to_be_bytes());
        self.bytes.extend_from_slice(bytes);
    }

    fn base_clipboard(&mut self) {
        let length = match self.rng.below(10) {
            0 => 0,
            1 => self.rng.between(200, 5000),
            _ => self.rng.between(1, 40),
        };
        let text = self.rng.bytes(length);
        self.cut_text(length as i32, &text);
    }

    fn extended_clipboard(&mut self) {
        let formats = self.rng.next() as u32 & 0xffff;
        let (flags, data) = match self.rng.below(10) {
            0..3 => {
                // Capabilities: one size for each format, and any actions.
                let actions = self.rng.next() as u32 & 0x1e00_0000;
                let sizes = self.rng.bytes(formats.count_ones() as usize * 4);
                (CLIPBOARD_CAPS | actions | formats, sizes)
            }
            3 | 4 => (CLIPBOARD_NOTIFY | formats, Vec::new()),
            5 => (CLIPBOARD_REQUEST | formats, Vec::new()),
            6..9 => {
                let text = text(self.rng);
                let mut payload = (text.len() as u32).to_be_bytes().to_vec();
                payload.extend_from_slice(text.as_bytes());
                (
                    CLIPBOARD_PROVIDE | CLIPBOARD_TEXT,
                    deflated(self.rng, &payload),
                )
            }
            // What the client did not ask for is passed over, whatever it holds.
            _ => {
                let length = self.rng.below(24);
                (
                    self.rng.pick(&[
                        CLIPBOARD_PEEK,
                        CLIPBOARD_PROVIDE | 2,
                        CLIPBOARD_PROVIDE,
                        1 << 29,
                        0,
                    ]),
                    self.rng.bytes(length),
                )
            }
        };
        let mut message = flags.to_be_bytes().to_vec();
        message.extend_from_slice(&data);
        self.cut_text(-(message.len() as i32), &message);
    }

    fn sound(&mut self) {
        if !self.audio {
            // The server acknowledges the extension before it uses it.
            self.field();
            self.bytes.extend_from_slice(&[0, 0, 0, 1]);
            self.rectangle(0, 0, 0, 0, -259);
            self.audio = true;
        }
        self.field();
        self.bytes.extend_from_slice(&[255, 1]);
        let operation = self.rng.below(4).min(2);
        self.field();
        self.u16(operation as u16);
        if operation == 2 {
            let frames = match self.rng.below(10) {
                0 => 0,
                1 => 16_384,
                _ => self.rng.between(1, 64),
            };
            self.field();
            self.u32(frames as u32 * 4);
            self.random(frames * 4);
        }
    }

    /// The header of an update of `count` rectangles.
    fn update_of(&mut self, count: u16) {
        self.field();
        self.bytes.extend_from_slice(&[0, 0]);
        self.field();
        self.u16(count);
    }

    /// A CopyRect of the whole desktop onto itself: a frame's worth of the
    /// update's budget in sixteen bytes.
    fn whole_copy(&mut self) {
        self.rectangle(0, 0, self.width, self.height, 1);
        self.bytes.extend_from_slice(&[0, 0, 0, 0]);
    }

    /// An ExtendedDesktopSize rectangle with one screen for each of `screens`:
    /// identifier, left, top, width and height.
    fn layout(&mut self, reason: u16, size: (u16, u16), screens: &[(u32, u16, u16, u16, u16)]) {
        self.rectangle(reason, 0, size.0, size.1, -308);
        self.field();
        self.bytes.push(screens.len() as u8);
        self.random(3);
        for (id, left, top, across, down) in screens {
            self.field();
            self.u32(*id);
            for value in [left, top, across, down] {
                self.u16(*value);
            }
            self.random(4);
        }
    }

    /// The same, with one screen that covers the desktop.
    fn sized(&mut self, size: (u16, u16)) {
        self.layout(0, size, &[(7, 0, 0, size.0, size.1)]);
    }

    /// A size the desktop does not have now.
    fn other_size(&mut self) -> (u16, u16) {
        loop {
            let size = (
                self.rng.between(1, 24) as u16,
                self.rng.between(1, 24) as u16,
            );
            if size != (self.width, self.height) {
                return size;
            }
        }
    }

    /// A cursor rectangle and as many bytes as a cursor of that size has.
    fn cursor_of(&mut self, x: u16, y: u16, width: u16, height: u16) {
        self.rectangle(x, y, width, height, -239);
        let (width, height) = (usize::from(width), usize::from(height));
        self.fill(width * height * 4 + width.div_ceil(8) * height);
    }

    /// Payload nobody looks at closely, which costs nothing to write.
    fn fill(&mut self, length: usize) {
        let byte = self.rng.byte();
        self.bytes.resize(self.bytes.len() + length, byte);
    }

    fn extended_message(&mut self, flags: u32, data: &[u8]) {
        let mut message = flags.to_be_bytes().to_vec();
        message.extend_from_slice(data);
        self.cut_text(-(message.len() as i32), &message);
    }

    fn acknowledge_audio(&mut self) {
        self.update_of(1);
        self.rectangle(0, 0, 0, 0, -259);
        self.audio = true;
    }

    fn sound_of(&mut self, operation: u16, length: Option<u32>) {
        self.field();
        self.bytes.extend_from_slice(&[255, 1]);
        self.field();
        self.u16(operation);
        if let Some(length) = length {
            self.field();
            self.u32(length);
        }
    }

    /// One message that sits on a limit of the protocol or one step past it.
    /// The reference says which side each is on; what matters here is that
    /// both sides are written, because changing bytes at random hardly ever
    /// lands on a limit. `choice` selects among the forms of one kind, so that
    /// counting through kinds and choices writes every form.
    fn edge(&mut self, kind: usize, choice: usize) {
        fn chosen<T: Copy>(items: &[T], choice: usize) -> T {
            items[choice % items.len()]
        }
        let (width, height) = (self.width, self.height);
        match kind {
            // The pixel budget of an update: four frames, a pixel more, and
            // the new desktop's own budget after a change of size.
            0 => {
                let copies = 4 + (choice & 1) as u16;
                let resize = choice & 2 != 0;
                let pixels = chosen(&[None, Some(1), None, Some(0)], choice / 4);
                self.update_of(copies + u16::from(resize) + u16::from(pixels.is_some()));
                if resize {
                    let size = self.other_size();
                    self.sized(size);
                    (self.width, self.height) = size;
                }
                for _ in 0..copies {
                    self.whole_copy();
                }
                if let Some(side) = pixels {
                    self.rectangle(0, 0, side, side, 0);
                    self.random(usize::from(side) * 4);
                }
            }
            // DesktopSize: last in its update, at the origin, of a size a
            // frame may have.
            1 => {
                let size = self.other_size();
                self.update_of(2);
                self.rectangle(0, 0, size.0, size.1, -223);
                self.rectangle(0, 0, 0, 0, 0);
            }
            2 => {
                let (x, y) = chosen(&[(1, 0), (0, 1), (0, 0)], choice);
                self.update_of(1);
                self.rectangle(x, y, width, height, -223);
            }
            3 => {
                let sizes = [(0, 5), (5, 0), (4097, 4096), (65_535, 65_535), (0, 0)];
                let size = chosen(&sizes, choice);
                self.update_of(1);
                if choice >= sizes.len() {
                    self.rectangle(0, 0, size.0, size.1, -223);
                } else {
                    self.sized(size);
                }
            }
            // One change of size in an update: naming the new size again is
            // not a second change.
            4 => {
                let (first, second) = (self.other_size(), self.other_size());
                self.update_of(3);
                self.sized(first);
                self.sized(if choice & 1 == 0 { first } else { second });
                let last = if choice & 2 == 0 { first } else { second };
                self.rectangle(0, 0, last.0, last.1, -223);
                (self.width, self.height) = last;
            }
            // Screens: at least one, each on the desktop, no identifier twice.
            5 => {
                let size = self.other_size();
                let screens: &[(u32, u16, u16, u16, u16)] = match choice % 7 {
                    0 => &[],
                    1 => &[(1, 0, 0, 1, 1), (1, 0, 0, 1, 1)],
                    2 => &[(1, 1, 0, size.0, size.1)],
                    3 => &[(1, 0, 1, size.0, size.1)],
                    4 => &[(1, 0, 0, 0, 1)],
                    5 => &[(1, 0, 0, 1, 0)],
                    _ => &[(1, 0, 0, size.0, size.1), (2, size.0 - 1, size.1 - 1, 1, 1)],
                };
                let reason = self.rng.pick(&[0, 1, 2]);
                self.update_of(1);
                self.layout(reason, size, screens);
            }
            // Rectangles one pixel off the desktop, to the right and below.
            6 => {
                let (x, y) = chosen(&[(1, 0), (0, 1), (0, 0)], choice);
                self.update_of(1);
                self.rectangle(x, y, width, height, 0);
                self.fill(usize::from(width) * usize::from(height) * 4);
            }
            7 => {
                let over = chosen(
                    &[
                        (1, 0, 0, 0),
                        (0, 1, 0, 0),
                        (0, 0, 1, 0),
                        (0, 0, 0, 1),
                        (0, 0, 0, 0),
                    ],
                    choice,
                );
                self.update_of(1);
                self.rectangle(over.0, over.1, width, height, 1);
                self.u16(over.2);
                self.u16(over.3);
            }
            // Cursors: the hotspot on the shape, a shape or none, and the
            // sizes at which a cursor is passed over and refused.
            8 => {
                let (across, down) = (self.rng.between(1, 9) as u16, self.rng.between(1, 9) as u16);
                let (x, y) = chosen(&[(across, 0), (0, down), (across - 1, down - 1)], choice);
                self.update_of(1);
                self.cursor_of(x, y, across, down);
            }
            9 => {
                let (across, down) = chosen(
                    &[
                        (0, 3),
                        (3, 0),
                        (128, 1),
                        (129, 1),
                        (1, 128),
                        (1, 129),
                        (1024, 1),
                        (1025, 1),
                        (1, 1025),
                        (128, 128),
                    ],
                    choice,
                );
                self.update_of(1);
                self.cursor_of(0, 0, across, down);
            }
            // Audio: only once acknowledged, in whole stereo frames, 64 KiB
            // at most.
            10 => {
                if self.audio && choice & 1 == 0 {
                    // Acknowledged, but not a message of the extension.
                    self.bytes.extend_from_slice(&[255, 2, 0, 1]);
                } else if self.audio {
                    self.sound_of(0, None);
                } else {
                    self.sound_of(choice as u16 % 3, None);
                    self.fill(8);
                }
            }
            11 => {
                if !self.audio {
                    self.acknowledge_audio();
                }
                let length = chosen(&[65_536, 65_540, 65_538, 6, 0], choice);
                self.sound_of(2, Some(length));
                if length.is_multiple_of(4) && length <= 65_536 {
                    self.fill(length as usize);
                }
            }
            12 => {
                if !self.audio {
                    self.acknowledge_audio();
                }
                self.sound_of(chosen(&[3, 256, 65_535], choice), None);
            }
            // Clipboard lengths one past each limit: refused before a byte
            // of the text is read or a buffer made for it.
            13 => {
                let length = chosen(
                    &[
                        model::MAX_CLIPBOARD as i32 + 1,
                        i32::MAX,
                        -(model::MAX_WIRE_TEXT as i32 + 4097),
                        i32::MIN,
                        -1,
                        -3,
                    ],
                    choice,
                );
                self.cut_text(length, &[]);
                let rest = self.rng.below(8);
                self.fill(rest);
            }
            // Extended clipboard messages with a wrong shape.
            14 => match choice % 5 {
                0 => self.extended_message(CLIPBOARD_CAPS | 3, &[]),
                1 => self.extended_message(CLIPBOARD_CAPS | 3, &[0; 4]),
                2 => self.extended_message(CLIPBOARD_CAPS | 3, &[0; 12]),
                3 => self.extended_message(CLIPBOARD_NOTIFY | CLIPBOARD_TEXT, &[0]),
                _ => self.extended_message(CLIPBOARD_REQUEST | CLIPBOARD_TEXT, &[0]),
            },
            // Provided text that is not what its message says it is.
            15 => {
                let text = text(self.rng);
                let mut payload = (text.len() as u32).to_be_bytes().to_vec();
                payload.extend_from_slice(text.as_bytes());
                match choice % 5 {
                    0 => payload[3] = payload[3].wrapping_add(1),
                    1 => payload.push(b'a'),
                    2 => payload.truncate(self.rng.below(4)),
                    _ => {}
                }
                let mut stream = deflated(self.rng, &payload);
                match choice % 5 {
                    3 => stream.push(0),
                    4 => {
                        stream.pop();
                    }
                    _ => {}
                }
                self.extended_message(CLIPBOARD_PROVIDE | CLIPBOARD_TEXT, &stream);
            }
            // What the protocol has no meaning for.
            16 => {
                let encoding = chosen(&[2, 5, 16, -224, -240, i32::MAX, i32::MIN], choice);
                self.update_of(1);
                self.rectangle(0, 0, 1, 1, encoding);
                self.fill(4);
            }
            // The pixel budget across a change of size: whole copies of the
            // old desktop, the change, then as many copies of the new desktop
            // as it allows less what the old ones took, or one more.
            17 => {
                let before = (choice / 2 % 5) as u16;
                let size = self.other_size();
                let taken = usize::from(before) * usize::from(width) * usize::from(height);
                let area = usize::from(size.0) * usize::from(size.1);
                let after = ((area * 4).saturating_sub(taken) / area + (choice & 1)) as u16;
                self.update_of(before + 1 + after);
                for _ in 0..before {
                    self.whole_copy();
                }
                self.sized(size);
                (self.width, self.height) = size;
                for _ in 0..after {
                    self.whole_copy();
                }
            }
            // A message type the protocol does not have.
            _ => {
                self.field();
                self.bytes.push(chosen(&[1, 4, 150, 254], choice));
                let rest = self.rng.below(12);
                self.fill(rest);
            }
        }
    }

    /// An edge that takes megabytes, for campaigns: text at the clipboard
    /// limits, the largest cursor the wire may carry and the largest chunk of
    /// audio.
    fn large_edge(&mut self) {
        match self.rng.below(5) {
            0 => {
                // Latin-1 text at the limit, and over it once converted.
                let mut text = vec![b'a'; model::MAX_CLIPBOARD];
                if self.rng.chance(2) {
                    text[self.rng.below(model::MAX_CLIPBOARD)] = 0xe9;
                }
                self.cut_text(text.len() as i32, &text);
            }
            1 => {
                // The longest extended message: an action nobody asked for.
                let data = vec![0x5a; model::MAX_WIRE_TEXT + 4092];
                self.extended_message(CLIPBOARD_PEEK, &data);
            }
            2 => {
                // Provided text at the limit, a byte over it, and at the
                // limit only once its line ends are converted.
                let text = match self.rng.below(4) {
                    0 => "a".repeat(model::MAX_CLIPBOARD),
                    1 => "a".repeat(model::MAX_CLIPBOARD + 1),
                    2 => "\r\n".repeat(model::MAX_CLIPBOARD) + "\0",
                    _ => "\r\n".repeat(model::MAX_CLIPBOARD) + "a\0",
                };
                let mut payload = (text.len() as u32).to_be_bytes().to_vec();
                payload.extend_from_slice(text.as_bytes());
                let stream = deflated(self.rng, &payload);
                self.extended_message(CLIPBOARD_PROVIDE | CLIPBOARD_TEXT, &stream);
            }
            3 => {
                self.update_of(1);
                self.cursor_of(1023, 1023, 1024, 1024);
            }
            _ => {
                // A desktop of the most pixels a frame may have.
                self.update_of(1);
                self.sized((4096, 4096));
                (self.width, self.height) = (4096, 4096);
            }
        }
    }

    fn message(&mut self) {
        match self.rng.below(100) {
            0..62 => self.update(),
            62..68 => {
                self.field();
                self.bytes.push(2);
            }
            68..78 => self.base_clipboard(),
            78..90 => self.extended_clipboard(),
            _ => self.sound(),
        }
    }
}

/// Clipboard text of every kind the conversion has a rule for.
pub fn text(rng: &mut Rng) -> String {
    let pieces = [
        "a",
        "Z",
        " ",
        "\n",
        "\r\n",
        "\r",
        "\r\r\n",
        "\0",
        "\u{e9}",
        "\u{ff}",
        "\u{100}",
        "\u{65e5}",
        "\u{1f680}",
        "\u{2028}",
        "\t",
    ];
    (0..rng.below(12)).map(|_| rng.pick(&pieces)).collect()
}

/// A zlib stream of `data`: stored blocks written by hand, or the encoder's
/// own output at any level.
pub fn deflated(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    if rng.chance(2) {
        use std::io::Write;
        let level = flate2::Compression::new(rng.below(10) as u32);
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), level);
        encoder.write_all(data).unwrap();
        return encoder.finish().unwrap();
    }
    // The header for a 32 KiB window, then blocks of raw bytes and the
    // checksum of all of them.
    let mut stream = vec![0x78, 0x01];
    let most = rng.pick(&[1, 7, 65_535]);
    let mut blocks = data.chunks(most).peekable();
    if blocks.peek().is_none() {
        stream.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        stream.push(u8::from(blocks.peek().is_none()));
        stream.extend_from_slice(&(block.len() as u16).to_le_bytes());
        stream.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        stream.extend_from_slice(block);
    }
    let (mut low, mut high) = (1u32, 0u32);
    for byte in data {
        low = (low + u32::from(*byte)) % 65_521;
        high = (high + low) % 65_521;
    }
    stream.extend_from_slice(&(high << 16 | low).to_be_bytes());
    stream
}

/// A valid stream, the offsets of its fields and the client's password.
fn generated(rng: &mut Rng) -> (Vec<u8>, Vec<usize>, Option<&'static str>) {
    let mut generator = Generator {
        rng,
        bytes: Vec::new(),
        fields: Vec::new(),
        width: 0,
        height: 0,
        audio: false,
    };
    let password = generator.handshake();
    let messages = match generator.rng.below(10) {
        0 => 0,
        1 => generator.rng.between(10, 30),
        _ => generator.rng.between(1, 8),
    };
    for _ in 0..messages {
        generator.message();
    }
    (generator.bytes, generator.fields, password)
}

/// How many kinds of edge [`Generator::edge`] writes.
const EDGES: usize = 19;
/// How many kinds of edge [`refused_handshake`] writes.
const HANDSHAKE_EDGES: usize = 9;
/// The most forms one kind of edge has.
const EDGE_CHOICES: usize = 10;

/// A handshake that stops at one of its limits or refusals, and the client's
/// password.
fn refused_handshake(rng: &mut Rng, kind: usize, choice: usize) -> (Vec<u8>, Option<&'static str>) {
    fn chosen<T: Copy>(items: &[T], choice: usize) -> T {
        items[choice % items.len()]
    }
    // A reason of any length, and the bytes of one that may be read.
    let length = chosen(&[0, 12, 65_536, 65_537, u32::MAX], choice);
    let mut reason = length.to_be_bytes().to_vec();
    reason.extend_from_slice(&rng.bytes(length.min(65_536) as usize));
    let mut password = None;
    let mut bytes = match kind {
        // The server refuses.
        0 => [b"RFB 003.008\n\0".as_slice(), &reason].concat(),
        1 => [b"RFB 003.003\n\0\0\0\0".as_slice(), &reason].concat(),
        2 => [b"RFB 003.008\n\x01\x01\0\0\0\x01".as_slice(), &reason].concat(),
        3 => {
            password = Some("secret");
            let mut bytes = b"RFB 003.007\n\x01\x02".to_vec();
            bytes.extend_from_slice(&rng.bytes(16));
            bytes.extend_from_slice(&chosen(&[1, 2, u32::MAX], choice).to_be_bytes());
            bytes
        }
        // Nothing the client can do is on offer.
        4 => {
            password = chosen(&[None, Some("secret")], choice / 4);
            let offered = chosen(&[&[5, 16, 19][..], &[2], &[0], &[2, 2]], choice);
            let mut bytes = format!("RFB 003.00{}\n", rng.pick(&[7, 8])).into_bytes();
            bytes.push(offered.len() as u8);
            bytes.extend_from_slice(offered);
            bytes.extend_from_slice(&rng.bytes(24));
            bytes
        }
        5 => {
            password = chosen(&[None, Some("secret")], choice / 4);
            let mut bytes = b"RFB 003.003\n".to_vec();
            bytes.extend_from_slice(&chosen(&[2, 3, 30, u32::MAX], choice).to_be_bytes());
            bytes.extend_from_slice(&rng.bytes(24));
            bytes
        }
        // A desktop no frame can hold.
        6 => {
            let (width, height) = chosen(
                &[(0, 4), (4, 0), (4097, 4096), (65_535, 65_535), (0, 0)],
                choice,
            );
            desktop(width, height, RGBX, "test")
        }
        // A name at the limit and past it.
        7 => {
            let length = chosen(&[65_536, 65_537, u32::MAX], choice);
            let mut bytes = desktop(3, 2, BGRX, "");
            bytes.truncate(bytes.len() - 4);
            bytes.extend_from_slice(&length.to_be_bytes());
            if length == 65_536 {
                bytes.resize(bytes.len() + 65_536, b'n');
            }
            bytes
        }
        // Banners that are not a version of this protocol.
        _ => {
            let banner: &[u8] = chosen(
                &[
                    b"RFB 004.000\n",
                    b"RFB 003.008\r",
                    b"rfb 003.008\n",
                    b"RFB 003.00a\n",
                    b"RFB 003 008\n",
                    b"RFB 002.008\n",
                    b"RFB 03.0008\n",
                ],
                choice,
            );
            let mut bytes = banner.to_vec();
            bytes.extend_from_slice(&desktop(3, 2, RGBX, "test")[12..]);
            bytes
        }
    };
    if rng.chance(4) {
        // Whatever follows a refusal is not read.
        bytes.extend_from_slice(&[2, 2, 2]);
    }
    (bytes, password)
}

/// A stream with edge number `number` among valid messages. Counting through
/// the numbers writes every form of every kind of edge.
fn generated_edge(rng: &mut Rng, number: u64) -> (Vec<u8>, Vec<usize>, Option<&'static str>) {
    let kinds = EDGES + HANDSHAKE_EDGES;
    let (kind, choice) = (
        (number % kinds as u64) as usize,
        (number / kinds as u64) as usize % EDGE_CHOICES,
    );
    if kind >= EDGES {
        let (bytes, password) = refused_handshake(rng, kind - EDGES, choice);
        return (bytes, Vec::new(), password);
    }
    let mut generator = Generator {
        rng,
        bytes: Vec::new(),
        fields: Vec::new(),
        width: 0,
        height: 0,
        audio: false,
    };
    let password = generator.handshake();
    for _ in 0..generator.rng.below(3) {
        generator.message();
    }
    // One time in two hundred, one of the edges that are megabytes long.
    if generator.rng.chance(200) {
        generator.large_edge();
    } else {
        generator.edge(kind, choice);
    }
    // What follows shows whether the edge was read to its end, if it was
    // accepted, and is not read at all if it was not.
    generator.field();
    generator.bytes.push(2);
    (generator.bytes, generator.fields, password)
}

/// Edges checked in one iteration.
const EDGES_EACH: u64 = 5;
/// Iterations on every run: enough to write every form of every edge once.
/// Each checks a valid stream, [`EDGES_EACH`] with an edge, and three with
/// changed bytes.
const DEFAULT_ITERATIONS: u64 = ((EDGES + HANDSHAKE_EDGES) * EDGE_CHOICES) as u64 / EDGES_EACH;

#[test]
fn fuzz_server_stream() {
    let runtime = runtime();
    support::run("rfb-server-stream", DEFAULT_ITERATIONS, |rng, iteration| {
        let (bytes, fields, password) = generated(rng);
        let outcome = checked(&runtime, &bytes, password, rng.next());
        assert!(
            outcome.whole,
            "a generated stream was refused after {} messages, at {:?}",
            outcome.messages,
            &bytes[outcome.stopped_at..bytes.len().min(outcome.stopped_at + 64)]
        );
        let mut edge = (Vec::new(), Vec::new(), None);
        for number in 0..EDGES_EACH {
            edge = generated_edge(rng, iteration * EDGES_EACH + number);
            checked(&runtime, &edge.0, edge.2, rng.next());
        }
        for (bytes, fields, password) in [
            (&bytes, &fields, password),
            (&bytes, &fields, password),
            (&edge.0, &edge.1, edge.2),
        ] {
            let mut changed = bytes.clone();
            for _ in 0..1 + rng.below(3) {
                support::mutate(rng, &mut changed, fields, true);
            }
            // Now and then the client has a password where it had none, or none
            // where it had one.
            let password = if rng.chance(8) {
                password.xor(Some("other"))
            } else {
                password
            };
            checked(&runtime, &changed, password, rng.next());
        }
    });
}

/// A handshake for a `width`x`height` desktop over RFB 3.8 without a password.
fn desktop(width: u16, height: u16, format: [u8; 16], name: &str) -> Vec<u8> {
    let mut bytes = b"RFB 003.008\n\x01\x01\0\0\0\0".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&format);
    bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

#[test]
fn the_reference_reads_a_stream_the_way_the_protocol_describes_it() {
    // The fuzzer trusts the reference, so the reference is checked by hand
    // once: a 3x2 desktop in BGRX, one Raw rectangle and one overlapping copy.
    let mut bytes = desktop(3, 2, BGRX, "QEMU");
    bytes.extend_from_slice(&[0, 0, 0, 2]);
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0]);
    bytes.extend_from_slice(&[3, 2, 1, 9, 6, 5, 4, 9]);
    // Copy the top row's first two pixels one to the right.
    bytes.extend_from_slice(&[0, 1, 0, 0, 0, 2, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0]);
    bytes.push(2);
    let mut reference = Reference::new(&bytes, false);
    assert_eq!(reference.desktop.as_ref().unwrap().name, "QEMU");
    assert!(matches!(reference.next(), Step::Report(Reported::Frame)));
    let black = [0, 0, 0, 255];
    let expected = [
        [1, 2, 3, 255],
        [1, 2, 3, 255],
        [4, 5, 6, 255],
        black,
        black,
        black,
    ];
    assert_eq!(reference.pixels, expected.concat());
    assert!(matches!(reference.next(), Step::Report(Reported::Bell)));
    assert!(matches!(reference.next(), Step::End(Ending::Truncated)));
    assert!(reference.ended_between_messages);
    assert_eq!(reference.messages, 2);
    checked(&runtime(), &bytes, None, 0);
}
