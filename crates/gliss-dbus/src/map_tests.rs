//! Candidate tests; authored only. No test in this file has been executed.

use super::*;
use crate::tests::{MOCK_UUID, mock_control};
use crate::{
    CaptureOptions, Connection, Duration, Fd, LISTENER_PATH, MAP_INTERFACE, PIXMAN_A8B8G8R8,
    PIXMAN_A8R8G8B8, PIXMAN_B5G6R5, PIXMAN_B8G8R8, PIXMAN_R5G6B5, PIXMAN_R8G8B8, PIXMAN_X8B8G8R8,
    PIXMAN_X8R8G8B8, Proxy, connect_control, connect_control_with_options, timeout,
};
use std::{
    fs::OpenOptions,
    os::fd::AsRawFd,
    sync::atomic::{AtomicU64, Ordering},
};

fn backing(bytes: &[u8]) -> File {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let path = std::env::temp_dir().join(format!(
            "gliss-owned-map-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                // Unlinked ordinary files also exercise QEMU's non-memfd case.
                std::fs::remove_file(path).unwrap();
                file.write_all_at(bytes, 0).unwrap();
                return file;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("owned backing file: {error}"),
        }
    }
}

fn opted_surface(order: PixelByteOrder) -> CpuSurface {
    let mut surface = CpuSurface::new(order);
    surface.stats.map.advertised = true;
    surface
}

#[test]
fn nonzero_offset_padded_stride_byte_order_damage_and_owned_snapshot() {
    let little = [
        0, 0, 255, 0, 255, 0, 0, 0, 7, 7, 7, 7, 0, 255, 0, 0, 255, 255, 255, 0, 9, 9, 9, 9,
    ];
    let big = [
        0, 255, 0, 0, 0, 0, 0, 255, 7, 7, 7, 7, 0, 0, 255, 0, 0, 255, 255, 255, 9, 9, 9, 9,
    ];
    let expected = [
        255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 255, 255, 255, 255, 255,
    ];
    for (bytes, order) in [(little, PixelByteOrder::Little), (big, PixelByteOrder::Big)] {
        let mut data = vec![0x7b; 7];
        data.extend_from_slice(&bytes);
        let file = backing(&data);
        let mut surface = opted_surface(order);
        surface
            .scanout_map(&file, 7, 2, 2, 12, PIXMAN_X8R8G8B8)
            .unwrap();
        let snapshot = surface.frame().unwrap().clone();
        assert_eq!(snapshot.pixels, expected);
        let pixel = if order.little() {
            [51, 34, 17, 0]
        } else {
            [0, 17, 34, 51]
        };
        file.write_all_at(&pixel, 7 + 12 + 4).unwrap();
        surface.update_map([1, 1, 1, 1]).unwrap();
        assert_eq!(&surface.frame().unwrap().pixels[12..], [17, 34, 51, 255]);
        assert_eq!(snapshot.pixels, expected);
        assert_eq!(surface.stats.map.last_damage, Some([1, 1, 1, 1]));
        assert_eq!(surface.stats.map.scanout_maps, 1);
        assert_eq!(surface.stats.map.update_maps, 1);
        assert_eq!(surface.stats.map.bytes_read, 20); // Pixel bytes; padding is skipped.
        assert_eq!(surface.stats.source_bytes, 0);
    }
}

