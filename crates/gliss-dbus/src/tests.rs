use super::*;
use tokio::sync::mpsc;
use zbus::{Guid, zvariant::OwnedFd};

#[tokio::test]
async fn checked_cleanup_reports_worker_failure_and_timeout() {
    fn source(task: JoinHandle<Result<()>>) -> DbusSource {
        let (_, notices) = watch::channel(Notice::default());
        DbusSource {
            surface: Arc::new(Mutex::new(CpuSurface::new(PixelByteOrder::Native))),
            notices,
            last_frame_revision: 0,
            shutdown: None,
            task: Some(task),
        }
    }
    let failed = tokio::spawn(async { anyhow::bail!("injected peer-close failure") });
    let error = source(failed).close_checked().await.unwrap_err();
    assert!(format!("{error:#}").contains("injected peer-close failure"));
    let panicked = tokio::spawn(async {
        panic!("injected cleanup panic");
    });
    let error = source(panicked).close_checked().await.unwrap_err();
    assert!(format!("{error:#}").contains("D-Bus cleanup task failed"));
    let stalled = tokio::spawn(std::future::pending::<Result<()>>());
    let owned_abort = stalled.abort_handle();
    let error = source(stalled).close_checked().await.unwrap_err();
    owned_abort.abort();
    assert!(format!("{error:#}").contains("D-Bus cleanup task timed out"));
}

#[test]
fn independent_xrgb_vectors_endianness_and_padding() {
    let little = [
        0, 0, 255, 0, 255, 0, 0, 0, 7, 7, 7, 7, 0, 255, 0, 0, 255, 255, 255, 0, 9, 9, 9, 9,
    ];
    let big = [
        0, 255, 0, 0, 0, 0, 0, 255, 7, 7, 7, 7, 0, 0, 255, 0, 0, 255, 255, 255, 9, 9, 9, 9,
    ];
    let expected = vec![
        255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 255, 255, 255, 255, 255,
    ];
    for (data, order) in [(little, PixelByteOrder::Little), (big, PixelByteOrder::Big)] {
        let frame = normalize_plane(2, 2, 12, PIXMAN_X8R8G8B8, &data, order).unwrap();
        assert_eq!(frame.pixels, expected);
    }
}

#[test]
fn independent_supported_format_vectors() {
    for (format, bytes, expected) in [
        (PIXMAN_X8B8G8R8, vec![17, 34, 51, 0], [17, 34, 51, 255]),
        (PIXMAN_A8R8G8B8, vec![51, 34, 17, 255], [17, 34, 51, 255]),
        (PIXMAN_A8B8G8R8, vec![17, 34, 51, 255], [17, 34, 51, 255]),
        (PIXMAN_R8G8B8, vec![51, 34, 17], [17, 34, 51, 255]),
        (PIXMAN_B8G8R8, vec![17, 34, 51], [17, 34, 51, 255]),
        (PIXMAN_R5G6B5, vec![0, 248], [255, 0, 0, 255]),
        (PIXMAN_B5G6R5, vec![0, 248], [0, 0, 255, 255]),
    ] {
        assert_eq!(
            normalize_plane(
                1,
                1,
                bytes.len() as u32,
                format,
                &bytes,
                PixelByteOrder::Little
            )
            .unwrap()
            .pixels,
            expected
        );
    }
    assert!(
        normalize_plane(
            1,
            1,
            4,
            PIXMAN_A8R8G8B8,
            &[0, 0, 0, 128],
            PixelByteOrder::Little
        )
        .is_err()
    );
}

#[test]
fn geometry_stride_truncation_and_unknown_formats_preserve_surface() {
    let mut surface = CpuSurface::new(PixelByteOrder::Little);
    surface.scanout(2, 2, 8, PIXMAN_X8R8G8B8, &[0; 16]).unwrap();
    let expected = surface.frame().unwrap().clone();
    for (width, height, stride, format, data) in [
        (0, 1, 4, PIXMAN_X8R8G8B8, vec![0; 4]),
        (65535, 65535, 262140, PIXMAN_X8R8G8B8, vec![]),
        (2, 2, 7, PIXMAN_X8R8G8B8, vec![0; 14]),
        (2, 2, 8, PIXMAN_X8R8G8B8, vec![0; 15]),
        (2, 2, 8, PIXMAN_X8R8G8B8, vec![0; 17]),
        (2, 2, u32::MAX, PIXMAN_X8R8G8B8, vec![]),
        (2, 2, 8, 0xDEAD_BEEF, vec![0; 16]),
    ] {
        assert!(
            surface
                .scanout(width, height, stride, format, &data)
                .is_err()
        );
        assert_eq!(surface.frame().unwrap(), &expected);
        assert_eq!(surface.stats().scanouts, 1);
    }
    for (x, y, w, h) in [
        (-1, 0, 1, 1),
        (0, -1, 1, 1),
        (1, 0, 2, 1),
        (0, 1, 1, 2),
        (0, 0, 0, 1),
        (i32::MAX, 0, i32::MAX, 1),
    ] {
        assert!(
            surface
                .update(x, y, w, h, 4, PIXMAN_X8R8G8B8, &[0; 4])
                .is_err()
        );
        assert_eq!(surface.frame().unwrap(), &expected);
    }
}

