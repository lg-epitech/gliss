use gliss_core::Input;
use gliss_rfb::{Event, connect};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{Duration, timeout},
};

const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];

async fn init(stream: &mut TcpStream, minor: u8, width: u16, height: u16) {
    init_named(stream, minor, width, height, "test").await;
}

async fn init_named(stream: &mut TcpStream, minor: u8, width: u16, height: u16, name: &str) {
    stream
        .write_all(format!("RFB 003.{minor:03}\n").as_bytes())
        .await
        .unwrap();
    let mut version = [0; 12];
    stream.read_exact(&mut version).await.unwrap();
    assert_eq!(&version, format!("RFB 003.{minor:03}\n").as_bytes());
    if minor == 3 {
        stream.write_u32(1).await.unwrap();
    } else {
        stream.write_all(&[1, 1]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1);
        if minor == 8 {
            stream.write_u32(0).await.unwrap();
        }
    }
    init_after_security_named(stream, width, height, name).await;
}

async fn init_after_security(stream: &mut TcpStream, width: u16, height: u16) {
    init_after_security_named(stream, width, height, "test").await;
}

async fn init_after_security_named(stream: &mut TcpStream, width: u16, height: u16, name: &str) {
    assert_eq!(stream.read_u8().await.unwrap(), 1, "shared connection");
    stream.write_u16(width).await.unwrap();
    stream.write_u16(height).await.unwrap();
    stream.write_all(&PIXEL_FORMAT).await.unwrap();
    stream.write_u32(name.len() as u32).await.unwrap();
    stream.write_all(name.as_bytes()).await.unwrap();
    let mut format = [0; 20];
    stream.read_exact(&mut format).await.unwrap();
    assert_eq!(&format[..4], &[0, 0, 0, 0]);
    assert_eq!(&format[4..], &PIXEL_FORMAT);
    assert_eq!(stream.read_u8().await.unwrap(), 2);
    stream.read_u8().await.unwrap();
    let count = stream.read_u16().await.unwrap();
    let mut encodings = Vec::new();
    for _ in 0..count {
        encodings.push(stream.read_i32().await.unwrap());
    }
    assert_eq!(
        encodings,
        [0, 1, -239, -223, -308, 0xc0a1_e5ceu32 as i32, -259]
    );
}

async fn rectangle(stream: &mut TcpStream, x: u16, y: u16, width: u16, height: u16, encoding: i32) {
    for value in [x, y, width, height] {
        stream.write_u16(value).await.unwrap();
    }
    stream.write_i32(encoding).await.unwrap();
}

async fn event(reader: &mut gliss_rfb::RfbReader) -> anyhow::Result<Event> {
    timeout(Duration::from_secs(2), reader.read_event())
        .await
        .unwrap()
}

async fn audio_ack(stream: &mut TcpStream) {
    stream.write_all(&[0, 0, 0, 1]).await.unwrap();
    rectangle(stream, 0, 0, 16, 16, -259).await;
}

#[tokio::test]
async fn qemu_audio_waits_for_capability_and_sets_format_before_enabling() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 16, 16).await;
        // Ordinary servers are never sent QEMU controls before acknowledging.
        let mut key = [0; 8];
        stream.read_exact(&mut key).await.unwrap();
        assert_eq!(key, [4, 1, 0, 0, 0, 0, 0, 65]);
        audio_ack(&mut stream).await;
        let mut enable = [0; 14];
        stream.read_exact(&mut enable).await.unwrap();
        assert_eq!(enable, [255, 1, 0, 2, 3, 2, 0, 0, 187, 128, 255, 1, 0, 0]);
        stream.write_all(&[255, 1, 0, 1]).await.unwrap();
        stream
            .write_all(&[255, 1, 0, 2, 0, 0, 0, 8, 1, 0, 255, 127, 0, 128, 255, 255])
            .await
            .unwrap();
        stream.write_all(&[2]).await.unwrap();
        let mut disable = [0; 4];
        stream.read_exact(&mut disable).await.unwrap();
        assert_eq!(disable, [255, 1, 0, 1]);
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    let mut audio = reader.audio_events();
    assert!(writer.set_audio(true).await.is_err());
    writer
        .send_input(&Input::Key {
            keysym: 65,
            down: true,
            id: 1,
        })
        .await
        .unwrap();
    let mut available = writer.audio_available();
    timeout(Duration::from_secs(2), available.wait_for(|value| *value))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::Unchanged
    ));
    writer.set_audio(true).await.unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(2), audio.recv())
            .await
            .unwrap()
            .unwrap(),
        gliss_rfb::audio::Event::Begin
    ));
    let gliss_rfb::audio::Event::Data { bytes, .. } = timeout(Duration::from_secs(2), audio.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("audio data expected")
    };
    assert_eq!(bytes.as_ref(), &[1, 0, 255, 127, 0, 128, 255, 255]);
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Bell));
    writer.set_audio(false).await.unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn stalled_audio_consumer_does_not_block_display_events_or_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 16, 16).await;
        audio_ack(&mut stream).await;
        let mut enable = [0; 14];
        stream.read_exact(&mut enable).await.unwrap();
        for number in 0..64u32 {
            stream.write_all(&[255, 1, 0, 2, 0, 0, 0, 4]).await.unwrap();
            stream.write_all(&number.to_le_bytes()).await.unwrap();
        }
        stream.write_all(&[2]).await.unwrap();
        let mut key = [0; 8];
        stream.read_exact(&mut key).await.unwrap();
        assert_eq!(key, [4, 0, 0, 0, 0, 0, 0, 65]);
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    let mut audio = reader.audio_events();
    let mut available = writer.audio_available();
    timeout(Duration::from_secs(2), available.wait_for(|value| *value))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::Unchanged
    ));
    writer.set_audio(true).await.unwrap();
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Bell));
    assert!(matches!(
        audio.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    writer
        .send_input(&Input::Key {
            keysym: 65,
            down: false,
            id: 2,
        })
        .await
        .unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn raw_and_overlapping_copyrect_produce_opaque_rgba_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 3, 2).await;
        let mut request = [0; 10];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(request, [3, 0, 0, 0, 0, 0, 0, 3, 0, 2]);
        stream.write_all(&[0, 0, 0, 1]).await.unwrap();
        rectangle(&mut stream, 0, 0, 3, 2, 0).await;
        stream
            .write_all(&[
                1, 2, 3, 0, 4, 5, 6, 9, 7, 8, 9, 0, 10, 11, 12, 0, 13, 14, 15, 0, 16, 17, 18, 0,
            ])
            .await
            .unwrap();
        stream.write_all(&[0, 0, 0, 1]).await.unwrap();
        rectangle(&mut stream, 1, 0, 2, 2, 1).await;
        stream.write_all(&[0, 0, 0, 0]).await.unwrap();
    });
    let (mut reader, mut writer, info) = connect(&address.to_string(), None).await.unwrap();
    assert_eq!((info.width, info.height), (3, 2));
    writer.request_update(false, 3, 2).await.unwrap();
    let Event::Frame(first) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!(
        first.pixels,
        [
            1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255, 13, 14, 15, 255, 16, 17, 18,
            255
        ]
    );
    let Event::Frame(copied) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!(
        copied.pixels,
        [
            1, 2, 3, 255, 1, 2, 3, 255, 4, 5, 6, 255, 10, 11, 12, 255, 10, 11, 12, 255, 13, 14, 15,
            255
        ]
    );
    peer.await.unwrap();
}

