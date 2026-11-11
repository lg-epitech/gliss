//! The listener and the login endpoint, over loopback sockets.
//!
//! Timeouts are shortened so that a stalled peer is cut quickly, and every
//! wait for the server to act is generous: a slow machine makes these tests
//! slower, never wrong.
use super::test_support::{TOKEN, app};
use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    time::timeout,
};

const WAIT: Duration = Duration::from_secs(30);
const ORIGIN: &str = "http://127.0.0.1:8080";

fn limits(connections: usize, stall: Duration) -> http::Limits {
    http::Limits {
        connections,
        request_head: stall,
        write_stall: stall,
    }
}

const SHORT: Duration = Duration::from_millis(300);

async fn serve_router(router: Router, app: &App, limits: http::Limits) -> SocketAddr {
    let listener = tcp_listener::VerifiedListener::new(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        app.tcp_listener.clone(),
    );
    let address = axum::serve::Listener::local_addr(&listener).unwrap();
    tokio::spawn(http::serve(
        listener,
        router,
        limits,
        std::future::pending(),
    ));
    address
}

async fn serve(app: &Arc<App>, limits: http::Limits) -> SocketAddr {
    serve_router(router(app.clone()), app, limits).await
}

/// Connects from another loopback address: all of 127.0.0.0/8 is local on Linux.
async fn connect_from(source: &str, server: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(format!("{source}:0").parse().unwrap()).unwrap();
    socket.connect(server).await.unwrap()
}

/// Everything the server sends before it closes the connection.
async fn until_closed(stream: &mut TcpStream) -> String {
    let mut received = Vec::new();
    // A reset is a close too.
    let _ = timeout(WAIT, stream.read_to_end(&mut received))
        .await
        .expect("the server must close the connection");
    String::from_utf8_lossy(&received).into_owned()
}

/// Reads one response head and returns its status.
async fn status(stream: &mut TcpStream) -> u16 {
    let mut head = Vec::new();
    timeout(WAIT, async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.expect("a response is expected"));
        }
    })
    .await
    .expect("a response is expected");
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 "), "{head}");
    head[9..12].parse().unwrap()
}

fn login_head(origin: &str, length: usize) -> String {
    format!(
        "POST /api/session HTTP/1.1\r\nHost: gliss\r\nOrigin: {origin}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
    )
}

/// One login on its own connection; returns the status and the session cookie.
async fn login(source: &str, server: SocketAddr, token: &str) -> (u16, Option<String>) {
    let body = json!({ "token": token }).to_string();
    let mut stream = connect_from(source, server).await;
    stream
        .write_all(format!("{}{body}", login_head(ORIGIN, body.len())).as_bytes())
        .await
        .unwrap();
    // The request asks for the connection to be closed after the response.
    let response = until_closed(&mut stream).await;
    let cookie = response
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .map(|cookie| cookie.split(';').next().unwrap().to_owned());
    (response[9..12].parse().unwrap(), cookie)
}

#[tokio::test]
async fn a_connection_that_sends_nothing_is_closed() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, SHORT)).await;
    let mut silent = TcpStream::connect(server).await.unwrap();
    assert_eq!(until_closed(&mut silent).await, "");
}

#[tokio::test]
async fn a_request_head_that_stalls_is_closed() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, SHORT)).await;
    let mut slow = TcpStream::connect(server).await.unwrap();
    slow.write_all(b"GET /health HTTP/1.1\r\nHost: gliss\r\nX-Slow: ")
        .await
        .unwrap();
    let said = until_closed(&mut slow).await;
    assert!(
        said.is_empty() || said.starts_with("HTTP/1.1 408"),
        "{said}"
    );
}

#[tokio::test]
async fn an_idle_connection_is_closed_after_its_response() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, SHORT)).await;
    let mut idle = TcpStream::connect(server).await.unwrap();
    idle.write_all(b"GET /health HTTP/1.1\r\nHost: gliss\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(status(&mut idle).await, 200);
    // The body of the response, then nothing more: no error is sent on an idle connection.
    let rest = until_closed(&mut idle).await;
    assert!(
        rest.starts_with('{') && !rest.contains("HTTP/1.1"),
        "{rest}"
    );
}

