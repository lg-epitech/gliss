//! Bounded RFB capture and input over TCP.
//!
//! Wire formats follow [RFC 6143](https://www.rfc-editor.org/rfc/rfc6143.html).
//! Screen resizing follows the [RFB extension specification](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst).

pub mod audio;
mod clipboard;

use std::{
    ops::Deref,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use des::{
    Des,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use gliss_core::{Damage, Frame, Input, MAX_CLIPBOARD, MAX_PIXELS};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    net::TcpStream,
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};

/// Red in the low byte: the byte order of a canonical frame.
const RGBX: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];
/// Blue in the low byte: what QEMU and most little-endian servers store.
const BGRX: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];

/// Byte order of pixels on the wire.
///
/// A server asked for anything but its own format converts every pixel it
/// sends. QEMU does that one pixel at a time, which costs tens of milliseconds
/// for a full 1920x1080 update and stalls its update worker. Taking the
/// server's format and swapping two bytes here is far cheaper, so a server
/// that stores BGRX is asked for BGRX. Every other server is asked for RGBX.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Rgbx,
    Bgrx,
}

impl Layout {
    /// The 13 meaningful bytes of a PIXEL_FORMAT; the rest is padding.
    fn of_server(native: &[u8; 16]) -> Self {
        if native[..13] == BGRX[..13] {
            Self::Bgrx
        } else {
            Self::Rgbx
        }
    }

    fn pixel_format(self) -> &'static [u8; 16] {
        match self {
            Self::Rgbx => &RGBX,
            Self::Bgrx => &BGRX,
        }
    }

    /// Copy wire pixels from `src` into `dst` as opaque RGBA in a single pass.
    fn copy_to_rgba(self, dst: &mut [u8], src: &[u8]) {
        debug_assert_eq!(dst.len(), src.len());
        debug_assert!(dst.len().is_multiple_of(4));
        match self {
            Self::Rgbx => {
                #[cfg(target_arch = "x86_64")]
                if dst.len() >= 32 && std::arch::is_x86_feature_detected!("avx2") {
                    let simd_len = dst.len() & !31;
                    // SAFETY: AVX2 support checked above and both slices have length >= simd_len.
                    unsafe {
                        copy_rgbx_to_rgba_avx2(&mut dst[..simd_len], &src[..simd_len]);
                    }
                    copy_rgbx_to_rgba_scalar(&mut dst[simd_len..], &src[simd_len..]);
                    return;
                }
                copy_rgbx_to_rgba_scalar(dst, src);
            }
            Self::Bgrx => {
                #[cfg(target_arch = "x86_64")]
                if dst.len() >= 32 && std::arch::is_x86_feature_detected!("avx2") {
                    let simd_len = dst.len() & !31;
                    // SAFETY: AVX2 support checked above and both slices have length >= simd_len.
                    unsafe {
                        copy_bgrx_to_rgba_avx2(&mut dst[..simd_len], &src[..simd_len]);
                    }
                    copy_bgrx_to_rgba_scalar(&mut dst[simd_len..], &src[simd_len..]);
                    return;
                }
                copy_bgrx_to_rgba_scalar(dst, src);
            }
        }
    }

    /// Rewrite wire pixels as opaque RGBA in place.
    fn to_rgba(self, pixels: &mut [u8]) {
        match self {
            Self::Rgbx => {
                for pixel in pixels.as_chunks_mut::<4>().0 {
                    pixel[3] = 255;
                }
            }
            Self::Bgrx => {
                #[cfg(target_arch = "x86_64")]
                if pixels.len() >= 32 && std::arch::is_x86_feature_detected!("avx2") {
                    let simd_len = pixels.len() & !31;
                    // SAFETY: AVX2 support checked above and slice is truncated to a 32-byte multiple.
                    unsafe {
                        bgrx_to_rgba_avx2(&mut pixels[..simd_len]);
                    }
                    bgrx_to_rgba_scalar(&mut pixels[simd_len..]);
                    return;
                }
                bgrx_to_rgba_scalar(pixels);
            }
        }
    }
}

fn copy_rgbx_to_rgba_scalar(dst: &mut [u8], src: &[u8]) {
    for (out, inp) in dst
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<4>().0)
    {
        *out = [inp[0], inp[1], inp[2], 255];
    }
}