#[test]
fn overlapping_successive_updates_resize_disable_and_reset() {
    let mut surface = CpuSurface::new(PixelByteOrder::Little);
    assert!(
        surface
            .update(0, 0, 1, 1, 4, PIXMAN_X8R8G8B8, &[0; 4])
            .is_err()
    );
    surface.scanout(2, 2, 8, PIXMAN_X8R8G8B8, &[0; 16]).unwrap();
    surface
        .update(
            0,
            0,
            2,
            1,
            8,
            PIXMAN_X8R8G8B8,
            &[0, 0, 255, 0, 0, 255, 0, 0],
        )
        .unwrap();
    surface
        .update(
            1,
            0,
            1,
            2,
            4,
            PIXMAN_X8R8G8B8,
            &[255, 0, 0, 0, 255, 0, 0, 0],
        )
        .unwrap();
    assert_eq!(
        surface.frame().unwrap().pixels,
        [255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 0, 255, 0, 0, 255, 255]
    );
    surface
        .scanout(1, 1, 4, PIXMAN_X8R8G8B8, &[255, 255, 255, 0])
        .unwrap();
    assert_eq!(
        (
            surface.frame().unwrap().width,
            surface.frame().unwrap().height
        ),
        (1, 1)
    );
    surface.disable();
    assert!(surface.frame().is_none());
    assert!(
        surface
            .update(0, 0, 1, 1, 4, PIXMAN_X8R8G8B8, &[0; 4])
            .is_err()
    );
    surface.scanout(1, 1, 4, PIXMAN_X8R8G8B8, &[0; 4]).unwrap();
    surface.reset();
    assert!(surface.frame().is_none());
}

struct MockVm;
pub(super) const MOCK_UUID: &str = "00000000-0000-4000-8000-000000000003";
#[zbus::interface(name = "org.qemu.Display1.VM")]
impl MockVm {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        MOCK_UUID
    }
    #[zbus(property, name = "ConsoleIDs")]
    fn console_ids(&self) -> Vec<u32> {
        vec![0]
    }
}

struct MockConsole {
    peers: mpsc::Sender<Connection>,
}
#[zbus::interface(name = "org.qemu.Display1.Console", spawn = false)]
impl MockConsole {
    #[zbus(property, name = "Type")]
    fn console_type(&self) -> &str {
        "Graphic"
    }
    async fn register_listener(&self, listener: OwnedFd) -> zbus::fdo::Result<()> {
        let fd = std::os::fd::OwnedFd::from(listener);
        let stream = std::os::unix::net::UnixStream::from(fd);
        stream
            .set_nonblocking(true)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let stream = UnixStream::from_std(stream)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let peer = Builder::unix_stream(stream)
            .p2p()
            .server(Guid::generate())
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .build()
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        self.peers
            .send(peer)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("mock receiver closed".into()))?;
        Ok(())
    }
}

pub(super) async fn mock_control() -> (Connection, Connection, mpsc::Receiver<Connection>) {
    let (server, client) = UnixStream::pair().unwrap();
    let (peers, receiver) = mpsc::channel(1);
    let builder = Builder::unix_stream(server)
        .p2p()
        .server(Guid::generate())
        .unwrap()
        .serve_at("/org/qemu/Display1/VM", MockVm)
        .unwrap()
        .serve_at("/org/qemu/Display1/Console_0", MockConsole { peers })
        .unwrap();
    let (server, client) =
        tokio::join!(builder.build(), Builder::unix_stream(client).p2p().build());
    (server.unwrap(), client.unwrap(), receiver)
}

