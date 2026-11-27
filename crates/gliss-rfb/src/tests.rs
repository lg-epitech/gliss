//! Behaviour that needs an in-memory connection: no socket, and a paused clock.
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::duplex;

use super::*;

fn connection<R, W>(read: R, write: W) -> (RfbReader, RfbWriter)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let info = ServerInfo {
        width: 2,
        height: 1,
        name: "test".into(),
        protocol_version: (3, 8),
    };
    let (reader, writer, _) =
        start_tasks(read, write, Frame::new(2, 1).unwrap(), info, Layout::Rgbx);
    (reader, writer)
}

const KEY: Input = Input::Key {
    keysym: 97,
    down: true,
    id: 0,
};

#[tokio::test(start_paused = true)]
async fn a_write_that_makes_no_progress_fails_the_connection() {
    // The peer stays open and reads nothing: eight 8-byte keys fill its buffer.
    let (client, _peer) = duplex(64);
    let (read, write) = tokio::io::split(client);
    let (mut reader, mut writer) = connection(read, write);
    let started = tokio::time::Instant::now();
    let failure = timeout(Duration::from_secs(3600), async {
        loop {
            if let Err(error) = writer.send_input(&KEY).await {
                break error;
            }
        }
    })
    .await
    .expect("a write to a server that reads nothing must fail");
    assert_eq!(started.elapsed(), WRITE_STALL_TIMEOUT);
    assert!(
        format!("{failure:#}").contains("stopped reading"),
        "{failure:#}"
    );
    // The reader ends with the same reason, so the owner of the connection sees it.
    let ended = timeout(Duration::from_secs(3600), reader.read_event())
        .await
        .expect("a failed write must end the reader")
        .unwrap_err();
    assert!(
        format!("{ended:#}").contains("stopped reading"),
        "{ended:#}"
    );
    assert!(writer.send_input(&KEY).await.is_err());
    assert_eq!(started.elapsed(), WRITE_STALL_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn a_slow_server_that_keeps_reading_is_not_cut_off() {
    let (client, mut peer) = duplex(8);
    let (read, write) = tokio::io::split(client);
    let (_reader, mut writer) = connection(read, write);
    // One byte every nine seconds: each write waits, but never for ten.
    tokio::spawn(async move {
        let mut byte = [0];
        loop {
            tokio::time::sleep(Duration::from_secs(9)).await;
            if peer.read_exact(&mut byte).await.is_err() {
                break;
            }
        }
    });
    let started = tokio::time::Instant::now();
    for _ in 0..3 {
        writer.send_input(&KEY).await.unwrap();
    }
    // 24 bytes into an 8-byte buffer: the last 16 each waited for the peer.
    assert_eq!(started.elapsed(), Duration::from_secs(16 * 9));
}

/// Decodes `wire` from memory, where no read ever waits, on a desktop of
/// `width`x`height`, and returns how often another task got to run meanwhile.
async fn turns_of_another_task(width: u16, height: u16, wire: Vec<u8>) -> usize {
    let polls = Arc::new(AtomicUsize::new(0));
    let other_polls = polls.clone();
    // Stands for every other task on the worker: it runs only when decoding yields.
    let other = tokio::spawn(async move {
        loop {
            other_polls.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
        }
    });
    let info = ServerInfo {
        width,
        height,
        name: "test".into(),
        protocol_version: (3, 8),
    };
    let (mut reader, _writer, _) = start_tasks(
        std::io::Cursor::new(wire),
        tokio::io::sink(),
        Frame::new(width, height).unwrap(),
        info,
        Layout::Rgbx,
    );
    reader.read_event().await.unwrap();
    other.abort();
    polls.load(Ordering::Relaxed)
}

/// One update of `count` CopyRect rectangles, each moving the top-left `width`x`height`.
fn copies(count: u16, width: u16, height: u16) -> Vec<u8> {
    let mut wire = vec![0, 0];
    wire.extend_from_slice(&count.to_be_bytes());
    for _ in 0..count {
        wire.extend_from_slice(&[0, 0, 0, 0]);
        wire.extend_from_slice(&width.to_be_bytes());
        wire.extend_from_slice(&height.to_be_bytes());
        wire.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0]);
    }
    wire
}

