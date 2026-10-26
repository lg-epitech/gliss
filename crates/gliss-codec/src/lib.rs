//! Lossless CPU-only desktop tiles.
use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use gliss_core::{Frame, MAX_PIXELS};
use std::{borrow::Cow, collections::HashMap, io::Write, str::FromStr};

pub const HEADER_BYTES: usize = 24;
pub const RECT_HEADER_BYTES: usize = 16;
pub const MAX_PACKET_BYTES: usize = MAX_PIXELS * 5 + 1_048_576;
pub const MAX_BATCH_BODY_BYTES: usize = MAX_PIXELS * 4 + u16::MAX as usize * RECT_HEADER_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Raw,
    Deflate,
    Palette,
    Xor,
    Copy,
    Adaptive,
    Batch,
    Global,
    Sparse,
    GlobalCached,
}
impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Deflate => "deflate",
            Self::Palette => "palette",
            Self::Xor => "xor",
            Self::Copy => "copy",
            Self::Adaptive => "adaptive",
            Self::Batch => "batch",
            Self::Global => "global",
            Self::Sparse => "sparse",
            Self::GlobalCached => "global-cached",
        }
    }
}
impl FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "raw" => Ok(Self::Raw),
            "deflate" => Ok(Self::Deflate),
            "palette" => Ok(Self::Palette),
            "xor" => Ok(Self::Xor),
            "copy" => Ok(Self::Copy),
            "adaptive" | "desktop" => Ok(Self::Adaptive),
            "batch" => Ok(Self::Batch),
            "global" => Ok(Self::Global),
            "sparse" => Ok(Self::Sparse),
            "global-cached" => Ok(Self::GlobalCached),
            _ => bail!("unknown codec mode: {s}"),
        }
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}
impl Rect {
    fn area(self) -> usize {
        usize::from(self.w) * usize::from(self.h)
    }
    fn inside(self, width: u16, height: u16) -> bool {
        self.w > 0
            && self.h > 0
            && u32::from(self.x) + u32::from(self.w) <= u32::from(width)
            && u32::from(self.y) + u32::from(self.h) <= u32::from(height)
    }
}

