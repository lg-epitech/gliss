//! Shared fixtures: a configured server without a listener, and a scripted VNC server.
use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub const TOKEN: &str = "01234567890123456789012345678901";

/// The state `run` builds from a command line, here `--token TOKEN` plus `extra`.
pub fn app(extra: &[&str]) -> Arc<App> {
    let args = Args::try_parse_from(
        ["gliss-server", "--token", TOKEN]
            .into_iter()
            .chain(extra.iter().copied()),
    )
    .unwrap();
    let video_timing = configured_video_timing(&args).unwrap();
    let origins = vec![format!("http://{}", args.bind)];
    Arc::new(App::new(args, video_timing, origins))
}

/// Accepts one RFB 3.8 connection for a `width`x`height` desktop without
/// authentication and returns it once the client has sent its setup messages.
pub async fn vnc_server(width: u16, height: u16) -> (String, tokio::task::JoinHandle<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let accepted = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).await.unwrap();
        stream.write_all(&[1, 1]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1);
        stream.write_u32(0).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1, "shared connection");
        stream.write_u16(width).await.unwrap();
        stream.write_u16(height).await.unwrap();
        // Red in the low byte: the client keeps this format.
        stream
            .write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
            .await
            .unwrap();
        stream.write_u32(4).await.unwrap();
        stream.write_all(b"test").await.unwrap();
        // SetPixelFormat, then SetEncodings with its list.
        let mut format = [0; 20];
        stream.read_exact(&mut format).await.unwrap();
        let mut encodings = [0; 4];
        stream.read_exact(&mut encodings).await.unwrap();
        let count = usize::from(u16::from_be_bytes([encodings[2], encodings[3]]));
        let mut list = vec![0; count * 4];
        stream.read_exact(&mut list).await.unwrap();
        stream
    });
    (address, accepted)
}

/// Reads one FramebufferUpdateRequest and returns whether it is incremental.
pub async fn update_request(stream: &mut TcpStream) -> bool {
    let mut request = [0; 10];
    stream.read_exact(&mut request).await.unwrap();
    assert_eq!(request[0], 3, "FramebufferUpdateRequest expected");
    request[1] == 1
}

/// A FramebufferUpdate that fills a `width`x`height` desktop with one colour.
pub fn full_update(width: u16, height: u16, colour: [u8; 3]) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 1, 0, 0, 0, 0];
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    for _ in 0..usize::from(width) * usize::from(height) {
        bytes.extend_from_slice(&[colour[0], colour[1], colour[2], 0]);
    }
    bytes
}
