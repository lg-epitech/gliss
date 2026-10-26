//! Synthetic startup witnesses for the unchanged public RFB reader.
//! A successful exit confirms the declared old behavior, not a repaired reader.
use gliss_rfb::{Event, connect_unix};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::oneshot,
    time::{Duration, timeout},
};

const WIDTH: u16 = 4;
const HEIGHT: u16 = 2;
const FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];

#[derive(Clone, Copy)]
enum First {
    Empty,
    DesktopSize,
    ExtendedDesktopSize,
    PartialRaw,
    FullRaw,
}

impl First {
    fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::DesktopSize => "desktop-size",
            Self::ExtendedDesktopSize => "extended-desktop-size",
            Self::PartialRaw => "partial-raw",
            Self::FullRaw => "full-raw-control",
        }
    }

    fn initialized_pixels(self) -> usize {
        match self {
            Self::Empty | Self::DesktopSize | Self::ExtendedDesktopSize => 0,
            Self::PartialRaw => 1,
            Self::FullRaw => 8,
        }
    }
}

fn full_pixels() -> Vec<u8> {
    (0u8..8)
        .flat_map(|n| [30 + n, 60 + n, 90 + n, 255])
        .collect()
}

async fn handshake(peer: &mut UnixStream) {
    peer.write_all(b"RFB 003.008\n").await.unwrap();
    let mut version = [0; 12];
    peer.read_exact(&mut version).await.unwrap();
    assert_eq!(&version, b"RFB 003.008\n");
    peer.write_all(&[1, 1]).await.unwrap();
    assert_eq!(peer.read_u8().await.unwrap(), 1);
    peer.write_u32(0).await.unwrap();
    assert_eq!(peer.read_u8().await.unwrap(), 1);
    peer.write_u16(WIDTH).await.unwrap();
    peer.write_u16(HEIGHT).await.unwrap();
    peer.write_all(&FORMAT).await.unwrap();
    peer.write_u32(9).await.unwrap();
    peer.write_all(b"synthetic").await.unwrap();
    let mut set_format = [0; 20];
    peer.read_exact(&mut set_format).await.unwrap();
    assert_eq!(&set_format[..4], &[0, 0, 0, 0]);
    assert_eq!(&set_format[4..], &FORMAT);
    assert_eq!(peer.read_u8().await.unwrap(), 2);
    assert_eq!(peer.read_u8().await.unwrap(), 0);
    let count = peer.read_u16().await.unwrap();
    assert_eq!(count, 6);
    for expected in [0, 1, -239, -223, -308, 0xc0a1_e5ceu32 as i32] {
        assert_eq!(peer.read_i32().await.unwrap(), expected);
    }
}

async fn request(peer: &mut UnixStream) {
    let mut bytes = [0; 10];
    peer.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, [3, 0, 0, 0, 0, 0, 0, 4, 0, 2]);
}

async fn rectangle(peer: &mut UnixStream, width: u16, height: u16, encoding: i32) {
    peer.write_all(&[0, 0, 0, 1]).await.unwrap();
    for value in [0, 0, width, height] {
        peer.write_u16(value).await.unwrap();
    }
    peer.write_i32(encoding).await.unwrap();
}

async fn full(peer: &mut UnixStream) {
    rectangle(peer, WIDTH, HEIGHT, 0).await;
    peer.write_all(&full_pixels()).await.unwrap();
}

async fn run(first: First) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let (done_tx, done_rx) = oneshot::channel();
    let producer = tokio::spawn(async move {
        handshake(&mut peer).await;
        request(&mut peer).await;
        match first {
            First::Empty => peer.write_all(&[0, 0, 0, 0]).await.unwrap(),
            First::DesktopSize => rectangle(&mut peer, WIDTH, HEIGHT, -223).await,
            First::ExtendedDesktopSize => {
                rectangle(&mut peer, WIDTH, HEIGHT, -308).await;
                peer.write_all(&[1, 0, 0, 0]).await.unwrap();
                peer.write_u32(1).await.unwrap();
                for value in [0, 0, WIDTH, HEIGHT] {
                    peer.write_u16(value).await.unwrap();
                }
                peer.write_u32(0).await.unwrap();
            }
            First::PartialRaw => {
                rectangle(&mut peer, 1, 1, 0).await;
                peer.write_all(&[7, 8, 9, 0]).await.unwrap();
            }
            First::FullRaw => full(&mut peer).await,
        }
        request(&mut peer).await;
        full(&mut peer).await;
        done_rx.await.unwrap();
    });
    let (mut reader, mut writer, info) = connect_unix(client, None).await.unwrap();
    assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
    writer.request_update(false, WIDTH, HEIGHT).await.unwrap();
    let Event::Frame(initial) = reader.read_event().await.unwrap() else {
        panic!("declared initial frame event expected")
    };
    let nonblack = initial
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[..3] != [0, 0, 0])
        .count();
    assert_eq!(nonblack, first.initialized_pixels());
    assert!(
        initial
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[3] == 255)
    );
    let first_matches = initial.pixels == full_pixels();
    assert_eq!(first_matches, matches!(first, First::FullRaw));
    if matches!(first, First::PartialRaw) {
        assert_eq!(&initial.pixels[..4], &[7, 8, 9, 255]);
        assert!(
            initial.pixels[4..]
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 0, 0, 255])
        );
    }
    writer.request_update(false, WIDTH, HEIGHT).await.unwrap();
    let Event::Frame(next) = reader.read_event().await.unwrap() else {
        panic!("declared full source frame event expected")
    };
    assert_eq!(next.pixels, full_pixels());
    done_tx.send(()).unwrap();
    producer.await.unwrap();
    drop(writer);
    drop(reader);
    tokio::task::yield_now().await;
    println!(
        "{{\"case\":\"{}\",\"initial_frame_published\":true,\"initial_nonblack_pixels\":{},\"total_pixels\":8,\"first_matches_full_source\":{},\"following_full_source_matches\":true,\"real_vm\":false,\"actual_v7_cause_proved\":false}}",
        first.name(),
        nonblack,
        first_matches
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    for first in [
        First::Empty,
        First::DesktopSize,
        First::ExtendedDesktopSize,
        First::PartialRaw,
        First::FullRaw,
    ] {
        timeout(Duration::from_secs(2), run(first))
            .await
            .expect("synthetic position deadline");
    }
}