pub struct Encoder {
    mode: Mode,
    tile_size: u16,
    previous: Option<Frame>,
    sequence: Option<u32>,
    copy_hashes: Option<FrameHashes>,
}
impl Encoder {
    pub fn new(mode: Mode, tile_size: u16) -> Result<Self> {
        ensure!(
            (16..=256).contains(&tile_size),
            "tile size must be 16..=256"
        );
        Ok(Self {
            mode,
            tile_size,
            previous: None,
            sequence: None,
            copy_hashes: None,
        })
    }
    /// Force a full independent keyframe on the next encode (for reconnect/recovery).
    pub fn reset(&mut self) {
        self.previous = None;
        self.sequence = None;
        self.copy_hashes = None;
    }
    pub fn encode(&mut self, frame: &Frame, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        ensure!(frame.is_valid(), "invalid input frame");
        let columns = usize::from(frame.width).div_ceil(usize::from(self.tile_size));
        let rows = usize::from(frame.height).div_ceil(usize::from(self.tile_size));
        ensure!(
            columns * rows <= usize::from(u16::MAX),
            "too many tiles; increase tile size"
        );
        let keyframe = self
            .previous
            .as_ref()
            .is_none_or(|p| p.width != frame.width || p.height != frame.height)
            || self.sequence.is_none_or(|s| s.wrapping_add(1) != sequence);
        let previous = if keyframe {
            None
        } else {
            self.previous.as_ref()
        };
        let tiles = tile_rects(frame.width, frame.height, self.tile_size);
        let dirty: Vec<_> = tiles
            .iter()
            .copied()
            .filter(|r| previous.is_none_or(|p| !rect_equal(frame, *r, p, *r)))
            .collect();
        // Copy search is bounded to a tile index and four full-width vertical shift candidates.
        let copy_worth_trying = self.mode == Mode::Copy
            || (matches!(
                self.mode,
                Mode::Adaptive | Mode::Batch | Mode::Global | Mode::Sparse | Mode::GlobalCached
            ) && dirty.len() >= 4
                && previous.is_some_and(|p| temporal_similarity(frame, p, &dirty)));
        // This opt-in cache belongs to the owned previous-frame clone from one
        // successful encode. Caller buffer addresses and hashes never establish
        // frame identity. Keep existing wrap semantics but start its cache cold.
        let old_hashes = previous.and_then(|p| {
            self.copy_hashes.as_ref().filter(|hashes| {
                !(self.sequence == Some(u32::MAX) && sequence == 0)
                    && hashes.matches(p, self.sequence, self.tile_size, tiles.len())
            })
        });
        let dirty_tiles = (self.mode == Mode::GlobalCached).then(|| {
            let mut mask = vec![false; tiles.len()];
            for rect in &dirty {
                mask[usize::from(rect.y / self.tile_size) * columns
                    + usize::from(rect.x / self.tile_size)] = true;
            }
            mask
        });
        let mut next_hashes = dirty_tiles
            .as_ref()
            .map(|mask| FrameHashes::next(frame, sequence, self.tile_size, mask, old_hashes));
        let copies = if !dirty.is_empty() && copy_worth_trying {
            previous.map(|p| {
                if let (Some(mask), Some(next)) = (&dirty_tiles, &mut next_hashes) {
                    CopyIndex::new_cached(p, &tiles, frame, old_hashes, mask, next)
                } else {
                    CopyIndex::new(p, &tiles, frame)
                }
            })
        } else {
            None
        };
        let mut packet = Vec::with_capacity(HEADER_BYTES + dirty.len() * RECT_HEADER_BYTES);
        packet.extend_from_slice(b"GLS1");
        put32(&mut packet, sequence);
        put32(&mut packet, input_id);
        put16(&mut packet, frame.width);
        put16(&mut packet, frame.height);
        put16(&mut packet, dirty.len() as u16);
        put16(&mut packet, u16::from(keyframe));
        put32(&mut packet, 0);
        for rect in dirty {
            let raw = extract(frame, rect);
            let (codec, body) = if let (Some(index), Some(p)) = (&copies, previous) {
                let source = if let Some(next) = &mut next_hashes {
                    let tile = next.tile_index(rect);
                    index.find_cached(frame, rect, p, &mut next.tiles[tile])
                } else {
                    index.find(frame, rect, p)
                };
                if let Some(source) = source {
                    let mut body = Vec::with_capacity(4);
                    put16(&mut body, source.x);
                    put16(&mut body, source.y);
                    (5, body)
                } else {
                    self.encode_pixels(raw, rect, previous)?
                }
            } else {
                self.encode_pixels(raw, rect, previous)?
            };
            put16(&mut packet, rect.x);
            put16(&mut packet, rect.y);
            put16(&mut packet, rect.w);
            put16(&mut packet, rect.h);
            packet.push(codec);
            packet.extend_from_slice(&[0; 3]);
            put32(&mut packet, body.len() as u32);
            packet.extend_from_slice(&body);
        }
        if matches!(
            self.mode,
            Mode::Batch | Mode::Global | Mode::Sparse | Mode::GlobalCached
        ) {
            let expanded_len = packet.len() - HEADER_BYTES;
            ensure!(
                expanded_len <= MAX_BATCH_BODY_BYTES,
                "batch body exceeds limit"
            );
            let packed = compress(&packet[HEADER_BYTES..])?;
            packet.truncate(HEADER_BYTES);
            packet[..4].copy_from_slice(match self.mode {
                Mode::Sparse => b"GLB3",
                Mode::Global | Mode::GlobalCached => b"GLB2",
                _ => b"GLB1",
            });
            packet[20..24].copy_from_slice(&(expanded_len as u32).to_le_bytes());
            packet.extend_from_slice(&packed);
        }
        ensure!(
            packet.len() <= MAX_PACKET_BYTES,
            "encoded packet exceeds limit"
        );
        self.previous = Some(frame.clone());
        self.sequence = Some(sequence);
        self.copy_hashes = next_hashes;
        Ok(packet)
    }
    fn encode_pixels(
        &self,
        raw: Vec<u8>,
        rect: Rect,
        previous: Option<&Frame>,
    ) -> Result<(u8, Vec<u8>)> {
        if self.mode == Mode::Sparse
            && let Some(previous) = previous
            && let Some(patch) = sparse_patch(&raw, rect, previous)
        {
            return Ok((8, patch));
        }
        if matches!(self.mode, Mode::Global | Mode::Sparse | Mode::GlobalCached) {
            return Ok(global_transform(raw, rect, previous));
        }
        if matches!(self.mode, Mode::Raw | Mode::Batch) {
            return Ok((0, raw));
        }
        let zipped = compress(&raw)?;
        let mut best = if zipped.len() < raw.len() {
            (1, zipped)
        } else {
            (0, raw.clone())
        };
        if matches!(self.mode, Mode::Palette | Mode::Adaptive)
            && let Some(palette) = palette_encode(&raw)
        {
            let zipped = compress(&palette)?;
            if palette.len() < best.1.len() {
                best = (2, palette);
            }
            if zipped.len() < best.1.len() {
                best = (3, zipped);
            }
        }
        if matches!(self.mode, Mode::Xor | Mode::Adaptive)
            && let Some(p) = previous
        {
            let old = extract(p, rect);
            // Experimental XOR mode always tries the predictor; adaptive skips uncorrelated RGB.
            if self.mode == Mode::Adaptive && !rgb_similarity(&raw, &old) {
                return Ok(best);
            }
            let xor: Vec<_> = raw.iter().zip(old).map(|(a, b)| a ^ b).collect();
            let zipped = compress(&xor)?;
            if zipped.len() < best.1.len() {
                best = (4, zipped);
            }
        }
        Ok(best)
    }
}

