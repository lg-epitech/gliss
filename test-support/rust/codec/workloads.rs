//! Frozen desktop-v1 synthetic evaluation. Changes require a new version and seed.
use gliss_core::Frame;

pub const VERSION: &str = "desktop-v1";
pub const SEED: u64 = 0x7d4e_c918_3a20_06b5;
pub const FRAME_COUNT: usize = 36;
pub const FPS: f64 = 30.0;

pub struct Workload {
    pub name: &'static str,
    pub held_out: bool,
    pub frames: Vec<Frame>,
}
pub fn suite() -> Vec<Workload> {
    vec![
        Workload {
            name: "static_typing_code",
            held_out: false,
            frames: (0..FRAME_COUNT).map(typing).collect(),
        },
        Workload {
            name: "vertical_scroll",
            held_out: false,
            frames: (0..FRAME_COUNT).map(scrolling).collect(),
        },
        Workload {
            name: "drag_resize",
            held_out: false,
            frames: (0..FRAME_COUNT).map(drag_resize).collect(),
        },
        Workload {
            name: "high_motion_noise",
            held_out: false,
            frames: (0..FRAME_COUNT).map(high_motion).collect(),
        },
        Workload {
            name: "held_out_dashboard",
            held_out: true,
            frames: (0..FRAME_COUNT).map(dashboard).collect(),
        },
    ]
}
pub fn fingerprint(frames: &[Frame]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for frame in frames {
        for byte in frame
            .width
            .to_le_bytes()
            .into_iter()
            .chain(frame.height.to_le_bytes())
            .chain(frame.pixels.iter().copied())
        {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    format!("fnv1a64:{hash:016x}")
}
fn background(width: u16, height: u16) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    fill(&mut frame, 0, 0, width as i32, height as i32, [26, 29, 35]);
    fill(&mut frame, 0, 0, width as i32, 18, [51, 55, 63]);
    text(
        &mut frame,
        8,
        5,
        "GLISS DESKTOP - WORKSPACE",
        [209, 216, 225],
    );
    fill(
        &mut frame,
        0,
        height as i32 - 13,
        width as i32,
        13,
        [40, 45, 55],
    );
    text(
        &mut frame,
        8,
        height as i32 - 10,
        "CPU ONLY   UTF-8   RGBA8",
        [178, 186, 199],
    );
    frame
}
fn code(frame: &mut Frame, scroll: usize, lines: usize) {
    let phrases = [
        "FN ENCODE(FRAME: RGBA) -> BYTES {",
        "  LET TILES = FRAME.DIRTY_TILES();",
        "  FOR TILE IN TILES {",
        "    LET BODY = LOSSLESS(TILE);",
        "    SEND.SEQUENCE(BODY);",
        "  }",
        "  RETURN EXACT_PIXELS;",
        "}",
    ];
    for line in 0..lines {
        let y = 23 + line as i32 * 11 - scroll as i32;
        if y >= 20 && y + 7 < frame.height as i32 - 15 {
            text(frame, 8, y, &format!("{:04}", line + 1), [118, 130, 145]);
            text(
                frame,
                42,
                y,
                phrases[line % phrases.len()],
                if line % 3 == 0 {
                    [136, 199, 241]
                } else {
                    [204, 216, 228]
                },
            );
        }
    }
}
fn typing(t: usize) -> Frame {
    let mut frame = background(512, 288);
    code(&mut frame, 0, 20);
    let line = "LET INPUT_ACK = FRAME.SEQUENCE;";
    let typed = t.saturating_sub(18).min(line.len());
    fill(&mut frame, 42, 254, 430, 10, [26, 29, 35]);
    text(&mut frame, 42, 255, &line[..typed], [239, 204, 126]);
    if t % 12 >= 6 {
        fill(
            &mut frame,
            42 + typed as i32 * 6,
            254,
            1,
            9,
            [224, 231, 241],
        );
    }
    frame
}
fn scrolling(t: usize) -> Frame {
    let mut frame = background(512, 288);
    code(&mut frame, t * 8, 60);
    frame
}
fn drag_resize(t: usize) -> Frame {
    let (width, height) = if (12..24).contains(&t) {
        (544, 320)
    } else {
        (512, 288)
    };
    let mut frame = background(width, height);
    for y in 18..height as i32 - 13 {
        for x in 0..width as i32 {
            if ((x / 16) + (y / 16)) % 2 == 0 {
                pixel(&mut frame, x, y, [31, 36, 44]);
            }
        }
    }
    let x = 32 + ((t * 9) % 170) as i32;
    let y = 36 + ((t * 3) % 40) as i32;
    let w = 224 + ((t / 4) % 3) as i32 * 16;
    fill(&mut frame, x + 3, y + 3, w, 150, [10, 12, 17]);
    fill(&mut frame, x, y, w, 150, [237, 240, 243]);
    fill(&mut frame, x, y, w, 16, [69, 83, 112]);
    text(
        &mut frame,
        x + 6,
        y + 4,
        "TERMINAL / DRAG RESIZE",
        [240, 243, 247],
    );
    for line in 0..10 {
        text(
            &mut frame,
            x + 8,
            y + 25 + line * 11,
            &format!("BUILD TASK {:02} ... OK", line),
            [32, 39, 49],
        );
    }
    pointer(&mut frame, x + 140, y + 8);
    frame
}
fn high_motion(t: usize) -> Frame {
    let mut frame = Frame::new(512, 288).unwrap();
    let mut state = SEED ^ ((t as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15));
    for pixel in frame.pixels.as_chunks_mut::<4>().0.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        pixel[..3].copy_from_slice(&state.to_le_bytes()[..3]);
    }
    frame
}
fn dashboard(t: usize) -> Frame {
    let mut frame = background(512, 288);
    for y in 25..155 {
        for x in 275..500 {
            let value = ((x * 13 + y * 7) & 255) as u8;
            pixel(
                &mut frame,
                x,
                y,
                [value / 2, value, 190u8.wrapping_sub(value / 3)],
            );
        }
    }
    text(&mut frame, 10, 25, "OPERATIONS / HELD OUT", [180, 214, 245]);
    for row in 0..8 {
        let value = 120 + ((row as usize * 31 + t * 7) % 200);
        text(
            &mut frame,
            12,
            44 + row * 13,
            &format!("SERVICE {:02}    {:03} MS", row, value),
            [208, 218, 231],
        );
    }
    fill(&mut frame, 12, 168, 488, 95, [40, 46, 58]);
    for x in 0..480 {
        let height = 12 + ((x / 8 + t as i32) * 17 % 65);
        fill(&mut frame, 16 + x, 257 - height, 1, height, [84, 180, 143]);
    }
    pointer(
        &mut frame,
        290 + ((t * 11) % 160) as i32,
        45 + ((t * 7) % 90) as i32,
    );
    frame
}
fn pointer(frame: &mut Frame, x: i32, y: i32) {
    for row in 0..12 {
        for col in 0..=row / 2 {
            pixel(
                frame,
                x + col,
                y + row,
                if col == 0 || col == row / 2 {
                    [5, 5, 5]
                } else {
                    [250, 250, 250]
                },
            );
        }
    }
}
fn pixel(frame: &mut Frame, x: i32, y: i32, color: [u8; 3]) {
    if x >= 0 && y >= 0 && x < frame.width as i32 && y < frame.height as i32 {
        let start = (y as usize * frame.width as usize + x as usize) * 4;
        frame.pixels[start..start + 3].copy_from_slice(&color);
    }
}
fn fill(frame: &mut Frame, x: i32, y: i32, width: i32, height: i32, color: [u8; 3]) {
    for row in y.max(0)..(y + height).min(frame.height as i32) {
        for col in x.max(0)..(x + width).min(frame.width as i32) {
            pixel(frame, col, row, color);
        }
    }
}
fn text(frame: &mut Frame, x: i32, y: i32, value: &str, color: [u8; 3]) {
    for (i, byte) in value.bytes().enumerate() {
        let glyph = glyph(byte);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) != 0 {
                    pixel(frame, x + i as i32 * 6 + col, y + row as i32, color);
                }
            }
        }
    }
}
fn glyph(byte: u8) -> [u8; 7] {
    match byte {
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'B' => [30, 17, 17, 30, 17, 17, 30],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'F' => [31, 16, 16, 30, 16, 16, 16],
        b'G' => [14, 17, 16, 23, 17, 17, 15],
        b'H' => [17, 17, 17, 31, 17, 17, 17],
        b'I' => [14, 4, 4, 4, 4, 4, 14],
        b'J' => [7, 2, 2, 2, 18, 18, 12],
        b'K' => [17, 18, 20, 24, 20, 18, 17],
        b'L' => [16, 16, 16, 16, 16, 16, 31],
        b'M' => [17, 27, 21, 21, 17, 17, 17],
        b'N' => [17, 25, 21, 19, 17, 17, 17],
        b'O' => [14, 17, 17, 17, 17, 17, 14],
        b'P' => [30, 17, 17, 30, 16, 16, 16],
        b'Q' => [14, 17, 17, 17, 21, 18, 13],
        b'R' => [30, 17, 17, 30, 20, 18, 17],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'T' => [31, 4, 4, 4, 4, 4, 4],
        b'U' => [17, 17, 17, 17, 17, 17, 14],
        b'V' => [17, 17, 17, 17, 17, 10, 4],
        b'W' => [17, 17, 17, 21, 21, 21, 10],
        b'X' => [17, 17, 10, 4, 10, 17, 17],
        b'Y' => [17, 17, 10, 4, 4, 4, 4],
        b'Z' => [31, 1, 2, 4, 8, 16, 31],
        b'0' => [14, 17, 19, 21, 25, 17, 14],
        b'1' => [4, 12, 4, 4, 4, 4, 14],
        b'2' => [14, 17, 1, 2, 4, 8, 31],
        b'3' => [30, 1, 1, 14, 1, 1, 30],
        b'4' => [2, 6, 10, 18, 31, 2, 2],
        b'5' => [31, 16, 16, 30, 1, 1, 30],
        b'6' => [14, 16, 16, 30, 17, 17, 14],
        b'7' => [31, 1, 2, 4, 8, 8, 8],
        b'8' => [14, 17, 17, 14, 17, 17, 14],
        b'9' => [14, 17, 17, 15, 1, 1, 14],
        b'{' => [2, 4, 4, 8, 4, 4, 2],
        b'}' => [8, 4, 4, 2, 4, 4, 8],
        b'(' => [2, 4, 8, 8, 8, 4, 2],
        b')' => [8, 4, 2, 2, 2, 4, 8],
        b';' => [0, 4, 4, 0, 4, 4, 8],
        b':' => [0, 4, 4, 0, 4, 4, 0],
        b'_' => [0, 0, 0, 0, 0, 0, 31],
        b'-' => [0, 0, 0, 31, 0, 0, 0],
        b'>' => [16, 8, 4, 2, 4, 8, 16],
        b'=' => [0, 0, 31, 0, 31, 0, 0],
        b'.' => [0, 0, 0, 0, 0, 4, 4],
        b'/' => [1, 2, 2, 4, 8, 8, 16],
        b',' => [0, 0, 0, 0, 0, 4, 8],
        _ => [0; 7],
    }
}