/// QEMU's native format: blue in the low byte.
const BGRX_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];

/// A server that stores BGRX and, like QEMU, sends whatever format it is asked for.
async fn init_bgrx(stream: &mut TcpStream, width: u16, height: u16) {
    stream.write_all(b"RFB 003.008\n").await.unwrap();
    let mut version = [0; 12];
    stream.read_exact(&mut version).await.unwrap();
    stream.write_all(&[1, 1]).await.unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), 1);
    stream.write_u32(0).await.unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), 1, "shared connection");
    stream.write_u16(width).await.unwrap();
    stream.write_u16(height).await.unwrap();
    stream.write_all(&BGRX_FORMAT).await.unwrap();
    stream.write_u32(4).await.unwrap();
    stream.write_all(b"QEMU").await.unwrap();
    let mut format = [0; 20];
    stream.read_exact(&mut format).await.unwrap();
    assert_eq!(&format[..4], &[0, 0, 0, 0]);
    assert_eq!(
        &format[4..],
        &BGRX_FORMAT,
        "a BGRX server must be asked for its own format"
    );
    assert_eq!(stream.read_u8().await.unwrap(), 2);
    stream.read_u8().await.unwrap();
    let count = stream.read_u16().await.unwrap();
    for _ in 0..count {
        stream.read_i32().await.unwrap();
    }
}

