//! Independent absolute GLT2 desktop tiles. Negotiated fragmentation and bounded current-state refresh.
use anyhow::{Result, ensure};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use gliss_core::Frame;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
};

pub const TILE: usize = 32;
pub const HEADER: usize = 32;
pub const DATAGRAM: usize = 1200;
pub const PAYLOAD: usize = DATAGRAM - HEADER;
pub const REFRESH_STEPS: usize = 15;
pub const MAX_ASSEMBLIES: usize = 128;
pub const MAX_ASSEMBLY_BYTES: usize = 524_288;
pub const EXPIRY_STEPS: u64 = 2;
pub const MIN_PAYLOAD: usize = 1024;
pub const MAX_CODEC_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_PENDING_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const PENDING_SLOT_METADATA_BYTES: usize = 128;
pub const ACTIVE_TILE_RESERVE_BYTES: usize = 1024 * 1024;
pub const MAX_TILE_COUNT: usize = 8160;

pub fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

fn dimensions(width: u16, height: u16) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= 3840 && height <= 2160,
        "geometry outside 1..3840 by 1..2160"
    );
    Ok(())
}

pub fn tile_count(width: u16, height: u16) -> usize {
    usize::from(width).div_ceil(TILE) * usize::from(height).div_ceil(TILE)
}

fn rect(width: u16, height: u16, tile: usize) -> (usize, usize, usize, usize) {
    let columns = usize::from(width).div_ceil(TILE);
    let x = tile % columns * TILE;
    let y = tile / columns * TILE;
    (
        x,
        y,
        TILE.min(usize::from(width) - x),
        TILE.min(usize::from(height) - y),
    )
}

fn extract(frame: &Frame, tile: usize) -> Vec<u8> {
    let (x, y, width, height) = rect(frame.width, frame.height, tile);
    let stride = usize::from(frame.width) * 4;
    let mut bytes = Vec::with_capacity(width * height * 4);
    for row in y..y + height {
        bytes
            .extend_from_slice(&frame.pixels[row * stride + x * 4..row * stride + (x + width) * 4]);
    }
    bytes
}

fn equal_tile(a: &Frame, b: &Frame, tile: usize) -> bool {
    let (x, y, width, height) = rect(a.width, a.height, tile);
    let stride = usize::from(a.width) * 4;
    (y..y + height).all(|row| {
        let range = row * stride + x * 4..row * stride + (x + width) * 4;
        a.pixels[range.clone()] == b.pixels[range]
    })
}

#[derive(Clone, Default, Debug, Serialize)]
pub struct Stats {
    pub tiles: u64,
    pub candidate_trials: u64,
    pub palette_protected_tiles: u64,
    pub palette_protected_pixels: u64,
    pub selected_encoding_policy: [[u64; 3]; 3],
}

pub fn palette_protected(raw: &[u8]) -> bool {
    if raw.is_empty() || !raw.len().is_multiple_of(4) || raw.len() > TILE * TILE * 4 {
        return false;
    }
    let mut colors = BTreeSet::new();
    for pixel in raw.as_chunks::<4>().0 {
        colors.insert(*pixel);
        if colors.len() > 64 {
            return false;
        }
    }
    true
}

fn quotient(value: i32, step: i32) -> i32 {
    if value >= 0 {
        (value + step / 2) / step
    } else {
        -((-value + step / 2) / step)
    }
}

/// Four byte planes with reversible left-byte residuals, no cross-tile state.
pub fn forward(raw: &[u8], width: usize, height: usize, policy: u8) -> Result<Vec<u8>> {
    ensure!(
        width > 0
            && width <= TILE
            && height > 0
            && height <= TILE
            && raw.len() == width * height * 4
            && policy <= 2,
        "invalid transform input"
    );
    let area = width * height;
    let mut planes = vec![0u8; raw.len()];
    for (index, pixel) in raw.as_chunks::<4>().0.iter().enumerate() {
        let [r, g, b, a] = pixel.map(i32::from);
        let values = if policy == 0 {
            [g, (r - g).rem_euclid(256), (b - g).rem_euclid(256), a]
        } else {
            let co = r - b;
            let t = b + co.div_euclid(2);
            let cg = g - t;
            let y = t + cg.div_euclid(2);
            let step = if policy == 1 { 16 } else { 32 };
            let y = if policy == 1 {
                y
            } else {
                (quotient(y, 2) * 2).clamp(0, 255)
            };
            [y, quotient(co, step) + 128, quotient(cg, step) + 128, a]
        };
        for (channel, value) in values.into_iter().enumerate() {
            planes[channel * area + index] = value as u8;
        }
    }
    for channel in 0..4 {
        for row in 0..height {
            let mut left = 0u8;
            for x in 0..width {
                let offset = channel * area + row * width + x;
                let current = planes[offset];
                planes[offset] = current.wrapping_sub(left);
                left = current;
            }
        }
    }
    Ok(planes)
}

