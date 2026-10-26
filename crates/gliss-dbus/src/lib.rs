//! Experimental CPU QEMU D-Bus capture. No guest or QMP is touched at import.
//! Byte arrays remain the default; Unix Map advertisement is an explicit opt-in.
//!
//! Start an owned fixture on a private Unix D-Bus with `gl=off`; never attach
//! this listener to a baseline VM while measuring its VNC refresh behavior.

use anyhow::{Context, Result, ensure};
use gliss_core::{Frame, MAX_PIXELS};
use std::{
    os::fd::AsFd,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    net::UnixStream,
    sync::{oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use zbus::{Connection, Proxy, connection::Builder, zvariant::Fd};

mod unix_map;
pub use unix_map::{MAP_INTERFACE, MapDescription, MapStats};

/// Optional CPU transport advertisement. Array callbacks remain accepted and
/// counted when Map is enabled; callers must prove successful Map callbacks.
#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureOptions {
    pub unix_map: bool,
}

pub const MAX_IMAGE_BYTES: usize = MAX_PIXELS * 4;
pub const MAX_CURSOR_PIXELS: usize = 1024 * 1024;
pub const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

pub const PIXMAN_X8R8G8B8: u32 = 0x2002_0888;
pub const PIXMAN_A8R8G8B8: u32 = 0x2002_8888;
pub const PIXMAN_X8B8G8R8: u32 = 0x2003_0888;
pub const PIXMAN_A8B8G8R8: u32 = 0x2003_8888;
pub const PIXMAN_R8G8B8: u32 = 0x1802_0888;
pub const PIXMAN_B8G8R8: u32 = 0x1803_0888;
pub const PIXMAN_R5G6B5: u32 = 0x1002_0565;
pub const PIXMAN_B5G6R5: u32 = 0x1003_0565;

/// Pixman image arrays use producer host byte order, independent of the D-Bus
/// message header and the guest CPU. A Unix-local QEMU shares our host order.
#[derive(Clone, Copy, Debug)]
pub enum PixelByteOrder {
    Native,
    Little,
    Big,
}

impl PixelByteOrder {
    fn little(self) -> bool {
        match self {
            Self::Native => cfg!(target_endian = "little"),
            Self::Little => true,
            Self::Big => false,
        }
    }
    fn word(self, bytes: &[u8]) -> u32 {
        if self.little() {
            bytes
                .iter()
                .enumerate()
                .fold(0, |word, (i, byte)| word | (u32::from(*byte) << (i * 8)))
        } else {
            bytes
                .iter()
                .fold(0, |word, byte| (word << 8) | u32::from(*byte))
        }
    }
}

fn format_size(format: u32) -> Result<usize> {
    match format {
        PIXMAN_X8R8G8B8 | PIXMAN_A8R8G8B8 | PIXMAN_X8B8G8R8 | PIXMAN_A8B8G8R8 => Ok(4),
        PIXMAN_R8G8B8 | PIXMAN_B8G8R8 => Ok(3),
        PIXMAN_R5G6B5 | PIXMAN_B5G6R5 => Ok(2),
        _ => anyhow::bail!("unsupported Pixman format 0x{format:08x}"),
    }
}

fn normalize_row(format: u32, data: &[u8], order: PixelByteOrder, pixels: &mut [u8]) -> Result<()> {
    let size = format_size(format)?;
    ensure!(
        data.len().is_multiple_of(size) && pixels.len() == data.len() / size * 4,
        "invalid canonical row size"
    );
    for (x, pixel) in data.chunks_exact(size).enumerate() {
        let word = order.word(pixel);
        let (r, g, b) = match format {
            PIXMAN_R5G6B5 | PIXMAN_B5G6R5 => {
                let high = ((word >> 11) & 31) * 255 / 31;
                let green = ((word >> 5) & 63) * 255 / 63;
                let low = (word & 31) * 255 / 31;
                if format == PIXMAN_R5G6B5 {
                    (high, green, low)
                } else {
                    (low, green, high)
                }
            }
            _ => {
                if matches!(format, PIXMAN_A8R8G8B8 | PIXMAN_A8B8G8R8) {
                    ensure!(
                        word >> 24 == 255,
                        "nonopaque premultiplied framebuffer alpha is unsupported"
                    );
                }
                let high = (word >> 16) & 255;
                let green = (word >> 8) & 255;
                let low = word & 255;
                if matches!(format, PIXMAN_X8B8G8R8 | PIXMAN_A8B8G8R8 | PIXMAN_B8G8R8) {
                    (low, green, high)
                } else {
                    (high, green, low)
                }
            }
        };
        let destination = x * 4;
        pixels[destination..destination + 4].copy_from_slice(&[r as u8, g as u8, b as u8, 255]);
    }
    Ok(())
}

/// Normalize a complete, strided CPU image plane to tightly packed opaque RGBA.
/// Validation completes before a caller's canonical surface can be changed.
pub fn normalize_plane(
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
    data: &[u8],
    order: PixelByteOrder,
) -> Result<Frame> {
    ensure!(
        width > 0 && height > 0 && width <= u32::from(u16::MAX) && height <= u32::from(u16::MAX),
        "invalid CPU image dimensions"
    );
    let area = (width as usize)
        .checked_mul(height as usize)
        .context("image area overflow")?;
    ensure!(area <= MAX_PIXELS, "CPU image exceeds MAX_PIXELS");
    let size = format_size(format)?;
    let packed = width as usize * size;
    ensure!(
        stride as usize >= packed,
        "CPU image stride is shorter than a row"
    );
    let expected = (stride as usize)
        .checked_mul(height as usize)
        .context("stride overflow")?;
    ensure!(
        expected <= MAX_IMAGE_BYTES && data.len() == expected,
        "truncated, trailing or excessive CPU image plane"
    );
    let mut pixels = vec![0; area * 4];
    for (y, row) in data.chunks_exact(stride as usize).enumerate() {
        let destination = y * width as usize * 4;
        normalize_row(
            format,
            &row[..packed],
            order,
            &mut pixels[destination..destination + width as usize * 4],
        )?;
    }
    Ok(Frame {
        width: width as u16,
        height: height as u16,
        pixels,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    /// Native ARGB words, retaining alpha exactly; no cursor compositing here.
    pub argb: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct CaptureStats {
    pub map: MapStats,
    pub scanouts: u64,
    pub updates: u64,
    pub source_bytes: u64,
    pub normalization_ns: u64,
    pub latest_format: u32,
    pub latest_stride: u32,
}

/// Canonical CPU surface with atomic validation; independent of D-Bus transport.
pub struct CpuSurface {
    map: Option<unix_map::MapPlane>,
    frame: Option<Frame>,
    cursor: Option<Cursor>,
    mouse: Option<(i32, i32, bool)>,
    disabled: bool,
    revision: u64,
    frame_revision: u64,
    stats: CaptureStats,
    received_at: Option<Instant>,
    order: PixelByteOrder,
    closed: bool,
}

impl CpuSurface {
    pub fn new(order: PixelByteOrder) -> Self {
        Self {
            map: None,
            frame: None,
            cursor: None,
            mouse: None,
            disabled: true,
            revision: 0,
            frame_revision: 0,
            stats: CaptureStats::default(),
            received_at: None,
            order,
            closed: false,
        }
    }
    pub fn frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }
    pub fn stats(&self) -> &CaptureStats {
        &self.stats
    }
    pub fn scanout(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        data: &[u8],
    ) -> Result<()> {
        let received = Instant::now();
        if self.stats.map.advertised {
            self.stats.map.array_scanout_calls =
                self.stats.map.array_scanout_calls.saturating_add(1);
        }
        let frame = normalize_plane(width, height, stride, format, data, self.order)?;
        self.release_map();
        if self.stats.map.advertised {
            self.stats.map.array_scanouts = self.stats.map.array_scanouts.saturating_add(1);
            self.stats.map.array_bytes =
                self.stats.map.array_bytes.saturating_add(data.len() as u64);
        }
        self.stats.normalization_ns +=
            received.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.stats.scanouts += 1;
        self.stats.source_bytes += data.len() as u64;
        self.stats.latest_format = format;
        self.stats.latest_stride = stride;
        self.frame = Some(frame);
        self.disabled = false;
        self.changed_frame(received);
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        format: u32,
        data: &[u8],
    ) -> Result<()> {
        let received = Instant::now();
        if self.stats.map.advertised {
            self.stats.map.array_update_calls = self.stats.map.array_update_calls.saturating_add(1);
        }
        ensure!(
            x >= 0 && y >= 0 && width > 0 && height > 0,
            "invalid update rectangle"
        );
        let previous = self.frame.as_ref().context("CPU update requires Scanout")?;
        ensure!(
            i64::from(x) + i64::from(width) <= i64::from(previous.width)
                && i64::from(y) + i64::from(height) <= i64::from(previous.height),
            "CPU update outside framebuffer"
        );
        let region = normalize_plane(
            width as u32,
            height as u32,
            stride,
            format,
            data,
            self.order,
        )?;
        self.release_map();
        if self.stats.map.advertised {
            self.stats.map.array_updates = self.stats.map.array_updates.saturating_add(1);
            self.stats.map.array_bytes =
                self.stats.map.array_bytes.saturating_add(data.len() as u64);
        }
        let frame = self.frame.as_mut().unwrap();
        for row in 0..height as usize {
            let source = row * width as usize * 4;
            let destination = ((y as usize + row) * usize::from(frame.width) + x as usize) * 4;
            frame.pixels[destination..destination + width as usize * 4]
                .copy_from_slice(&region.pixels[source..source + width as usize * 4]);
        }
        self.stats.normalization_ns +=
            received.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.stats.updates += 1;
        self.stats.source_bytes += data.len() as u64;
        self.stats.latest_format = format;
        self.stats.latest_stride = stride;
        self.changed_frame(received);
        Ok(())
    }
    fn changed_frame(&mut self, received: Instant) {
        self.revision = self.revision.wrapping_add(1);
        self.frame_revision = self.frame_revision.wrapping_add(1);
        self.received_at = Some(received);
    }
    pub fn disable(&mut self) {
        self.release_map();
        self.frame = None;
        self.cursor = None;
        self.mouse = None;
        self.disabled = true;
        self.changed_frame(Instant::now());
    }
    pub fn reset(&mut self) {
        self.disable();
    }
}

#[derive(Clone, Debug, Default)]
struct Notice {
    closed: bool,
}

/// Latest complete canonical pixels when changed, plus current cursor/position.
/// Coalesced intermediate updates are reflected in stats; source state stays exact.
#[derive(Clone, Debug)]
pub struct Event {
    pub revision: u64,
    pub frame_revision: u64,
    pub frame: Option<Frame>,
    pub disabled: bool,
    pub cursor: Option<Cursor>,
    pub mouse: Option<(i32, i32, bool)>,
    pub stats: CaptureStats,
    pub received_at: Option<Instant>,
    pub snapshot_ns: u64,
}

pub struct DbusSource {
    surface: Arc<Mutex<CpuSurface>>,
    notices: watch::Receiver<Notice>,
    last_frame_revision: u64,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
}

impl DbusSource {
    /// Read counters without consuming notifications or cloning frame pixels.
    /// Available even after closure so zero-Map/fallback results remain visible.
    pub fn map_stats(&self) -> Result<MapStats> {
        let surface = self
            .surface
            .lock()
            .map_err(|_| anyhow::anyhow!("CPU surface poisoned"))?;
        Ok(surface.stats.map.clone())
    }

    /// Wait only for transport closure, without reading/cloning/hash processing
    /// of display frames. A canceled wait leaves the source usable.
    pub async fn wait_closed(&self) {
        let mut notices = self.notices.clone();
        loop {
            if notices.borrow().closed {
                return;
            }
            if notices.changed().await.is_err() {
                return;
            }
        }
    }

    /// Cancellation-safe wait: canceled reads never steal a display update.
    pub async fn read_event(&mut self) -> Result<Event> {
        ensure!(
            !self.notices.borrow().closed,
            "QEMU D-Bus connection closed"
        );
        self.notices
            .changed()
            .await
            .context("D-Bus event stream closed")?;
        ensure!(
            !self.notices.borrow().closed,
            "QEMU D-Bus connection closed"
        );
        let started = Instant::now();
        let surface = self
            .surface
            .lock()
            .map_err(|_| anyhow::anyhow!("CPU surface poisoned"))?;
        let changed = self.last_frame_revision != surface.frame_revision;
        let frame = if changed { surface.frame.clone() } else { None };
        self.last_frame_revision = surface.frame_revision;
        Ok(Event {
            revision: surface.revision,
            frame_revision: surface.frame_revision,
            frame,
            disabled: surface.disabled,
            cursor: surface.cursor.clone(),
            mouse: surface.mouse,
            stats: surface.stats.clone(),
            received_at: surface.received_at,
            snapshot_ns: started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
        })
    }
    /// Close both peers and verify the owned cleanup task completed successfully.
    /// A timeout or task failure leaves completion uncertain and is reported.
    pub async fn close_checked(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            timeout(Duration::from_secs(5), task)
                .await
                .context("D-Bus cleanup task timed out")?
                .context("D-Bus cleanup task failed")??;
        }
        Ok(())
    }
    pub async fn close(self) {
        let _ = self.close_checked().await;
    }
}

impl Drop for DbusSource {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        // The bounded cleanup task owns both connections and closes them. It is
        // deliberately not aborted; abrupt abort would skip explicit peer close.
    }
}

#[derive(Clone)]
struct CpuListener {
    unix_map: bool,
    surface: Arc<Mutex<CpuSurface>>,
    notices: watch::Sender<Notice>,
}

impl CpuListener {
    fn apply(&self, change: impl FnOnce(&mut CpuSurface) -> Result<()>) -> zbus::fdo::Result<()> {
        let mut surface = self
            .surface
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("CPU surface poisoned".into()))?;
        if surface.closed {
            return Err(zbus::fdo::Error::Failed(
                "capture connection is closed".into(),
            ));
        }
        let revision = surface.revision;
        let result = change(&mut surface);
        if result.is_err() && surface.stats.map.advertised && surface.revision == revision {
            surface.revision = surface.revision.wrapping_add(1);
        }
        // Failed Map copies clear their generation before this notification.
        // Default array errors retain the existing no-notification behavior.
        if result.is_ok() || surface.revision != revision {
            self.notices.send_replace(Notice::default());
        }
        result.map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))
    }
}