#[tokio::test]
async fn bgrx_server_keeps_its_format_and_frames_and_cursors_arrive_as_rgba() {
    // Wide enough that a full-width rectangle spans several read pieces, with a
    // partial last piece, and a partial-width rectangle takes the per-row path.
    const WIDTH: u16 = 640;
    const HEIGHT: u16 = 250;
    let wire = |index: usize| {
        // Distinct blue, green and red per pixel; the fourth byte is undefined on the wire.
        [index as u8, (index >> 8) as u8, (index * 7) as u8, 0x5a]
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init_bgrx(&mut stream, WIDTH, HEIGHT).await;
        let mut request = [0; 10];
        stream.read_exact(&mut request).await.unwrap();
        stream.write_all(&[0, 0, 0, 1]).await.unwrap();
        rectangle(&mut stream, 0, 0, WIDTH, HEIGHT, 0).await;
        let pixels: Vec<u8> = (0..usize::from(WIDTH) * usize::from(HEIGHT))
            .flat_map(wire)
            .collect();
        stream.write_all(&pixels).await.unwrap();
        stream.write_all(&[0, 0, 0, 2]).await.unwrap();
        rectangle(&mut stream, 5, 3, 2, 2, 0).await;
        stream
            .write_all(&[1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0])
            .await
            .unwrap();
        // A 2x1 cursor with only its first pixel opaque.
        rectangle(&mut stream, 0, 0, 2, 1, -239).await;
        stream
            .write_all(&[20, 30, 40, 0, 50, 60, 70, 0, 0b1000_0000])
            .await
            .unwrap();
        // CopyRect moves pixels that are already RGBA; it must not convert them again.
        stream.write_all(&[0, 0, 0, 1]).await.unwrap();
        rectangle(&mut stream, 20, 10, 2, 2, 1).await;
        stream.write_all(&[0, 5, 0, 3]).await.unwrap();
        // The negotiated format outlives a resize.
        stream.write_all(&[0, 0, 0, 1]).await.unwrap();
        rectangle(&mut stream, 0, 0, 2, 1, -223).await;
        stream.write_all(&[0, 0, 0, 1]).await.unwrap();
        rectangle(&mut stream, 0, 0, 2, 1, 0).await;
        stream.write_all(&[1, 2, 3, 0, 4, 5, 6, 0]).await.unwrap();
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    writer.request_update(false, WIDTH, HEIGHT).await.unwrap();
    let Event::Frame(full) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    for (index, pixel) in full.pixels.as_chunks::<4>().0.iter().enumerate() {
        let [blue, green, red, _] = wire(index);
        assert_eq!(*pixel, [red, green, blue, 255], "pixel {index}");
    }
    let Event::Cursor(cursor) = event(&mut reader).await.unwrap() else {
        panic!("cursor expected")
    };
    assert_eq!(cursor.pixels, [40, 30, 20, 255, 70, 60, 50, 0]);
    let Event::Frame(patched) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    let at = |x: usize, y: usize| {
        let start = (y * usize::from(WIDTH) + x) * 4;
        patched.pixels[start..start + 4].to_vec()
    };
    assert_eq!(at(5, 3), [3, 2, 1, 255]);
    assert_eq!(at(6, 3), [6, 5, 4, 255]);
    assert_eq!(at(5, 4), [9, 8, 7, 255]);
    assert_eq!(at(6, 4), [12, 11, 10, 255]);
    let [blue, green, red, _] = wire(4 * usize::from(WIDTH) + 7);
    assert_eq!(at(7, 4), [red, green, blue, 255], "neighbour is untouched");
    let Event::Frame(copied) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    let start = (10 * usize::from(WIDTH) + 20) * 4;
    assert_eq!(
        copied.pixels[start..start + 8],
        [3, 2, 1, 255, 6, 5, 4, 255]
    );
    let Event::Frame(resized) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!((resized.width, resized.height), (2, 1));
    let Event::Frame(after) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!(after.pixels, [3, 2, 1, 255, 6, 5, 4, 255]);
    peer.await.unwrap();
}

#[tokio::test]
async fn refresh_hold_repeats_the_audio_format_only_after_the_server_confirms_audio() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 16, 16).await;
        audio_ack(&mut stream).await;
        // The hint is the format message alone: it must not enable capture.
        let mut hints = [0; 20];
        stream.read_exact(&mut hints).await.unwrap();
        let format = [255, 1, 0, 2, 3, 2, 0, 0, 187, 128];
        assert_eq!(hints[..10], format);
        assert_eq!(hints[10..], format);
        let mut key = [0; 8];
        stream.read_exact(&mut key).await.unwrap();
        assert_eq!(key, [4, 1, 0, 0, 0, 0, 0, 65]);
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    assert!(
        writer.hold_refresh().await.is_err(),
        "a server without the audio extension would drop the connection"
    );
    let mut available = writer.audio_available();
    timeout(Duration::from_secs(2), available.wait_for(|value| *value))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::Unchanged
    ));
    writer.hold_refresh().await.unwrap();
    writer.hold_refresh().await.unwrap();
    writer
        .send_input(&Input::Key {
            keysym: 65,
            down: true,
            id: 1,
        })
        .await
        .unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn vnc_authentication_uses_bit_reversed_password_bytes_and_forwards_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).await.unwrap();
        stream.write_all(&[2, 1, 2]).await.unwrap();
        assert_eq!(
            stream.read_u8().await.unwrap(),
            2,
            "prefer auth when password supplied"
        );
        stream
            .write_all(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
            .await
            .unwrap();
        let mut response = [0; 16];
        stream.read_exact(&mut response).await.unwrap();
        // Independent fixture from OpenSSL DES-ECB with the VNC bit-reversed key.
        assert_eq!(
            response,
            [
                0xb8, 0x66, 0x92, 0x41, 0x25, 0xc8, 0xee, 0xbb, 0x9d, 0xeb, 0xc1, 0xdb, 0x61, 0xc5,
                0x38, 0xe2
            ]
        );
        stream.write_u32(0).await.unwrap();
        init_after_security(&mut stream, 10, 10).await;
        let mut inputs = [0; 25];
        stream.read_exact(&mut inputs).await.unwrap();
        assert_eq!(
            inputs,
            [
                4, 1, 0, 0, 0, 0, 255, 13, 5, 5, 0, 7, 0, 8, 6, 0, 0, 0, 0, 0, 0, 3, b'A', 0xe9,
                b'\n'
            ]
        );
    });
    let (_reader, mut writer, _) = connect(&address.to_string(), Some("password-extra"))
        .await
        .unwrap();
    writer
        .send_input(&Input::Key {
            keysym: 0xff0d,
            down: true,
            id: 1,
        })
        .await
        .unwrap();
    writer
        .send_input(&Input::Pointer {
            x: 7,
            y: 8,
            buttons: 5,
            id: 2,
        })
        .await
        .unwrap();
    writer
        .send_input(&Input::Clipboard {
            text: "Aé\n".into(),
            id: 3,
        })
        .await
        .unwrap();
    peer.await.unwrap();
}

