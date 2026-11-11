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