pub fn inverse(predicted: &[u8], width: usize, height: usize, policy: u8) -> Result<Vec<u8>> {
    ensure!(
        width > 0
            && width <= TILE
            && height > 0
            && height <= TILE
            && predicted.len() == width * height * 4
            && policy <= 2,
        "invalid inverse input"
    );
    let area = width * height;
    let mut planes = predicted.to_vec();
    for channel in 0..4 {
        for row in 0..height {
            let mut left = 0u8;
            for x in 0..width {
                let offset = channel * area + row * width + x;
                left = left.wrapping_add(planes[offset]);
                planes[offset] = left;
            }
        }
    }
    let mut rgba = vec![0; predicted.len()];
    for index in 0..area {
        let p = [
            i32::from(planes[index]),
            i32::from(planes[area + index]),
            i32::from(planes[2 * area + index]),
            i32::from(planes[3 * area + index]),
        ];
        let values = if policy == 0 {
            [
                (p[0] + p[1]).rem_euclid(256),
                p[0],
                (p[0] + p[2]).rem_euclid(256),
                p[3],
            ]
        } else {
            let bound = if policy == 1 { 16 } else { 8 };
            ensure!(
                (128 - bound..=128 + bound).contains(&p[1])
                    && (128 - bound..=128 + bound).contains(&p[2]),
                "quantized coefficient outside declared range"
            );
            let step = if policy == 1 { 16 } else { 32 };
            let co = (p[1] - 128) * step;
            let cg = (p[2] - 128) * step;
            let t = p[0] - cg.div_euclid(2);
            let g = cg + t;
            let b = t - co.div_euclid(2);
            [b + co, g, b, p[3]]
        };
        for (channel, value) in values.into_iter().enumerate() {
            rgba[index * 4 + channel] = value.clamp(0, 255) as u8;
        }
    }
    Ok(rgba)
}

fn zlib(raw: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(raw)?;
    Ok(encoder.finish()?)
}

fn select(
    raw: &[u8],
    width: usize,
    height: usize,
    requested: u8,
    stats: &mut Stats,
) -> Result<(u8, u8, Vec<u8>)> {
    let protected = palette_protected(raw);
    stats.tiles += 1;
    if protected {
        stats.palette_protected_tiles += 1;
        stats.palette_protected_pixels += (width * height) as u64;
    }
    let mut best = (0u8, 0u8, raw.to_vec());
    let exact = zlib(raw)?;
    stats.candidate_trials += 3; // raw, exact zlib, exact predictor.
    if exact.len() < best.2.len() {
        best = (1, 0, exact);
    }
    let predicted = zlib(&forward(raw, width, height, 0)?)?;
    if predicted.len() < best.2.len() {
        best = (2, 0, predicted);
    }
    if requested > 0 && !protected {
        let quantized = zlib(&forward(raw, width, height, requested)?)?;
        stats.candidate_trials += 1;
        if quantized.len() < best.2.len() {
            best = (2, requested, quantized);
        }
    }
    stats.selected_encoding_policy[usize::from(best.0)][usize::from(best.1)] += 1;
    Ok(best)
}