pub struct Decoder {
    previous: Option<Frame>,
    sequence: Option<u32>,
    input_id: u32,
}
impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}
impl Decoder {
    pub fn new() -> Self {
        Self {
            previous: None,
            sequence: None,
            input_id: 0,
        }
    }
    pub fn reset(&mut self) {
        self.previous = None;
        self.sequence = None;
        self.input_id = 0;
    }
    pub fn sequence(&self) -> Option<u32> {
        self.sequence
    }
    pub fn input_id(&self) -> u32 {
        self.input_id
    }
    /// Errors leave prior pixels and sequence untouched. A keyframe recovers from a missing delta.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Frame> {
        let expanded = expand_batch_packet(packet)?;
        let packet = expanded.as_ref();
        ensure!(
            packet.len() >= HEADER_BYTES && packet.len() <= MAX_PACKET_BYTES,
            "invalid packet length"
        );
        let sparse = &packet[..4] == b"GLB3";
        let global = sparse || &packet[..4] == b"GLB2";
        ensure!(global || &packet[..4] == b"GLS1", "invalid magic");
        let sequence = get32(packet, 4);
        let input_id = get32(packet, 8);
        let width = get16(packet, 12);
        let height = get16(packet, 14);
        let count = usize::from(get16(packet, 16));
        let flags = get16(packet, 18);
        ensure!(
            flags & !1 == 0 && get32(packet, 20) == 0,
            "unsupported flags or reserved header"
        );
        let keyframe = flags & 1 != 0;
        let area = usize::from(width) * usize::from(height);
        ensure!(area > 0 && area <= MAX_PIXELS, "invalid frame dimensions");
        if !keyframe {
            let p = self.previous.as_ref().context("delta needs a keyframe")?;
            ensure!(
                p.width == width && p.height == height,
                "delta changed dimensions"
            );
            ensure!(
                self.sequence.is_some_and(|s| s.wrapping_add(1) == sequence),
                "out-of-order delta; request a keyframe"
            );
        }
        ensure!(
            count <= (packet.len() - HEADER_BYTES) / RECT_HEADER_BYTES,
            "truncated rectangle headers"
        );
        // Validate every rectangle and bounded payload before allocating the output frame.
        let mut offset = HEADER_BYTES;
        let mut rects = Vec::with_capacity(count);
        for _ in 0..count {
            ensure!(
                packet.len() - offset >= RECT_HEADER_BYTES,
                "truncated rectangle header"
            );
            let rect = Rect {
                x: get16(packet, offset),
                y: get16(packet, offset + 2),
                w: get16(packet, offset + 4),
                h: get16(packet, offset + 6),
            };
            ensure!(rect.inside(width, height), "rectangle outside frame");
            let codec = packet[offset + 8];
            ensure!(
                packet[offset + 9..offset + 12] == [0; 3],
                "nonzero rectangle reserved bytes"
            );
            ensure!(
                (if global {
                    matches!(codec, 0 | 2 | 5 | 6 | 7) || sparse && codec == 8
                } else {
                    codec <= 5
                }) && !(keyframe && matches!(codec, 4 | 5 | 7 | 8)),
                "invalid codec for packet"
            );
            let len = get32(packet, offset + 12) as usize;
            offset += RECT_HEADER_BYTES;
            ensure!(len <= packet.len() - offset, "truncated rectangle payload");
            if global {
                ensure!(
                    len <= rect.area() * 4,
                    "global transform exceeds raw tile size"
                );
            }
            if matches!(codec, 0 | 7) {
                ensure!(len == rect.area() * 4, "wrong raw length");
            }
            if codec == 6 {
                ensure!(len == 4, "wrong solid length");
            }
            if global && codec == 2 {
                validate_global_palette(&packet[offset..offset + len], rect.area())?;
            }
            if codec == 8 {
                validate_sparse_patch(&packet[offset..offset + len], rect.area())?;
            }
            if codec == 5 {
                ensure!(len == 4, "wrong copy length");
                let source = Rect {
                    x: get16(packet, offset),
                    y: get16(packet, offset + 2),
                    ..rect
                };
                ensure!(source.inside(width, height), "copy source outside frame");
            }
            rects.push((rect, codec, &packet[offset..offset + len]));
            offset += len;
        }
        ensure!(offset == packet.len(), "trailing packet bytes");
        let mut covered = vec![0u8; area.div_ceil(8)];
        let mut coverage = 0;
        for (r, _, _) in &rects {
            for y in usize::from(r.y)..usize::from(r.y) + usize::from(r.h) {
                for x in usize::from(r.x)..usize::from(r.x) + usize::from(r.w) {
                    let i = y * usize::from(width) + x;
                    let mask = 1 << (i % 8);
                    ensure!(covered[i / 8] & mask == 0, "overlapping rectangles");
                    covered[i / 8] |= mask;
                    coverage += 1;
                }
            }
        }
        ensure!(
            !keyframe || coverage == area,
            "keyframe does not cover the frame"
        );
        let previous = self.previous.as_ref();
        let mut next = if keyframe {
            Frame {
                width,
                height,
                pixels: vec![0; area * 4],
            }
        } else {
            previous.unwrap().clone()
        };
        for (rect, codec, payload) in rects {
            if codec == 8 {
                // Unlisted pixels already come from the immutable previous
                // frame. Only changed values are copied into private output.
                for entry in payload[2..].as_chunks::<6>().0 {
                    let index = usize::from(get16(entry, 0));
                    let x = usize::from(rect.x) + index % usize::from(rect.w);
                    let y = usize::from(rect.y) + index / usize::from(rect.w);
                    let offset = (y * usize::from(width) + x) * 4;
                    next.pixels[offset..offset + 4].copy_from_slice(&entry[2..6]);
                }
                continue;
            }
            let pixels = match codec {
                0 => payload.to_vec(),
                1 => decompress(payload, rect.area() * 4, Some(rect.area() * 4))?,
                2 => palette_decode(payload, rect.area())?,
                3 => {
                    let body = decompress(payload, 2 + 256 * 4 + rect.area(), None)?;
                    palette_decode(&body, rect.area())?
                }
                4 => {
                    let mut xor = decompress(payload, rect.area() * 4, Some(rect.area() * 4))?;
                    let old = extract(previous.unwrap(), rect);
                    for (a, b) in xor.iter_mut().zip(old) {
                        *a ^= b;
                    }
                    xor
                }
                5 => extract(
                    previous.unwrap(),
                    Rect {
                        x: get16(payload, 0),
                        y: get16(payload, 2),
                        ..rect
                    },
                ),
                6 => payload.repeat(rect.area()),
                7 => {
                    let old = extract(previous.unwrap(), rect);
                    payload.iter().zip(old).map(|(a, b)| a ^ b).collect()
                }
                _ => unreachable!(),
            };
            insert(&mut next, rect, &pixels);
        }
        self.previous = Some(next.clone());
        self.sequence = Some(sequence);
        self.input_id = input_id;
        Ok(next)
    }
}