#[tokio::test]
async fn decoding_returns_to_the_runtime_as_its_work_adds_up() {
    // Each of these rectangles is a whole 256x256 frame: more than the work
    // allowed between two returns, so every one of them is followed by one.
    let large = turns_of_another_task(256, 256, copies(4, 256, 256)).await;
    assert!(
        large >= 4,
        "another task ran {large} times during 4 frames of copying"
    );
    // Rectangles that draw nothing cost little, and still add up.
    let empty = turns_of_another_task(256, 256, copies(12_800, 0, 0)).await;
    assert!(
        (100..=110).contains(&empty),
        "another task ran {empty} times during 12800 empty rectangles"
    );
    // Small rectangles count by their pixels: 3200 of 16x16 are 12.5 frames of work.
    let small = turns_of_another_task(1024, 1024, copies(3200, 16, 16)).await;
    assert!(
        (30..=45).contains(&small),
        "another task ran {small} times during 3200 small rectangles"
    );

    // These 1024 Raw rectangles fit in one read buffer. Rectangle work alone
    // requires eight yields, even when the pixel bytes are already available.
    let count = 1024u16;
    let mut wire = vec![0, 0];
    wire.extend_from_slice(&count.to_be_bytes());
    for _ in 0..count {
        rows(&mut wire, 1, 1);
    }
    let raw = turns_of_another_task(32, 32, wire).await;
    assert!(
        raw >= 8,
        "another task ran {raw} times during buffered Raw decoding"
    );
}

fn rectangle(wire: &mut Vec<u8>, x: u16, y: u16, width: u16, height: u16, encoding: i32) {
    for value in [x, y, width, height] {
        wire.extend_from_slice(&value.to_be_bytes());
    }
    wire.extend_from_slice(&encoding.to_be_bytes());
}

/// A CopyRect of the whole `width`x`height` desktop onto itself.
fn whole_copy(wire: &mut Vec<u8>, width: u16, height: u16) {
    rectangle(wire, 0, 0, width, height, 1);
    wire.extend_from_slice(&[0, 0, 0, 0]);
}

/// A Raw rectangle of `height` whole rows of a desktop `width` pixels wide.
fn rows(wire: &mut Vec<u8>, width: u16, height: u16) {
    rectangle(wire, 0, 0, width, height, 0);
    wire.resize(wire.len() + usize::from(width) * usize::from(height) * 4, 7);
}

/// An ExtendedDesktopSize rectangle: the desktop is `width`x`height` and one screen.
fn desktop_size(wire: &mut Vec<u8>, width: u16, height: u16) {
    rectangle(wire, 0, 0, width, height, -308);
    wire.extend_from_slice(&[1, 0, 0, 0]);
    wire.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0]);
    wire.extend_from_slice(&width.to_be_bytes());
    wire.extend_from_slice(&height.to_be_bytes());
    wire.extend_from_slice(&[0, 0, 0, 0]);
}

/// Decodes one update of `count` rectangles on a 100x100 desktop and returns
/// the size of the frame it reports.
async fn updated(count: u16, rectangles: &[u8]) -> Result<(u16, u16)> {
    let mut wire = vec![0, 0];
    wire.extend_from_slice(&count.to_be_bytes());
    wire.extend_from_slice(rectangles);
    let info = ServerInfo {
        width: 100,
        height: 100,
        name: "test".into(),
        protocol_version: (3, 8),
    };
    let (mut reader, _writer, _) = start_tasks(
        std::io::Cursor::new(wire),
        tokio::io::sink(),
        Frame::new(100, 100).unwrap(),
        info,
        Layout::Rgbx,
    );
    match reader.read_event().await? {
        Event::Frame(frame) => Ok((frame.width, frame.height)),
        other => bail!("a frame was expected, not {other:?}"),
    }
}

fn over_budget(result: Result<(u16, u16)>) -> bool {
    result.is_err_and(|error| format!("{error:#}").contains("exceeds its pixel budget"))
}