fn validate_payload(payload: usize) -> Result<()> {
    ensure!(
        (MIN_PAYLOAD..=PAYLOAD).contains(&payload),
        "GLT2 payload stride must be 1024..=1168"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct EncodedTile {
    pub epoch: u32,
    pub sequence: u32,
    pub width: u16,
    pub height: u16,
    pub tile: usize,
    pub kind: u8,
    pub policy: u8,
    pub raw: usize,
    pub body: Vec<u8>,
}

impl EncodedTile {
    pub fn validate(&self) -> Result<()> {
        dimensions(self.width, self.height)?;
        ensure!(
            self.tile < tile_count(self.width, self.height),
            "tile outside geometry"
        );
        let (_, _, width, height) = rect(self.width, self.height, self.tile);
        ensure!(
            self.raw == width * height * 4 && !self.body.is_empty() && self.body.len() <= self.raw,
            "invalid encoded tile body"
        );
        ensure!(
            self.kind <= 2
                && self.policy <= 2
                && (self.kind == 2 || self.policy == 0)
                && (self.kind != 0 || self.body.len() == self.raw),
            "invalid tile encoding or policy"
        );
        Ok(())
    }
    pub fn fragment_count(&self, payload: usize) -> Result<usize> {
        validate_payload(payload)?;
        self.validate()?;
        let count = self.body.len().div_ceil(payload);
        ensure!((1..=4).contains(&count), "tile fragment bound");
        Ok(count)
    }
    pub fn wire_bytes(&self, payload: usize) -> Result<usize> {
        Ok(self.body.len() + self.fragment_count(payload)? * HEADER)
    }
    /// Complete-tile admission charge: application bytes plus maximum 8-byte
    /// session header and 128-byte conservative transport metadata per fragment.
    pub fn admission_bytes(&self, payload: usize) -> Result<usize> {
        Ok(self.body.len() + self.fragment_count(payload)? * (HEADER + 8 + 128))
    }
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.body.capacity()
    }
    pub fn fragments(&self, payload: usize) -> Result<Vec<Vec<u8>>> {
        let count = self.fragment_count(payload)?;
        let mut packets = Vec::with_capacity(count);
        for (index, body) in self.body.chunks(payload).enumerate() {
            let mut packet = vec![0u8; HEADER];
            packet[..4].copy_from_slice(b"GLT2");
            packet[4..8].copy_from_slice(&self.epoch.to_le_bytes());
            packet[8..12].copy_from_slice(&self.sequence.to_le_bytes());
            packet[12..14].copy_from_slice(&self.width.to_le_bytes());
            packet[14..16].copy_from_slice(&self.height.to_le_bytes());
            packet[16..18].copy_from_slice(&(self.tile as u16).to_le_bytes());
            packet[18] = TILE as u8;
            packet[19] = self.kind;
            packet[20..22].copy_from_slice(&(self.body.len() as u16).to_le_bytes());
            packet[22] = index as u8;
            packet[23] = count as u8;
            packet[24..26].copy_from_slice(&((index * payload) as u16).to_le_bytes());
            packet[26..28].copy_from_slice(&(body.len() as u16).to_le_bytes());
            packet[28..30].copy_from_slice(&(self.raw as u16).to_le_bytes());
            packet[30] = self.policy;
            packet.extend_from_slice(body);
            packets.push(packet);
        }
        Ok(packets)
    }
}

/// Conservative codec allocation planning. Caller-owned pending bodies and
/// metadata have fixed declared reserves. Transport's separate 1 MiB queue and
/// process RSS are not codec allocations; this estimate is not an RSS cap.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MemoryEstimate {
    pub incoming_frame_bytes: usize,
    pub retained_reference_bytes: usize,
    pub pending_body_reserve_bytes: usize,
    pub pending_metadata_reserve_bytes: usize,
    pub active_tile_reserve_bytes: usize,
    pub total_codec_bytes: usize,
}

