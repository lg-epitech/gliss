//! Bounded RFB capture and input over TCP or an owned established Unix stream.
//!
//! Wire formats follow [RFC 6143](https://www.rfc-editor.org/rfc/rfc6143.html).
//! Screen resizing follows the [RFB extension specification](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst).

mod teardown;
pub mod trace;
#[cfg(test)]
mod trace_tests;
pub use teardown::{
    DRAIN_SCRATCH_BYTES, DrainReport, DrainTermination, MAX_DRAIN_BYTES, MAX_DRAIN_DURATION,
    ReaderDrainControl,
};

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use des::{
    Des,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use gliss_core::{Frame, Input, MAX_CLIPBOARD, MAX_PIXELS};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];
const ENCODINGS: [i32; 5] = [0, 1, -239, -223, -308];
const MAX_TEXT: usize = 64 * 1024;
const MAX_CURSOR_PIXELS: usize = 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
    Frame(Frame),
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
    events: mpsc::Receiver<Result<Event>>,
    task: JoinHandle<()>,
    drain: ReaderDrainControl,
    trace: trace::Sink,
    traced_updates_read: u64,
}

/// Messages are serialized by an owned task, even if the caller cancels its wait.
pub struct RfbWriter {
    commands: mpsc::Sender<WriteCommand>,
    task: JoinHandle<()>,
    layout: Arc<Mutex<Option<Screen>>>,
    trace: trace::Sink,
    traced_commands: u64,
}

struct WriteCommand {
    bytes: Vec<u8>,
    completed: oneshot::Sender<Result<()>>,
    ordinal: u64,
    input_id: Option<u32>,
    capture_request: bool,
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
    connect_traced(address, password, None).await
}

pub async fn connect_traced(
    address: &str,
    password: Option<&str>,
    trace: trace::Sink,
) -> Result<(RfbReader, RfbWriter, ServerInfo)> {
    timeout(HANDSHAKE_TIMEOUT, connect_inner(address, password, trace))
        .await
        .context("RFB connection/handshake timed out")?
}

/// Negotiate RFB on an already established, owned Unix stream.
///
/// This consumes the stream, including on handshake failure or cancellation;
/// it never resolves a socket path, opens another connection or bypasses RFB
/// security. The caller owns FD adoption, target authorization and identity
/// attestation. No expected VM generation is validated here; the authoritative
/// lease/compare-and-attach boundary remains outside this transport seam, and
/// the real Distribox gateway remains unavailable. The caller must
/// request the initial non-incremental update and release held inputs before
/// dropping the returned handles, as for [`connect`].
#[cfg(unix)]
pub async fn connect_unix(
    stream: UnixStream,
    password: Option<&str>,
) -> Result<(RfbReader, RfbWriter, ServerInfo)> {
    connect_unix_traced(stream, password, None).await
}

#[cfg(unix)]
pub async fn connect_unix_traced(
    stream: UnixStream,
    password: Option<&str>,
    trace: trace::Sink,
) -> Result<(RfbReader, RfbWriter, ServerInfo)> {
    timeout(
        HANDSHAKE_TIMEOUT,
        connect_unix_inner(stream, password, trace),
    )
    .await
    .context("RFB established Unix handshake timed out")?
}

async fn connect_inner(
    address: &str,
    password: Option<&str>,
    trace: trace::Sink,
) -> Result<(RfbReader, RfbWriter, ServerInfo)> {
    let mut stream = TcpStream::connect(address)
        .await
        .context("connect to RFB server")?;
    stream.set_nodelay(true)?;
    let (frame, info) = handshake(&mut stream, password).await?;
    // Preserve TCP's existing owned split: no generic split mutex in this path.
    let (read, write) = stream.into_split();
    start_tasks_traced(read, write, frame, info, trace)
}

#[cfg(unix)]
async fn connect_unix_inner(
    mut stream: UnixStream,
    password: Option<&str>,
    trace: trace::Sink,
) -> Result<(RfbReader, RfbWriter, ServerInfo)> {
    let (frame, info) = handshake(&mut stream, password).await?;
    let (read, write) = stream.into_split();
    start_tasks_traced(read, write, frame, info, trace)
}