#[test]
fn supported_formats_native_order_and_opaque_alpha_contract() {
    for (format, bytes, expected) in [
        (PIXMAN_X8R8G8B8, vec![51, 34, 17, 0], [17, 34, 51, 255]),
        (PIXMAN_X8B8G8R8, vec![17, 34, 51, 0], [17, 34, 51, 255]),
        (PIXMAN_A8R8G8B8, vec![51, 34, 17, 255], [17, 34, 51, 255]),
        (PIXMAN_A8B8G8R8, vec![17, 34, 51, 255], [17, 34, 51, 255]),
        (PIXMAN_R8G8B8, vec![51, 34, 17], [17, 34, 51, 255]),
        (PIXMAN_B8G8R8, vec![17, 34, 51], [17, 34, 51, 255]),
        (PIXMAN_R5G6B5, vec![0, 248], [255, 0, 0, 255]),
        (PIXMAN_B5G6R5, vec![0, 248], [0, 0, 255, 255]),
    ] {
        let file = backing(&bytes);
        let mut surface = opted_surface(PixelByteOrder::Little);
        surface
            .scanout_map(&file, 0, 1, 1, bytes.len() as u32, format)
            .unwrap();
        assert_eq!(surface.frame().unwrap().pixels, expected);
        let big = backing(&bytes.into_iter().rev().collect::<Vec<_>>());
        let mut surface = opted_surface(PixelByteOrder::Big);
        surface
            .scanout_map(&big, 0, 1, 1, surface_size(format), format)
            .unwrap();
        assert_eq!(surface.frame().unwrap().pixels, expected);
    }
    let file = backing(&0xff11_2233u32.to_ne_bytes());
    let mut native = opted_surface(PixelByteOrder::Native);
    native
        .scanout_map(&file, 0, 1, 1, 4, PIXMAN_A8R8G8B8)
        .unwrap();
    assert_eq!(native.frame().unwrap().pixels, [17, 34, 51, 255]);
    for format in [PIXMAN_A8R8G8B8, PIXMAN_A8B8G8R8] {
        let bad = backing(&[0, 0, 0, 128]);
        let mut surface = opted_surface(PixelByteOrder::Little);
        assert!(surface.scanout_map(&bad, 0, 1, 1, 4, format).is_err());
        assert!(surface.frame().is_none() && surface.map.is_none());
        assert_eq!(surface.stats.map.scanout_maps, 0);
    }
}

fn surface_size(format: u32) -> u32 {
    match format {
        PIXMAN_R8G8B8 | PIXMAN_B8G8R8 => 3,
        PIXMAN_R5G6B5 | PIXMAN_B5G6R5 => 2,
        _ => 4,
    }
}

#[test]
fn geometry_offset_stride_bounds_and_truncated_candidates_fail_closed() {
    let file = backing(&[0; 16]);
    for (offset, width, height, stride, format) in [
        (0, 0, 1, 4, PIXMAN_X8R8G8B8),
        (0, 1, 0, 4, PIXMAN_X8R8G8B8),
        (0, 65536, 1, 262144, PIXMAN_X8R8G8B8),
        (0, 65535, 65535, 262140, PIXMAN_X8R8G8B8),
        (0, u32::MAX, u32::MAX, u32::MAX, PIXMAN_X8R8G8B8),
        (0, 2, 2, 7, PIXMAN_X8R8G8B8),
        (0, 2, 2, u32::MAX, PIXMAN_X8R8G8B8),
        (u32::MAX, 1, 1, 4, PIXMAN_X8R8G8B8),
        (1, 2, 2, 8, PIXMAN_X8R8G8B8),
        (0, 2, 2, 8, 0xdead_beef),
    ] {
        let mut surface = opted_surface(PixelByteOrder::Native);
        surface
            .scanout_map(&file, 0, 2, 2, 8, PIXMAN_X8R8G8B8)
            .unwrap();
        assert!(
            surface
                .scanout_map(&file, offset, width, height, stride, format)
                .is_err()
        );
        assert!(surface.frame().is_none() && surface.map.is_none() && surface.disabled);
        assert_eq!(surface.stats.map.scanout_map_errors, 1);
        assert_eq!(surface.stats.map.active_generation, None);
    }
    let short = backing(&[0; 15]);
    let mut surface = opted_surface(PixelByteOrder::Native);
    assert!(
        surface
            .scanout_map(&short, 0, 2, 2, 8, PIXMAN_X8R8G8B8)
            .is_err()
    );
    let (socket, _other) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(
        surface
            .scanout_map(&socket, 0, 1, 1, 4, PIXMAN_X8R8G8B8)
            .is_err()
    );
}

#[test]
fn invalid_damage_is_rejected_and_does_not_publish_a_clamped_rectangle() {
    let file = backing(&[0; 16]);
    for damage in [
        [-1, 0, 1, 1],
        [0, -1, 1, 1],
        [0, 0, 0, 1],
        [0, 0, 1, 0],
        [1, 0, 2, 1],
        [0, 1, 1, 2],
        [i32::MAX, 0, i32::MAX, 1],
    ] {
        let mut surface = opted_surface(PixelByteOrder::Native);
        surface
            .scanout_map(&file, 0, 2, 2, 8, PIXMAN_X8R8G8B8)
            .unwrap();
        assert!(surface.update_map(damage).is_err());
        assert!(surface.frame().is_none() && surface.map.is_none());
        assert_eq!(surface.stats.map.update_maps, 0);
        assert_eq!(surface.stats.map.update_map_errors, 1);
    }
}

