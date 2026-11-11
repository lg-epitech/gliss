//! The VNC side of a session, against a scripted server.
use super::test_support::{app, full_update, update_request, vnc_server};
use super::*;
use tokio::{io::AsyncWriteExt, time::timeout};

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn an_update_without_pixels_answers_the_request_and_publishes_no_frame() {
    let (address, accepted) = vnc_server(16, 16).await;
    let app = app(&["--vnc", &address]);
    let mut source = timeout(WAIT, source(&app)).await.unwrap().unwrap();
    let mut server = accepted.await.unwrap();
    assert!(!update_request(&mut server).await);
    server
        .write_all(&full_update(16, 16, [1, 2, 3]))
        .await
        .unwrap();
    timeout(WAIT, source.frames.changed())
        .await
        .unwrap()
        .unwrap();
    let first = source.frames.borrow_and_update().clone().unwrap();
    assert_eq!(first.frame.pixels[..4], [1, 2, 3, 255]);
    // Each empty update must be followed by a new request, or the display would freeze.
    for _ in 0..3 {
        assert!(timeout(WAIT, update_request(&mut server)).await.unwrap());
        server.write_all(&[0, 0, 0, 0]).await.unwrap();
    }
    assert!(timeout(WAIT, update_request(&mut server)).await.unwrap());
    // Nothing to encode: the session only starts an encode for a published frame.
    assert!(!source.frames.has_changed().unwrap());
    assert!(Arc::ptr_eq(
        &source.frames.borrow().as_ref().unwrap().frame,
        &first.frame
    ));
}

/// Reads client messages up to a pointer event with `marker` as its x, and
/// returns the key events before it as (keysym, down).
async fn keys_until_pointer(server: &mut tokio::net::TcpStream, marker: u16) -> Vec<(u32, bool)> {
    use tokio::io::AsyncReadExt;
    let mut keys = Vec::new();
    loop {
        match server.read_u8().await.unwrap() {
            3 => {
                let mut request = [0; 9];
                server.read_exact(&mut request).await.unwrap();
            }
            4 => {
                let mut key = [0; 7];
                server.read_exact(&mut key).await.unwrap();
                keys.push((
                    u32::from_be_bytes([key[3], key[4], key[5], key[6]]),
                    key[0] == 1,
                ));
            }
            5 => {
                let mut pointer = [0; 5];
                server.read_exact(&mut pointer).await.unwrap();
                if u16::from_be_bytes([pointer[1], pointer[2]]) == marker {
                    return keys;
                }
            }
            other => panic!("unexpected client message {other}"),
        }
    }
}

#[tokio::test]
async fn at_most_256_keys_are_held_and_one_more_is_refused_without_ending_the_session() {
    let (address, accepted) = vnc_server(16, 16).await;
    let app = app(&["--vnc", &address]);
    let source = timeout(WAIT, source(&app)).await.unwrap().unwrap();
    let mut server = accepted.await.unwrap();
    let key = |keysym, down| Input::Key {
        keysym,
        down,
        id: 0,
    };
    let pointer = |x| Input::Pointer {
        x,
        y: 0,
        buttons: 0,
        id: 0,
    };
    for keysym in 1000..1300 {
        source.input.send(key(keysym, true)).await.unwrap();
    }
    source.input.send(pointer(1)).await.unwrap();
    let written = timeout(WAIT, keys_until_pointer(&mut server, 1))
        .await
        .unwrap();
    let expected: Vec<_> = (1000..1256).map(|keysym| (keysym, true)).collect();
    assert_eq!(written, expected);
    let notice = timeout(WAIT, source.notices.next()).await.unwrap();
    assert_eq!(
        notice.text,
        r#"{"message":"Too many keys are held down. Release one first.","type":"input_error"}"#
    );
    assert!(!notice.last);
    // A held key may repeat, and releasing one makes room for another.
    for input in [
        key(1000, true),
        key(1000, false),
        key(1299, true),
        key(1298, true),
        pointer(2),
    ] {
        source.input.send(input).await.unwrap();
    }
    let written = timeout(WAIT, keys_until_pointer(&mut server, 2))
        .await
        .unwrap();
    assert_eq!(written, [(1000, true), (1000, false), (1299, true)]);
    // Ending the session releases what was written as held, and nothing else.
    let Source {
        input,
        mut shutdown,
        ..
    } = source;
    drop(input);
    timeout(WAIT, shutdown.take().unwrap())
        .await
        .unwrap()
        .unwrap();
    let mut released = Vec::new();
    {
        use tokio::io::AsyncReadExt;
        let mut tail = Vec::new();
        timeout(WAIT, server.read_to_end(&mut tail))
            .await
            .unwrap()
            .unwrap();
        for message in tail.chunks(8) {
            assert_eq!(message[..2], [4, 0], "only key releases follow");
            released.push(u32::from_be_bytes([
                message[4], message[5], message[6], message[7],
            ]));
        }
    }
    released.sort_unstable();
    let mut held: Vec<u32> = (1001..1256).collect();
    held.push(1299);
    assert_eq!(released, held);
}