async fn handshake<S>(stream: &mut S, password: Option<&str>) -> Result<(Frame, ServerInfo)>
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
    let mut format = vec![0, 0, 0, 0];
    format.extend_from_slice(&PIXEL_FORMAT);
    stream.write_all(&format).await?;
    let mut encodings = vec![2, 0];
    encodings.extend_from_slice(&(ENCODINGS.len() as u16).to_be_bytes());
    for encoding in ENCODINGS {
        encodings.extend_from_slice(&encoding.to_be_bytes());
    }
    stream.write_all(&encodings).await?;
    Ok((frame, info))
}

#[cfg(test)]
fn start_tasks<R, W>(
    read: R,
    write: W,
    frame: Frame,
    info: ServerInfo,
) -> (RfbReader, RfbWriter, ServerInfo)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    start_tasks_traced(read, write, frame, info, None)
        .expect("disabled trace does not reject actor admission")
}

fn start_tasks_traced<R, W>(
    read: R,
    write: W,
    frame: Frame,
    info: ServerInfo,
    trace: trace::Sink,
) -> Result<(RfbReader, RfbWriter, ServerInfo)>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let layout = Arc::new(Mutex::new(None));
    let (event_tx, events) = mpsc::channel(2);
    let read_layout = layout.clone();
    let (drain_tx, mut drain_rx) = mpsc::channel::<teardown::DrainRequest>(1);
    let read_trace = trace.clone();
    let read_actor = trace::Actor::new(trace.clone())?;
    let write_actor = trace::Actor::new(trace.clone())?;
    let read_task = tokio::spawn(async move {
        let _trace_actor = read_actor;
        let mut read = read;
        enum Outcome {
            Decoded(Result<()>),
            Drain(teardown::DrainRequest),
        }
        let outcome = {
            // Cancellation drops the decode future but retains its borrowed
            // stream for the terminal drain of that exact same owned half.
            let decode = decode(&mut read, frame, read_layout, &event_tx, &read_trace);
            tokio::pin!(decode);
            tokio::select! {
                biased;
                Some(request) = drain_rx.recv() => Outcome::Drain(request),
                result = &mut decode => Outcome::Decoded(result),
            }
        };
        match outcome {
            Outcome::Decoded(result) => {
                // Preserve ordinary close-before-error-send behavior.
                drop(read);
                if let Err(error) = result {
                    let _ = event_tx.send(Err(error)).await;
                }
            }
            Outcome::Drain(request) => {
                let report = teardown::discard(&mut read, request.deadline).await;
                drop(read);
                let _ = request.completed.send(report);
            }
        }
    });
    let drain = ReaderDrainControl::new(drain_tx, read_task.abort_handle());
    let (commands, command_rx) = mpsc::channel(8);
    let write_trace = trace.clone();
    let write_task = tokio::spawn(async move {
        let _trace_actor = write_actor;
        write_messages(write, command_rx, write_trace).await;
    });
    Ok((
        RfbReader {
            events,
            task: read_task,
            drain,
            trace: trace.clone(),
            traced_updates_read: 0,
        },
        RfbWriter {
            commands,
            task: write_task,
            layout,
            trace,
            traced_commands: 0,
        },
        info,
    ))
}

async fn read_text<R: AsyncRead + Unpin>(stream: &mut R, max: usize) -> Result<String> {
    let length = stream.read_u32().await? as usize;
    ensure!(length <= max, "RFB text exceeds {max} bytes");
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
                read_text(stream, MAX_TEXT).await?
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
            read_text(stream, MAX_TEXT).await?
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
                    read_text(stream, MAX_TEXT).await?
                );
            }
            bail!("RFB authentication failed (status {status})");
        }
    }
    Ok(())
}

impl RfbReader {
    pub fn teardown_control(&self) -> ReaderDrainControl {
        self.drain.clone()
    }