#[test]
fn truncation_growth_short_read_and_midcopy_extent_change_invalidate_generation() {
    for extent in [7, 12] {
        let file = backing(&[0; 8]);
        let mut surface = opted_surface(PixelByteOrder::Little);
        surface
            .scanout_map(&file, 0, 1, 2, 4, PIXMAN_X8R8G8B8)
            .unwrap();
        file.set_len(extent).unwrap();
        assert!(surface.update_map([0, 0, 1, 2]).is_err());
        assert!(surface.frame().is_none() && surface.map.is_none());
        assert!(
            surface
                .stats
                .map
                .last_error
                .as_ref()
                .unwrap()
                .contains("extent changed")
        );
    }
    for shrink in [true, false] {
        let file = backing(&[0; 8]);
        let mut surface = opted_surface(PixelByteOrder::Little);
        surface
            .scanout_map(&file, 0, 1, 2, 4, PIXMAN_X8R8G8B8)
            .unwrap();
        let snapshot = surface.frame().unwrap().clone();
        file.write_all_at(&[51, 34, 17, 0], 0).unwrap();
        let result = surface.update_map_using([0, 0, 1, 2], |handle, row, offset| {
            if offset == 4 && shrink {
                file.set_len(4)?; // First row has already changed canonical bytes.
            }
            let read = handle.read_at(row, offset)?;
            if offset == 4 && !shrink {
                file.set_len(12)?; // All reads complete; post-read fstat must fail.
            }
            Ok(read)
        });
        assert!(result.is_err());
        assert!(surface.frame().is_none() && surface.map.is_none() && surface.disabled);
        assert_eq!(surface.stats.map.update_maps, 0);
        assert_eq!(snapshot.pixels, [0, 0, 0, 255].repeat(2));
        let error = surface.stats.map.last_error.as_ref().unwrap();
        assert!(error.contains(if shrink {
            "short Map read"
        } else {
            "extent changed"
        }));
    }
}

#[test]
fn partial_reads_and_interruption_finish_before_publication_but_io_and_alpha_errors_clear_it() {
    let file = backing(&[0, 0, 0, 255, 0, 0, 0, 255]);
    let mut surface = opted_surface(PixelByteOrder::Little);
    surface
        .scanout_map(&file, 0, 1, 2, 4, PIXMAN_A8R8G8B8)
        .unwrap();
    let mut interrupted = false;
    surface
        .update_map_using([0, 0, 1, 2], |handle, row, offset| {
            if !interrupted {
                interrupted = true;
                return Err(io::Error::from(ErrorKind::Interrupted));
            }
            handle.read_at(&mut row[..1], offset)
        })
        .unwrap();
    assert_eq!(surface.stats.map.update_maps, 1);
    assert_eq!(surface.stats.map.bytes_read, 16);
    assert_eq!(surface.stats.map.read_calls, 11);
    file.write_all_at(&[51, 34, 17, 255, 0, 0, 0, 128], 0)
        .unwrap();
    assert!(surface.update_map([0, 0, 1, 2]).is_err());
    assert!(surface.frame().is_none() && surface.map.is_none());
    assert!(
        surface
            .stats
            .map
            .last_error
            .as_ref()
            .unwrap()
            .contains("alpha")
    );
    file.write_all_at(&[0, 0, 0, 255], 4).unwrap();
    surface
        .scanout_map(&file, 0, 1, 2, 4, PIXMAN_A8R8G8B8)
        .unwrap();
    assert!(
        surface
            .update_map_using([0, 0, 1, 2], |_, _, _| {
                Err(io::Error::from(ErrorKind::PermissionDenied))
            })
            .is_err()
    );
    assert!(surface.frame().is_none() && surface.map.is_none());
}

