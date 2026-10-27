use gliss_core::Input;
use gliss_rfb::{Event, connect};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{Duration, timeout},
};

const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];

async fn init(stream: &mut TcpStream, minor: u8, width: u16, height: u16) {
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
    init_after_security(stream, width, height).await;
}

async fn init_after_security(stream: &mut TcpStream, width: u16, height: u16) {
    assert_eq!(stream.read_u8().await.unwrap(), 1, "shared connection");
    stream.write_u16(width).await.unwrap();
    stream.write_u16(height).await.unwrap();
    stream.write_all(&PIXEL_FORMAT).await.unwrap();
    stream.write_u32(4).await.unwrap();
    stream.write_all(b"test").await.unwrap();
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
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Frame(_)));
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
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Frame(_)));
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
    assert!(matches!(event(&mut reader).await.unwrap(), Event::Frame(_)));
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
    let Event::Frame(rejected) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!((rejected.width, rejected.height), (2, 1));
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
    let Event::Frame(empty_update) = event(&mut reader).await.unwrap() else {
        panic!("frame expected")
    };
    assert_eq!(empty_update, frame);
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
        assert!(matches!(event(&mut reader).await.unwrap(), Event::Frame(_)));
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
async fn qemu_extended_clipboard_transfers_utf8_in_both_directions() {
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