    pub async fn read_event(&mut self) -> Result<Event> {
        let value = self.events.recv().await.context("RFB reader closed")?;
        if self.trace.is_some() && matches!(&value, Ok(Event::Frame(_))) {
            self.traced_updates_read = self
                .traced_updates_read
                .checked_add(1)
                .context("trace update ordinal exhausted")?;
            trace::emit(
                &self.trace,
                trace::Kind::EventDequeued,
                self.traced_updates_read,
                None,
                1,
                trace::Outcome::Observed,
            );
        }
        value
    }

    pub fn traced_update_ordinal(&self) -> Option<u64> {
        self.trace.as_ref().map(|_| self.traced_updates_read)
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
        let mut bytes = vec![3, u8::from(incremental), 0, 0, 0, 0];
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        self.send(bytes, None, true).await
    }

    /// Base RFB clipboard text is Latin-1; unsupported characters return an error.
    pub async fn send_input(&mut self, input: &Input) -> Result<()> {
        let bytes = match input {
            Input::Key { keysym, down, .. } => {
                let mut bytes = vec![4, u8::from(*down), 0, 0];
                bytes.extend_from_slice(&keysym.to_be_bytes());
                bytes
            }
            Input::Pointer { x, y, buttons, .. } => {
                let mut bytes = vec![5, *buttons];
                bytes.extend_from_slice(&x.to_be_bytes());
                bytes.extend_from_slice(&y.to_be_bytes());
                bytes
            }
            Input::Clipboard { text, .. } => {
                ensure!(
                    text.len() <= MAX_CLIPBOARD,
                    "clipboard exceeds {MAX_CLIPBOARD} bytes"
                );
                let mut encoded = Vec::with_capacity(text.len());
                for character in text.chars() {
                    ensure!(
                        u32::from(character) <= 255,
                        "base RFB clipboard supports only Latin-1 text"
                    );
                    encoded.push(character as u8);
                }
                let mut bytes = vec![6, 0, 0, 0];
                bytes.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
                bytes.extend_from_slice(&encoded);
                bytes
            }
            Input::Resize { width, height, .. } => {
                dimensions(*width, *height)?;
                let screen = self
                    .layout
                    .lock()
                    .map_err(|_| anyhow::anyhow!("RFB layout lock poisoned"))?
                    .context("RFB server has not confirmed desktop resize support")?;
                let mut bytes = vec![251, 0];
                bytes.extend_from_slice(&width.to_be_bytes());
                bytes.extend_from_slice(&height.to_be_bytes());
                bytes.extend_from_slice(&[1, 0]);
                bytes.extend_from_slice(&screen.id.to_be_bytes());
                bytes.extend_from_slice(&[0; 4]);
                bytes.extend_from_slice(&width.to_be_bytes());
                bytes.extend_from_slice(&height.to_be_bytes());
                bytes.extend_from_slice(&screen.flags.to_be_bytes());
                bytes
            }
        };
        self.send(bytes, Some(input.id()), false).await
    }

    async fn send(
        &mut self,
        bytes: Vec<u8>,
        input_id: Option<u32>,
        capture_request: bool,
    ) -> Result<()> {
        let (completed, result) = oneshot::channel();
        let ordinal = if self.trace.is_some() {
            self.traced_commands = self
                .traced_commands
                .checked_add(1)
                .context("trace command ordinal exhausted")?;
            self.traced_commands
        } else {
            0
        };
        if let Some(trace) = &self.trace {
            trace.record(trace::Record {
                kind: trace::Kind::CommandQueued,
                ordinal,
                input_id,
                count: 0,
                outcome: trace::Outcome::Observed,
                capture_request,
            });
        }
        let terminal = trace::Terminal::new(
            &self.trace,
            trace::Record {
                kind: trace::Kind::CommandQueued,
                ordinal,
                input_id,
                count: 0,
                outcome: trace::Outcome::Canceled,
                capture_request,
            },
        );
        let sent = self
            .commands
            .send(WriteCommand {
                bytes,
                completed,
                ordinal,
                input_id,
                capture_request,
            })
            .await;
        terminal.finish(if sent.is_ok() {
            trace::Outcome::Accepted
        } else {
            trace::Outcome::Rejected
        });
        sent.context("RFB writer closed")?;
        result.await.context("RFB writer closed")?
    }
}