pub fn memory_estimate(
    width: u16,
    height: u16,
    retained_reference_bytes: usize,
) -> Result<MemoryEstimate> {
    dimensions(width, height)?;
    let incoming_frame_bytes = usize::from(width) * usize::from(height) * 4;
    // Initial capture also needs a reference clone. Resize releases its old
    // clone before allocating the replacement, so reserve their maximum.
    let retained_reference_bytes = retained_reference_bytes.max(incoming_frame_bytes);
    // Reserve the largest supported queue even during shrink/epoch handoff.
    let pending_metadata_reserve_bytes = MAX_TILE_COUNT * PENDING_SLOT_METADATA_BYTES;
    let total_codec_bytes = incoming_frame_bytes
        .checked_add(retained_reference_bytes)
        .and_then(|n| {
            n.checked_add(
                MAX_PENDING_BODY_BYTES + pending_metadata_reserve_bytes + ACTIVE_TILE_RESERVE_BYTES,
            )
        })
        .ok_or_else(|| anyhow::anyhow!("codec memory estimate overflow"))?;
    Ok(MemoryEstimate {
        incoming_frame_bytes,
        retained_reference_bytes,
        pending_body_reserve_bytes: MAX_PENDING_BODY_BYTES,
        pending_metadata_reserve_bytes,
        active_tile_reserve_bytes: ACTIVE_TILE_RESERVE_BYTES,
        total_codec_bytes,
    })
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EncodeSummary {
    pub epoch: u32,
    pub width: u16,
    pub height: u16,
    pub selected_tiles: usize,
    pub dirty_tiles: usize,
    pub refresh_tiles: usize,
    pub source_tiles_compared: usize,
    pub reference_cloned_bytes: usize,
    pub emitted_body_bytes: usize,
    pub refresh_cursor_before: usize,
    pub refresh_cursor_after: usize,
    pub epoch_changed: bool,
}

struct VisitOptions {
    source_is_new: bool,
    quota: usize,
    force_all: bool,
    legacy: bool,
}

/// One exact previous input is retained for dirty detection. Legacy encode()
/// preserves the measured 1168-stride stream. The live visitor separates new
/// capture-generation differences from independent current-state refresh ticks.
pub struct Encoder {
    policy: u8,
    payload: usize,
    memory_limit: usize,
    pub stats: Stats,
    previous: Option<Frame>,
    epoch: u32,
    refresh_cursor: usize,
    legacy_refresh_cursor: usize,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new(1, 0).expect("valid default policy")
    }
}

