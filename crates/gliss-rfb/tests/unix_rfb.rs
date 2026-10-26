#![cfg(unix)]

//! Independent owned socket-pair peers; no libvirt, VM, address resolution or FD adoption.
use gliss_core::Input;
use gliss_rfb::{Event, RfbReader, connect_unix};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::oneshot,
    time::{Duration, timeout},
};

const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];

async fn fragmented(stream: &mut UnixStream, bytes: &[u8]) {
    for chunk in bytes.chunks(3) {
        stream.write_all(chunk).await.unwrap();
        tokio::task::yield_now().await;
    }
}

async fn version(stream: &mut UnixStream, minor: u8) {
    let banner = format!("RFB 003.{minor:03}\n");
    fragmented(stream, banner.as_bytes()).await;
    let mut actual = [0; 12];
    stream.read_exact(&mut actual).await.unwrap();
    assert_eq!(actual.as_slice(), banner.as_bytes());
}

async fn server_init(stream: &mut UnixStream, width: u16, height: u16) {
    assert_eq!(stream.read_u8().await.unwrap(), 1, "shared ClientInit");
    let mut init = Vec::from(width.to_be_bytes());
    init.extend_from_slice(&height.to_be_bytes());
    init.extend_from_slice(&PIXEL_FORMAT);
    init.extend_from_slice(&4u32.to_be_bytes());
    init.extend_from_slice(b"unix");
    fragmented(stream, &init).await;
    // The peer's independent wire oracle includes format and all encodings.
    let mut format = [0; 20];
    stream.read_exact(&mut format).await.unwrap();
    assert_eq!(&format[..4], &[0, 0, 0, 0]);
    assert_eq!(&format[4..], &PIXEL_FORMAT);
    let mut encodings = [0; 28];
    stream.read_exact(&mut encodings).await.unwrap();
    assert_eq!(
        encodings,
        [
            2, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 1, 255, 255, 255, 17, 255, 255, 255, 33, 255, 255,
            254, 204, 0xc0, 0xa1, 0xe5, 0xce
        ]
    );
}

async fn none_init(stream: &mut UnixStream, minor: u8) {
    version(stream, minor).await;
    if minor == 3 {
        fragmented(stream, &[0, 0, 0, 1]).await;
    } else {
        fragmented(stream, &[1, 1]).await;
        assert_eq!(stream.read_u8().await.unwrap(), 1);
        if minor == 8 {
            fragmented(stream, &[0; 4]).await;
        }
    }
    server_init(stream, 2, 1).await;
}

async fn next_event(reader: &mut RfbReader) -> anyhow::Result<Event> {
    timeout(Duration::from_secs(2), reader.read_event())
        .await
        .expect("bounded RFB event")
}

