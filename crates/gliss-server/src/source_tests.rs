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