#[tokio::test]
async fn a_change_of_size_does_not_renew_the_pixel_budget_of_an_update() {
    // Four whole frames of copying are all a 100x100 desktop allows. Then it
    // grows by two rows, which allows 800 pixels more and not four frames.
    let mut spent = Vec::new();
    for _ in 0..4 {
        whole_copy(&mut spent, 100, 100);
    }
    desktop_size(&mut spent, 100, 102);
    assert_eq!(updated(5, &spent).await.unwrap(), (100, 102));
    let mut fifth_copy = spent.clone();
    whole_copy(&mut fifth_copy, 100, 102);
    assert!(over_budget(updated(6, &fifth_copy).await));
    let mut within = spent.clone();
    rows(&mut within, 100, 8);
    assert_eq!(updated(6, &within).await.unwrap(), (100, 102));
    let mut beyond = spent.clone();
    rows(&mut beyond, 100, 9);
    assert!(over_budget(updated(6, &beyond).await));

    // An update that changes the size and then draws has the new desktop's
    // whole budget: four frames of it, and not a fifth.
    let mut resized = Vec::new();
    desktop_size(&mut resized, 100, 102);
    rows(&mut resized, 100, 102);
    assert_eq!(updated(2, &resized).await.unwrap(), (100, 102));
    for _ in 0..3 {
        whole_copy(&mut resized, 100, 102);
    }
    assert_eq!(updated(5, &resized).await.unwrap(), (100, 102));
    whole_copy(&mut resized, 100, 102);
    assert!(over_budget(updated(6, &resized).await));

    // A smaller desktop allows less: what was drawn on the larger one counts.
    let mut shrunk = Vec::new();
    rows(&mut shrunk, 100, 100);
    desktop_size(&mut shrunk, 100, 30);
    rows(&mut shrunk, 100, 20);
    assert_eq!(updated(3, &shrunk).await.unwrap(), (100, 30));
    rows(&mut shrunk, 100, 1);
    assert!(over_budget(updated(4, &shrunk).await));
}

#[tokio::test]
async fn split_and_buffered_raw_rectangles_preserve_row_padding_and_message_boundaries() {
    let mut wire = vec![0, 0, 0, 1];
    rectangle(&mut wire, 0, 0, 5, 3, 0);
    wire.extend([0, 0, 77, 0].repeat(15));

    wire.extend_from_slice(&[0, 0, 0, 3]);
    rectangle(&mut wire, 1, 0, 2, 3, 0);
    let split = wire.len() + 9; // One full row and one byte of the next pixel.
    for red in 1..=6 {
        wire.extend_from_slice(&[0, 0, red, 0]);
    }
    // This whole Raw rectangle and the following CopyRect arrive together.
    rectangle(&mut wire, 3, 1, 2, 2, 0);
    for red in 7..=10 {
        wire.extend_from_slice(&[0, 0, red, 0]);
    }
    rectangle(&mut wire, 0, 0, 1, 3, 1);
    wire.extend_from_slice(&[0, 4, 0, 0]); // Copy column 4 to column 0.
    wire.push(2); // Bell, a separate server message.

    let tail = wire.split_off(split);
    let stream = std::io::Cursor::new(wire).chain(std::io::Cursor::new(tail));
    let info = ServerInfo {
        width: 5,
        height: 3,
        name: "test".into(),
        protocol_version: (3, 8),
    };
    let (mut reader, _writer, _) = start_tasks(
        stream,
        tokio::io::sink(),
        Frame::new(5, 3).unwrap(),
        info,
        Layout::Bgrx,
    );
    let PooledEvent::Frame(before) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected initial frame");
    };
    let PooledEvent::Frame(after) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected updated frame");
    };
    let expected: Vec<u8> = [77, 1, 2, 77, 77, 8, 3, 4, 7, 8, 10, 5, 6, 9, 10]
        .into_iter()
        .flat_map(|red| [red, 0, 0, 255])
        .collect();
    assert_eq!(after.pixels, expected);
    assert_eq!(before.pixels, [77, 0, 0, 255].repeat(15));
    assert!(matches!(
        reader.read_pooled_event().await.unwrap(),
        PooledEvent::Bell
    ));
}