#[test]
fn replacement_resize_array_fallback_disable_and_reset_release_owned_generations() {
    let file = backing(&[0; 16]);
    let mut surface = opted_surface(PixelByteOrder::Native);
    surface
        .scanout_map(&file, 0, 2, 2, 8, PIXMAN_X8R8G8B8)
        .unwrap();
    surface
        .scanout_map(&file, 0, 1, 1, 4, PIXMAN_X8R8G8B8)
        .unwrap();
    assert_eq!(
        (
            surface.frame().unwrap().width,
            surface.frame().unwrap().height
        ),
        (1, 1)
    );
    assert_eq!(surface.stats.map.active_generation, Some(2));
    assert_eq!(surface.stats.map.replacements, 1);
    assert_eq!(surface.stats.map.released_generations, 1);
    surface
        .update(0, 0, 1, 1, 4, PIXMAN_X8R8G8B8, &[0; 4])
        .unwrap();
    assert!(surface.map.is_none() && surface.frame().is_some());
    assert_eq!(surface.stats.map.array_updates, 1);
    assert_eq!(surface.stats.map.array_bytes, 4);
    surface
        .scanout_map(&file, 0, 1, 1, 4, PIXMAN_X8R8G8B8)
        .unwrap();
    surface.scanout(1, 1, 4, PIXMAN_X8R8G8B8, &[0; 4]).unwrap();
    assert!(surface.map.is_none());
    assert_eq!(surface.stats.map.array_scanouts, 1);
    surface
        .scanout_map(&file, 0, 1, 1, 4, PIXMAN_X8R8G8B8)
        .unwrap();
    surface.disable();
    assert!(surface.map.is_none() && surface.frame().is_none());
    assert!(surface.update_map([0, 0, 1, 1]).is_err());
    surface
        .scanout_map(&file, 0, 1, 1, 4, PIXMAN_X8R8G8B8)
        .unwrap();
    surface.reset();
    assert!(surface.map.is_none() && surface.frame().is_none());
    assert_eq!(surface.stats.map.released_generations, 5);
}

#[cfg(target_os = "linux")]
#[test]
fn retained_descriptor_is_cloexec_and_disable_closes_it() {
    let file = backing(&[0; 4]);
    let mut surface = opted_surface(PixelByteOrder::Native);
    surface
        .scanout_map(&file, 0, 1, 1, 4, PIXMAN_X8R8G8B8)
        .unwrap();
    let retained = surface.map.as_ref().unwrap().file.as_raw_fd();
    assert_ne!(retained, file.as_raw_fd());
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{retained}")).unwrap();
    let flags = info
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .unwrap();
    assert_ne!(u32::from_str_radix(flags, 8).unwrap() & 0o2000000, 0);
    surface.disable();
    assert!(std::fs::read_link(format!("/proc/self/fd/{retained}")).is_err());
}