impl Encoder {
    pub fn new(initial_epoch: u32, policy: u8) -> Result<Self> {
        Self::with_payload(initial_epoch, policy, PAYLOAD)
    }
    pub fn with_payload(initial_epoch: u32, policy: u8, payload: usize) -> Result<Self> {
        Self::with_memory_limit(initial_epoch, policy, payload, MAX_CODEC_BYTES)
    }
    pub fn with_memory_limit(
        initial_epoch: u32,
        policy: u8,
        payload: usize,
        memory_limit: usize,
    ) -> Result<Self> {
        ensure!(policy <= 2, "unknown requested transform policy");
        validate_payload(payload)?;
        ensure!(
            memory_limit > 0 && memory_limit <= MAX_CODEC_BYTES,
            "invalid codec memory limit"
        );
        Ok(Self {
            policy,
            payload,
            memory_limit,
            stats: Stats::default(),
            previous: None,
            epoch: initial_epoch,
            refresh_cursor: 0,
            legacy_refresh_cursor: 0,
        })
    }
    pub fn epoch(&self) -> u32 {
        self.epoch
    }
    pub fn dimensions(&self) -> Option<(u16, u16)> {
        self.previous.as_ref().map(|p| (p.width, p.height))
    }
    pub fn payload(&self) -> usize {
        self.payload
    }
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.previous.as_ref().map_or(0, |p| p.pixels.capacity())
    }
    pub fn refresh_cursor(&self) -> usize {
        self.refresh_cursor
    }
    pub fn memory_estimate(&self, width: u16, height: u16) -> Result<MemoryEstimate> {
        memory_estimate(
            width,
            height,
            self.previous.as_ref().map_or(0, |p| p.pixels.capacity()),
        )
    }
    pub fn encode(&mut self, frame: &Frame, sequence: u32) -> Result<Vec<Vec<u8>>> {
        dimensions(frame.width, frame.height)?;
        ensure!(frame.is_valid(), "invalid RGBA frame");
        let retained = self.previous.as_ref().map_or(0, |p| p.pixels.capacity());
        let estimate = memory_estimate(frame.width, frame.height, retained)?;
        // Legacy materializes packet vectors for compatibility. Live callers
        // use the visitor instead. Account this bounded complete-frame batch.
        let output_reserve = frame.pixels.len()
            + tile_count(frame.width, frame.height)
                * 4
                * (HEADER * 2 + std::mem::size_of::<Vec<u8>>() * 2);
        let input_excess_capacity = frame.pixels.capacity() - frame.pixels.len();
        ensure!(
            estimate
                .total_codec_bytes
                .checked_add(output_reserve)
                .and_then(|bytes| bytes.checked_add(input_excess_capacity))
                .is_some_and(|bytes| bytes <= self.memory_limit),
            "legacy packet batch exceeds codec memory budget"
        );
        let quota = tile_count(frame.width, frame.height).div_ceil(REFRESH_STEPS);
        let mut packets = Vec::new();
        let payload = self.payload;
        self.visit(
            frame,
            sequence,
            VisitOptions {
                source_is_new: true,
                quota,
                force_all: false,
                legacy: true,
            },
            |tile| {
                packets.extend(tile.fragments(payload)?);
                Ok(())
            },
        )?;
        Ok(packets)
    }
    /// source_is_new is a trusted native capture-generation hint. False means
    /// pixels are the same immutable current capture. Idle refresh does no full
    /// scan or clone. Emit consumes/drops one complete tile before returning;
    /// retaining updates belongs to the caller's bounded pending queue.
    /// On emit error, close or resync before continuing: already-emitted updates
    /// cannot be undone. No unbounded completed-tile batch is retained here.
    pub fn encode_with_refresh_budget<F>(
        &mut self,
        frame: &Frame,
        sequence: u32,
        source_is_new: bool,
        refresh_quota: usize,
        force_all: bool,
        emit: F,
    ) -> Result<EncodeSummary>
    where
        F: FnMut(EncodedTile) -> Result<()>,
    {
        self.visit(
            frame,
            sequence,
            VisitOptions {
                source_is_new,
                quota: refresh_quota,
                force_all,
                legacy: false,
            },
            emit,
        )
    }
    fn visit<F>(
        &mut self,
        frame: &Frame,
        sequence: u32,
        options: VisitOptions,
        mut emit: F,
    ) -> Result<EncodeSummary>
    where
        F: FnMut(EncodedTile) -> Result<()>,
    {
        let VisitOptions {
            source_is_new,
            quota,
            force_all,
            legacy,
        } = options;
        // Complete validation and reserve accounting precede output/state
        // changes and all framebuffer cloning/allocation.
        dimensions(frame.width, frame.height)?;
        ensure!(frame.is_valid(), "invalid RGBA frame");
        let before_bytes = self.previous.as_ref().map_or(0, |p| p.pixels.capacity());
        let estimate = memory_estimate(frame.width, frame.height, before_bytes)?;
        let input_excess_capacity = frame.pixels.capacity() - frame.pixels.len();
        ensure!(
            estimate
                .total_codec_bytes
                .checked_add(input_excess_capacity)
                .is_some_and(|bytes| bytes <= self.memory_limit),
            "encoder state exceeds codec memory budget"
        );
        let count = tile_count(frame.width, frame.height);
        let resized = self
            .previous
            .as_ref()
            .is_some_and(|p| (p.width, p.height) != (frame.width, frame.height));
        let first = self.previous.is_none();
        let epoch_changed = resized || force_all;
        let next_epoch = if epoch_changed {
            self.epoch.wrapping_add(1)
        } else {
            self.epoch
        };
        let cursor = if resized {
            0
        } else if legacy {
            self.legacy_refresh_cursor
        } else {
            self.refresh_cursor
        };
        let quota = quota.min(count);
        let all = first || resized || force_all;
        let mut summary = EncodeSummary {
            epoch: next_epoch,
            width: frame.width,
            height: frame.height,
            refresh_cursor_before: cursor,
            epoch_changed,
            ..EncodeSummary::default()
        };
        // Reserve a fresh epoch before emitting any of its tiles. A failed
        // callback followed by an explicit resync must not reuse that epoch.
        // Input/geometry/memory validation has already completed above.
        self.epoch = next_epoch;
        let indices: Box<dyn Iterator<Item = usize>> = if source_is_new || all {
            Box::new(0..count)
        } else {
            Box::new((0..quota).map(move |offset| (cursor + offset) % count))
        };
        for tile in indices {
            let refresh = (tile + count - cursor) % count < quota;
            let dirty = if all {
                true
            } else if source_is_new {
                summary.source_tiles_compared += 1;
                !equal_tile(frame, self.previous.as_ref().unwrap(), tile)
            } else {
                false
            };
            if !dirty && !refresh {
                continue;
            }
            summary.dirty_tiles += usize::from(dirty);
            summary.refresh_tiles += usize::from(refresh);
            let raw = extract(frame, tile);
            let (_, _, width, height) = rect(frame.width, frame.height, tile);
            let (kind, policy, body) = select(&raw, width, height, self.policy, &mut self.stats)?;
            // Pending-body budgets count these owned allocations, rather than
            // leaving spare zlib Vec capacity uncharged in the caller's queue.
            let body = body.into_boxed_slice().into_vec();
            summary.emitted_body_bytes += body.len();
            summary.selected_tiles += 1;
            emit(EncodedTile {
                epoch: next_epoch,
                sequence,
                width: frame.width,
                height: frame.height,
                tile,
                kind,
                policy,
                raw: raw.len(),
                body,
            })?;
        }
        // Drop the old reference before cloning its replacement; no transient
        // second retained reference is required for resize or resync.
        if source_is_new || all {
            self.previous = None;
            self.previous = Some(frame.clone());
            summary.reference_cloned_bytes = frame.pixels.len();
        }
        self.epoch = next_epoch;
        if resized {
            self.refresh_cursor = 0;
            self.legacy_refresh_cursor = 0;
        }
        let after = (cursor + quota) % count;
        if legacy {
            self.legacy_refresh_cursor = after;
        } else {
            self.refresh_cursor = after;
        }
        summary.refresh_cursor_after = after;
        Ok(summary)
    }
}