async fn extended_size(stream: &mut TcpStream, reason: u16, status: u16, width: u16, height: u16) {
    stream.write_all(&[0, 0, 0, 1]).await.unwrap();
    rectangle(stream, reason, status, width, height, -308).await;
    stream.write_all(&[1, 0, 0, 0]).await.unwrap();
    stream.write_u32(7).await.unwrap();
    stream.write_all(&[0; 4]).await.unwrap();
    stream.write_u16(width).await.unwrap();
    stream.write_u16(height).await.unwrap();
    stream.write_u32(0x80).await.unwrap();
}

#[tokio::test]
async fn desktop_resize_waits_for_capability_preserves_screen_identity_and_reports_rejection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 2, 1).await;
        let mut request = [0; 10];
        stream.read_exact(&mut request).await.unwrap();
        extended_size(&mut stream, 0, 0, 2, 1).await;
        let mut request = [0; 24];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(
            request,
            [
                251, 0, 0, 3, 0, 2, 1, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 3, 0, 2, 0, 0, 0, 0x80
            ]
        );
        // Failure fields are undefined: consume the structure without accepting zero dimensions.
        extended_size(&mut stream, 1, 1, 0, 0).await;
        extended_size(&mut stream, 1, 0, 3, 2).await;
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    assert!(!writer.supports_resize());
    assert!(
        writer
            .send_input(&Input::Resize {
                width: 3,
                height: 2,
                id: 1
            })
            .await
            .is_err()
    );
    writer.request_update(false, 2, 1).await.unwrap();
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::Unchanged
    ));
    assert!(writer.supports_resize());
    writer
        .send_input(&Input::Resize {
            width: 3,
            height: 2,
            id: 2,
        })
        .await
        .unwrap();
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::ResizeResult { status: 1, .. }
    ));
    // A rejected request leaves the desktop as it was.
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::Unchanged
    ));
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::ResizeResult {
            width: 3,
            height: 2,
            status: 0
        }
    ));
    let Event::Frame(resized) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!(
        (resized.width, resized.height, resized.pixels.len()),
        (3, 2, 24)
    );
    assert!(
        resized
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [0, 0, 0, 255])
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn cursor_mask_desktop_size_clipboard_bell_and_empty_updates_are_independent_events() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 2, 1).await;
        stream.write_all(&[0, 0, 0, 2]).await.unwrap();
        rectangle(&mut stream, 1, 0, 3, 1, -239).await;
        stream
            .write_all(&[1, 2, 3, 0, 4, 5, 6, 255, 7, 8, 9, 1, 0xa0])
            .await
            .unwrap();
        rectangle(&mut stream, 0, 0, 4, 1, -223).await;
        stream
            .write_all(&[2, 3, 0, 0, 0, 0, 0, 0, 2, 0xe9, b'\n', 0, 0, 0, 0])
            .await
            .unwrap();
    });
    let (mut reader, writer, _) = connect(&address.to_string(), None).await.unwrap();
    let Event::Cursor(cursor) = event(&mut reader).await.unwrap() else {
        panic!("cursor expected")
    };
    assert_eq!(
        (
            cursor.width,
            cursor.height,
            cursor.hotspot_x,
            cursor.hotspot_y
        ),
        (3, 1, 1, 0)
    );
    assert_eq!(cursor.pixels, [1, 2, 3, 255, 4, 5, 6, 0, 7, 8, 9, 255]);
    let Event::Frame(frame) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!((frame.width, frame.height), (4, 1));
    assert!(
        !writer.supports_resize(),
        "DesktopSize does not enable client resize"
    );
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Bell));
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Clipboard(text) if text == "é\n"));
    assert!(matches!(
        event(&mut reader).await.unwrap(),
        Event::Unchanged
    ));
    peer.await.unwrap();
}

#[tokio::test]
async fn cancellation_during_fragmented_update_preserves_wire_alignment() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 1, 1).await;
        stream.write_all(&[0, 0, 0]).await.unwrap();
        started_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        stream.write_u8(1).await.unwrap();
        rectangle(&mut stream, 0, 0, 1, 1, 0).await;
        stream.write_all(&[20, 40]).await.unwrap();
        tokio::task::yield_now().await;
        stream.write_all(&[60, 0]).await.unwrap();
    });
    let (mut reader, _writer, _) = connect(&address.to_string(), None).await.unwrap();
    started_rx.await.unwrap();
    assert!(
        timeout(Duration::from_millis(20), reader.read_event())
            .await
            .is_err()
    );
    resume_tx.send(()).unwrap();
    let Event::Frame(frame) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!(frame.pixels, [20, 40, 60, 255]);
    peer.await.unwrap();
}

#[tokio::test]
async fn none_authentication_negotiates_all_published_versions() {
    for minor in [3, 7, 8] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            init(&mut stream, minor, 1, 1).await;
            stream.write_all(&[0, 0, 0, 0]).await.unwrap();
        });
        let (mut reader, _writer, info) = connect(&address.to_string(), None).await.unwrap();
        assert_eq!(info.protocol_version, (3, minor));
        assert_eq!(info.name, "test");
        assert!(matches!(
            event(&mut reader).await.unwrap(),
            Event::Unchanged
        ));
        peer.await.unwrap();
    }
}