async fn send_map(
    peer: &Connection,
    file: &File,
    width: u32,
    height: u32,
    stride: u32,
) -> zbus::Result<zbus::Message> {
    peer.call_method(
        None::<&str>,
        LISTENER_PATH,
        Some(MAP_INTERFACE),
        "ScanoutMap",
        &(
            Fd::from(file.as_fd()),
            3u32,
            width,
            height,
            stride,
            PIXMAN_X8R8G8B8,
        ),
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn real_fd_transfer_optional_negotiation_cancellation_fallback_and_disconnect() {
    timeout(Duration::from_secs(5), async {
        for advertised in [false, true] {
            let (control, client, mut peers) = mock_control().await;
            let mut source = if advertised {
                connect_control_with_options(
                    client,
                    MOCK_UUID,
                    0,
                    CaptureOptions { unix_map: true },
                )
                .await
                .unwrap()
            } else {
                connect_control(client, MOCK_UUID, 0).await.unwrap()
            };
            let peer = peers.recv().await.unwrap();
            let listener = Proxy::new(
                &peer,
                "org.qemu",
                LISTENER_PATH,
                "org.qemu.Display1.Listener",
            )
            .await
            .unwrap();
            let interfaces: Vec<String> = listener.get_property("Interfaces").await.unwrap();
            assert_eq!(
                interfaces,
                if advertised {
                    vec![MAP_INTERFACE.to_owned()]
                } else {
                    vec![]
                }
            );
            drop(listener);
            let mut bytes = vec![0x7b; 3];
            bytes.extend_from_slice(&0x00ff_0000u32.to_ne_bytes());
            bytes.extend_from_slice(&[9; 4]);
            bytes.extend_from_slice(&0x0000_ff00u32.to_ne_bytes());
            let file = backing(&bytes);
            let writer = file.try_clone().unwrap();
            let sent = send_map(&peer, &file, 1, 2, 8).await;
            drop(file); // The receiver must retain its own descriptor.
            if !advertised {
                assert!(sent.is_err());
                assert_eq!(source.map_stats().unwrap().scanout_map_calls, 0);
                peer.call_method(
                    None::<&str>,
                    LISTENER_PATH,
                    Some("org.qemu.Display1.Listener"),
                    "Scanout",
                    &(1u32, 1u32, 4u32, PIXMAN_X8R8G8B8, &[0u8; 4][..]),
                )
                .await
                .unwrap();
                let event = source.read_event().await.unwrap();
                assert_eq!(event.stats.scanouts, 1);
                assert!(!event.stats.map.advertised);
                source.close_checked().await.unwrap();
                control.close().await.unwrap();
                continue;
            }
            sent.unwrap();
            let initial = source.read_event().await.unwrap();
            let snapshot = initial.frame.unwrap();
            assert_eq!(snapshot.pixels, [255, 0, 0, 255, 0, 255, 0, 255]);
            assert_eq!(initial.stats.map.scanout_map_calls, 1);
            assert_eq!(initial.stats.map.scanout_maps, 1);
            assert_eq!(
                initial.stats.map.last_description.unwrap().inode,
                writer.metadata().unwrap().ino()
            );
            assert_eq!(initial.stats.scanouts, 0);
            assert!(
                timeout(Duration::from_millis(5), source.read_event())
                    .await
                    .is_err()
            );
            writer
                .write_all_at(&0x0011_2233u32.to_ne_bytes(), 11)
                .unwrap();
            peer.call_method(
                None::<&str>,
                LISTENER_PATH,
                Some(MAP_INTERFACE),
                "UpdateMap",
                &(0i32, 1i32, 1i32, 1i32),
            )
            .await
            .unwrap();
            let update = source.read_event().await.unwrap();
            assert_eq!(&update.frame.unwrap().pixels[4..], [17, 34, 51, 255]);
            assert_eq!(snapshot.pixels, [255, 0, 0, 255, 0, 255, 0, 255]);
            assert_eq!(update.stats.map.update_map_calls, 1);
            assert_eq!(update.stats.map.update_maps, 1);
            assert!(
                peer.call_method(
                    None::<&str>,
                    LISTENER_PATH,
                    Some(MAP_INTERFACE),
                    "UpdateMap",
                    &(0i32, 1i32, 1i32, 2i32)
                )
                .await
                .is_err()
            );
            let invalid = source.read_event().await.unwrap();
            assert!(invalid.disabled && invalid.frame.is_none());
            assert_eq!(invalid.stats.map.update_map_errors, 1);
            peer.call_method(
                None::<&str>,
                LISTENER_PATH,
                Some("org.qemu.Display1.Listener"),
                "Scanout",
                &(1u32, 1u32, 4u32, PIXMAN_X8R8G8B8, &[0u8; 4][..]),
            )
            .await
            .unwrap();
            let fallback = source.read_event().await.unwrap();
            assert_eq!(fallback.stats.map.array_scanout_calls, 1);
            assert_eq!(fallback.stats.map.array_scanouts, 1);
            assert_eq!(fallback.stats.map.array_bytes, 4);
            send_map(&peer, &writer, 1, 1, 4).await.unwrap();
            source.read_event().await.unwrap();
            peer.call_method(
                None::<&str>,
                LISTENER_PATH,
                Some("org.qemu.Display1.Listener"),
                "Disable",
                &(),
            )
            .await
            .unwrap();
            let disabled = source.read_event().await.unwrap();
            assert!(disabled.disabled && disabled.frame.is_none());
            assert_eq!(disabled.stats.map.active_generation, None);
            send_map(&peer, &writer, 1, 1, 4).await.unwrap();
            source.read_event().await.unwrap();
            assert!(
                timeout(Duration::from_millis(5), source.wait_closed())
                    .await
                    .is_err()
            );
            peer.close().await.unwrap();
            source.wait_closed().await;
            {
                let surface = source.surface.lock().unwrap();
                assert!(surface.closed && surface.map.is_none() && surface.frame().is_none());
                assert_eq!(surface.stats.map.active_generation, None);
            }
            source.close_checked().await.unwrap();
            control.close().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_source_runs_owned_cleanup_and_releases_active_map() {
    timeout(Duration::from_secs(5), async {
        let (control, client, mut peers) = mock_control().await;
        let source =
            connect_control_with_options(client, MOCK_UUID, 0, CaptureOptions { unix_map: true })
                .await
                .unwrap();
        let peer = peers.recv().await.unwrap();
        let file = backing(&[0; 7]);
        send_map(&peer, &file, 1, 1, 4).await.unwrap();
        let surface = source.surface.clone();
        assert!(surface.lock().unwrap().map.is_some());
        drop(source); // Drop requests the existing bounded cleanup task.
        peer.closed().await;
        {
            let surface = surface.lock().unwrap();
            assert!(surface.closed && surface.disabled && surface.map.is_none());
            assert!(surface.frame().is_none());
            assert_eq!(surface.stats.map.released_generations, 1);
        }
        control.close().await.unwrap();
    })
    .await
    .unwrap();
}