// QEMU sends ordered updates. The default spawning policy may reorder them;
// sequential dispatch makes overlapping updates and Disable/Scanout safe.
#[zbus::interface(name = "org.qemu.Display1.Listener", spawn = false)]
impl CpuListener {
    fn scanout(
        &self,
        width: u32,
        height: u32,
        stride: u32,
        pixman_format: u32,
        data: &[u8],
    ) -> zbus::fdo::Result<()> {
        self.apply(|surface| surface.scanout(width, height, stride, pixman_format, data))
    }
    #[allow(clippy::too_many_arguments)]
    fn update(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        pixman_format: u32,
        data: &[u8],
    ) -> zbus::fdo::Result<()> {
        self.apply(|surface| surface.update(x, y, width, height, stride, pixman_format, data))
    }
    fn disable(&self) -> zbus::fdo::Result<()> {
        self.apply(|surface| {
            surface.disable();
            Ok(())
        })
    }
    fn mouse_set(&self, x: i32, y: i32, on: i32) -> zbus::fdo::Result<()> {
        self.apply(|surface| {
            ensure!(on == 0 || on == 1, "invalid mouse visibility");
            surface.mouse = Some((x, y, on == 1));
            surface.revision = surface.revision.wrapping_add(1);
            Ok(())
        })
    }
    fn cursor_define(
        &self,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: &[u8],
    ) -> zbus::fdo::Result<()> {
        self.apply(|surface| {
            ensure!(
                width > 0
                    && height > 0
                    && width <= i32::from(u16::MAX)
                    && height <= i32::from(u16::MAX)
                    && hot_x >= 0
                    && hot_y >= 0
                    && hot_x < width
                    && hot_y < height,
                "invalid cursor dimensions/hotspot"
            );
            let area = width as usize * height as usize;
            ensure!(
                area <= MAX_CURSOR_PIXELS && data.len() == area * 4,
                "invalid cursor data length"
            );
            let argb = data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| surface.order.word(bytes))
                .collect();
            surface.cursor = Some(Cursor {
                width: width as u16,
                height: height as u16,
                hotspot_x: hot_x as u16,
                hotspot_y: hot_y as u16,
                argb,
            });
            surface.revision = surface.revision.wrapping_add(1);
            Ok(())
        })
    }
    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        if self.unix_map {
            vec![MAP_INTERFACE.to_owned()]
        } else {
            Vec::new()
        }
    } // No GL or DMA-BUF capability.
}