fn update_rectangle(x: u16, y: u16, width: u16, height: u16, encoding: i32) -> Vec<u8> {
    let mut message = vec![0, 0, 0, 1];
    for value in [x, y, width, height] {
        message.extend_from_slice(&value.to_be_bytes());
    }
    message.extend_from_slice(&encoding.to_be_bytes());
    message
}

#[tokio::test]
async fn malformed_server_messages_fail_before_allocating_or_emitting_a_frame() {
    let mut copy_outside = update_rectangle(0, 0, 2, 1, 1);
    copy_outside.extend_from_slice(&[0, 1, 0, 0]);
    let cases = [
        (update_rectangle(1, 0, 2, 1, 0), "rectangle outside"),
        (copy_outside, "rectangle outside"),
        (
            update_rectangle(0, 0, 1, 1, 42),
            "unsupported RFB encoding 42",
        ),
        (update_rectangle(0, 0, 65535, 65535, -223), "excessive"),
        (update_rectangle(0, 0, 1025, 1, -239), "cursor is too large"),
        (
            update_rectangle(1, 0, 1, 1, -239),
            "invalid RFB cursor hotspot",
        ),
        (vec![3, 0, 0, 0, 0, 0x10, 0, 1], "clipboard exceeds"),
        (vec![99], "unsupported RFB server message 99"),
    ];
    for (bytes, expected) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            init(&mut stream, 8, 2, 1).await;
            stream.write_all(&bytes).await.unwrap();
        });
        let (mut reader, _writer, _) = connect(&address.to_string(), None).await.unwrap();
        let error = event(&mut reader).await.unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error:?}"
        );
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_client_messages_return_errors_without_corrupting_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 1, 1).await;
        done_rx.await.unwrap();
        assert!(
            timeout(Duration::from_millis(20), stream.read_u8())
                .await
                .is_err()
        );
        stream.write_all(&[2]).await.unwrap();
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    assert!(writer.request_update(false, 0, 1).await.is_err());
    assert!(writer.request_update(false, 65535, 65535).await.is_err());
    assert!(
        writer
            .send_input(&Input::Clipboard {
                text: "🦀".into(),
                id: 0
            })
            .await
            .is_err()
    );
    assert!(
        writer
            .send_input(&Input::Clipboard {
                text: "x".repeat(gliss_core::MAX_CLIPBOARD + 1),
                id: 0
            })
            .await
            .is_err()
    );
    done_tx.send(()).unwrap();
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Bell));
    peer.await.unwrap();
}

#[tokio::test]
async fn refused_authentication_is_reported_with_bounded_server_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).await.unwrap();
        stream.write_all(&[1, 2]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 2);
        stream.write_all(&[0; 16]).await.unwrap();
        let mut response = [0; 16];
        stream.read_exact(&mut response).await.unwrap();
        stream.write_u32(1).await.unwrap();
        stream.write_u32(12).await.unwrap();
        stream.write_all(b"bad password").await.unwrap();
    });
    let error = connect(&address.to_string(), Some("test"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("authentication failed: bad password"));
    peer.await.unwrap();
}

// QEMU advertises zero unsolicited bytes: both directions must use notify/request.
async fn extended_clipboard(stream: &mut TcpStream, flags: u32, data: &[u8]) {
    stream.write_all(&[3, 0, 0, 0]).await.unwrap();
    stream.write_i32(-((4 + data.len()) as i32)).await.unwrap();
    stream.write_u32(flags).await.unwrap();
    stream.write_all(data).await.unwrap();
}

async fn client_clipboard(stream: &mut TcpStream) -> (u32, Vec<u8>) {
    let mut header = [0; 4];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(header, [6, 0, 0, 0]);
    let length = stream.read_i32().await.unwrap();
    assert!(length <= -4);
    let flags = stream.read_u32().await.unwrap();
    let mut data = vec![0; (-length - 4) as usize];
    stream.read_exact(&mut data).await.unwrap();
    (flags, data)
}

#[tokio::test]
async fn standard_extended_clipboard_transfers_utf8_in_both_directions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 1, 1).await;
        extended_clipboard(&mut stream, 0x1b00_0001, &0u32.to_be_bytes()).await;
        let (caps, sizes) = client_clipboard(&mut stream).await;
        assert_eq!(caps, 0x1b00_0001);
        assert_eq!(sizes, 0u32.to_be_bytes());
        extended_clipboard(&mut stream, 0x0800_0001, &[]).await;
        assert_eq!(client_clipboard(&mut stream).await, (0x0200_0001, vec![]));
        // Independent zlib fixture for UTF-8 text, including protocol CRLF and NUL.
        let text = "guest: 日本語\r\n🙂\0".as_bytes();
        let mut payload = (text.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(text);
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &payload).unwrap();
        extended_clipboard(&mut stream, 0x1000_0001, &encoder.finish().unwrap()).await;
        assert_eq!(client_clipboard(&mut stream).await, (0x0800_0001, vec![]));
        extended_clipboard(&mut stream, 0x0200_0001, &[]).await;
        let (flags, compressed) = client_clipboard(&mut stream).await;
        assert_eq!(flags, 0x1000_0001);
        let mut payload = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(&compressed[..]),
            &mut payload,
        )
        .unwrap();
        let expected = "browser: Ελληνικά\r\n🚀\0".as_bytes();
        assert_eq!(
            u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize,
            expected.len()
        );
        assert_eq!(&payload[4..], expected);
        stream.write_u8(2).await.unwrap();
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    assert!(
        matches!(event(&mut reader).await.unwrap(), Event::Clipboard(text) if text == "guest: 日本語\n🙂")
    );
    writer
        .send_input(&Input::Clipboard {
            id: 1,
            text: "browser: Ελληνικά\n🚀".to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Bell));
    peer.await.unwrap();
}