#[tokio::test]
async fn pooled_frames_reuse_buffers_and_sync_dirty_tiles_across_updates_and_resizes() {
    let (w, h) = (130u16, 130u16);
    let mut wire = Vec::new();
    let push_raw = |wire: &mut Vec<u8>, x: u16, y: u16, rw: u16, rh: u16, rgb: [u8; 3]| {
        wire.extend_from_slice(&[0, 0, 0, 1]);
        rectangle(wire, x, y, rw, rh, 0);
        for _ in 0..usize::from(rw) * usize::from(rh) {
            wire.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0]);
        }
    };
    // Update 1: fill top-left 64x64 with [10, 20, 30]
    push_raw(&mut wire, 0, 0, 64, 64, [10, 20, 30]);
    // Update 2: fill bottom-right 20x20 with [40, 50, 60]
    push_raw(&mut wire, 100, 100, 20, 20, [40, 50, 60]);
    // Update 3: overwrite top-left 10x10 with [70, 80, 90] (buffer from update 1 recycled!)
    push_raw(&mut wire, 0, 0, 10, 10, [70, 80, 90]);
    // Update 4: resize to 80x80 and fill 8x8 with [110, 120, 130]
    wire.extend_from_slice(&[0, 0, 0, 2]);
    desktop_size(&mut wire, 80, 80);
    rectangle(&mut wire, 0, 0, 8, 8, 0);
    for _ in 0..64 {
        wire.extend_from_slice(&[110, 120, 130, 0]);
    }

    let info = ServerInfo {
        width: w,
        height: h,
        name: "pool-test".into(),
        protocol_version: (3, 8),
    };
    let (mut reader, _writer, _) = start_tasks(
        std::io::Cursor::new(wire),
        tokio::io::sink(),
        Frame::new(w, h).unwrap(),
        info,
        Layout::Rgbx,
    );

    let px = |frame: &Frame, x: usize, y: usize| -> [u8; 4] {
        let o = (y * usize::from(frame.width) + x) * 4;
        [
            frame.pixels[o],
            frame.pixels[o + 1],
            frame.pixels[o + 2],
            frame.pixels[o + 3],
        ]
    };

    let PooledEvent::Frame(f1) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected frame 1");
    };
    assert!(!f1.damage().is_full());
    assert_eq!(px(&f1, 0, 0), [10, 20, 30, 255]);
    assert_eq!(px(&f1, 100, 100), [0, 0, 0, 255]);
    // Drop f1 so its pixel buffer returns to the pool before update 2/3.
    drop(f1);

    let PooledEvent::Frame(f2) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected frame 2");
    };
    assert!(!f2.damage().is_full());
    assert_eq!(px(&f2, 0, 0), [10, 20, 30, 255]);
    assert_eq!(px(&f2, 100, 100), [40, 50, 60, 255]);
    drop(f2);

    let PooledEvent::Frame(f3) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected frame 3");
    };
    assert!(!f3.damage().is_full());
    assert_eq!(px(&f3, 0, 0), [70, 80, 90, 255]);
    assert_eq!(px(&f3, 20, 20), [10, 20, 30, 255]);
    assert_eq!(px(&f3, 100, 100), [40, 50, 60, 255]);
    drop(f3);

    let PooledEvent::Frame(f4) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected frame 4");
    };
    assert!(f4.damage().is_full());
    assert_eq!((f4.width, f4.height), (80, 80));
    assert_eq!(px(&f4, 0, 0), [110, 120, 130, 255]);
    assert_eq!(px(&f4, 20, 20), [0, 0, 0, 255]);
}

#[test]
fn bgrx_to_rgba_matches_scalar_across_aligned_and_tail_lengths() {
    for pixels in [0usize, 1, 7, 8, 9, 15, 16, 17, 63, 64, 1920] {
        let mut actual = vec![0u8; pixels * 4];
        let mut state = 0x9e37_79b9_u32;
        for byte in &mut actual {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (state >> 24) as u8;
        }
        let mut expected = actual.clone();
        Layout::Bgrx.to_rgba(&mut actual);
        bgrx_to_rgba_scalar(&mut expected);
        assert_eq!(actual, expected, "mismatch for {pixels} pixels");
    }
}