fn copy_bgrx_to_rgba_scalar(dst: &mut [u8], src: &[u8]) {
    for (out, inp) in dst
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<4>().0)
    {
        *out = [inp[2], inp[1], inp[0], 255];
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_rgbx_to_rgba_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::*;
    let alpha = _mm256_set1_epi32(0xff00_0000u32 as i32);
    let simd_len = dst.len().min(src.len()) & !31;
    // SAFETY: `simd_len` is a multiple of 32 and within both slices.
    unsafe {
        let mut s = src.as_ptr();
        let mut d = dst.as_mut_ptr();
        let end = d.add(simd_len);
        while d < end {
            let v = _mm256_loadu_si256(s.cast());
            _mm256_storeu_si256(d.cast(), _mm256_or_si256(v, alpha));
            s = s.add(32);
            d = d.add(32);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_bgrx_to_rgba_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::*;
    let shuf = _mm256_setr_epi8(
        2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, 2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11,
        14, 13, 12, 15,
    );
    let alpha = _mm256_set1_epi32(0xff00_0000u32 as i32);
    let simd_len = dst.len().min(src.len()) & !31;
    // SAFETY: `simd_len` is a multiple of 32 and within both slices.
    unsafe {
        let mut s = src.as_ptr();
        let mut d = dst.as_mut_ptr();
        let end = d.add(simd_len);
        while d < end {
            let v = _mm256_loadu_si256(s.cast());
            let swapped = _mm256_or_si256(_mm256_shuffle_epi8(v, shuf), alpha);
            _mm256_storeu_si256(d.cast(), swapped);
            s = s.add(32);
            d = d.add(32);
        }
    }
}

fn bgrx_to_rgba_scalar(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let bgrx = u32::from_le_bytes(*pixel);
        let rgba =
            (bgrx & 0x0000_ff00) | ((bgrx >> 16) & 0xff) | ((bgrx & 0xff) << 16) | 0xff00_0000;
        *pixel = rgba.to_le_bytes();
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bgrx_to_rgba_avx2(pixels: &mut [u8]) {
    use std::arch::x86_64::*;
    let shuf = _mm256_setr_epi8(
        2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, 2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11,
        14, 13, 12, 15,
    );
    let alpha = _mm256_set1_epi32(0xff00_0000u32 as i32);
    let simd_len = pixels.len() & !31;
    // SAFETY: `simd_len <= pixels.len()` is a multiple of 32 bytes.
    unsafe {
        let mut ptr = pixels.as_mut_ptr();
        let end = ptr.add(simd_len);
        while ptr < end {
            let v = _mm256_loadu_si256(ptr.cast());
            let swapped = _mm256_or_si256(_mm256_shuffle_epi8(v, shuf), alpha);
            _mm256_storeu_si256(ptr.cast(), swapped);
            ptr = ptr.add(32);
        }
    }
}
const ENCODINGS: [i32; 7] = [0, 1, -239, -223, -308, clipboard::ENCODING, audio::ENCODING];
/// Most bytes of a full-width Raw rectangle taken by one direct read so in-place
/// SIMD conversion stays L2-cache-resident.
const RAW_READ_BYTES: usize = 256 * 1024;
/// Contiguous Raw rectangles at least as large as the 32 KiB read buffer bypass
/// the buffer via `read_exact`; smaller rects convert straight from `fill_buf`.
const DIRECT_READ_THRESHOLD: usize = 32 * 1024;
const MAX_TEXT: usize = 64 * 1024;
/// Longest side of a cursor the wire may carry. A larger one is a protocol error.
const MAX_WIRE_CURSOR_SIDE: u16 = 1024;
/// Longest side of a cursor reported as an [`Event::Cursor`]. Browsers ignore a
/// cursor image above 128x128, so a larger shape is read off the wire and
/// dropped instead of being held in memory and sent on.
pub const MAX_CURSOR_SIDE: u16 = 128;
/// Pixels one framebuffer update may write or move, as a multiple of the frame
/// area. A server draws each pixel once and may scroll the frame first, which
/// is twice the area; four leaves room for overlapping rectangles. Without a
/// bound, 16-byte CopyRect rectangles each move a whole frame.
const UPDATE_AREA_BUDGET: usize = 4;
/// Work one update does between two returns to the runtime, in pixels written
/// or moved. Other tasks on the worker wait at most this long plus one
/// rectangle, whatever the server sends and however fast it arrives. A fixed
/// return for every rectangle would bound the wait the same but costs about a
/// microsecond each: QEMU sends thousands of one-line rectangles for a
/// scrolling terminal, and that measured 19% more CPU for the whole server.
const YIELD_WORK: usize = 65_536;
/// What a rectangle counts for besides its pixels, so that rectangles that
/// draw nothing add up too: 128 of them between two returns.
const RECTANGLE_WORK: usize = YIELD_WORK / 128;
/// Most characters of server text kept in an error message. A refusal reason
/// is one sentence; the protocol allows 64 KiB of it.
const MAX_REPORTED_TEXT: usize = 256;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a write to the server may make no progress. A server that reads
/// at all takes a 10-byte message at once; one that has stopped reading would
/// otherwise hold the writer, and every input behind it, for the whole session.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub width: u16,
    pub height: u16,
    pub name: String,
    pub protocol_version: (u8, u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub pixels: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A framebuffer update that changed pixels or the size.
    Frame(Frame),
    /// A framebuffer update that changed neither. It still answers an update
    /// request, but there is no new picture to copy or encode.
    Unchanged,
    Clipboard(String),
    Cursor(Cursor),
    Bell,
    ResizeResult {
        width: u16,
        height: u16,
        status: u16,
    },
}

const POOL_CAPACITY: usize = 2;
const HISTORY_LEN: usize = 4;

struct PooledBuffer {
    pixels: Vec<u8>,
    generation: u64,
}

#[derive(Default)]
struct FramePool {
    buffers: Vec<PooledBuffer>,
}

impl FramePool {
    fn take(&mut self, byte_len: usize) -> Option<PooledBuffer> {
        // Drop buffers from older dimensions so memory stays bounded across resizes.
        self.buffers.retain(|buf| buf.pixels.len() == byte_len);
        // Prefer the most recent generation to minimize dirty tiles to sync.
        let best = self
            .buffers
            .iter()
            .enumerate()
            .max_by_key(|(_, buf)| buf.generation)
            .map(|(index, _)| index)?;
        Some(self.buffers.swap_remove(best))
    }

    fn put(&mut self, pixels: Vec<u8>, generation: u64) {
        if self.buffers.len() >= POOL_CAPACITY
            && let Some((oldest, _)) = self
                .buffers
                .iter()
                .enumerate()
                .min_by_key(|(_, buf)| buf.generation)
        {
            self.buffers.swap_remove(oldest);
        }
        self.buffers.push(PooledBuffer { pixels, generation });
    }
}

/// An immutable framebuffer snapshot whose pixel allocation returns to the RFB
/// reader's pool when the last handle drops, avoiding per-frame `mmap`/`munmap`.
#[derive(Debug)]
pub struct PooledFrame {
    frame: Frame,
    damage: Damage,
    generation: u64,
    pool: Weak<Mutex<FramePool>>,
}

impl PooledFrame {
    pub fn from_frame(frame: Frame) -> Self {
        let damage = Damage::full(frame.width, frame.height);
        Self {
            frame,
            damage,
            generation: 0,
            pool: Weak::new(),
        }
    }

    pub fn damage(&self) -> &Damage {
        &self.damage
    }

    pub fn into_frame(mut self) -> Frame {
        self.pool = Weak::new();
        std::mem::replace(
            &mut self.frame,
            Frame {
                width: 0,
                height: 0,
                pixels: Vec::new(),
            },
        )
    }
}

impl Deref for PooledFrame {
    type Target = Frame;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        if self.frame.pixels.is_empty() {
            return;
        }
        if let Some(pool) = self.pool.upgrade() {
            let pixels = std::mem::take(&mut self.frame.pixels);
            if let Ok(mut guard) = pool.lock() {
                guard.put(pixels, self.generation);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum PooledEvent {
    Frame(Arc<PooledFrame>),
    Unchanged,
    Clipboard(String),
    Cursor(Cursor),
    Bell,
    ResizeResult {
        width: u16,
        height: u16,
        status: u16,
    },
}

/// Dropping this handle stops wire decoding. Cancelling `read_event` leaves it intact.
pub struct RfbReader {
    events: mpsc::Receiver<Result<PooledEvent>>,
    audio: broadcast::Sender<audio::Event>,
    task: JoinHandle<()>,
}

/// Messages are serialized by an owned task, even if the caller cancels its wait.
pub struct RfbWriter {
    commands: mpsc::Sender<WriteCommand>,
    audio_available: watch::Receiver<bool>,
    task: JoinHandle<()>,
    layout: Arc<Mutex<Option<Screen>>>,
}

enum WriteMessage {
    Inline { len: u8, bytes: [u8; 24] },
    ClipboardInput(String),
    ClipboardRemote { extended: bool, bytes: Vec<u8> },
}

impl WriteMessage {
    fn inline(slice: &[u8]) -> Self {
        debug_assert!(slice.len() <= 24);
        let mut bytes = [0; 24];
        bytes[..slice.len()].copy_from_slice(slice);
        Self::Inline {
            len: slice.len() as u8,
            bytes,
        }
    }
}

struct WriteCommand {
    message: WriteMessage,
    completed: oneshot::Sender<Result<Option<String>>>,
}

#[derive(Clone, Copy)]
struct Screen {
    id: u32,
    flags: u32,
}

/// Connect with RFB 3.3, 3.7 or 3.8 and negotiate opaque, little-endian RGBA pixels.
/// The caller must request the initial non-incremental update.
pub async fn connect(
    address: &str,
    password: Option<&str>,
) -> Result<(RfbReader, RfbWriter, ServerInfo)> {
    timeout(HANDSHAKE_TIMEOUT, connect_inner(address, password))
        .await
        .context("RFB connection/handshake timed out")?
}

async fn connect_inner(
    address: &str,
    password: Option<&str>,
) -> Result<(RfbReader, RfbWriter, ServerInfo)> {
    let mut stream = TcpStream::connect(address)
        .await
        .context("connect to RFB server")?;
    stream.set_nodelay(true)?;
    let (frame, info, layout) = handshake(&mut stream, password).await?;
    // Owned halves: a generic split would put a mutex between reading and writing.
    let (read, write) = stream.into_split();
    Ok(start_tasks(read, write, frame, info, layout))
}

async fn handshake<S>(stream: &mut S, password: Option<&str>) -> Result<(Frame, ServerInfo, Layout)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut banner = [0; 12];
    stream
        .read_exact(&mut banner)
        .await
        .context("read RFB version")?;
    ensure!(
        &banner[..4] == b"RFB "
            && banner[7] == b'.'
            && banner[11] == b'\n'
            && banner[4..7]
                .iter()
                .chain(banner[8..11].iter())
                .all(u8::is_ascii_digit),
        "invalid RFB version banner"
    );
    ensure!(&banner[4..7] == b"003", "unsupported RFB major version");
    let minor = match &banner[8..11] {
        b"008" => 8,
        b"007" => 7,
        _ => 3,
    };
    stream
        .write_all(format!("RFB 003.{minor:03}\n").as_bytes())
        .await?;
    negotiate_security(stream, minor, password).await?;
    stream.write_u8(1).await?;
    let width = stream.read_u16().await?;
    let height = stream.read_u16().await?;
    let frame =
        Frame::new(width, height).context("invalid or excessive RFB framebuffer dimensions")?;
    let mut native_format = [0; 16];
    stream.read_exact(&mut native_format).await?;
    let name = read_text(stream, MAX_TEXT).await?;
    let info = ServerInfo {
        width,
        height,
        name,
        protocol_version: (3, minor),
    };
    let layout = Layout::of_server(&native_format);
    let mut format = vec![0, 0, 0, 0];
    format.extend_from_slice(layout.pixel_format());
    stream.write_all(&format).await?;
    let mut encodings = vec![2, 0];
    encodings.extend_from_slice(&(ENCODINGS.len() as u16).to_be_bytes());
    for encoding in ENCODINGS {
        encodings.extend_from_slice(&encoding.to_be_bytes());
    }
    stream.write_all(&encodings).await?;
    Ok((frame, info, layout))
}

fn start_tasks<R, W>(
    read: R,
    write: W,
    frame: Frame,
    info: ServerInfo,
    pixel_layout: Layout,
) -> (RfbReader, RfbWriter, ServerInfo)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let layout = Arc::new(Mutex::new(None));
    let (commands, command_rx) = mpsc::channel(8);
    let read_commands = commands.clone();
    let (event_tx, events) = mpsc::channel(2);
    let (audio, _) = broadcast::channel(8);
    let (audio_available_tx, audio_available) = watch::channel(false);
    let reader_audio = audio::Sink {
        events: audio.clone(),
        available: audio_available_tx,
    };
    let read_layout = layout.clone();
    let (write_failed_tx, write_failed) = oneshot::channel();
    let read_task = tokio::spawn(async move {
        // QEMU sends many small rectangle headers and partial rows during scroll.
        // A 32 KiB buffer fits in L1d and lets `decode` fuse read + pixel conversion.
        let read = BufReader::with_capacity(32 * 1024, read);
        let result = tokio::select! {
            result = decode(
                read,
                Surface {
                    frame,
                    pixels: pixel_layout,
                },
                read_layout,
                &read_commands,
                &event_tx,
                &reader_audio,
            ) => result,
            // A connection that cannot be written to is over for reading too:
            // nothing can ask it for another update.
            Ok(error) = write_failed => Err(error),
        };
        // The read half is closed by now: decoding owned it.
        if let Err(error) = result {
            let _ = event_tx.send(Err(error)).await;
        }
    });
    let clipboard = clipboard::Clipboard::new(&info.name);
    let write_task = tokio::spawn(write_messages(
        write,
        clipboard,
        command_rx,
        write_failed_tx,
    ));
    (
        RfbReader {
            events,
            audio,
            task: read_task,
        },
        RfbWriter {
            commands,
            audio_available,
            task: write_task,
            layout,
        },
        info,
    )
}

async fn read_text<R: AsyncRead + Unpin>(stream: &mut R, max: usize) -> Result<String> {
    let length = stream.read_u32().await? as usize;
    ensure!(length <= max, "RFB text exceeds {max} bytes");
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Server text for an error message, which ends up in a log.
fn reported(text: &str) -> String {
    gliss_core::printable(text, MAX_REPORTED_TEXT)
}

async fn negotiate_security<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    minor: u8,
    password: Option<&str>,
) -> Result<()> {
    let security = if minor == 3 {
        stream.read_u32().await?
    } else {
        let count = stream.read_u8().await?;
        if count == 0 {
            bail!(
                "RFB server refused connection: {}",
                reported(&read_text(stream, MAX_TEXT).await?)
            );
        }
        let mut types = vec![0; usize::from(count)];
        stream.read_exact(&mut types).await?;
        let selected = if password.is_some() && types.contains(&2) {
            2
        } else if types.contains(&1) {
            1
        } else if types.contains(&2) {
            bail!("RFB server requires a VNC password");
        } else {
            bail!("RFB server offers no supported security type");
        };
        stream.write_u8(selected).await?;
        u32::from(selected)
    };
    if security == 0 {
        bail!(
            "RFB server refused connection: {}",
            reported(&read_text(stream, MAX_TEXT).await?)
        );
    }
    ensure!(
        security == 1 || security == 2,
        "unsupported RFB security type {security}"
    );
    if security == 2 {
        let password = password.context("RFB server requires a VNC password")?;
        let mut key = [0; 8];
        // Standard DES numbers bits oppositely to VNC's password key convention.
        for (destination, byte) in key.iter_mut().zip(password.as_bytes()) {
            *destination = byte.reverse_bits();
        }
        let cipher = Des::new_from_slice(&key).expect("DES key has eight bytes");
        let mut challenge = [0; 16];
        stream.read_exact(&mut challenge).await?;
        for block in challenge.as_chunks_mut::<8>().0 {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        stream.write_all(&challenge).await?;
    }
    if minor == 8 || security == 2 {
        let status = stream.read_u32().await?;
        if status != 0 {
            if minor == 8 {
                bail!(
                    "RFB authentication failed: {}",
                    reported(&read_text(stream, MAX_TEXT).await?)
                );
            }
            bail!("RFB authentication failed (status {status})");
        }
    }
    Ok(())
}

impl RfbReader {
    /// Audio never waits for a consumer: lagging subscribers lose old samples.
    pub fn audio_events(&self) -> broadcast::Receiver<audio::Event> {
        self.audio.subscribe()
    }

    pub async fn read_pooled_event(&mut self) -> Result<PooledEvent> {
        self.events.recv().await.context("RFB reader closed")?
    }

    pub async fn read_event(&mut self) -> Result<Event> {
        let event = self.read_pooled_event().await?;
        Ok(match event {
            PooledEvent::Frame(frame) => {
                let frame = match Arc::try_unwrap(frame) {
                    Ok(pooled) => pooled.into_frame(),
                    Err(shared) => (**shared).clone(),
                };
                Event::Frame(frame)
            }
            PooledEvent::Unchanged => Event::Unchanged,
            PooledEvent::Clipboard(text) => Event::Clipboard(text),
            PooledEvent::Cursor(cursor) => Event::Cursor(cursor),
            PooledEvent::Bell => Event::Bell,
            PooledEvent::ResizeResult {
                width,
                height,
                status,
            } => Event::ResizeResult {
                width,
                height,
                status,
            },
        })
    }
}

impl Drop for RfbReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Drop for RfbWriter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RfbWriter {
    pub fn audio_available(&self) -> watch::Receiver<bool> {
        self.audio_available.clone()
    }

    /// Enable capture only after the server acknowledges QEMU's audio extension.
    pub async fn set_audio(&mut self, enabled: bool) -> Result<()> {
        ensure!(
            *self.audio_available.borrow(),
            "RFB server has not confirmed audio support"
        );
        let (bytes, len) = audio::control(enabled);
        self.send(WriteMessage::inline(&bytes[..usize::from(len)]))
            .await
    }

    /// Ask QEMU to look for display changes at its base rate again.
    ///
    /// QEMU resets its VNC refresh timer for every client message except
    /// update requests and the two setup messages. Repeating the audio format
    /// is the one such message that changes no guest or session state. It
    /// needs the audio extension, so servers without it are left alone.
    pub async fn hold_refresh(&mut self) -> Result<()> {
        ensure!(
            *self.audio_available.borrow(),
            "RFB server has not confirmed audio support"
        );
        self.send(WriteMessage::inline(&audio::format())).await
    }

    pub fn supports_resize(&self) -> bool {
        self.layout.lock().is_ok_and(|layout| layout.is_some())
    }

    pub async fn request_update(
        &mut self,
        incremental: bool,
        width: u16,
        height: u16,
    ) -> Result<()> {
        dimensions(width, height)?;
        let w = width.to_be_bytes();
        let h = height.to_be_bytes();
        let bytes = [3, u8::from(incremental), 0, 0, 0, 0, w[0], w[1], h[0], h[1]];
        self.send(WriteMessage::inline(&bytes)).await
    }

    /// Clipboard text uses UTF-8 when negotiated, otherwise the base Latin-1 format.
    pub async fn send_input(&mut self, input: &Input) -> Result<()> {
        let message = match input {
            Input::Key { keysym, down, .. } => {
                let k = keysym.to_be_bytes();
                WriteMessage::inline(&[4, u8::from(*down), 0, 0, k[0], k[1], k[2], k[3]])
            }
            Input::Pointer { x, y, buttons, .. } => {
                let xb = x.to_be_bytes();
                let yb = y.to_be_bytes();
                WriteMessage::inline(&[5, *buttons, xb[0], xb[1], yb[0], yb[1]])
            }
            Input::Clipboard { text, .. } => {
                ensure!(
                    text.len() <= MAX_CLIPBOARD,
                    "clipboard exceeds {MAX_CLIPBOARD} bytes"
                );
                return self.send(WriteMessage::ClipboardInput(text.clone())).await;
            }
            Input::Resize { width, height, .. } => {
                dimensions(*width, *height)?;
                let screen = self
                    .layout
                    .lock()
                    .map_err(|_| anyhow::anyhow!("RFB layout lock poisoned"))?
                    .context("RFB server has not confirmed desktop resize support")?;
                let mut bytes = [0; 24];
                bytes[0] = 251;
                bytes[2..4].copy_from_slice(&width.to_be_bytes());
                bytes[4..6].copy_from_slice(&height.to_be_bytes());
                bytes[6] = 1;
                bytes[8..12].copy_from_slice(&screen.id.to_be_bytes());
                bytes[16..18].copy_from_slice(&width.to_be_bytes());
                bytes[18..20].copy_from_slice(&height.to_be_bytes());
                bytes[20..24].copy_from_slice(&screen.flags.to_be_bytes());
                WriteMessage::Inline { len: 24, bytes }
            }
        };
        self.send(message).await
    }

    async fn send(&mut self, message: WriteMessage) -> Result<()> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(WriteCommand { message, completed })
            .await
            .context("RFB writer closed")?;
        result.await.context("RFB writer closed")?.map(|_| ())
    }
}

/// Write a whole message, failing when the peer accepts nothing for
/// [`WRITE_STALL_TIMEOUT`]. A slow peer that keeps reading is never cut off.
async fn write_progressing<W: AsyncWrite + Unpin>(write: &mut W, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        let written = timeout(WRITE_STALL_TIMEOUT, write.write(bytes))
            .await
            .context("RFB server stopped reading")?
            .context("write RFB message")?;
        ensure!(written > 0, "RFB server closed the connection");
        bytes = &bytes[written..];
    }
    Ok(())
}

async fn write_messages<W: AsyncWrite + Unpin>(
    mut write: W,
    mut clipboard: clipboard::Clipboard,
    mut commands: mpsc::Receiver<WriteCommand>,
    failed_writes: oneshot::Sender<anyhow::Error>,
) {
    while let Some(command) = commands.recv().await {
        let mut failure = None;
        let result = async {
            // Clipboard negotiation, ownership and response selection share the
            // same actor as input writes, so crossed updates cannot reorder a
            // cached provide after a newer notification.
            let owned;
            let (bytes, text): (Option<&[u8]>, Option<String>) = match command.message {
                WriteMessage::Inline { len, ref bytes } => (Some(&bytes[..usize::from(len)]), None),
                WriteMessage::ClipboardInput(text) => {
                    owned = clipboard.input(&text)?;
                    (Some(owned.as_slice()), None)
                }
                WriteMessage::ClipboardRemote {
                    extended: true,
                    bytes,
                } => {
                    let (reply, text) = clipboard.receive(&bytes)?;
                    owned = reply.unwrap_or_default();
                    let slice = if owned.is_empty() {
                        None
                    } else {
                        Some(owned.as_slice())
                    };
                    (slice, text)
                }
                WriteMessage::ClipboardRemote {
                    extended: false,
                    bytes,
                } => {
                    clipboard.remote_copy();
                    (None, clipboard::latin1_text(&bytes))
                }
            };
            if let Some(bytes) = bytes
                && let Err(error) = write_progressing(&mut write, bytes).await
            {
                // The caller and the reader each get the reason.
                failure = Some(format!("{error:#}"));
                return Err(error);
            }
            Ok(text)
        }
        .await;
        let _ = command.completed.send(result);
        if let Some(failure) = failure {
            let _ = failed_writes.send(anyhow::anyhow!(failure));
            break;
        }
    }
}

fn bounds(frame: &Frame, x: u16, y: u16, width: u16, height: u16) -> Result<()> {
    ensure!(
        u32::from(x) + u32::from(width) <= u32::from(frame.width)
            && u32::from(y) + u32::from(height) <= u32::from(frame.height),
        "RFB rectangle outside framebuffer"
    );
    Ok(())
}

fn dimensions(width: u16, height: u16) -> Result<()> {
    let pixels = usize::from(width) * usize::from(height);
    ensure!(
        pixels > 0 && pixels <= MAX_PIXELS,
        "invalid or excessive RFB framebuffer dimensions"
    );
    Ok(())
}

/// Read and discard `length` bytes without holding them.
async fn skip<R: AsyncRead + Unpin>(read: &mut R, length: usize) -> Result<()> {
    let skipped = tokio::io::copy(&mut read.take(length as u64), &mut tokio::io::sink()).await?;
    if skipped != length as u64 {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    Ok(())
}

/// What one framebuffer update has done so far, and what it may still do.
struct Update {
    /// Whether a pixel or the size changed.
    changed: bool,
    resized: bool,
    /// Pixels the remaining rectangles may write or move.
    budget: usize,
    /// Work done since decoding last returned to the runtime.
    work: usize,
    /// Tiles dirtied by this update.
    damage: Damage,
}

impl Update {
    fn new(frame: &Frame) -> Self {
        Self {
            changed: false,
            resized: false,
            budget: Self::budget(frame),
            work: 0,
            damage: Damage::empty(frame.width, frame.height),
        }
    }

    fn budget(frame: &Frame) -> usize {
        usize::from(frame.width) * usize::from(frame.height) * UPDATE_AREA_BUDGET
    }

    /// Account for a rectangle that writes or moves `width`x`height` pixels.
    fn draws(&mut self, x: u16, y: u16, width: u16, height: u16) -> Result<()> {
        let pixels = usize::from(width) * usize::from(height);
        self.budget = self
            .budget
            .checked_sub(pixels)
            .context("RFB update exceeds its pixel budget")?;
        if pixels > 0 {
            self.changed = true;
            self.damage.mark_rect(x, y, width, height);
        }
        self.work += pixels;
        Ok(())
    }

    /// Replace the frame when the size differs. Each replacement allocates a
    /// whole frame, so an update gets one.
    fn resizes(
        &mut self,
        frame: &mut Arc<PooledFrame>,
        width: u16,
        height: u16,
        pool: &Arc<Mutex<FramePool>>,
    ) -> Result<()> {
        if (width, height) == (frame.width, frame.height) {
            return Ok(());
        }
        ensure!(
            !self.resized,
            "RFB update changes the desktop size more than once"
        );
        // The rectangles that follow draw the new desktop, within what it
        // allows less what the update has done already: a change of size
        // does not give an update a second budget.
        let spent = Self::budget(frame) - self.budget;
        let resized_frame = Frame::new(width, height).context("invalid RFB resize")?;
        *frame = Arc::new(PooledFrame {
            frame: resized_frame,
            damage: Damage::full(width, height),
            generation: 0,
            pool: Arc::downgrade(pool),
        });
        self.resized = true;
        self.changed = true;
        self.damage = Damage::full(width, height);
        self.budget = Self::budget(frame).saturating_sub(spent);
        Ok(())
    }
}

fn compute_sync_damage(
    buf_generation: u64,
    generation: u64,
    width: u16,
    height: u16,
    history: &[(u64, Damage); HISTORY_LEN],
) -> Damage {
    if buf_generation == 0
        || buf_generation >= generation
        || generation - buf_generation > HISTORY_LEN as u64
    {
        return Damage::full(width, height);
    }
    let mut sync = Damage::empty(width, height);
    for step in (buf_generation + 1)..=generation {
        let (recorded, recorded_damage) = history[(step as usize) % HISTORY_LEN];
        if recorded == step && recorded_damage.dimensions() == (width, height) {
            sync.union_with(&recorded_damage);
        } else {
            return Damage::full(width, height);
        }
    }
    sync
}

/// Ensures `frame` has exclusive ownership for in-place RFB decoding.
///
/// Fast path: when downstream consumers have already released the previous
/// snapshot (`Arc::strong_count == 1`), decoding mutates the buffer in place
/// with zero copies. Slow path: detaches into a pooled buffer synced by tile
/// damage so live consumers keep an immutable snapshot.
fn make_frame_mut<'a>(
    frame: &'a mut Arc<PooledFrame>,
    pool: &Arc<Mutex<FramePool>>,
    history: &[(u64, Damage); HISTORY_LEN],
    generation: u64,
) -> &'a mut Frame {
    if Arc::strong_count(frame) > 1 {
        let width = frame.width;
        let height = frame.height;
        let byte_len = frame.pixels.len();
        let recycled = pool.lock().ok().and_then(|mut guard| guard.take(byte_len));
        let pixels = match recycled {
            Some(mut buf) => {
                let sync = compute_sync_damage(buf.generation, generation, width, height, history);
                copy_damaged_pixels(&mut buf.pixels, &frame.pixels, width, &sync);
                buf.pixels
            }
            None => frame.pixels.clone(),
        };
        *frame = Arc::new(PooledFrame {
            frame: Frame {
                width,
                height,
                pixels,
            },
            damage: Damage::empty(width, height),
            generation,
            pool: Arc::downgrade(pool),
        });
    }
    &mut Arc::get_mut(frame).expect("unique RFB frame owner").frame
}

#[inline]
async fn read_rect_header<R: AsyncBufRead + Unpin>(read: &mut R) -> Result<[u8; 12]> {
    let buf = read.fill_buf().await?;
    if buf.len() >= 12 {
        let header: [u8; 12] = buf[..12].try_into().unwrap();
        read.consume(12);
        return Ok(header);
    }
    let mut header = [0u8; 12];
    read.read_exact(&mut header).await?;
    Ok(header)
}

/// Reads `dst.len()` wire bytes (a multiple of 4) and writes opaque RGBA into
/// `dst` in one fused pass straight out of the `AsyncBufRead` buffer.
async fn read_pixels_converted<R: AsyncBufRead + Unpin>(
    read: &mut R,
    dst: &mut [u8],
    layout: Layout,
) -> Result<()> {
    debug_assert!(dst.len().is_multiple_of(4));
    let mut offset = 0usize;
    let mut carry = [0u8; 4];
    let mut carry_len = 0usize;
    while offset < dst.len() {
        let buf = read.fill_buf().await?;
        if buf.is_empty() {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        if carry_len > 0 {
            let need = 4 - carry_len;
            let take = buf.len().min(need);
            carry[carry_len..carry_len + take].copy_from_slice(&buf[..take]);
            read.consume(take);
            carry_len += take;
            if carry_len == 4 {
                layout.to_rgba(&mut carry);
                dst[offset..offset + 4].copy_from_slice(&carry);
                offset += 4;
                carry_len = 0;
            }
            continue;
        }
        let rem = dst.len() - offset;
        let avail = buf.len().min(rem);
        let aligned = avail & !3;
        if aligned > 0 {
            layout.copy_to_rgba(&mut dst[offset..offset + aligned], &buf[..aligned]);
            read.consume(aligned);
            offset += aligned;
        } else {
            carry[..avail].copy_from_slice(&buf[..avail]);
            carry_len = avail;
            read.consume(avail);
        }
    }
    Ok(())
}

fn copy_damaged_pixels(dst: &mut [u8], src: &[u8], width: u16, damage: &Damage) {
    if damage.is_full() {
        dst.copy_from_slice(src);
        return;
    }
    let stride = usize::from(width) * 4;
    damage.for_each_span(|x, y, w, h| {
        let row_bytes = w * 4;
        if row_bytes == stride {
            let start = y * stride;
            let len = h * stride;
            dst[start..start + len].copy_from_slice(&src[start..start + len]);
        } else {
            for row in y..y + h {
                let start = row * stride + x * 4;
                dst[start..start + row_bytes].copy_from_slice(&src[start..start + row_bytes]);
            }
        }
    });
}

impl Update {
    /// Called before each rectangle: lets other tasks run once the rectangles
    /// so far have done [`YIELD_WORK`]. The bytes of an update may all be
    /// buffered already, so no read would wait and nothing else would yield.
    async fn share_worker(&mut self) {
        self.work += RECTANGLE_WORK;
        if self.work >= YIELD_WORK {
            self.work = 0;
            tokio::task::yield_now().await;
        }
    }
}

/// The framebuffer a connection decodes into and the byte order it arrives in.
struct Surface {
    frame: Frame,
    pixels: Layout,
}

async fn decode<R: AsyncBufRead + Unpin>(
    mut read: R,
    surface: Surface,
    layout: Arc<Mutex<Option<Screen>>>,
    commands: &mpsc::Sender<WriteCommand>,
    events: &mpsc::Sender<Result<PooledEvent>>,
    audio: &audio::Sink,
) -> Result<()> {
    let Surface {
        frame: initial_frame,
        pixels: pixel_layout,
    } = surface;
    let pool = Arc::new(Mutex::new(FramePool::default()));
    let mut generation = 0u64;
    let mut history: [(u64, Damage); HISTORY_LEN] =
        [(0, Damage::empty(initial_frame.width, initial_frame.height)); HISTORY_LEN];
    let mut frame = Arc::new(PooledFrame {
        damage: Damage::full(initial_frame.width, initial_frame.height),
        frame: initial_frame,
        generation: 0,
        pool: Arc::downgrade(&pool),
    });
    loop {
        let message = read.read_u8().await.context("read RFB message")?;
        match message {
            0 => {
                read.read_u8().await?;
                let count = read.read_u16().await?;
                let mut update = Update::new(&frame);
                for rectangle_index in 0..count {
                    update.share_worker().await;
                    let header = read_rect_header(&mut read).await?;
                    let x = u16::from_be_bytes([header[0], header[1]]);
                    let y = u16::from_be_bytes([header[2], header[3]]);
                    let width = u16::from_be_bytes([header[4], header[5]]);
                    let height = u16::from_be_bytes([header[6], header[7]]);
                    let encoding =
                        i32::from_be_bytes([header[8], header[9], header[10], header[11]]);
                    match encoding {
                        audio::ENCODING => {
                            audio.available.send_replace(true);
                        }
                        0 => {
                            bounds(&frame, x, y, width, height)?;
                            update.draws(x, y, width, height)?;
                            if width == 0 || height == 0 {
                                continue;
                            }
                            let frame_mut = make_frame_mut(&mut frame, &pool, &history, generation);
                            let stride = usize::from(frame_mut.width) * 4;
                            let row_bytes = usize::from(width) * 4;
                            if row_bytes == stride {
                                let start = usize::from(y) * stride;
                                let rows = &mut frame_mut.pixels
                                    [start..start + usize::from(height) * stride];
                                if rows.len() >= DIRECT_READ_THRESHOLD {
                                    for chunk in rows.chunks_mut(RAW_READ_BYTES) {
                                        read.read_exact(chunk).await?;
                                        pixel_layout.to_rgba(chunk);
                                    }
                                } else {
                                    read_pixels_converted(&mut read, rows, pixel_layout).await?;
                                }
                            } else {
                                for row in usize::from(y)..usize::from(y) + usize::from(height) {
                                    let start = row * stride + usize::from(x) * 4;
                                    let pixels = &mut frame_mut.pixels[start..start + row_bytes];
                                    read_pixels_converted(&mut read, pixels, pixel_layout).await?;
                                }
                            }
                        }
                        1 => {
                            bounds(&frame, x, y, width, height)?;
                            let source_x = read.read_u16().await?;
                            let source_y = read.read_u16().await?;
                            bounds(&frame, source_x, source_y, width, height)?;
                            update.draws(x, y, width, height)?;
                            if width == 0 || height == 0 {
                                continue;
                            }
                            let frame_mut = make_frame_mut(&mut frame, &pool, &history, generation);
                            let stride = usize::from(frame_mut.width) * 4;
                            let row_bytes = usize::from(width) * 4;
                            // Bottom-up copying handles vertical overlap; copy_within handles horizontal overlap.
                            for index in 0..usize::from(height) {
                                let row = if y > source_y {
                                    usize::from(height) - 1 - index
                                } else {
                                    index
                                };
                                let source = (usize::from(source_y) + row) * stride
                                    + usize::from(source_x) * 4;
                                let destination =
                                    (usize::from(y) + row) * stride + usize::from(x) * 4;
                                frame_mut
                                    .pixels
                                    .copy_within(source..source + row_bytes, destination);
                            }
                        }
                        -239 => {
                            let count = usize::from(width) * usize::from(height);
                            ensure!(
                                width <= MAX_WIRE_CURSOR_SIDE && height <= MAX_WIRE_CURSOR_SIDE,
                                "RFB cursor is too large"
                            );
                            ensure!(
                                (width == 0 && height == 0)
                                    || (width > 0 && height > 0 && x < width && y < height),
                                "invalid RFB cursor hotspot or dimensions"
                            );
                            let mask_stride = usize::from(width).div_ceil(8);
                            if width > MAX_CURSOR_SIDE || height > MAX_CURSOR_SIDE {
                                // The bytes still have to leave the wire, but
                                // nothing downstream would use the shape.
                                let unused = count * 4 + mask_stride * usize::from(height);
                                skip(&mut read, unused).await?;
                                continue;
                            }
                            let mut pixels = vec![0; count * 4];
                            read_pixels_converted(&mut read, &mut pixels, pixel_layout).await?;
                            let mut mask = vec![0; mask_stride * usize::from(height)];
                            read.read_exact(&mut mask).await?;
                            for row in 0..usize::from(height) {
                                for column in 0..usize::from(width) {
                                    let opaque = mask[row * mask_stride + column / 8]
                                        & (0x80 >> (column % 8))
                                        != 0;
                                    pixels[(row * usize::from(width) + column) * 4 + 3] =
                                        if opaque { 255 } else { 0 };
                                }
                            }
                            events
                                .send(Ok(PooledEvent::Cursor(Cursor {
                                    width,
                                    height,
                                    hotspot_x: x,
                                    hotspot_y: y,
                                    pixels,
                                })))
                                .await
                                .context("RFB receiver dropped")?;
                        }
                        -223 => {
                            ensure!(
                                rectangle_index + 1 == count,
                                "RFB DesktopSize must be the final rectangle"
                            );
                            ensure!(x == 0 && y == 0, "invalid RFB DesktopSize origin");
                            dimensions(width, height)?;
                            update.resizes(&mut frame, width, height, &pool)?;
                        }
                        -308 => {
                            let count = read.read_u8().await?;
                            let mut padding = [0; 3];
                            read.read_exact(&mut padding).await?;
                            let mut screens = Vec::with_capacity(usize::from(count));
                            for _ in 0..count {
                                let id = read.read_u32().await?;
                                let screen_x = read.read_u16().await?;
                                let screen_y = read.read_u16().await?;
                                let screen_width = read.read_u16().await?;
                                let screen_height = read.read_u16().await?;
                                let flags = read.read_u32().await?;
                                screens.push((
                                    Screen { id, flags },
                                    screen_x,
                                    screen_y,
                                    screen_width,
                                    screen_height,
                                ));
                            }
                            // A rejected request has undefined dimensions and screen fields.
                            if x != 1 || y == 0 {
                                dimensions(width, height)?;
                                ensure!(!screens.is_empty(), "RFB desktop layout has no screens");
                                for (index, (screen, sx, sy, sw, sh)) in screens.iter().enumerate()
                                {
                                    ensure!(
                                        *sw > 0
                                            && *sh > 0
                                            && u32::from(*sx) + u32::from(*sw) <= u32::from(width)
                                            && u32::from(*sy) + u32::from(*sh) <= u32::from(height),
                                        "RFB screen outside desktop layout"
                                    );
                                    ensure!(
                                        !screens[..index]
                                            .iter()
                                            .any(|previous| previous.0.id == screen.id),
                                        "duplicate RFB screen ID"
                                    );
                                }
                                *layout
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("RFB layout lock poisoned"))? =
                                    Some(screens[0].0);
                                update.resizes(&mut frame, width, height, &pool)?;
                            }
                            if x == 1 {
                                events
                                    .send(Ok(PooledEvent::ResizeResult {
                                        width,
                                        height,
                                        status: y,
                                    }))
                                    .await
                                    .context("RFB receiver dropped")?;
                            }
                        }
                        other => bail!("unsupported RFB encoding {other}"),
                    }
                }
                let event = if update.changed {
                    generation += 1;
                    let damage = update.damage;
                    history[(generation as usize) % HISTORY_LEN] = (generation, damage);
                    // `make_frame_mut` / `resizes` ensured unique ownership whenever
                    // `update.changed` became true.
                    let inner = Arc::get_mut(&mut frame).expect("mutated RFB frame is unique");
                    inner.damage = damage;
                    inner.generation = generation;
                    PooledEvent::Frame(Arc::clone(&frame))
                } else {
                    PooledEvent::Unchanged
                };
                events
                    .send(Ok(event))
                    .await
                    .context("RFB receiver dropped")?;
            }
            2 => events
                .send(Ok(PooledEvent::Bell))
                .await
                .context("RFB receiver dropped")?,
            3 => {
                let mut padding = [0; 3];
                read.read_exact(&mut padding).await?;
                let length = read.read_i32().await?;
                let size = length.unsigned_abs() as usize;
                let limit = if length < 0 {
                    clipboard::MAX_MESSAGE
                } else {
                    MAX_CLIPBOARD
                };
                ensure!(size <= limit, "RFB clipboard exceeds {limit} bytes");
                let mut bytes = vec![0; size];
                read.read_exact(&mut bytes).await?;
                let (completed, result) = oneshot::channel();
                commands
                    .send(WriteCommand {
                        message: WriteMessage::ClipboardRemote {
                            extended: length < 0,
                            bytes,
                        },
                        completed,
                    })
                    .await
                    .context("RFB writer closed")?;
                let text = result.await.context("RFB writer closed")??;
                if let Some(text) = text {
                    events
                        .send(Ok(PooledEvent::Clipboard(text)))
                        .await
                        .context("RFB receiver dropped")?;
                }
            }
            255 => {
                ensure!(*audio.available.borrow(), "unnegotiated QEMU audio message");
                let event = audio::read(&mut read).await?;
                let _ = audio.events.send(event);
            }
            other => bail!("unsupported RFB server message {other}"),
        }
    }
}

#[cfg(test)]
mod fuzz;
#[cfg(test)]
mod tests;