#[tokio::test]
async fn qemu_named_server_receives_browser_text_without_protocol_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init_named(&mut stream, 8, 1, 1, "QEMU (guest)").await;
        extended_clipboard(&mut stream, 0x1b00_0001, &0u32.to_be_bytes()).await;
        client_clipboard(&mut stream).await;
        stream.write_u8(2).await.unwrap();
        assert_eq!(client_clipboard(&mut stream).await, (0x0800_0001, vec![]));
        extended_clipboard(&mut stream, 0x0200_0001, &[]).await;
        let (flags, compressed) = client_clipboard(&mut stream).await;
        assert_eq!(flags, 0x1000_0001);
        let mut payload = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(&compressed[..]),
            &mut payload,
        )
        .unwrap();
        let expected = "browser: Ελληνικά\n🚀\n".as_bytes();
        assert_eq!(
            u32::from_be_bytes(payload[..4].try_into().unwrap()) as usize,
            expected.len()
        );
        assert_eq!(&payload[4..], expected);
    });
    let (mut reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    // The bell follows the capabilities, so the input below uses the extended format.
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Bell));
    writer
        .send_input(&Input::Clipboard {
            id: 1,
            text: "browser: Ελληνικά\r\n🚀\n".to_owned(),
        })
        .await
        .unwrap();
    peer.await.unwrap();
}

async fn banner(stream: &mut TcpStream) {
    stream.write_all(b"RFB 003.008\n").await.unwrap();
    let mut version = [0; 12];
    stream.read_exact(&mut version).await.unwrap();
    assert_eq!(&version, b"RFB 003.008\n");
}

async fn expect_eof(stream: &mut TcpStream) {
    let mut byte = [0; 1];
    assert_eq!(
        timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .expect("the client must close the connection")
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn cancelling_a_handshake_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let pending = tokio::spawn(async move { connect(&address.to_string(), None).await });
    let (mut stream, _) = listener.accept().await.unwrap();
    // The version reply proves the handshake began; the security list never comes.
    banner(&mut stream).await;
    pending.abort();
    assert!(pending.await.err().unwrap().is_cancelled());
    expect_eof(&mut stream).await;
}

#[tokio::test]
async fn missing_or_unsupported_security_fails_before_client_init() {
    for (security, expected) in [
        (2, "requires a VNC password"),
        (19, "no supported security type"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            banner(&mut stream).await;
            stream.write_all(&[1, security]).await.unwrap();
            // Rejected before a security type is chosen: no ClientInit or input follows.
            expect_eof(&mut stream).await;
        });
        let error = connect(&address.to_string(), None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(expected), "{error}");
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn oversized_refusal_reason_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        banner(&mut stream).await;
        // No security types, then a reason one byte longer than the limit.
        stream.write_all(&[0, 0, 1, 0, 1]).await.unwrap();
        expect_eof(&mut stream).await;
    });
    let error = connect(&address.to_string(), Some("password-extra"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("RFB text exceeds"), "{error}");
    peer.await.unwrap();
}