#[derive(Clone, Debug)]
pub struct Header {
    pub epoch: u32,
    pub sequence: u32,
    pub width: u16,
    pub height: u16,
    pub tile: usize,
    pub kind: u8,
    pub total: usize,
    pub index: usize,
    pub count: usize,
    pub offset: usize,
    pub raw: usize,
    pub policy: u8,
}

pub fn parse(packet: &[u8]) -> Result<Header> {
    parse_with_payload(packet, PAYLOAD)
}

pub fn parse_with_payload(packet: &[u8], payload: usize) -> Result<Header> {
    validate_payload(payload)?;
    ensure!(
        (HEADER + 1..=HEADER + payload).contains(&packet.len()),
        "invalid datagram size"
    );
    ensure!(&packet[..4] == b"GLT2", "invalid tile magic");
    let u16_at = |offset| u16::from_le_bytes(packet[offset..offset + 2].try_into().unwrap());
    let u32_at = |offset| u32::from_le_bytes(packet[offset..offset + 4].try_into().unwrap());
    let header = Header {
        epoch: u32_at(4),
        sequence: u32_at(8),
        width: u16_at(12),
        height: u16_at(14),
        tile: usize::from(u16_at(16)),
        kind: packet[19],
        total: usize::from(u16_at(20)),
        index: usize::from(packet[22]),
        count: usize::from(packet[23]),
        offset: usize::from(u16_at(24)),
        raw: usize::from(u16_at(28)),
        policy: packet[30],
    };
    dimensions(header.width, header.height)?;
    ensure!(
        packet[18] == TILE as u8
            && header.kind <= 2
            && header.policy <= 2
            && packet[31] == 0
            && (header.kind == 2 || header.policy == 0),
        "unsupported flags"
    );
    ensure!(
        header.tile < tile_count(header.width, header.height),
        "tile outside geometry"
    );
    let (_, _, width, height) = rect(header.width, header.height, header.tile);
    ensure!(header.raw == width * height * 4, "invalid raw tile length");
    ensure!(
        header.total > 0
            && header.total <= header.raw
            && (header.kind != 0 || header.total == header.raw),
        "invalid encoded length"
    );
    ensure!(
        header.count == header.total.div_ceil(payload)
            && (1..=4).contains(&header.count)
            && header.index < header.count,
        "invalid fragment count/index"
    );
    ensure!(
        header.offset == header.index * payload,
        "noncanonical fragment offset"
    );
    ensure!(
        usize::from(u16_at(26)) == payload.min(header.total - header.offset)
            && packet.len() == HEADER + usize::from(u16_at(26)),
        "invalid fragment length"
    );
    Ok(header)
}