async fn write_messages<W: AsyncWrite + Unpin>(
    mut write: W,
    mut commands: mpsc::Receiver<WriteCommand>,
    trace: trace::Sink,
) {
    while let Some(command) = commands.recv().await {
        if let Some(trace) = &trace {
            trace.record(trace::Record {
                kind: trace::Kind::WriteStarted,
                ordinal: command.ordinal,
                input_id: command.input_id,
                count: 0,
                outcome: trace::Outcome::Observed,
                capture_request: command.capture_request,
            });
        }
        let terminal = trace::Terminal::new(
            &trace,
            trace::Record {
                kind: trace::Kind::WriteCompleted,
                ordinal: command.ordinal,
                input_id: command.input_id,
                count: 0,
                outcome: trace::Outcome::Canceled,
                capture_request: command.capture_request,
            },
        );
        let result = write
            .write_all(&command.bytes)
            .await
            .context("write RFB message");
        let failed = result.is_err();
        terminal.finish(if failed {
            trace::Outcome::Error
        } else {
            trace::Outcome::Accepted
        });
        let _ = command.completed.send(result);
        if failed {
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

async fn decode<R: AsyncRead + Unpin>(
    mut read: R,
    mut frame: Frame,
    layout: Arc<Mutex<Option<Screen>>>,
    events: &mpsc::Sender<Result<Event>>,
    trace: &trace::Sink,
) -> Result<()> {
    let mut update_ordinal = 0u64;
    loop {
        let message = read.read_u8().await.context("read RFB message")?;
        match message {
            0 => {
                read.read_u8().await?;
                let count = read.read_u16().await?;
                if trace.is_some() {
                    update_ordinal = update_ordinal
                        .checked_add(1)
                        .context("trace update ordinal exhausted")?;
                }
                trace::emit(
                    trace,
                    trace::Kind::UpdateHeader,
                    update_ordinal,
                    None,
                    u32::from(count),
                    trace::Outcome::Ambiguous,
                );
                let update_terminal = trace::Terminal::new(
                    trace,
                    trace::Record {
                        kind: trace::Kind::UpdateDecoded,
                        ordinal: update_ordinal,
                        input_id: None,
                        count: u32::from(count),
                        outcome: trace::Outcome::Canceled,
                        capture_request: false,
                    },
                );
                let decoded = async {
                    let mut changed = false;
                    for rectangle_index in 0..count {
                        let x = read.read_u16().await?;
                        let y = read.read_u16().await?;
                        let width = read.read_u16().await?;
                        let height = read.read_u16().await?;
                        let encoding = read.read_i32().await?;
                        match encoding {
                            0 => {
                                changed |= width > 0 && height > 0;
                                bounds(&frame, x, y, width, height)?;
                                let stride = usize::from(frame.width) * 4;
                                let row_bytes = usize::from(width) * 4;
                                for row in usize::from(y)..usize::from(y) + usize::from(height) {
                                    let start = row * stride + usize::from(x) * 4;
                                    let pixels = &mut frame.pixels[start..start + row_bytes];
                                    read.read_exact(pixels).await?;
                                    for pixel in pixels.as_chunks_mut::<4>().0 {
                                        pixel[3] = 255;
                                    }
                                }
                            }
                            1 => {
                                changed |= width > 0 && height > 0;
                                bounds(&frame, x, y, width, height)?;
                                let source_x = read.read_u16().await?;
                                let source_y = read.read_u16().await?;
                                bounds(&frame, source_x, source_y, width, height)?;
                                let stride = usize::from(frame.width) * 4;
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
                                    frame
                                        .pixels
                                        .copy_within(source..source + row_bytes, destination);
                                }
                            }
                            -239 => {
                                let count = usize::from(width) * usize::from(height);
                                ensure!(
                                    width <= 1024 && height <= 1024 && count <= MAX_CURSOR_PIXELS,
                                    "RFB cursor is too large"
                                );
                                ensure!(
                                    (width == 0 && height == 0)
                                        || (width > 0 && height > 0 && x < width && y < height),
                                    "invalid RFB cursor hotspot or dimensions"
                                );
                                let mut pixels = vec![0; count * 4];
                                read.read_exact(&mut pixels).await?;
                                let mask_stride = usize::from(width).div_ceil(8);
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
                                    .send(Ok(Event::Cursor(Cursor {
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
                                if (width, height) != (frame.width, frame.height) {
                                    frame =
                                        Frame::new(width, height).context("invalid RFB resize")?;
                                }
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
                                    ensure!(
                                        !screens.is_empty(),
                                        "RFB desktop layout has no screens"
                                    );
                                    for (index, (screen, sx, sy, sw, sh)) in
                                        screens.iter().enumerate()
                                    {
                                        ensure!(
                                            *sw > 0
                                                && *sh > 0
                                                && u32::from(*sx) + u32::from(*sw)
                                                    <= u32::from(width)
                                                && u32::from(*sy) + u32::from(*sh)
                                                    <= u32::from(height),
                                            "RFB screen outside desktop layout"
                                        );
                                        ensure!(
                                            !screens[..index]
                                                .iter()
                                                .any(|previous| previous.0.id == screen.id),
                                            "duplicate RFB screen ID"
                                        );
                                    }
                                    *layout.lock().map_err(|_| {
                                        anyhow::anyhow!("RFB layout lock poisoned")
                                    })? = Some(screens[0].0);
                                    if (width, height) != (frame.width, frame.height) {
                                        frame = Frame::new(width, height)
                                            .context("invalid RFB resize")?;
                                    }
                                }
                                if x == 1 {
                                    events
                                        .send(Ok(Event::ResizeResult {
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
                    Ok::<bool, anyhow::Error>(changed)
                }
                .await;
                let changed = match decoded {
                    Ok(changed) => changed,
                    Err(error) => {
                        update_terminal.finish(trace::Outcome::Error);
                        return Err(error);
                    }
                };
                update_terminal.finish(if count == 0 {
                    trace::Outcome::Empty
                } else if changed {
                    trace::Outcome::Pixels
                } else {
                    trace::Outcome::Metadata
                });
                trace::emit(
                    trace,
                    trace::Kind::CloneStarted,
                    update_ordinal,
                    None,
                    0,
                    trace::Outcome::Observed,
                );
                let copied = frame.clone();
                trace::emit(
                    trace,
                    trace::Kind::CloneCompleted,
                    update_ordinal,
                    None,
                    0,
                    trace::Outcome::Observed,
                );
                trace::emit(
                    trace,
                    trace::Kind::EventQueued,
                    update_ordinal,
                    None,
                    0,
                    trace::Outcome::Observed,
                );
                let terminal = trace::Terminal::new(
                    trace,
                    trace::Record {
                        kind: trace::Kind::EventQueued,
                        ordinal: update_ordinal,
                        input_id: None,
                        count: 0,
                        outcome: trace::Outcome::Canceled,
                        capture_request: false,
                    },
                );
                let sent = events.send(Ok(Event::Frame(copied))).await;
                terminal.finish(if sent.is_ok() {
                    trace::Outcome::Accepted
                } else {
                    trace::Outcome::Rejected
                });
                sent.context("RFB receiver dropped")?;
            }
            2 => events
                .send(Ok(Event::Bell))
                .await
                .context("RFB receiver dropped")?,
            3 => {
                let mut padding = [0; 3];
                read.read_exact(&mut padding).await?;
                let length = read.read_u32().await? as usize;
                ensure!(
                    length <= MAX_CLIPBOARD,
                    "RFB clipboard exceeds {MAX_CLIPBOARD} bytes"
                );
                let mut bytes = vec![0; length];
                read.read_exact(&mut bytes).await?;
                let text = bytes.into_iter().map(char::from).collect();
                events
                    .send(Ok(Event::Clipboard(text)))
                    .await
                    .context("RFB receiver dropped")?;
            }
            other => bail!("unsupported RFB server message {other}"),
        }
    }
}

#[cfg(test)]
mod teardown_actor_tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context as TaskContext, Poll},
    };
    use tokio::{io::ReadBuf, time::Instant};

    struct TrackedRead {
        inner: tokio::io::DuplexStream,
        bytes: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    impl AsyncRead for TrackedRead {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            output: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let before = output.filled().len();
            let result = Pin::new(&mut self.inner).poll_read(cx, output);
            self.bytes
                .fetch_add(output.filled().len() - before, Ordering::SeqCst);
            result
        }
    }
    impl Drop for TrackedRead {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    type Actor = (
        RfbReader,
        RfbWriter,
        tokio::io::DuplexStream,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    );
    fn actor() -> Actor {
        let (inner, peer) = tokio::io::duplex(4096);
        let bytes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let read = TrackedRead {
            inner,
            bytes: bytes.clone(),
            dropped: dropped.clone(),
        };
        let info = ServerInfo {
            width: 16,
            height: 16,
            name: "test".into(),
            protocol_version: (3, 8),
        };
        let (reader, writer, _) =
            start_tasks(read, tokio::io::sink(), Frame::new(16, 16).unwrap(), info);
        (reader, writer, peer, bytes, dropped)
    }
    async fn consumed(bytes: &AtomicUsize, wanted: usize) {
        timeout(Duration::from_secs(1), async {
            while bytes.load(Ordering::SeqCst) < wanted {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    async fn finished(control: &ReaderDrainControl) {
        timeout(Duration::from_secs(1), async {
            while !control.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn partial_rectangle_cancellation_keeps_the_same_owned_receiver() {
        let (mut reader, _writer, mut peer, bytes, dropped) = actor();
        // One raw 1x1 rectangle, interrupted halfway through its pixel.
        let prefix = [0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 7, 8];
        peer.write_all(&prefix).await.unwrap();
        consumed(&bytes, prefix.len()).await;
        let control = reader.teardown_control();
        let report = control
            .request(Instant::now() + MAX_DRAIN_DURATION)
            .unwrap();
        peer.write_all(&[9, 10, 11, 12, 13]).await.unwrap();
        peer.shutdown().await.unwrap();
        let report = timeout(Duration::from_secs(1), report)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.termination, DrainTermination::PeerEof);
        assert_eq!(report.discarded_bytes, 5);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(reader.read_event().await.is_err());
        finished(&control).await;
    }

    #[tokio::test]
    async fn terminal_command_bypasses_a_full_decoded_event_queue() {
        let (mut reader, _writer, mut peer, bytes, dropped) = actor();
        peer.write_all(&[2, 2, 2]).await.unwrap();
        consumed(&bytes, 3).await;
        let control = reader.teardown_control();
        let report = control
            .request(Instant::now() + MAX_DRAIN_DURATION)
            .unwrap();
        peer.write_all(b"opaque terminal bytes").await.unwrap();
        peer.shutdown().await.unwrap();
        let report = timeout(Duration::from_secs(1), report)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.termination, DrainTermination::PeerEof);
        assert_eq!(report.discarded_bytes, 21);
        assert!(matches!(reader.read_event().await.unwrap(), Event::Bell));
        assert!(matches!(reader.read_event().await.unwrap(), Event::Bell));
        assert!(reader.read_event().await.is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        finished(&control).await;
    }

    #[tokio::test]
    async fn one_terminal_request_and_reader_drop_never_orphan_the_stream() {
        let (reader, _writer, _peer, _bytes, dropped) = actor();
        let control = reader.teardown_control();
        let clone = control.clone();
        let report = control
            .request(Instant::now() + MAX_DRAIN_DURATION)
            .unwrap();
        assert!(clone.request(Instant::now() + MAX_DRAIN_DURATION).is_err());
        drop(report);
        drop(reader);
        finished(&control).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_event_wait_preserves_ordinary_decode_progress() {
        let (mut reader, _writer, mut peer, _bytes, _dropped) = actor();
        assert!(
            timeout(Duration::from_millis(1), reader.read_event())
                .await
                .is_err()
        );
        peer.write_all(&[2]).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), reader.read_event())
                .await
                .unwrap()
                .unwrap(),
            Event::Bell
        ));
    }
}