#[tokio::test]
async fn server_init_geometry_and_name_limits_fail_the_handshake() {
    for (width, height, excessive_name) in [
        (0u16, 1u16, false),
        (u16::MAX, u16::MAX, false),
        (2, 1, true),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            banner(&mut stream).await;
            stream.write_all(&[1, 1]).await.unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), 1);
            stream.write_all(&[0; 4]).await.unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), 1);
            stream.write_u16(width).await.unwrap();
            stream.write_u16(height).await.unwrap();
            if excessive_name {
                stream.write_all(&PIXEL_FORMAT).await.unwrap();
                stream.write_u32(65_537).await.unwrap();
            }
            expect_eof(&mut stream).await;
        });
        let error = connect(&address.to_string(), None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains(if excessive_name {
                "RFB text exceeds"
            } else {
                "invalid or excessive RFB framebuffer"
            }),
            "{error}"
        );
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn truncated_update_and_eof_end_the_stream_without_a_partial_frame() {
    let mut truncated = update_rectangle(0, 0, 2, 1, 0);
    truncated.extend_from_slice(&[12, 34]);
    for (bytes, inside_message) in [(truncated, true), (vec![], false)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            init(&mut stream, 8, 2, 1).await;
            stream.write_all(&bytes).await.unwrap();
        });
        let (mut reader, _writer, _) = connect(&address.to_string(), None).await.unwrap();
        let error = event(&mut reader).await.unwrap_err();
        if inside_message {
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::UnexpectedEof)
            );
        } else {
            assert!(error.to_string().contains("read RFB message"), "{error}");
        }
        assert!(
            event(&mut reader)
                .await
                .unwrap_err()
                .to_string()
                .contains("reader closed")
        );
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn inputs_arrive_in_order_and_dropping_both_handles_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, 2, 1).await;
        let mut inputs = [0; 28];
        stream.read_exact(&mut inputs).await.unwrap();
        assert_eq!(
            inputs,
            [
                4, 1, 0, 0, 0, 0, 0, 97, 5, 1, 0, 1, 0, 0, 4, 0, 0, 0, 0, 0, 0, 97, 5, 0, 0, 1, 0,
                0
            ]
        );
        expect_eof(&mut stream).await;
    });
    let (reader, mut writer, _) = connect(&address.to_string(), None).await.unwrap();
    for input in [
        Input::Key {
            keysym: 97,
            down: true,
            id: 1,
        },
        Input::Pointer {
            x: 1,
            y: 0,
            buttons: 1,
            id: 2,
        },
        Input::Key {
            keysym: 97,
            down: false,
            id: 3,
        },
        Input::Pointer {
            x: 1,
            y: 0,
            buttons: 0,
            id: 4,
        },
    ] {
        writer.send_input(&input).await.unwrap();
    }
    // The caller releases held input itself; the writer does not synthesize releases.
    drop(reader);
    drop(writer);
    peer.await.unwrap();
}

/// A peer that completes the handshake for a `width`x`height` desktop, then sends `bytes`.
async fn scripted(
    width: u16,
    height: u16,
    bytes: Vec<u8>,
) -> (gliss_rfb::RfbReader, gliss_rfb::RfbWriter) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        init(&mut stream, 8, width, height).await;
        stream.write_all(&bytes).await.unwrap();
        // Stay open: the client must not mistake the end of the script for a failure.
        let _ = stream.read_u8().await;
    });
    let (reader, writer, _) = connect(&address.to_string(), None).await.unwrap();
    (reader, writer)
}

fn rectangle_header(x: u16, y: u16, width: u16, height: u16, encoding: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [x, y, width, height] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&encoding.to_be_bytes());
    bytes
}

fn cursor_rectangle(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = rectangle_header(0, 0, width, height, -239);
    bytes.resize(
        bytes.len() + usize::from(width) * usize::from(height) * 4,
        7,
    );
    bytes.resize(
        bytes.len() + usize::from(width).div_ceil(8) * usize::from(height),
        0xff,
    );
    bytes
}

fn extended_size_rectangle(reason: u16, width: u16, height: u16) -> Vec<u8> {
    let mut bytes = rectangle_header(reason, 0, width, height, -308);
    bytes.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0]);
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes
}

#[tokio::test]
async fn an_update_that_changes_no_pixel_reports_no_frame() {
    let mut script = Vec::new();
    // No rectangle at all.
    script.extend_from_slice(&[0, 0, 0, 0]);
    // A cursor shape only.
    script.extend_from_slice(&[0, 0, 0, 1]);
    script.extend(cursor_rectangle(1, 1));
    // The audio acknowledgement only.
    script.extend_from_slice(&[0, 0, 0, 1]);
    script.extend(rectangle_header(0, 0, 2, 1, -259));
    // A layout report for the size the desktop already has, and an empty CopyRect.
    script.extend_from_slice(&[0, 0, 0, 2]);
    script.extend(extended_size_rectangle(0, 2, 1));
    script.extend(rectangle_header(0, 0, 0, 1, 1));
    script.extend_from_slice(&[0, 0, 0, 0]);
    // One pixel.
    script.extend_from_slice(&[0, 0, 0, 1]);
    script.extend(rectangle_header(1, 0, 1, 1, 0));
    script.extend_from_slice(&[1, 2, 3, 0]);
    let (mut reader, _writer) = scripted(2, 1, script).await;
    let mut seen = Vec::new();
    let frame = loop {
        match event(&mut reader).await.unwrap() {
            Event::Frame(frame) => break frame,
            Event::Unchanged => seen.push("unchanged"),
            Event::Cursor(_) => seen.push("cursor"),
            other => panic!("unexpected {other:?}"),
        }
    };
    // Each update still ends with an event: it answered an update request.
    assert_eq!(
        seen,
        ["unchanged", "cursor", "unchanged", "unchanged", "unchanged"]
    );
    assert_eq!(frame.pixels, [0, 0, 0, 255, 1, 2, 3, 255]);
}

fn copy_rectangles(count: u16, width: u16, height: u16) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&count.to_be_bytes());
    for _ in 0..count {
        bytes.extend(rectangle_header(0, 0, width, height, 1));
        bytes.extend_from_slice(&[0, 0, 0, 0]);
    }
    bytes
}

