//! Unix Map is an FD-backed copy transport. It provides no image fence, seals,
//! or guarantee that bytes stay unchanged while a row is read.

use super::{
    CpuListener, CpuSurface, Frame, Instant, MAX_IMAGE_BYTES, MAX_PIXELS, PixelByteOrder,
    format_size, normalize_row,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{self, ErrorKind},
    os::fd::AsFd,
    os::unix::fs::{FileExt, MetadataExt},
};
use zbus::zvariant::OwnedFd;

pub const MAP_INTERFACE: &str = "org.qemu.Display1.Listener.Unix.Map";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapDescription {
    pub device: u64,
    pub inode: u64,
    /// Observed backing-file size, not an allocation or mapping request.
    pub file_extent: u64,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

/// Transport evidence only. Successful copies do not prove coherent images.
#[derive(Clone, Debug, Default)]
pub struct MapStats {
    pub advertised: bool,
    pub scanout_map_calls: u64,
    pub update_map_calls: u64,
    pub scanout_maps: u64,
    pub update_maps: u64,
    pub scanout_map_errors: u64,
    pub update_map_errors: u64,
    pub replacements: u64,
    pub released_generations: u64,
    pub invalidations: u64,
    pub active_generation: Option<u64>,
    pub last_description: Option<MapDescription>,
    pub last_damage: Option<[i32; 4]>,
    pub last_error: Option<String>,
    /// Array calls while Map is advertised, including rejected calls.
    pub array_scanout_calls: u64,
    pub array_update_calls: u64,
    /// Successful array copies while Map is advertised: explicit fallback.
    pub array_scanouts: u64,
    pub array_updates: u64,
    pub array_bytes: u64,
    /// Actual read_at attempts and returned bytes, including failed copies.
    pub read_calls: u64,
    pub bytes_read: u64,
    /// Includes FD validation, reads and conversion, including failed copies.
    pub read_convert_ns: u64,
}

pub(super) struct MapPlane {
    file: File,
    description: MapDescription,
    size: usize,
}

#[derive(Default)]
struct ReadWork {
    calls: u64,
    bytes: u64,
}

impl MapPlane {
    #[allow(clippy::too_many_arguments)]
    fn from_fd(
        handle: &impl AsFd,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> Result<Self> {
        ensure!(
            width > 0
                && height > 0
                && width <= u32::from(u16::MAX)
                && height <= u32::from(u16::MAX),
            "invalid Map dimensions"
        );
        let area = u64::from(width)
            .checked_mul(u64::from(height))
            .context("Map area overflow")?;
        ensure!(area <= MAX_PIXELS as u64, "Map exceeds MAX_PIXELS");
        let size = format_size(format)?;
        let packed = u64::from(width)
            .checked_mul(size as u64)
            .context("Map packed row overflow")?;
        ensure!(
            u64::from(stride) >= packed,
            "Map stride is shorter than a row"
        );
        let plane_extent = u64::from(stride)
            .checked_mul(u64::from(height))
            .context("Map stride extent overflow")?;
        ensure!(
            plane_extent <= MAX_IMAGE_BYTES as u64,
            "excessive Map extent"
        );
        let last_row_end = u64::from(offset)
            .checked_add(
                u64::from(height - 1)
                    .checked_mul(u64::from(stride))
                    .context("Map row offset overflow")?,
            )
            .and_then(|start| start.checked_add(packed))
            .context("Map final row overflow")?;
        // try_clone_to_owned uses a new owned CLOEXEC descriptor. Never reopen
        // a received pathname and never borrow the message's descriptor.
        let file = File::from(handle.as_fd().try_clone_to_owned()?);
        let metadata = file.metadata().context("Map fstat")?;
        ensure!(metadata.is_file(), "Map FD must be a regular file");
        ensure!(
            last_row_end <= metadata.len(),
            "Map final row exceeds file extent"
        );
        Ok(Self {
            file,
            description: MapDescription {
                device: metadata.dev(),
                inode: metadata.ino(),
                file_extent: metadata.len(),
                offset,
                width,
                height,
                stride,
                format,
            },
            size,
        })
    }

    fn check_extent(&self) -> Result<()> {
        ensure!(
            self.file.metadata().context("Map fstat during read")?.len()
                == self.description.file_extent,
            "Map file extent changed"
        );
        Ok(())
    }

    // The injectable read operation keeps interruption/short-read/resize tests
    // deterministic, using owned real files. The production operation is pread.
    #[allow(clippy::too_many_arguments)]
    fn copy_region_using(
        &self,
        frame: &mut Frame,
        [x, y, width, height]: [i32; 4],
        order: PixelByteOrder,
        work: &mut ReadWork,
        mut read_at: impl FnMut(&File, &mut [u8], u64) -> io::Result<usize>,
    ) -> Result<()> {
        let description = &self.description;
        ensure!(
            x >= 0 && y >= 0 && width > 0 && height > 0,
            "invalid Map damage rectangle"
        );
        ensure!(
            i64::from(x) + i64::from(width) <= i64::from(description.width)
                && i64::from(y) + i64::from(height) <= i64::from(description.height),
            "Map damage outside framebuffer"
        );
        ensure!(
            u32::from(frame.width) == description.width
                && u32::from(frame.height) == description.height
                && frame.pixels.len() == usize::from(frame.width) * usize::from(frame.height) * 4,
            "Map canonical generation differs from scanout"
        );
        self.check_extent()?;
        let packed = (width as usize)
            .checked_mul(self.size)
            .context("Map damage row overflow")?;
        // At most 262,140 bytes, independent of the backing file's size. No
        // rectangle temporary, mmap, or externally mutable borrowed slice.
        let mut row = vec![0; packed];
        for row_index in 0..height as usize {
            let source = u64::from(description.offset)
                .checked_add(
                    (y as u64 + row_index as u64)
                        .checked_mul(u64::from(description.stride))
                        .context("Map source row overflow")?,
                )
                .and_then(|offset| {
                    (x as u64)
                        .checked_mul(self.size as u64)
                        .and_then(|x| offset.checked_add(x))
                })
                .context("Map source offset overflow")?;
            let mut filled = 0;
            while filled < row.len() {
                let offset = source
                    .checked_add(filled as u64)
                    .context("Map read offset overflow")?;
                work.calls = work.calls.saturating_add(1);
                match read_at(&self.file, &mut row[filled..], offset) {
                    Ok(0) => anyhow::bail!("short Map read before complete row"),
                    Ok(count) => {
                        ensure!(count <= row.len() - filled, "invalid Map read length");
                        filled += count;
                        work.bytes = work.bytes.saturating_add(count as u64);
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("Map read_at"),
                }
            }
            let destination =
                ((y as usize + row_index) * usize::from(frame.width) + x as usize) * 4;
            normalize_row(
                description.format,
                &row,
                order,
                &mut frame.pixels[destination..destination + width as usize * 4],
            )?;
        }
        self.check_extent()?;
        Ok(())
    }
}

impl CpuSurface {
    pub(super) fn release_map(&mut self) {
        if self.map.take().is_some() {
            self.stats.map.released_generations =
                self.stats.map.released_generations.saturating_add(1);
        }
        self.stats.map.active_generation = None;
    }

    fn fail_map(&mut self, error: &anyhow::Error) {
        self.stats.map.invalidations = self.stats.map.invalidations.saturating_add(1);
        self.stats.map.last_error = Some(format!("{error:#}"));
        // A failed row copy may have changed private canonical bytes. Clear the
        // complete generation while holding the same mutex as read_event.
        self.disable();
    }

    #[allow(clippy::too_many_arguments)]
    fn scanout_map(
        &mut self,
        handle: &impl AsFd,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> Result<()> {
        self.stats.map.scanout_map_calls = self.stats.map.scanout_map_calls.saturating_add(1);
        let started = Instant::now();
        let mut work = ReadWork::default();
        let result: Result<()> = (|| {
            let plane = MapPlane::from_fd(handle, offset, width, height, stride, format)?;
            self.stats.map.last_description = Some(plane.description.clone());
            let mut frame = Frame {
                width: width as u16,
                height: height as u16,
                pixels: vec![0; width as usize * height as usize * 4],
            };
            plane.copy_region_using(
                &mut frame,
                [0, 0, width as i32, height as i32],
                self.order,
                &mut work,
                |file, row, offset| file.read_at(row, offset),
            )?;
            // Keep the old FD alive until candidate validation/copy finishes.
            if self.map.is_some() {
                self.stats.map.replacements = self.stats.map.replacements.saturating_add(1);
            }
            self.release_map();
            self.map = Some(plane);
            self.frame = Some(frame);
            self.disabled = false;
            self.stats.map.scanout_maps = self.stats.map.scanout_maps.saturating_add(1);
            self.stats.map.active_generation = Some(self.stats.map.scanout_maps);
            self.stats.latest_format = format;
            self.stats.latest_stride = stride;
            self.changed_frame(started);
            Ok(())
        })();
        self.record_map_work(started, work);
        if let Err(error) = &result {
            self.stats.map.scanout_map_errors = self.stats.map.scanout_map_errors.saturating_add(1);
            self.fail_map(error);
        }
        result
    }

    fn update_map(&mut self, damage: [i32; 4]) -> Result<()> {
        self.update_map_using(damage, |file, row, offset| file.read_at(row, offset))
    }

    fn update_map_using(
        &mut self,
        damage: [i32; 4],
        read_at: impl FnMut(&File, &mut [u8], u64) -> io::Result<usize>,
    ) -> Result<()> {
        self.stats.map.update_map_calls = self.stats.map.update_map_calls.saturating_add(1);
        self.stats.map.last_damage = Some(damage);
        let started = Instant::now();
        let mut work = ReadWork::default();
        let result: Result<()> = (|| {
            let plane = self.map.as_ref().context("UpdateMap requires ScanoutMap")?;
            let frame = self
                .frame
                .as_mut()
                .context("Map generation has no canonical frame")?;
            plane.copy_region_using(frame, damage, self.order, &mut work, read_at)?;
            self.stats.map.update_maps = self.stats.map.update_maps.saturating_add(1);
            self.changed_frame(started);
            Ok(())
        })();
        self.record_map_work(started, work);
        if let Err(error) = &result {
            self.stats.map.update_map_errors = self.stats.map.update_map_errors.saturating_add(1);
            self.fail_map(error);
        }
        result
    }

    fn record_map_work(&mut self, started: Instant, work: ReadWork) {
        self.stats.map.read_calls = self.stats.map.read_calls.saturating_add(work.calls);
        self.stats.map.bytes_read = self.stats.map.bytes_read.saturating_add(work.bytes);
        self.stats.map.read_convert_ns = self
            .stats
            .map
            .read_convert_ns
            .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
    }
}

pub(super) struct CpuMapListener(pub(super) CpuListener);

// Both interfaces dispatch sequentially on the same callback connection. Their
// shared surface mutex also serializes copies, snapshots and disconnect cleanup.
#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.Map", spawn = false)]
impl CpuMapListener {
    #[allow(clippy::too_many_arguments)]
    fn scanout_map(
        &self,
        handle: OwnedFd,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        pixman_format: u32,
    ) -> zbus::fdo::Result<()> {
        self.0.apply(|surface| {
            surface.scanout_map(&handle, offset, width, height, stride, pixman_format)
        })
    }

    fn update_map(&self, x: i32, y: i32, width: i32, height: i32) -> zbus::fdo::Result<()> {
        self.0
            .apply(|surface| surface.update_map([x, y, width, height]))
    }
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