/// Connect only to an explicitly configured private Unix bus. It never uses a
/// user's session/system bus, launches a daemon, or invokes QMP.
pub async fn connect_private_bus(
    address: &str,
    expected_vm_uuid: &str,
    console_id: u32,
) -> Result<DbusSource> {
    connect_private_bus_with_options(
        address,
        expected_vm_uuid,
        console_id,
        CaptureOptions::default(),
    )
    .await
}

/// Opt in only on an owned fixture after establishing its process/bus custody.
/// A socketpair's creator credentials alone do not identify its eventual peer.
/// This API validates UUID/console identity, not QEMU PID/start-time custody.
pub async fn connect_private_bus_with_options(
    address: &str,
    expected_vm_uuid: &str,
    console_id: u32,
    options: CaptureOptions,
) -> Result<DbusSource> {
    ensure!(
        address.starts_with("unix:path=") && !address.contains(';'),
        "require one explicit private Unix bus address"
    );
    timeout(CONNECT_TIMEOUT, async {
        let control = Builder::address(address)?
            .max_queued(4)
            .method_timeout(CALL_TIMEOUT)
            .build()
            .await?;
        connect_control_with_options(control, expected_vm_uuid, console_id, options).await
    })
    .await
    .context("private QEMU D-Bus connection timed out")?
}