#[tokio::test]
async fn an_update_may_write_or_move_four_times_the_frame_area_and_no_more() {
    // Four whole-frame copies are the budget of a 4x4 desktop.
    let (mut reader, _writer) = scripted(4, 4, copy_rectangles(4, 4, 4)).await;
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Frame(_)));
    for script in [
        copy_rectangles(5, 4, 4),
        // Raw pixels count too: four copies and one more pixel.
        {
            let mut script = copy_rectangles(5, 4, 4);
            script[3] = 5;
            script.truncate(4 + 4 * 16);
            script.extend(rectangle_header(0, 0, 1, 1, 0));
            script.extend_from_slice(&[1, 2, 3, 0]);
            script
        },
        // The 16-byte rectangles of the review: 65535 whole-frame copies.
        copy_rectangles(u16::MAX, 4, 4),
    ] {
        let (mut reader, _writer) = scripted(4, 4, script).await;
        let error = event(&mut reader).await.unwrap_err().to_string();
        assert!(error.contains("pixel budget"), "{error}");
    }
}

#[tokio::test]
async fn an_update_changes_the_desktop_size_at_most_once() {
    let update = |rectangles: &[Vec<u8>]| {
        let mut bytes = vec![0, 0, 0, rectangles.len() as u8];
        for rectangle in rectangles {
            bytes.extend_from_slice(rectangle);
        }
        bytes
    };
    // Repeating the new size allocates nothing and is accepted.
    let script = update(&[
        extended_size_rectangle(0, 4, 2),
        extended_size_rectangle(0, 4, 2),
        rectangle_header(0, 0, 4, 2, -223),
    ]);
    let (mut reader, _writer) = scripted(2, 1, script).await;
    let Event::Frame(frame) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!((frame.width, frame.height), (4, 2));
    for script in [
        update(&[
            extended_size_rectangle(0, 4, 2),
            extended_size_rectangle(0, 2, 1),
        ]),
        update(&[
            extended_size_rectangle(0, 4, 2),
            rectangle_header(0, 0, 2, 1, -223),
        ]),
    ] {
        let (mut reader, _writer) = scripted(2, 1, script).await;
        let error = event(&mut reader).await.unwrap_err().to_string();
        assert!(error.contains("size more than once"), "{error}");
    }
}

#[tokio::test]
async fn a_cursor_above_128_pixels_a_side_is_read_off_the_wire_and_not_reported() {
    let mut script = vec![0, 0, 0, 5];
    for (width, height) in [(129, 1), (1, 129), (1024, 1024), (128, 128), (129, 129)] {
        script.extend(cursor_rectangle(width, height));
    }
    // The bell is only found if every cursor was consumed to its last byte.
    script.push(2);
    let (mut reader, _writer) = scripted(2, 1, script).await;
    let mut cursors = Vec::new();
    loop {
        match event(&mut reader).await.unwrap() {
            Event::Cursor(cursor) => {
                cursors.push((cursor.width, cursor.height, cursor.pixels.len()))
            }
            Event::Bell => break,
            _ => {}
        }
    }
    assert_eq!(cursors, [(128, 128, 128 * 128 * 4)]);
    assert_eq!(gliss_rfb::MAX_CURSOR_SIDE, 128);
}

#[tokio::test]
async fn a_latin1_clipboard_over_the_limit_as_utf8_is_dropped_and_the_connection_continues() {
    let clipboard = |length: usize| {
        let mut bytes = vec![3, 0, 0, 0];
        bytes.extend_from_slice(&(length as u32).to_be_bytes());
        // Every byte above 0x7f takes two bytes as UTF-8.
        bytes.resize(bytes.len() + length, 0xe9);
        bytes
    };
    let half = gliss_core::MAX_CLIPBOARD / 2;
    let mut script = clipboard(gliss_core::MAX_CLIPBOARD);
    script.extend(clipboard(half + 1));
    script.extend(clipboard(half));
    script.push(2);
    let (mut reader, _writer) = scripted(2, 1, script).await;
    let mut lengths = Vec::new();
    loop {
        match timeout(Duration::from_secs(20), reader.read_event())
            .await
            .unwrap()
            .unwrap()
        {
            Event::Clipboard(text) => lengths.push(text.len()),
            Event::Bell => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(lengths, [gliss_core::MAX_CLIPBOARD]);
}

#[tokio::test]
async fn server_text_in_an_error_cannot_start_a_log_line_and_is_bounded() {
    let reason = format!(
        "no\r\n2026-01-01T00:00:00Z  WARN forged line\u{1b}[31m\u{85}\u{2028}{}",
        "x".repeat(60_000)
    );
    for refused_before_security in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reason = reason.clone();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            banner(&mut stream).await;
            if refused_before_security {
                stream.write_u8(0).await.unwrap();
            } else {
                stream.write_all(&[1, 1]).await.unwrap();
                assert_eq!(stream.read_u8().await.unwrap(), 1);
                stream.write_u32(1).await.unwrap();
            }
            stream.write_u32(reason.len() as u32).await.unwrap();
            stream.write_all(reason.as_bytes()).await.unwrap();
        });
        let error = format!(
            "{:#}",
            connect(&address.to_string(), None).await.err().unwrap()
        );
        assert!(
            !error
                .chars()
                .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}'),
            "{error:?}"
        );
        assert!(
            error.contains(
                r"no\r\n2026-01-01T00:00:00Z  WARN forged line\u{1b}[31m\u{85}\u{2028}xxx"
            ),
            "{error:?}"
        );
        assert!(error.len() <= 400, "{} bytes", error.len());
        peer.await.unwrap();
    }
}
