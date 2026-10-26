use anyhow::{Result, ensure};
use gliss_core::Frame;

pub const BASE_TILE_SIZE: u16 = 64;
pub const MAX_TILES: usize = 8192;
pub const MAX_SAMPLES: usize = 64;
pub const MIN_DISTINCT: usize = 48;
pub const ACTIVATE_FRAMES: u8 = 3;
pub const QUIET_FRAMES: u8 = 6;

pub fn valid_grid(size: u16) -> bool {
    matches!(size, 16 | 32)
}
pub fn grid(width: u16, height: u16, size: u16) -> (usize, usize) {
    (
        usize::from(width).div_ceil(usize::from(size)),
        usize::from(height).div_ceil(usize::from(size)),
    )
}
pub fn tile_bounds(index: u16, width: u16, height: u16, size: u16) -> (usize, usize, usize, usize) {
    let (columns, _) = grid(width, height, size);
    let x = usize::from(index) % columns * usize::from(size);
    let y = usize::from(index) / columns * usize::from(size);
    (
        x,
        y,
        (usize::from(width) - x).min(usize::from(size)),
        (usize::from(height) - y).min(usize::from(size)),
    )
}

#[derive(Clone)]
struct Tile {
    previous: [u32; MAX_SAMPLES],
    initialized: bool,
    run: u8,
    quiet: u8,
    active: bool,
}
impl Default for Tile {
    fn default() -> Self {
        Self {
            previous: [0; MAX_SAMPLES],
            initialized: false,
            run: 0,
            quiet: 0,
            active: false,
        }
    }
}
#[derive(Clone)]
pub struct Classifier {
    size: u16,
    dimensions: Option<(u16, u16)>,
    tiles: Vec<Tile>,
}
pub struct Classification {
    pub masks: Vec<u16>,
    pub grid_fallback: bool,
}
impl Classifier {
    pub fn new(size: u16) -> Result<Self> {
        ensure!(valid_grid(size), "GLH2 mask grid must be 16 or 32");
        Ok(Self {
            size,
            dimensions: None,
            tiles: Vec::new(),
        })
    }
    pub fn state_tiles(&self) -> usize {
        self.tiles.len()
    }
    pub fn update(&mut self, frame: &Frame) -> Result<Classification> {
        ensure!(frame.is_valid(), "Invalid classifier frame");
        let (columns, rows) = grid(frame.width, frame.height, self.size);
        if columns * rows > MAX_TILES {
            self.tiles = Vec::new();
            self.dimensions = Some((frame.width, frame.height));
            return Ok(Classification {
                masks: Vec::new(),
                grid_fallback: true,
            });
        }
        if self.dimensions != Some((frame.width, frame.height)) {
            self.tiles = vec![Tile::default(); columns * rows];
            self.dimensions = Some((frame.width, frame.height));
        }
        let mut active = Vec::new();
        for (index, tile) in self.tiles.iter_mut().enumerate() {
            let (x, y, width, height) =
                tile_bounds(index as u16, frame.width, frame.height, self.size);
            let nx = width.min(8);
            let ny = height.min(8);
            let samples = nx * ny;
            let mut colors = [0u32; MAX_SAMPLES];
            let mut distinct = 0;
            let mut changed = 0;
            for sy in 0..ny {
                for sx in 0..nx {
                    let sample = sy * nx + sx;
                    let px = x + ((2 * sx + 1) * width / (2 * nx)).min(width - 1);
                    let py = y + ((2 * sy + 1) * height / (2 * ny)).min(height - 1);
                    let offset = (py * usize::from(frame.width) + px) * 4;
                    let color =
                        u32::from_le_bytes(frame.pixels[offset..offset + 4].try_into().unwrap());
                    changed += usize::from(tile.initialized && tile.previous[sample] != color);
                    tile.previous[sample] = color;
                    if !colors[..distinct].contains(&color) {
                        colors[distinct] = color;
                        distinct += 1;
                    }
                }
            }
            let qualifies = tile.initialized && distinct >= MIN_DISTINCT && changed * 4 >= samples;
            tile.initialized = true;
            if qualifies {
                tile.run = tile.run.saturating_add(1).min(ACTIVATE_FRAMES);
                tile.quiet = 0;
                if tile.run >= ACTIVATE_FRAMES {
                    tile.active = true;
                }
            } else {
                tile.run = 0;
                if tile.active {
                    tile.quiet += 1;
                    if tile.quiet >= QUIET_FRAMES {
                        tile.active = false;
                        tile.quiet = 0;
                    }
                }
            }
            if tile.active {
                active.push(index as u16);
            }
        }
        Ok(Classification {
            masks: active,
            grid_fallback: false,
        })
    }
}