/// Attach a listener through a supplied D-Bus control connection. This permits
/// a private mock peer to exercise real registration/FD passing without QEMU.
pub async fn connect_control(
    control: Connection,
    expected_vm_uuid: &str,
    console_id: u32,
) -> Result<DbusSource> {
    connect_control_with_options(
        control,
        expected_vm_uuid,
        console_id,
        CaptureOptions::default(),
    )
    .await
}

/// Supplied-control equivalent of connect_private_bus_with_options, also used
/// by owned synthetic peers. Optional Map calls never imply hardware eligibility.
pub async fn connect_control_with_options(
    control: Connection,
    expected_vm_uuid: &str,
    console_id: u32,
    options: CaptureOptions,
) -> Result<DbusSource> {
    timeout(
        CONNECT_TIMEOUT,
        connect_control_inner(control, expected_vm_uuid, console_id, options),
    )
    .await
    .context("QEMU D-Bus listener registration timed out")?
}

async fn connect_control_inner(
    control: Connection,
    expected_vm_uuid: &str,
    console_id: u32,
    options: CaptureOptions,
) -> Result<DbusSource> {
    ensure!(
        !expected_vm_uuid.is_empty() && expected_vm_uuid.len() <= 128,
        "expected VM UUID is required"
    );
    let vm = Proxy::new(
        &control,
        "org.qemu",
        "/org/qemu/Display1/VM",
        "org.qemu.Display1.VM",
    )
    .await?;
    let uuid: String = vm.get_property("UUID").await?;
    ensure!(
        uuid == expected_vm_uuid,
        "D-Bus VM UUID differs from configured fixture"
    );
    let consoles: Vec<u32> = vm.get_property("ConsoleIDs").await?;
    ensure!(
        consoles.len() <= 256 && consoles.contains(&console_id),
        "configured display console is unavailable"
    );
    let path = format!("/org/qemu/Display1/Console_{console_id}");
    let console = Proxy::new(
        &control,
        "org.qemu",
        path.as_str(),
        "org.qemu.Display1.Console",
    )
    .await?;
    let kind: String = console.get_property("Type").await?;
    ensure!(
        kind == "Graphic",
        "configured D-Bus console is not graphical"
    );
    let mut initial_surface = CpuSurface::new(PixelByteOrder::Native);
    initial_surface.stats.map.advertised = options.unix_map;
    let surface = Arc::new(Mutex::new(initial_surface));
    let (notices, receiver) = watch::channel(Notice::default());
    let (local, remote) = UnixStream::pair()?;
    let remote = remote.into_std()?;
    let listener = CpuListener {
        unix_map: options.unix_map,
        surface: surface.clone(),
        notices: notices.clone(),
    };
    let builder = Builder::unix_stream(local)
        .p2p()
        .max_queued(2)
        .method_timeout(CALL_TIMEOUT)
        .serve_at(LISTENER_PATH, listener.clone())?;
    let builder = if options.unix_map {
        builder.serve_at(LISTENER_PATH, unix_map::CpuMapListener(listener))?
    } else {
        builder
    };
    let fd = Fd::from(remote.as_fd());
    let arguments = (fd,);
    // QEMU authenticates as server on the passed peer. Build/registration must
    // progress concurrently; no raw descriptors or unsafe ownership transfer.
    let (callback, registered) = tokio::join!(
        builder.build(),
        console.call::<_, _, ()>("RegisterListener", &arguments)
    );
    registered.context("QEMU RegisterListener")?;
    let callback = callback.context("callback peer D-Bus handshake")?;
    drop(console);
    drop(vm);
    drop(arguments);
    drop(remote);
    let (shutdown, stopped) = oneshot::channel();
    let task_surface = surface.clone();
    let task = tokio::spawn(async move {
        tokio::select! { _ = stopped => {}, _ = callback.closed() => {}, _ = control.closed() => {} }
        if let Ok(mut surface) = task_surface.lock() {
            surface.disable(); // Drop every shared FD before announcing closure.
            surface.closed = true;
        }
        notices.send_replace(Notice { closed: true });
        // Attempt both closes even when the first fails; propagate uncertainty.
        let callback_result = timeout(Duration::from_secs(2), callback.close())
            .await
            .context("D-Bus callback close timed out")
            .and_then(|result| result.context("D-Bus callback close failed"));
        let control_result = timeout(Duration::from_secs(2), control.close())
            .await
            .context("D-Bus control close timed out")
            .and_then(|result| result.context("D-Bus control close failed"));
        callback_result.and(control_result)
    });
    Ok(DbusSource {
        surface,
        notices: receiver,
        last_frame_revision: 0,
        shutdown: Some(shutdown),
        task: Some(task),
    })
}

#[cfg(test)]
mod tests;