#[tokio::test]
async fn connections_over_the_limit_wait_for_one_to_close() {
    let app = app(&["--demo"]);
    // A long timeout: the two idle connections keep their slots until dropped.
    let server = serve(&app, limits(2, Duration::from_secs(600))).await;
    let first = TcpStream::connect(server).await.unwrap();
    let _second = TcpStream::connect(server).await.unwrap();
    let mut third = TcpStream::connect(server).await.unwrap();
    third
        .write_all(b"GET /health HTTP/1.1\r\nHost: gliss\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0];
    assert!(
        timeout(Duration::from_millis(500), third.read(&mut byte))
            .await
            .is_err(),
        "a third connection was served while two were open"
    );
    drop(first);
    assert_eq!(
        timeout(WAIT, third.read(&mut byte)).await.unwrap().unwrap(),
        1
    );
    assert_eq!(byte, *b"H");
}

#[tokio::test]
async fn a_response_nobody_reads_is_cut_and_gives_its_connection_back() {
    const BODY: usize = 64 * 1024 * 1024;
    let app = app(&["--demo"]);
    let router = Router::new()
        .route("/large", get(|| async { vec![0u8; BODY] }))
        .route("/small", get(|| async { "ok" }));
    let server = serve_router(router, &app, limits(1, SHORT)).await;
    let mut stuck = TcpStream::connect(server).await.unwrap();
    stuck
        .write_all(b"GET /large HTTP/1.1\r\nHost: gliss\r\n\r\n")
        .await
        .unwrap();
    // The only slot is taken until the server gives up on the first response.
    let mut next = TcpStream::connect(server).await.unwrap();
    next.write_all(b"GET /small HTTP/1.1\r\nHost: gliss\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(status(&mut next).await, 200);
    // What was buffered is all the first client ever gets.
    let mut received = Vec::new();
    let _ = timeout(WAIT, stuck.read_to_end(&mut received))
        .await
        .expect("the server must have closed the connection");
    assert!(received.len() < BODY, "the whole response was delivered");
}

/// Reads one unmasked WebSocket frame and returns its opcode and payload.
async fn frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let opcode = stream.read_u8().await.unwrap() & 0x0f;
    let length = match stream.read_u8().await.unwrap() {
        126 => u64::from(stream.read_u16().await.unwrap()),
        127 => stream.read_u64().await.unwrap(),
        short => u64::from(short),
    };
    let mut payload = vec![0; length as usize];
    stream.read_exact(&mut payload).await.unwrap();
    (opcode, payload)
}

/// A short text message in a masked frame, as a browser sends it.
fn masked(text: &[u8]) -> Vec<u8> {
    assert!(text.len() < 126);
    let mask = [1, 2, 3, 4];
    let mut message = vec![0x81, 0x80 | text.len() as u8];
    message.extend_from_slice(&mask);
    message.extend(
        text.iter()
            .zip(mask.iter().cycle())
            .map(|(byte, mask)| byte ^ mask),
    );
    message
}

#[tokio::test]
async fn a_stream_outlives_every_http_timeout() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, SHORT)).await;
    let (code, cookie) = login("127.0.0.1", server, TOKEN).await;
    assert_eq!(code, 200);
    let mut stream = TcpStream::connect(server).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /ws HTTP/1.1\r\nHost: gliss\r\nOrigin: {ORIGIN}\r\nCookie: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: AAECAwQFBgcICQoLDA0ODw==\r\nSec-WebSocket-Version: 13\r\n\r\n",
                cookie.unwrap()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(status(&mut stream).await, 101);
    let (opcode, hello) = timeout(WAIT, frame(&mut stream)).await.unwrap();
    assert_eq!(opcode, 1);
    assert!(
        String::from_utf8(hello)
            .unwrap()
            .contains(r#""type":"hello""#)
    );
    // The first picture is acknowledged, as a browser does, so the stream has
    // no deadline of its own running while the HTTP timeouts pass.
    let sequence = timeout(WAIT, async {
        loop {
            let (opcode, payload) = frame(&mut stream).await;
            if opcode == 2 && payload.starts_with(b"GLV1") {
                break u32::from_le_bytes(payload[4..8].try_into().unwrap());
            }
        }
    })
    .await
    .expect("the stream must send its first frame");
    let acknowledgement = json!({"type":"ack","sequence":sequence}).to_string();
    stream
        .write_all(&masked(acknowledgement.as_bytes()))
        .await
        .unwrap();
    // Many times the header and idle timeout, with nothing sent by the browser.
    tokio::time::sleep(SHORT * 6).await;
    stream
        .write_all(&masked(br#"{"type":"ping","id":5}"#))
        .await
        .unwrap();
    timeout(WAIT, async {
        loop {
            let (opcode, payload) = frame(&mut stream).await;
            if opcode == 1 && payload == br#"{"id":5,"type":"pong"}"# {
                break;
            }
        }
    })
    .await
    .expect("the stream must still answer");
}

#[tokio::test]
async fn failed_logins_at_one_address_do_not_lock_out_another() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, WAIT)).await;
    let mut refused = 0;
    for _ in 0..40 {
        match login("127.0.0.1", server, "wrong").await.0 {
            401 => assert_eq!(
                refused, 0,
                "a guess was checked after the address was limited"
            ),
            429 => refused += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    // Guessing stays limited at the address that guesses.
    assert_eq!(refused, 10);
    assert_eq!(login("127.0.0.1", server, TOKEN).await.0, 429);
    // Another address logs in as if nothing had happened.
    let (code, cookie) = login("127.0.0.2", server, TOKEN).await;
    assert_eq!(code, 200);
    assert!(cookie.unwrap().starts_with("gliss_session="));
}

#[tokio::test]
async fn successful_logins_use_none_of_the_allowance() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, WAIT)).await;
    for _ in 0..40 {
        assert_eq!(login("127.0.0.1", server, TOKEN).await.0, 200);
    }
    // The whole allowance of failures is still there afterwards.
    for _ in 0..30 {
        assert_eq!(login("127.0.0.1", server, "wrong").await.0, 401);
    }
    assert_eq!(login("127.0.0.1", server, "wrong").await.0, 429);
}