async fn expect_eof(stream: &mut UnixStream) {
    let mut byte = [0; 1];
    assert_eq!(
        timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .expect("owned peer must close")
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn unix_stream_negotiates_each_version_and_fragmented_opaque_frame() {
    for minor in [3, 7, 8] {
        let (client, mut remote) = UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            none_init(&mut remote, minor).await;
            let mut request = [0; 10];
            remote.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [3, 0, 0, 0, 0, 0, 0, 2, 0, 1]);
            fragmented(
                &mut remote,
                &[
                    0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 12, 34, 56, 0, 78, 90, 123, 99,
                ],
            )
            .await;
        });
        let (mut reader, mut writer, info) = connect_unix(client, None).await.unwrap();
        assert_eq!(
            (info.width, info.height, info.protocol_version),
            (2, 1, (3, minor))
        );
        assert_eq!(info.name, "unix");
        writer.request_update(false, 2, 1).await.unwrap();
        let Event::Frame(frame) = next_event(&mut reader).await.unwrap() else {
            panic!("independent raw rectangle must emit a frame")
        };
        assert_eq!(frame.pixels, [12, 34, 56, 255, 78, 90, 123, 255]);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_unix_event_wait_preserves_fragmented_message_alignment() {
    let (client, mut remote) = UnixStream::pair().unwrap();
    let (started, waiting) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    let peer = tokio::spawn(async move {
        none_init(&mut remote, 8).await;
        remote.write_all(&[0, 0, 0]).await.unwrap();
        started.send(()).unwrap();
        resumed.await.unwrap();
        fragmented(
            &mut remote,
            &[
                1, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 12, 34, 56, 0, 78, 90, 123, 0, 2,
            ],
        )
        .await;
    });
    let (mut reader, _writer, _) = connect_unix(client, None).await.unwrap();
    waiting.await.unwrap();
    assert!(
        timeout(Duration::from_millis(20), reader.read_event())
            .await
            .is_err()
    );
    resume.send(()).unwrap();
    let Event::Frame(frame) = next_event(&mut reader).await.unwrap() else {
        panic!("frame after canceled waiter")
    };
    assert_eq!(frame.pixels, [12, 34, 56, 255, 78, 90, 123, 255]);
    assert!(matches!(
        next_event(&mut reader).await.unwrap(),
        Event::Bell
    ));
    peer.await.unwrap();
}

#[tokio::test]
async fn cancellation_during_unix_security_setup_closes_the_owned_stream() {
    let (client, mut remote) = UnixStream::pair().unwrap();
    let pending = tokio::spawn(connect_unix(client, None));
    // Receiving the version reply proves the handshake began; withhold the
    // security list so cancellation occurs before any wire actors are created.
    version(&mut remote, 8).await;
    pending.abort();
    assert!(pending.await.err().unwrap().is_cancelled());
    expect_eof(&mut remote).await;
}

#[tokio::test]
async fn unix_stream_authentication_keeps_the_independent_des_wire_vector() {
    let (client, mut remote) = UnixStream::pair().unwrap();
    let peer = tokio::spawn(async move {
        version(&mut remote, 8).await;
        fragmented(&mut remote, &[2, 1, 2]).await;
        assert_eq!(
            remote.read_u8().await.unwrap(),
            2,
            "supplied password selects VNC auth"
        );
        fragmented(
            &mut remote,
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        )
        .await;
        let mut response = [0; 16];
        remote.read_exact(&mut response).await.unwrap();
        // OpenSSL DES-ECB oracle for the bit-reversed first eight bytes of
        // 'password-extra'; no cipher from the implementation generates this.
        assert_eq!(
            response,
            [
                0xb8, 0x66, 0x92, 0x41, 0x25, 0xc8, 0xee, 0xbb, 0x9d, 0xeb, 0xc1, 0xdb, 0x61, 0xc5,
                0x38, 0xe2
            ]
        );
        fragmented(&mut remote, &[0; 4]).await;
        server_init(&mut remote, 2, 1).await;
        let mut key = [0; 8];
        remote.read_exact(&mut key).await.unwrap();
        assert_eq!(key, [4, 0, 0, 0, 0, 0, 255, 13]);
    });
    let (_reader, mut writer, _) = connect_unix(client, Some("password-extra")).await.unwrap();
    writer
        .send_input(&Input::Key {
            keysym: 0xff0d,
            down: false,
            id: 7,
        })
        .await
        .unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn preconnected_unix_stream_does_not_bypass_missing_or_unsupported_security() {
    for (security, expected) in [
        (2, "requires a VNC password"),
        (19, "no supported security type"),
    ] {
        let (client, mut remote) = UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            version(&mut remote, 8).await;
            fragmented(&mut remote, &[1, security]).await;
            // Reject before choosing a security type; no ClientInit or input.
            expect_eof(&mut remote).await;
        });
        let error = connect_unix(client, None).await.err().unwrap().to_string();
        assert!(error.contains(expected), "{error}");
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn unix_authentication_failure_and_oversized_refusal_reason_remain_errors() {
    for oversized in [false, true] {
        let (client, mut remote) = UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            version(&mut remote, 8).await;
            if oversized {
                fragmented(&mut remote, &[0, 0, 1, 0, 1]).await;
            } else {
                fragmented(&mut remote, &[1, 2]).await;
                assert_eq!(remote.read_u8().await.unwrap(), 2);
                fragmented(&mut remote, &[0; 16]).await;
                let mut response = [0; 16];
                remote.read_exact(&mut response).await.unwrap();
                fragmented(
                    &mut remote,
                    &[0, 0, 0, 1, 0, 0, 0, 6, b'd', b'e', b'n', b'i', b'e', b'd'],
                )
                .await;
            }
            expect_eof(&mut remote).await;
        });
        let error = connect_unix(client, Some("password-extra"))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains(if oversized {
                "RFB text exceeds"
            } else {
                "authentication failed: denied"
            }),
            "{error}"
        );
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn explicit_input_releases_use_the_same_unix_stream_and_dropping_both_handles_closes_it() {
    let (client, mut remote) = UnixStream::pair().unwrap();
    let peer = tokio::spawn(async move {
        none_init(&mut remote, 8).await;
        let mut inputs = [0; 28];
        // Consume one byte at a time across all four command boundaries.
        for byte in inputs.chunks_mut(1) {
            remote.read_exact(byte).await.unwrap();
            tokio::task::yield_now().await;
        }
        assert_eq!(
            inputs,
            [
                4, 1, 0, 0, 0, 0, 0, 97, 5, 1, 0, 1, 0, 0, 4, 0, 0, 0, 0, 0, 0, 97, 5, 0, 0, 1, 0,
                0
            ]
        );
        expect_eof(&mut remote).await;
    });
    let (reader, mut writer, _) = connect_unix(client, None).await.unwrap();
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
    // The caller explicitly releases input; RfbWriter does not synthesize it.
    drop(reader);
    drop(writer);
    peer.await.unwrap();
}

#[tokio::test]
async fn invalid_unix_inputs_emit_no_bytes_and_leave_the_stream_usable() {
    let (client, mut remote) = UnixStream::pair().unwrap();
    let (done, validated) = oneshot::channel();
    let peer = tokio::spawn(async move {
        none_init(&mut remote, 8).await;
        validated.await.unwrap();
        assert!(
            timeout(Duration::from_millis(20), remote.read_u8())
                .await
                .is_err()
        );
        remote.write_all(&[2]).await.unwrap();
        let mut key = [0; 8];
        remote.read_exact(&mut key).await.unwrap();
        assert_eq!(key, [4, 0, 0, 0, 0, 0, 0, 98]);
    });
    let (mut reader, mut writer, _) = connect_unix(client, None).await.unwrap();
    assert!(writer.request_update(false, 0, 1).await.is_err());
    assert!(
        writer
            .request_update(false, u16::MAX, u16::MAX)
            .await
            .is_err()
    );
    assert!(
        writer
            .send_input(&Input::Clipboard {
                text: "🦀".into(),
                id: 1
            })
            .await
            .is_err()
    );
    assert!(
        writer
            .send_input(&Input::Clipboard {
                text: "x".repeat(1_048_577),
                id: 2
            })
            .await
            .is_err()
    );
    assert!(
        writer
            .send_input(&Input::Resize {
                width: 3,
                height: 1,
                id: 3
            })
            .await
            .is_err()
    );
    done.send(()).unwrap();
    assert!(matches!(
        next_event(&mut reader).await.unwrap(),
        Event::Bell
    ));
    writer
        .send_input(&Input::Key {
            keysym: 98,
            down: false,
            id: 4,
        })
        .await
        .unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn unix_malformed_frames_and_eof_never_emit_a_partial_frame() {
    let cases = [
        (
            vec![0, 0, 0, 1, 0, 1, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0],
            "rectangle outside",
        ),
        (
            vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 12, 34],
            "EOF",
        ),
        (vec![3, 0, 0, 0, 0, 0x10, 0, 1], "clipboard exceeds"),
        (vec![99], "unsupported RFB server message"),
        (vec![], "read RFB message"),
    ];
    for (bytes, expected) in cases {
        let (client, mut remote) = UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            none_init(&mut remote, 8).await;
            remote.write_all(&bytes).await.unwrap();
        });
        let (mut reader, mut writer, _) = connect_unix(client, None).await.unwrap();
        let error = next_event(&mut reader).await.err().unwrap();
        if expected == "EOF" {
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::UnexpectedEof)
            );
        } else {
            assert!(error.to_string().contains(expected), "{error}");
        }
        assert!(
            next_event(&mut reader)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("reader closed")
        );
        peer.await.unwrap();
        assert!(
            writer
                .send_input(&Input::Key {
                    keysym: 97,
                    down: false,
                    id: 0
                })
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn unix_server_init_geometry_and_name_limits_fail_before_starting_wire_actors() {
    for (width, height, excessive_name) in [
        (0u16, 1u16, false),
        (u16::MAX, u16::MAX, false),
        (2, 1, true),
    ] {
        let (client, mut remote) = UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            version(&mut remote, 8).await;
            fragmented(&mut remote, &[1, 1]).await;
            assert_eq!(remote.read_u8().await.unwrap(), 1);
            remote.write_all(&[0; 4]).await.unwrap();
            assert_eq!(remote.read_u8().await.unwrap(), 1);
            remote.write_u16(width).await.unwrap();
            remote.write_u16(height).await.unwrap();
            if excessive_name {
                remote.write_all(&PIXEL_FORMAT).await.unwrap();
                remote.write_u32(65_537).await.unwrap();
            }
            expect_eof(&mut remote).await;
        });
        let error = connect_unix(client, None).await.err().unwrap().to_string();
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