/// Expand a GLB1/GLB2/GLB3 envelope once; existing GLS1 packets stay borrowed.
///
/// GLB1 becomes GLS1 and retains its original raw/copy-only contract. GLB2 keeps
/// its version marker in the expanded buffer for context-specific transforms;
/// it permits raw, palette, copy, solid and raw XOR. Neither permits nested zlib.
/// This helper validates bounded framing; Decoder validates pixels/atomic state.
pub fn expand_batch_packet(packet: &[u8]) -> Result<Cow<'_, [u8]>> {
    ensure!(
        packet.len() >= HEADER_BYTES && packet.len() <= MAX_PACKET_BYTES,
        "invalid packet length"
    );
    if &packet[..4] == b"GLS1" {
        return Ok(Cow::Borrowed(packet));
    }
    let sparse = &packet[..4] == b"GLB3";
    let global = sparse || &packet[..4] == b"GLB2";
    ensure!(global || &packet[..4] == b"GLB1", "invalid magic");
    let width = get16(packet, 12);
    let height = get16(packet, 14);
    let area = usize::from(width) * usize::from(height);
    ensure!(
        area > 0 && area <= MAX_PIXELS,
        "invalid batch frame dimensions"
    );
    ensure!(get16(packet, 18) & !1 == 0, "unsupported batch flags");
    let count = usize::from(get16(packet, 16));
    let expanded_len = get32(packet, 20) as usize;
    ensure!(
        expanded_len <= MAX_BATCH_BODY_BYTES
            && expanded_len <= area * 4 + count * RECT_HEADER_BYTES,
        "advertised batch body exceeds limit"
    );
    ensure!(
        count <= expanded_len / RECT_HEADER_BYTES,
        "truncated batch rectangle headers"
    );
    let body = decompress(&packet[HEADER_BYTES..], expanded_len, Some(expanded_len))?;
    let mut offset = 0;
    for _ in 0..count {
        ensure!(
            body.len() - offset >= RECT_HEADER_BYTES,
            "truncated batch rectangle header"
        );
        ensure!(
            if global {
                matches!(body[offset + 8], 0 | 2 | 5 | 6 | 7) || sparse && body[offset + 8] == 8
            } else {
                matches!(body[offset + 8], 0 | 5)
            },
            "nested or unsupported batch rectangle codec"
        );
        let length = get32(&body, offset + 12) as usize;
        offset += RECT_HEADER_BYTES;
        ensure!(
            length <= body.len() - offset,
            "truncated batch rectangle payload"
        );
        offset += length;
    }
    ensure!(offset == body.len(), "trailing batch body bytes");
    let mut expanded = Vec::with_capacity(HEADER_BYTES + body.len());
    expanded.extend_from_slice(&packet[..HEADER_BYTES]);
    if !global {
        expanded[..4].copy_from_slice(b"GLS1");
    }
    expanded[20..24].fill(0);
    expanded.extend_from_slice(&body);
    Ok(Cow::Owned(expanded))
}

fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}
fn decompress(bytes: &[u8], limit: usize, exact: Option<usize>) -> Result<Vec<u8>> {
    let mut decoder = Decompress::new(true);
    let mut output = vec![0; limit + 1];
    let status = decoder
        .decompress(bytes, &mut output, FlushDecompress::Finish)
        .context("invalid zlib payload")?;
    let written = decoder.total_out() as usize;
    ensure!(written <= limit, "decompressed payload exceeds limit");
    ensure!(
        status == Status::StreamEnd,
        "incomplete or oversized zlib payload"
    );
    ensure!(
        exact.is_none_or(|len| len == written),
        "wrong decompressed length"
    );
    ensure!(
        decoder.total_in() as usize == bytes.len(),
        "trailing zlib bytes"
    );
    output.truncate(written);
    Ok(output)
}

fn palette_encode(raw: &[u8]) -> Option<Vec<u8>> {
    let mut map = HashMap::new();
    let mut palette = Vec::new();
    let mut indices = Vec::with_capacity(raw.len() / 4);
    for pixel in raw.as_chunks::<4>().0 {
        let color = u32::from_le_bytes(*pixel);
        let index = if let Some(index) = map.get(&color) {
            *index
        } else {
            if palette.len() == 256 {
                return None;
            }
            let index = palette.len() as u8;
            palette.push(color);
            map.insert(color, index);
            index
        };
        indices.push(index);
    }
    let mut body = Vec::with_capacity(2 + palette.len() * 4 + indices.len());
    put16(&mut body, palette.len() as u16);
    for color in palette {
        put32(&mut body, color);
    }
    body.extend(indices);
    Some(body)
}
fn palette_decode(body: &[u8], area: usize) -> Result<Vec<u8>> {
    ensure!(body.len() >= 2, "truncated palette");
    let count = usize::from(get16(body, 0));
    ensure!(
        (1..=256).contains(&count) && body.len() == 2 + count * 4 + area,
        "invalid palette length"
    );
    let palette = &body[2..2 + count * 4];
    let mut pixels = Vec::with_capacity(area * 4);
    for &index in &body[2 + count * 4..] {
        ensure!(usize::from(index) < count, "invalid palette index");
        pixels.extend_from_slice(&palette[usize::from(index) * 4..usize::from(index) * 4 + 4]);
    }
    Ok(pixels)
}
// GLB2 chooses only reversible transforms here. Exactly one outer compression
// pass follows; bounded sampling selects work without probing compressed sizes.
fn global_transform(raw: Vec<u8>, rect: Rect, previous: Option<&Frame>) -> (u8, Vec<u8>) {
    let pixels = raw.as_chunks::<4>().0;
    if pixels.iter().all(|pixel| pixel == &pixels[0]) {
        return (6, pixels[0].to_vec());
    }
    let samples = pixels.len().min(64);
    let mut colors = [0u32; 33];
    let mut distinct = 0;
    for sample in 0..samples {
        let color = u32::from_le_bytes(pixels[sample * pixels.len() / samples]);
        if !colors[..distinct].contains(&color) {
            colors[distinct] = color;
            distinct += 1;
            if distinct > 32 {
                break;
            }
        }
    }
    if distinct <= 32
        && let Some(palette) = palette_encode(&raw)
        && palette.len() <= raw.len()
    {
        return (2, palette);
    }
    if let Some(previous) = previous {
        let mut equal_rgb_bytes = 0;
        for sample in 0..samples {
            let index = sample * pixels.len() / samples;
            let x = usize::from(rect.x) + index % usize::from(rect.w);
            let y = usize::from(rect.y) + index / usize::from(rect.w);
            let offset = (y * usize::from(previous.width) + x) * 4;
            equal_rgb_bytes += pixels[index][..3]
                .iter()
                .zip(&previous.pixels[offset..offset + 3])
                .filter(|(a, b)| a == b)
                .count();
        }
        // A sparse change over an otherwise high-color tile gives a mostly-zero
        // XOR stream. Alpha is excluded so opaque noise cannot pass this gate.
        if equal_rgb_bytes * 4 >= samples * 3 * 3 {
            let old = extract(previous, rect);
            return (7, raw.iter().zip(old).map(|(a, b)| a ^ b).collect());
        }
    }
    (0, raw)
}
fn validate_global_palette(body: &[u8], area: usize) -> Result<()> {
    ensure!(
        body.len() >= 2 && body.len() <= area * 4,
        "invalid global palette bound"
    );
    let count = usize::from(get16(body, 0));
    ensure!(
        (2..=256).contains(&count) && count <= area && body.len() == 2 + count * 4 + area,
        "invalid global palette length"
    );
    let mut used = [false; 256];
    for &index in &body[2 + count * 4..] {
        ensure!(usize::from(index) < count, "invalid global palette index");
        used[usize::from(index)] = true;
    }
    ensure!(
        used[..count].iter().all(|used| *used),
        "unused global palette entry"
    );
    Ok(())
}
// GLB3's fixed gate limits collection to128 edits, not all memory comparisons:
// exact discovery still visits at most65,536 pixels after64 sample probes.
fn sparse_patch(raw: &[u8], rect: Rect, previous: &Frame) -> Option<Vec<u8>> {
    let area = rect.area();
    let limit = (area / 16).min(128);
    if area > 65_536 || limit == 0 {
        return None;
    }
    let pixels = raw.as_chunks::<4>().0;
    let samples = area.min(64);
    let mut changed = 0;
    for sample in 0..samples {
        let index = sample * area / samples;
        let x = usize::from(rect.x) + index % usize::from(rect.w);
        let y = usize::from(rect.y) + index / usize::from(rect.w);
        let offset = (y * usize::from(previous.width) + x) * 4;
        changed += usize::from(pixels[index] != previous.pixels[offset..offset + 4]);
    }
    if changed * 16 > samples {
        return None;
    }
    let mut body = Vec::with_capacity(2 + limit * 6);
    put16(&mut body, 0);
    let mut count = 0;
    for row in 0..usize::from(rect.h) {
        let start =
            ((usize::from(rect.y) + row) * usize::from(previous.width) + usize::from(rect.x)) * 4;
        let old = previous.pixels[start..start + usize::from(rect.w) * 4]
            .as_chunks::<4>()
            .0;
        for (column, old) in old.iter().enumerate() {
            let index = row * usize::from(rect.w) + column;
            if pixels[index] != *old {
                if count == limit {
                    return None;
                }
                put16(&mut body, index as u16);
                body.extend_from_slice(&pixels[index]);
                count += 1;
            }
        }
    }
    if count == 0 || body.len() > raw.len() {
        return None;
    }
    body[..2].copy_from_slice(&(count as u16).to_le_bytes());
    Some(body)
}
fn validate_sparse_patch(body: &[u8], area: usize) -> Result<()> {
    ensure!(
        area <= 65_536 && body.len() >= 2 && body.len() <= area * 4,
        "invalid sparse patch bound"
    );
    let count = usize::from(get16(body, 0));
    ensure!(
        count > 0 && count <= area && body.len() == 2 + count * 6,
        "invalid sparse patch count or length"
    );
    let mut previous = None;
    for entry in body[2..].as_chunks::<6>().0 {
        let index = usize::from(get16(entry, 0));
        ensure!(
            index < area && previous.is_none_or(|last| index > last),
            "sparse offsets must be in-range and strictly increasing"
        );
        previous = Some(index);
    }
    Ok(())
}
fn tile_rects(width: u16, height: u16, size: u16) -> Vec<Rect> {
    let mut tiles = Vec::new();
    for y in (0..usize::from(height)).step_by(usize::from(size)) {
        for x in (0..usize::from(width)).step_by(usize::from(size)) {
            tiles.push(Rect {
                x: x as u16,
                y: y as u16,
                w: size.min(width - x as u16),
                h: size.min(height - y as u16),
            });
        }
    }
    tiles
}
fn extract(frame: &Frame, rect: Rect) -> Vec<u8> {
    let mut out = Vec::with_capacity(rect.area() * 4);
    for row in usize::from(rect.y)..usize::from(rect.y) + usize::from(rect.h) {
        let start = (row * usize::from(frame.width) + usize::from(rect.x)) * 4;
        out.extend_from_slice(&frame.pixels[start..start + usize::from(rect.w) * 4]);
    }
    out
}
fn insert(frame: &mut Frame, rect: Rect, pixels: &[u8]) {
    for (row, source) in pixels.chunks_exact(usize::from(rect.w) * 4).enumerate() {
        let start =
            ((usize::from(rect.y) + row) * usize::from(frame.width) + usize::from(rect.x)) * 4;
        frame.pixels[start..start + source.len()].copy_from_slice(source);
    }
}
fn rect_equal(a: &Frame, ar: Rect, b: &Frame, br: Rect) -> bool {
    (0..usize::from(ar.h)).all(|row| {
        let ai = ((usize::from(ar.y) + row) * usize::from(a.width) + usize::from(ar.x)) * 4;
        let bi = ((usize::from(br.y) + row) * usize::from(b.width) + usize::from(br.x)) * 4;
        a.pixels[ai..ai + usize::from(ar.w) * 4] == b.pixels[bi..bi + usize::from(ar.w) * 4]
    })
}
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
fn rect_hash(frame: &Frame, rect: Rect) -> u64 {
    let mut value = 0xcbf29ce484222325u64;
    for row in usize::from(rect.y)..usize::from(rect.y) + usize::from(rect.h) {
        let start = (row * usize::from(frame.width) + usize::from(rect.x)) * 4;
        for byte in &frame.pixels[start..start + usize::from(rect.w) * 4] {
            value = (value ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    value
}
/// Internal identity is the owned previous frame committed alongside this
/// cache, never a caller's mutable buffer or a hash-equality assumption.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameHashes {
    sequence: u32,
    width: u16,
    height: u16,
    tile_size: u16,
    tiles: Vec<Option<u64>>,
    rows: Option<Vec<u64>>,
}
impl FrameHashes {
    fn matches(&self, frame: &Frame, sequence: Option<u32>, tile_size: u16, tiles: usize) -> bool {
        Some(self.sequence) == sequence
            && self.width == frame.width
            && self.height == frame.height
            && self.tile_size == tile_size
            && self.tiles.len() == tiles
            && self
                .rows
                .as_ref()
                .is_none_or(|rows| rows.len() == usize::from(frame.height))
    }
    fn next(
        frame: &Frame,
        sequence: u32,
        tile_size: u16,
        dirty: &[bool],
        previous: Option<&Self>,
    ) -> Self {
        Self {
            sequence,
            width: frame.width,
            height: frame.height,
            tile_size,
            tiles: dirty
                .iter()
                .enumerate()
                .map(|(i, changed)| {
                    if *changed {
                        None
                    } else {
                        previous.and_then(|cache| cache.tiles[i])
                    }
                })
                .collect(),
            // No search and no changed tile proves every source pixel equal.
            // Otherwise invalidate rows until the ordinary row lookup needs
            // them; a gate rejection does not add a full-frame hashing pass.
            rows: if dirty.iter().all(|changed| !changed) {
                previous.and_then(|cache| cache.rows.clone())
            } else {
                None
            },
        }
    }
    fn tile_index(&self, rect: Rect) -> usize {
        usize::from(rect.y / self.tile_size)
            * usize::from(self.width).div_ceil(usize::from(self.tile_size))
            + usize::from(rect.x / self.tile_size)
    }
}
struct CopyIndex {
    tiles: HashMap<(u16, u16, u64), Vec<Rect>>,
    shifts: Vec<i32>,
}
impl CopyIndex {
    fn new(previous: &Frame, tiles: &[Rect], current: &Frame) -> Self {
        let mut index: HashMap<(u16, u16, u64), Vec<Rect>> = HashMap::new();
        for &rect in tiles {
            let bucket = index
                .entry((rect.w, rect.h, rect_hash(previous, rect)))
                .or_default();
            if bucket.len() < 8 {
                bucket.push(rect);
            }
        }
        let row_bytes = usize::from(previous.width) * 4;
        let mut rows: HashMap<u64, Vec<i32>> = HashMap::new();
        for (y, row) in previous.pixels.chunks_exact(row_bytes).enumerate() {
            let bucket = rows.entry(hash(row)).or_default();
            if bucket.len() < 8 {
                bucket.push(y as i32);
            }
        }
        let mut votes: HashMap<i32, usize> = HashMap::new();
        for (y, row) in current.pixels.chunks_exact(row_bytes).enumerate() {
            if let Some(matches) = rows.get(&hash(row)) {
                for &old_y in matches {
                    let shift = old_y - y as i32;
                    if shift != 0 && shift.unsigned_abs() <= 512 {
                        *votes.entry(shift).or_default() += 1;
                    }
                }
            }
        }
        let mut shifts: Vec<_> = votes.into_iter().collect();
        shifts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Self {
            tiles: index,
            shifts: shifts.into_iter().take(4).map(|(shift, _)| shift).collect(),
        }
    }
    // Keep the ordinary builder above as the control. Bucket caps, insertion
    // order, vote bounds and deterministic shift ordering stay identical.
    fn new_cached(
        previous: &Frame,
        tiles: &[Rect],
        current: &Frame,
        old_hashes: Option<&FrameHashes>,
        dirty: &[bool],
        next_hashes: &mut FrameHashes,
    ) -> Self {
        let mut index: HashMap<(u16, u16, u64), Vec<Rect>> = HashMap::new();
        for (i, &rect) in tiles.iter().enumerate() {
            let value = old_hashes
                .and_then(|cache| cache.tiles[i])
                .unwrap_or_else(|| rect_hash(previous, rect));
            let bucket = index.entry((rect.w, rect.h, value)).or_default();
            if bucket.len() < 8 {
                bucket.push(rect);
            }
            if !dirty[i] {
                next_hashes.tiles[i] = Some(value);
            }
        }
        let row_bytes = usize::from(previous.width) * 4;
        let known_rows = old_hashes.and_then(|cache| cache.rows.as_deref());
        let mut rows: HashMap<u64, Vec<i32>> = HashMap::new();
        for (y, row) in previous.pixels.chunks_exact(row_bytes).enumerate() {
            let value = known_rows.map_or_else(|| hash(row), |known| known[y]);
            let bucket = rows.entry(value).or_default();
            if bucket.len() < 8 {
                bucket.push(y as i32);
            }
        }
        let mut votes: HashMap<i32, usize> = HashMap::new();
        let mut current_rows = Vec::with_capacity(usize::from(current.height));
        for (y, row) in current.pixels.chunks_exact(row_bytes).enumerate() {
            let value = if let Some(known) = known_rows
                && previous.pixels[y * row_bytes..(y + 1) * row_bytes] == *row
            {
                known[y]
            } else {
                hash(row)
            };
            current_rows.push(value);
            if let Some(matches) = rows.get(&value) {
                for &old_y in matches {
                    let shift = old_y - y as i32;
                    if shift != 0 && shift.unsigned_abs() <= 512 {
                        *votes.entry(shift).or_default() += 1;
                    }
                }
            }
        }
        next_hashes.rows = Some(current_rows);
        let mut shifts: Vec<_> = votes.into_iter().collect();
        shifts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Self {
            tiles: index,
            shifts: shifts.into_iter().take(4).map(|(shift, _)| shift).collect(),
        }
    }
    fn find(&self, current: &Frame, rect: Rect, previous: &Frame) -> Option<Rect> {
        for &shift in &self.shifts {
            let source_y = i32::from(rect.y) + shift;
            if source_y >= 0 && source_y + i32::from(rect.h) <= i32::from(previous.height) {
                let source = Rect {
                    y: source_y as u16,
                    ..rect
                };
                if rect_equal(current, rect, previous, source) {
                    return Some(source);
                }
            }
        }
        self.tiles
            .get(&(rect.w, rect.h, rect_hash(current, rect)))?
            .iter()
            .copied()
            .find(|source| rect_equal(current, rect, previous, *source))
    }
    fn find_cached(
        &self,
        current: &Frame,
        rect: Rect,
        previous: &Frame,
        current_hash: &mut Option<u64>,
    ) -> Option<Rect> {
        for &shift in &self.shifts {
            let source_y = i32::from(rect.y) + shift;
            if source_y >= 0 && source_y + i32::from(rect.h) <= i32::from(previous.height) {
                let source = Rect {
                    y: source_y as u16,
                    ..rect
                };
                if rect_equal(current, rect, previous, source) {
                    return Some(source);
                }
            }
        }
        let value = rect_hash(current, rect);
        *current_hash = Some(value);
        self.tiles
            .get(&(rect.w, rect.h, value))?
            .iter()
            .copied()
            .find(|source| rect_equal(current, rect, previous, *source))
    }
}
// Gates affect byte-cost/CPU choice only: every chosen encoding remains pixel exact.
// At most64 evenly spaced samples per dirty tile;12.5% matching RGB distinguishes
// useful desktop temporal structure from independently changing colors.
fn temporal_similarity(current: &Frame, previous: &Frame, rectangles: &[Rect]) -> bool {
    let mut matched = 0;
    let mut total = 0;
    for &rect in rectangles {
        for sample in 0..rect.area().min(64) {
            let index = sample * rect.area() / rect.area().min(64);
            let x = usize::from(rect.x) + index % usize::from(rect.w);
            let y = usize::from(rect.y) + index / usize::from(rect.w);
            let offset = (y * usize::from(current.width) + x) * 4;
            matched += usize::from(
                current.pixels[offset..offset + 3] == previous.pixels[offset..offset + 3],
            );
            total += 1;
        }
    }
    matched * 8 >= total
}
fn rgb_similarity(current: &[u8], previous: &[u8]) -> bool {
    let area = current.len() / 4;
    let samples = area.min(64);
    let mut matched = 0;
    for sample in 0..samples {
        let offset = sample * area / samples * 4;
        matched += usize::from(current[offset..offset + 3] == previous[offset..offset + 3]);
    }
    matched * 8 >= samples
}
fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn get16(bytes: &[u8], start: usize) -> u16 {
    u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap())
}
fn get32(bytes: &[u8], start: usize) -> u32 {
    u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests;