struct Assembly {
    header: Header,
    body: Vec<u8>,
    received: u8,
    first_step: u64,
}

/// Receiver applies complete independent tiles without waiting for a whole frame.
pub struct Decoder {
    payload: usize,
    memory_limit: usize,
    pub frame: Option<Frame>,
    epoch: Option<u32>,
    sequences: Vec<Option<u32>>,
    assemblies: BTreeMap<(u32, usize), Assembly>,
    assembly_bytes: usize,
    pub applied_tiles: u64,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::with_payload(PAYLOAD).expect("valid default stride")
    }
    pub fn with_payload(payload: usize) -> Result<Self> {
        Self::with_memory_limit(payload, MAX_CODEC_BYTES)
    }
    pub fn with_memory_limit(payload: usize, memory_limit: usize) -> Result<Self> {
        validate_payload(payload)?;
        ensure!(
            memory_limit > 0 && memory_limit <= MAX_CODEC_BYTES,
            "invalid codec memory limit"
        );
        Ok(Self {
            payload,
            memory_limit,
            frame: None,
            epoch: None,
            sequences: Vec::new(),
            assemblies: BTreeMap::new(),
            assembly_bytes: 0,
            applied_tiles: 0,
        })
    }
    pub fn epoch(&self) -> Option<u32> {
        self.epoch
    }
    pub fn dimensions(&self) -> Option<(u16, u16)> {
        self.frame.as_ref().map(|f| (f.width, f.height))
    }
    pub fn payload(&self) -> usize {
        self.payload
    }
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.frame.as_ref().map_or(0, |f| f.pixels.capacity())
            + self.sequences.capacity() * std::mem::size_of::<Option<u32>>()
            + self
                .assemblies
                .values()
                .map(|a| a.body.capacity() + std::mem::size_of::<Assembly>() + 128)
                .sum::<usize>()
    }
    pub fn sequences(&self) -> &[Option<u32>] {
        &self.sequences
    }
    pub fn assembly_usage(&self) -> (usize, usize) {
        (self.assemblies.len(), self.assembly_bytes)
    }
    pub fn advance(&mut self, step: u64) {
        let expired: Vec<_> = self
            .assemblies
            .iter()
            .filter(|(_, a)| step.saturating_sub(a.first_step) > EXPIRY_STEPS)
            .map(|(tile, _)| *tile)
            .collect();
        for tile in expired {
            self.remove(tile);
        }
    }
    fn remove(&mut self, tile: (u32, usize)) -> Option<Assembly> {
        let assembly = self.assemblies.remove(&tile)?;
        self.assembly_bytes -= assembly.body.len();
        Some(assembly)
    }
    pub fn ingest(&mut self, packet: &[u8], step: u64) -> Result<bool> {
        let header = parse_with_payload(packet, self.payload)?;
        let prospective_frame_bytes = usize::from(header.width) * usize::from(header.height) * 4;
        let retained_frame_bytes = self.frame.as_ref().map_or(0, |f| f.pixels.capacity());
        let frame_reserve = if self.epoch == Some(header.epoch) {
            0
        } else {
            prospective_frame_bytes
        };
        let retained_sequence_bytes =
            self.sequences.capacity() * std::mem::size_of::<Option<u32>>();
        let prospective_sequence_bytes = if self.epoch == Some(header.epoch) {
            0
        } else {
            tile_count(header.width, header.height) * std::mem::size_of::<Option<u32>>()
        };
        ensure!(
            retained_frame_bytes
                + frame_reserve
                + MAX_ASSEMBLY_BYTES
                + retained_sequence_bytes
                + prospective_sequence_bytes
                + MAX_ASSEMBLIES * (std::mem::size_of::<Assembly>() + 128)
                + ACTIVE_TILE_RESERVE_BYTES
                <= self.memory_limit,
            "decoder state exceeds codec memory budget"
        );
        self.advance(step);
        if let Some(epoch) = self.epoch {
            if header.epoch != epoch && !newer(header.epoch, epoch) {
                return Ok(false);
            }
            if header.epoch == epoch {
                let frame = self.frame.as_ref().unwrap();
                ensure!(
                    (frame.width, frame.height) == (header.width, header.height),
                    "geometry changed without epoch"
                );
            }
        }
        // Future epochs are staged within the same bounded assembly pool. A
        // complete, validated tile is required before replacing committed pixels.
        ensure!(
            self.assemblies
                .values()
                .all(|a| a.header.epoch != header.epoch
                    || (a.header.width, a.header.height) == (header.width, header.height)),
            "conflicting staged epoch geometry"
        );
        if self.epoch == Some(header.epoch)
            && self.sequences[header.tile].is_some_and(|sequence| !newer(header.sequence, sequence))
        {
            return Ok(false);
        }
        let key = (header.epoch, header.tile);
        if let Some(old) = self.assemblies.get(&key) {
            if old.header.sequence != header.sequence {
                if !newer(header.sequence, old.header.sequence) {
                    return Ok(false);
                }
                self.remove(key);
            } else {
                ensure!(
                    (
                        old.header.kind,
                        old.header.total,
                        old.header.raw,
                        old.header.policy
                    ) == (header.kind, header.total, header.raw, header.policy),
                    "conflicting tile fragments"
                );
            }
        }
        if !self.assemblies.contains_key(&key) {
            while self.assemblies.len() >= MAX_ASSEMBLIES
                || self.assembly_bytes + header.total > MAX_ASSEMBLY_BYTES
            {
                let oldest = self
                    .assemblies
                    .iter()
                    .min_by_key(|(tile, a)| (a.first_step, **tile))
                    .map(|(tile, _)| *tile);
                if let Some(tile) = oldest {
                    self.remove(tile);
                } else {
                    break;
                }
            }
            self.assembly_bytes += header.total;
            self.assemblies.insert(
                key,
                Assembly {
                    body: vec![0; header.total],
                    received: 0,
                    first_step: step,
                    header: header.clone(),
                },
            );
        }
        let assembly = self.assemblies.get_mut(&key).unwrap();
        let bit = 1 << header.index;
        let range = header.offset..header.offset + packet.len() - HEADER;
        if assembly.received & bit != 0 {
            ensure!(
                assembly.body[range] == packet[HEADER..],
                "conflicting duplicate fragment"
            );
            return Ok(false);
        }
        assembly.body[range].copy_from_slice(&packet[HEADER..]);
        assembly.received |= bit;
        if assembly.received != (1 << header.count) - 1 {
            return Ok(false);
        }
        let assembly = self.remove(key).unwrap();
        let mut raw = if header.kind == 0 {
            assembly.body
        } else {
            let mut raw = vec![0; header.raw + 1];
            let mut inflater = Decompress::new(true);
            let status = inflater.decompress(&assembly.body, &mut raw, FlushDecompress::Finish)?;
            ensure!(
                status == Status::StreamEnd
                    && inflater.total_in() == header.total as u64
                    && inflater.total_out() == header.raw as u64,
                "invalid or oversized zlib tile"
            );
            raw.truncate(header.raw);
            raw
        };
        if header.kind == 2 {
            let (_, _, width, height) = rect(header.width, header.height, header.tile);
            raw = inverse(&raw, width, height, header.policy)?;
        }
        if self.epoch != Some(header.epoch) {
            // Allocation and state replacement happen only after decompression
            // and exact output-length validation succeeded.
            self.frame = Frame::new(header.width, header.height);
            self.epoch = Some(header.epoch);
            self.sequences = vec![None; tile_count(header.width, header.height)];
            self.assemblies
                .retain(|(epoch, _), _| *epoch == header.epoch);
            self.assembly_bytes = self.assemblies.values().map(|a| a.body.len()).sum();
        }
        let frame = self.frame.as_mut().unwrap();
        let (x, y, width, height) = rect(frame.width, frame.height, header.tile);
        let stride = usize::from(frame.width) * 4;
        for row in 0..height {
            frame.pixels[(y + row) * stride + x * 4..(y + row) * stride + (x + width) * 4]
                .copy_from_slice(&raw[row * width * 4..(row + 1) * width * 4]);
        }
        self.sequences[header.tile] = Some(header.sequence);
        self.applied_tiles += 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
