//! Deterministic CPU-only source for local functional tests, not a VM baseline.
use crate::Captured;
use anyhow::{Result, bail};
use gliss_core::{Frame, Input};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};

pub fn validate_workload(workload: &str) -> Result<()> {
    if !["static", "scroll", "drag", "video"].contains(&workload) {
        bail!("workload must be static, scroll, drag, or video");
    }
    Ok(())
}

fn rect(frame: &mut Frame, x: usize, y: usize, w: usize, h: usize, color: [u8; 4]) {
    for py in y..(y + h).min(usize::from(frame.height)) {
        for px in x..(x + w).min(usize::from(frame.width)) {
            let pos = (py * usize::from(frame.width) + px) * 4;
            frame.pixels[pos..pos + 4].copy_from_slice(&color);
        }
    }
}

fn render(
    width: u16,
    height: u16,
    tick: u32,
    id: u32,
    workload: &str,
    pointer: (u16, u16),
) -> Frame {
    let mut frame = Frame::new(width, height).unwrap();
    let w = usize::from(width);
    let h = usize::from(height);
    rect(&mut frame, 0, 0, w, h, [34, 48, 61, 255]);
    rect(
        &mut frame,
        20,
        50,
        w.saturating_sub(40),
        h.saturating_sub(80),
        [242, 244, 246, 255],
    );
    rect(
        &mut frame,
        20,
        50,
        w.saturating_sub(40),
        25,
        [80, 97, 116, 255],
    );
    let offset = if workload == "scroll" {
        tick as usize * 3
    } else {
        0
    };
    for row in 0..h.saturating_sub(110) / 16 {
        for col in 0..w.saturating_sub(80) / 8 {
            let seed = (row * 27 + col * 13 + offset / 16) % 97;
            if seed % 5 != 0 {
                rect(
                    &mut frame,
                    38 + col * 8,
                    85 + row * 16,
                    3 + seed % 4,
                    8,
                    [30 + (seed % 60) as u8, 65, 98, 255],
                );
            }
        }
    }
    if workload == "drag" {
        let x = 30 + (tick as usize * 7) % w.saturating_sub(230).max(1);
        let y = 80 + (tick as usize * 3) % h.saturating_sub(230).max(1);
        rect(&mut frame, x, y, 180, 130, [109, 157, 194, 255]);
    }
    if workload == "video" {
        for y in 80..h.saturating_sub(30) {
            for x in 30..w.saturating_sub(30) {
                let p = (y * w + x) * 4;
                frame.pixels[p] = (x as u32 + tick * 5) as u8;
                frame.pixels[p + 1] = (y as u32 + tick * 3) as u8;
                frame.pixels[p + 2] = ((x ^ y) as u32 + tick * 11) as u8;
            }
        }
    }
    rect(
        &mut frame,
        usize::from(pointer.0),
        usize::from(pointer.1),
        8,
        8,
        [230, 91, 56, 255],
    );
    // The reserved probe remains visible after pointer movement, blur and release.
    rect(
        &mut frame,
        0,
        0,
        24,
        24,
        [id as u8, (id >> 8) as u8, (id >> 16) as u8, 255],
    );
    let probe_id = id & 0x00ff_ffff;
    let checksum = (probe_id as u8) ^ ((probe_id >> 8) as u8) ^ ((probe_id >> 16) as u8) ^ 0xa5;
    let probe = probe_id | (u32::from(checksum) << 24);
    for bit in 0..32 {
        let value = if probe & (1 << bit) != 0 { 235 } else { 20 };
        rect(
            &mut frame,
            32 + (bit % 8) * 8,
            (bit / 8) * 8,
            8,
            8,
            [value, value, value, 255],
        );
    }
    frame
}

pub async fn run(
    workload: &str,
    fps: u32,
    tx: watch::Sender<Option<Captured>>,
    mut inputs: mpsc::Receiver<Input>,
) {
    let mut width = 960u16;
    let mut height = 600u16;
    let mut tick = 0u32;
    let mut id = 0;
    let mut pointer = (480, 300);
    let mut dirty = true;
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(fps)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            input=inputs.recv() => {
                let Some(input)=input else {break;};
                id=input.id(); dirty=true;
                match input {
                    Input::Pointer{x,y,..} => pointer=(x.min(width-1),y.min(height-1)),
                    Input::Resize{width:w,height:h,..} if w>0 && h>0 && u32::from(w)*u32::from(h)<=4_194_304 => {width=w;height=h;},
                    _ => {}
                }
            }
            _=interval.tick() => {
                if dirty || workload!="static" {
                    if tx.send(Some(Captured{frame:Arc::new(render(width,height,tick,id,workload,pointer)),forwarded_input_id:id,stage_generation:None,published_at:None})).is_err(){break;}
                    dirty=false;tick=tick.wrapping_add(1);
                }
            }
        }
    }
}