#[tokio::test]
async fn fused_pixel_reader_matches_in_place_conversion_across_chunk_sizes() {
    for layout in [Layout::Bgrx, Layout::Rgbx] {
        let mut raw = vec![0u8; 73 * 4];
        let mut state = 0x1357_9bdf_u32;
        for byte in &mut raw {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (state >> 24) as u8;
        }
        let mut expected = raw.clone();
        layout.to_rgba(&mut expected);
        for chunk in [1usize, 3, 5, 17, 32, 64, 292] {
            let mut reader = BufReader::with_capacity(chunk, std::io::Cursor::new(raw.clone()));
            let mut actual = vec![0u8; raw.len()];
            read_pixels_converted(&mut reader, &mut actual, layout)
                .await
                .unwrap();
            assert_eq!(actual, expected, "layout={layout:?} chunk={chunk}");
        }
    }
}

#[tokio::test]
async fn pooled_frame_reuses_buffer_in_place_when_unshared_and_detaches_when_held() {
    let mut wire = Vec::new();
    // Update 1: 4x4 rect at (0, 0) filled with [10, 20, 30, 255].
    wire.extend_from_slice(&[0, 0, 0, 1]);
    wire.extend_from_slice(&0u16.to_be_bytes());
    wire.extend_from_slice(&0u16.to_be_bytes());
    wire.extend_from_slice(&4u16.to_be_bytes());
    wire.extend_from_slice(&4u16.to_be_bytes());
    wire.extend_from_slice(&0i32.to_be_bytes());
    for _ in 0..16 {
        wire.extend_from_slice(&[10, 20, 30, 0]);
    }
    // Update 2: 4x4 rect at (4, 0) filled with [40, 50, 60, 255].
    wire.extend_from_slice(&[0, 0, 0, 1]);
    wire.extend_from_slice(&4u16.to_be_bytes());
    wire.extend_from_slice(&0u16.to_be_bytes());
    wire.extend_from_slice(&4u16.to_be_bytes());
    wire.extend_from_slice(&4u16.to_be_bytes());
    wire.extend_from_slice(&0i32.to_be_bytes());
    for _ in 0..16 {
        wire.extend_from_slice(&[40, 50, 60, 0]);
    }
    // Update 3: 4x4 rect at (0, 0) overwritten with [70, 80, 90, 255].
    wire.extend_from_slice(&[0, 0, 0, 1]);
    wire.extend_from_slice(&0u16.to_be_bytes());
    wire.extend_from_slice(&0u16.to_be_bytes());
    wire.extend_from_slice(&4u16.to_be_bytes());
    wire.extend_from_slice(&4u16.to_be_bytes());
    wire.extend_from_slice(&0i32.to_be_bytes());
    for _ in 0..16 {
        wire.extend_from_slice(&[70, 80, 90, 0]);
    }

    let info = ServerInfo {
        width: 16,
        height: 16,
        name: "test".into(),
        protocol_version: (3, 8),
    };
    let (mut reader, _writer, _) = start_tasks(
        std::io::Cursor::new(wire),
        tokio::io::sink(),
        Frame::new(16, 16).unwrap(),
        info,
        Layout::Rgbx,
    );
    let PooledEvent::Frame(f1) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected frame 1");
    };
    assert_eq!(&f1.pixels[..4], &[10, 20, 30, 255]);
    // Hold `f1` across update 2: decoder must detach into a separate buffer so
    // `f1` remains immutable.
    let PooledEvent::Frame(f2) = reader.read_pooled_event().await.unwrap() else {
        panic!("expected frame 2");
    };
    assert!(!Arc::ptr_eq(&f1, &f2));
    assert_eq!(&f1.pixels[..4], &[10, 20, 30, 255]);
    assert_eq!(&f1.pixels[16..20], &[0, 0, 0, 255]);
    assert_eq!(&f2.pixels[..4], &[10, 20, 30, 255]);
    assert_eq!(&f2.pixels[16..20], &[40, 50, 60, 255]);

    // Drop `f1` and `f2` and verify in-place reuse when unshared.
    let f2_addr = Arc::as_ptr(&f2) as usize;
    drop(f1);
    drop(f2);
    // Use a duplex pipe so update 3 arrives strictly after `f2` is dropped.
    let (client, mut peer) = duplex(4096);
    let (read, write) = tokio::io::split(client);
    let info2 = ServerInfo {
        width: 16,
        height: 16,
        name: "test".into(),
        protocol_version: (3, 8),
    };
    let (mut reader2, _writer2, _) = start_tasks(
        read,
        write,
        Frame::new(16, 16).unwrap(),
        info2,
        Layout::Rgbx,
    );
    let mut u1 = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 4, 0, 4, 0, 0, 0, 0];
    for _ in 0..16 {
        u1.extend_from_slice(&[1, 2, 3, 0]);
    }
    peer.write_all(&u1).await.unwrap();
    let PooledEvent::Frame(a) = reader2.read_pooled_event().await.unwrap() else {
        panic!("expected a");
    };
    let a_addr = Arc::as_ptr(&a) as usize;
    assert_eq!(&a.pixels[..4], &[1, 2, 3, 255]);
    drop(a);

    let mut u2 = vec![0, 0, 0, 1, 0, 4, 0, 0, 0, 4, 0, 4, 0, 0, 0, 0];
    for _ in 0..16 {
        u2.extend_from_slice(&[4, 5, 6, 0]);
    }
    peer.write_all(&u2).await.unwrap();
    let PooledEvent::Frame(b) = reader2.read_pooled_event().await.unwrap() else {
        panic!("expected b");
    };
    assert_eq!(Arc::as_ptr(&b) as usize, a_addr);
    assert_eq!(&b.pixels[..4], &[1, 2, 3, 255]);
    assert_eq!(&b.pixels[16..20], &[4, 5, 6, 255]);
    assert_ne!(f2_addr, 0);

    // Holding only a Weak handle must still detach cleanly without panicking.
    let weak = Arc::downgrade(&b);
    drop(b);
    let mut u3 = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 4, 0, 4, 0, 0, 0, 0];
    for _ in 0..16 {
        u3.extend_from_slice(&[9, 8, 7, 0]);
    }
    peer.write_all(&u3).await.unwrap();
    let PooledEvent::Frame(c) = reader2.read_pooled_event().await.unwrap() else {
        panic!("expected c");
    };
    assert_ne!(Arc::as_ptr(&c) as usize, a_addr);
    assert_eq!(&c.pixels[..4], &[9, 8, 7, 255]);
    assert_eq!(&c.pixels[16..20], &[4, 5, 6, 255]);
    assert!(weak.upgrade().is_none());
}

