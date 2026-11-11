//! Behaviour that needs an in-memory connection: no socket, and a paused clock.
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