#[tokio::test]
async fn the_origin_and_the_allowance_are_checked_before_a_login_body_is_read() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, WAIT)).await;
    // The head announces a body that never comes.
    let mut foreign = TcpStream::connect(server).await.unwrap();
    foreign
        .write_all(login_head("https://untrusted.invalid", 64).as_bytes())
        .await
        .unwrap();
    assert_eq!(status(&mut foreign).await, 403);
    for _ in 0..30 {
        assert_eq!(login("127.0.0.3", server, "wrong").await.0, 401);
    }
    let mut limited = connect_from("127.0.0.3", server).await;
    limited
        .write_all(login_head(ORIGIN, 64).as_bytes())
        .await
        .unwrap();
    assert_eq!(status(&mut limited).await, 429);
}

#[tokio::test]
async fn a_login_body_that_stalls_is_answered_408_and_the_connection_closed() {
    let app = Arc::new(App {
        login_body_timeout: SHORT,
        ..Arc::into_inner(app(&["--demo"])).unwrap()
    });
    let server = serve(&app, limits(16, WAIT)).await;
    let mut slow = TcpStream::connect(server).await.unwrap();
    slow.write_all(format!("{}{{\"token\":", login_head(ORIGIN, 64)).as_bytes())
        .await
        .unwrap();
    let said = until_closed(&mut slow).await;
    assert!(said.starts_with("HTTP/1.1 408"), "{said}");
}

#[tokio::test]
async fn a_login_body_over_4096_bytes_is_refused_unread() {
    let app = app(&["--demo"]);
    let server = serve(&app, limits(16, WAIT)).await;
    let mut large = TcpStream::connect(server).await.unwrap();
    large
        .write_all(login_head(ORIGIN, 4097).as_bytes())
        .await
        .unwrap();
    assert_eq!(status(&mut large).await, 413);
    // The limit itself is accepted and parsed: this body is JSON, padded with spaces.
    let body = format!("{:<4096}", json!({ "token": TOKEN }).to_string());
    let mut exact = TcpStream::connect(server).await.unwrap();
    exact
        .write_all(format!("{}{body}", login_head(ORIGIN, body.len())).as_bytes())
        .await
        .unwrap();
    assert_eq!(status(&mut exact).await, 200);
}