#[test]
fn frame_pool_preserves_fresher_generations_and_sync_damage_bounds_history() {
    let mut pool = FramePool::default();
    pool.put(vec![1; 16], 10);
    pool.put(vec![2; 16], 12);
    pool.put(vec![3; 16], 11);
    pool.put(vec![4; 16], 13);
    // An older generation (or 0) must not evict fresher pooled buffers.
    pool.put(vec![9; 16], 5);
    pool.put(vec![8; 16], 0);
    let mut taken = Vec::new();
    while let Some(buf) = pool.take(16) {
        taken.push(buf.generation);
    }
    assert_eq!(taken.len(), POOL_CAPACITY);
    assert_eq!(taken, vec![13, 12]);

    // History boundary tests for `compute_sync_damage`.
    let mut history = [(0u64, Damage::empty(128, 128)); HISTORY_LEN];
    for step in 1..=(HISTORY_LEN as u64 + 1) {
        let mut d = Damage::empty(128, 128);
        d.mark_rect(((step as u16) % 2) * 64, 0, 16, 16);
        history[(step as usize) % HISTORY_LEN] = (step, d);
    }
    // Exactly HISTORY_LEN steps behind: all entries present -> partial damage.
    let exact = compute_sync_damage(1, 1 + HISTORY_LEN as u64, 128, 128, &history);
    assert!(!exact.is_full());
    // HISTORY_LEN + 1 steps behind: falls back to full damage.
    let too_old = compute_sync_damage(1, 2 + HISTORY_LEN as u64, 128, 128, &history);
    assert!(too_old.is_full());
    // Generation 0 (uncommitted) always falls back to full damage.
    assert!(compute_sync_damage(0, 2, 128, 128, &history).is_full());
    // Dimension mismatch in history falls back to full damage.
    let mut mismatched = history;
    mismatched[2 % HISTORY_LEN] = (2, Damage::empty(64, 64));
    assert!(compute_sync_damage(1, 2, 128, 128, &mismatched).is_full());
}