#[tokio::test(flavor = "current_thread")]
async fn real_mock_peer_registration_fd_passing_and_cancellation() {
    timeout(Duration::from_secs(5), async {
        let (control, client, mut peers) = mock_control().await;
        let mut source = connect_control(client, MOCK_UUID, 0).await.unwrap();
        let peer = peers.recv().await.unwrap();
        assert!(
            timeout(Duration::from_millis(5), source.read_event())
                .await
                .is_err()
        );
        peer.call_method(
            None::<&str>,
            LISTENER_PATH,
            Some("org.qemu.Display1.Listener"),
            "Scanout",
            &(2u32, 2u32, 8u32, PIXMAN_X8R8G8B8, &[0u8; 16][..]),
        )
        .await
        .unwrap();
        let event = source.read_event().await.unwrap();
        assert_eq!(event.frame.unwrap().pixels, [0, 0, 0, 255].repeat(4));
        assert_eq!(event.stats.scanouts, 1);
        for color in 1..=4u8 {
            peer.call_method(
                None::<&str>,
                LISTENER_PATH,
                Some("org.qemu.Display1.Listener"),
                "Update",
                &(
                    1i32,
                    1i32,
                    1i32,
                    1i32,
                    4u32,
                    PIXMAN_X8R8G8B8,
                    &[color, 0, 0, 0][..],
                ),
            )
            .await
            .unwrap();
        }
        let event = source.read_event().await.unwrap();
        assert_eq!(&event.frame.unwrap().pixels[12..], [0, 0, 4, 255]);
        assert_eq!(event.stats.updates, 4); // Notifications coalesce; canonical pixels do not drop updates.
        assert!(
            peer.call_method(
                None::<&str>,
                LISTENER_PATH,
                Some("org.qemu.Display1.Listener"),
                "Update",
                &(2i32, 2i32, 1i32, 1i32, 4u32, PIXMAN_X8R8G8B8, &[0u8; 4][..])
            )
            .await
            .is_err()
        );
        assert!(
            timeout(Duration::from_millis(5), source.read_event())
                .await
                .is_err()
        );
        peer.call_method(
            None::<&str>,
            LISTENER_PATH,
            Some("org.qemu.Display1.Listener"),
            "CursorDefine",
            &(1i32, 1i32, 0i32, 0i32, &0x80ff0000u32.to_ne_bytes()[..]),
        )
        .await
        .unwrap();
        let event = source.read_event().await.unwrap();
        assert!(event.frame.is_none());
        assert_eq!(event.cursor.unwrap().argb, [0x80ff0000]);
        peer.call_method(
            None::<&str>,
            LISTENER_PATH,
            Some("org.qemu.Display1.Listener"),
            "Disable",
            &(),
        )
        .await
        .unwrap();
        let event = source.read_event().await.unwrap();
        assert!(event.disabled && event.frame.is_none());
        source.close_checked().await.unwrap();
        peer.closed().await;
        control.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_vm_console_and_remote_close_are_detected() {
    timeout(Duration::from_secs(5), async {
        let (server, client, _) = mock_control().await;
        assert!(connect_control(client, "wrong-uuid", 0).await.is_err());
        server.close().await.unwrap();
        let (server, client, _) = mock_control().await;
        assert!(connect_control(client, MOCK_UUID, 1).await.is_err());
        server.close().await.unwrap();
        let (server, client, mut peers) = mock_control().await;
        let mut source = connect_control(client, MOCK_UUID, 0).await.unwrap();
        peers.recv().await.unwrap().close().await.unwrap();
        assert!(source.read_event().await.is_err());
        source.close().await;
        server.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn hold_wait_detects_disconnect_without_consuming_frames() {
    timeout(Duration::from_secs(5), async {
        let (server, client, mut peers) = mock_control().await;
        let mut source = connect_control(client, MOCK_UUID, 0).await.unwrap();
        let peer = peers.recv().await.unwrap();
        peer.call_method(
            None::<&str>,
            LISTENER_PATH,
            Some("org.qemu.Display1.Listener"),
            "Scanout",
            &(2u32, 2u32, 8u32, PIXMAN_X8R8G8B8, &[0u8; 16][..]),
        )
        .await
        .unwrap();
        assert!(
            timeout(Duration::from_millis(5), source.wait_closed())
                .await
                .is_err()
        );
        assert_eq!(source.last_frame_revision, 0);
        assert_eq!(source.surface.lock().unwrap().stats.scanouts, 1);
        peer.close().await.unwrap();
        source.wait_closed().await;
        assert!(source.read_event().await.is_err());
        source.close().await;
        server.close().await.unwrap();
    })
    .await
    .unwrap();
}
