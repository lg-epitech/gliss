use super::*;
use crate::{App, Args, Input, Source};
use clap::Parser;
use futures_util::FutureExt;
use serde_json::json;
use std::sync::{Arc, Mutex, atomic::AtomicU64};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    time::{advance, timeout},
};

const PERIOD: Duration = Duration::from_millis(100);

#[test]
fn cli_omission_selects_eager_and_every_policy_is_selectable() {
    let base = ["gliss-server", "--vnc", "127.0.0.1:5901", "--token", "test"];
    assert_eq!(
        Args::try_parse_from(base).unwrap().rfb_request_policy,
        Policy::Eager
    );
    assert_eq!(
        Args::try_parse_from(base.into_iter().chain(["--rfb-request-policy", "eager"]))
            .unwrap()
            .rfb_request_policy,
        Policy::Eager
    );
    assert_eq!(
        Args::try_parse_from(base.into_iter().chain(["--rfb-request-policy", "tick"]))
            .unwrap()
            .rfb_request_policy,
        Policy::Tick
    );
    assert_eq!(
        Args::try_parse_from(base.into_iter().chain(["--rfb-request-policy", "ready"]))
            .unwrap()
            .rfb_request_policy,
        Policy::Ready
    );
    assert!(
        Args::try_parse_from(base.into_iter().chain(["--rfb-request-policy", "burst"])).is_err()
    );
}

#[test]
fn eager_spacing_is_25_ms_between_a_fast_cap_and_twice_a_slow_one() {
    let fps = |value: u32| Duration::from_secs(1) / value;
    assert_eq!(Policy::Tick.spacing(fps(30)), None);
    assert_eq!(Policy::Ready.spacing(fps(30)), Some(fps(30)));
    // Below QEMU's 30 ms timer for every cap from 20 to 40 fps.
    for cap in [20, 24, 30, 34, 40] {
        assert_eq!(
            Policy::Eager.spacing(fps(cap)),
            Some(Duration::from_millis(25))
        );
    }
    // A faster cap is its own limit; a slower one allows twice its rate.
    assert_eq!(Policy::Eager.spacing(fps(60)), Some(fps(60)));
    assert_eq!(Policy::Eager.spacing(fps(10)), Some(fps(20)));
    assert_eq!(Policy::Eager.spacing(fps(1)), Some(fps(2)));
}

#[tokio::test(start_paused = true)]
async fn refresh_hold_runs_for_one_second_after_activity_and_then_stops() {
    let mut hold = RefreshHold::new();
    assert!(!hold.active(), "an untouched desktop is left alone");

    // Input resets the server's timer itself, so the first hint is a period later.
    hold.input_sent();
    assert!(hold.active());
    assert_eq!(hold.deadline(), Instant::now() + Duration::from_millis(25));
    let mut hints = 0;
    while hold.active() {
        tokio::time::sleep_until(hold.deadline()).await;
        if hold.active() {
            hold.hint_sent();
            hints += 1;
        }
    }
    assert_eq!(hints, 39, "one hint per 25 ms within the second");

    // A display change extends the hold and the stale deadline fires at once.
    advance(Duration::from_secs(5)).await;
    hold.display_changed();
    assert!(hold.active());
    assert!(hold.deadline() <= Instant::now());
    advance(Duration::from_millis(1001)).await;
    assert!(!hold.active());
}

#[tokio::test(start_paused = true)]
async fn pending_and_stale_notifications_never_authorize_a_request() {
    let pending = AtomicBool::new(true);
    let completion = Notify::new();
    let ready = Ready::new(PERIOD);
    completion.notify_one();
    let mut wait = Box::pin(ready.wait(&pending, &completion));
    assert!(wait.as_mut().now_or_never().is_none());
    advance(PERIOD * 10).await;
    assert!(wait.as_mut().now_or_never().is_none());
    pending.store(false, Ordering::Release);
    completion.notify_one();
    assert!(wait.as_mut().now_or_never().is_some());
}

#[tokio::test(start_paused = true)]
async fn completion_before_registration_is_retained_and_rate_gated() {
    let pending = AtomicBool::new(true);
    let completion = Notify::new();
    let mut ready = Ready::new(PERIOD);
    ready.invoked();
    advance(PERIOD / 4).await;
    // Complete before even constructing/polling the wait future.
    pending.store(false, Ordering::Release);
    completion.notify_one();
    let mut wait = Box::pin(ready.wait(&pending, &completion));
    assert!(wait.as_mut().now_or_never().is_none());
    advance(PERIOD / 2).await;
    assert!(wait.as_mut().now_or_never().is_none());
    advance(PERIOD / 4).await;
    assert!(wait.as_mut().now_or_never().is_some());
}

#[tokio::test(start_paused = true)]
async fn canceling_for_input_keeps_the_original_invocation_deadline() {
    let pending = AtomicBool::new(false);
    let completion = Notify::new();
    let mut ready = Ready::new(PERIOD);
    ready.invoked();
    let mut wait = Box::pin(ready.wait(&pending, &completion));
    assert!(wait.as_mut().now_or_never().is_none());
    advance(PERIOD / 2).await;
    drop(wait); // The input arm won the actor's select.
    let mut resumed = Box::pin(ready.wait(&pending, &completion));
    assert!(resumed.as_mut().now_or_never().is_none());
    advance(PERIOD / 2).await;
    assert!(resumed.as_mut().now_or_never().is_some());
}

#[tokio::test(start_paused = true)]
async fn late_completion_starts_one_request_and_reanchors_without_catch_up() {
    let pending = AtomicBool::new(true);
    let completion = Notify::new();
    let mut ready = Ready::new(PERIOD);
    ready.invoked();
    advance(PERIOD * 7).await;
    pending.store(false, Ordering::Release);
    completion.notify_one();
    assert!(ready.wait(&pending, &completion).now_or_never().is_some());
    assert!(!pending.swap(true, Ordering::AcqRel));
    ready.invoked(); // A real invocation, rather than the old overdue deadline.
    pending.store(false, Ordering::Release);
    completion.notify_one();
    let mut next = Box::pin(ready.wait(&pending, &completion));
    assert!(next.as_mut().now_or_never().is_none());
    advance(PERIOD / 2).await;
    assert!(next.as_mut().now_or_never().is_none());
    advance(PERIOD / 2).await;
    assert!(next.as_mut().now_or_never().is_some());
}

fn app(address: &str, policy: &str) -> App {
    let args = Args::try_parse_from([
        "gliss-server",
        "--vnc",
        address,
        "--token",
        "test",
        "--fps",
        "10",
        "--rfb-request-policy",
        policy,
    ])
    .unwrap();
    App {
        args,
        mode: None,
        hybrid_config: gliss_hybrid::HybridConfig::default(),
        hybrid_mask_grid: None,
        video_timing: None,
        datagram: None,
        benchmark: None,
        stage_trace: None,
        tcp_listener: crate::tcp_listener::SocketPolicy::new(false),
        #[cfg(unix)]
        rfb_fd: None,
        source_deadline: None,
        worker_shutdown: watch::channel(false).0,
        origins: Vec::new(),
        cookie_key: [0; 32],
        clients: Arc::new(Semaphore::new(1)),
        encoders: Arc::new(Semaphore::new(1)),
        login_attempts: Mutex::new((std::time::Instant::now(), 0)),
        bytes: AtomicU64::new(0),
        frames: AtomicU64::new(0),
        started: std::time::Instant::now(),
        shared_capture: None,
    }
}

async fn handshake(listener: TcpListener) -> TcpStream {
    let (mut peer, _) = listener.accept().await.unwrap();
    peer.set_nodelay(true).unwrap();
    peer.write_all(b"RFB 003.008\n").await.unwrap();
    let mut banner = [0; 12];
    peer.read_exact(&mut banner).await.unwrap();
    assert_eq!(&banner, b"RFB 003.008\n");
    peer.write_all(&[1, 1]).await.unwrap();
    assert_eq!(peer.read_u8().await.unwrap(), 1);
    peer.write_u32(0).await.unwrap();
    assert_eq!(peer.read_u8().await.unwrap(), 1);
    peer.write_u16(16).await.unwrap();
    peer.write_u16(16).await.unwrap();
    peer.write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0])
        .await
        .unwrap();
    peer.write_u32(4).await.unwrap();
    peer.write_all(b"test").await.unwrap();
    let mut pixel_format = [0; 20];
    peer.read_exact(&mut pixel_format).await.unwrap();
    assert_eq!(pixel_format[0], 0);
    assert_eq!(peer.read_u8().await.unwrap(), 2);
    assert_eq!(peer.read_u8().await.unwrap(), 0);
    let count = peer.read_u16().await.unwrap();
    for _ in 0..count {
        peer.read_i32().await.unwrap();
    }
    peer
}

async fn actor(policy: &str) -> (Source, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = app(&listener.local_addr().unwrap().to_string(), policy);
    let (source, peer) = timeout(Duration::from_secs(2), async {
        tokio::join!(crate::source(&app), handshake(listener))
    })
    .await
    .unwrap();
    (source.unwrap(), peer)
}

async fn message(peer: &mut TcpStream, length: usize) -> Vec<u8> {
    let mut bytes = vec![0; length];
    timeout(Duration::from_secs(2), peer.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    bytes
}

fn request(incremental: bool, width: u16, height: u16) -> Vec<u8> {
    let mut bytes = vec![3, u8::from(incremental), 0, 0, 0, 0];
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

async fn no_message(peer: &mut TcpStream) {
    // Negative wire evidence across several request periods, not a scheduler
    // count copied from the implementation.
    assert!(timeout(PERIOD * 3, peer.read_u8()).await.is_err());
}

async fn stop(mut source: Source, mut peer: TcpStream) {
    drop(source.input);
    timeout(Duration::from_secs(2), source.shutdown.take().unwrap())
        .await
        .unwrap()
        .unwrap();
    // Stream teardown drops the JoinSet. Await cancellation here as additional
    // test evidence that neither original actor survives its source owner.
    source._tasks.shutdown().await;
    let mut tail = Vec::new();
    timeout(Duration::from_secs(2), peer.read_to_end(&mut tail))
        .await
        .unwrap()
        .unwrap();
    assert!(
        tail.is_empty(),
        "unexpected writes after source shutdown: {tail:?}"
    );
}

#[tokio::test]
async fn real_actor_waits_for_completion_and_resize_still_forces_full_refresh() {
    for policy in ["tick", "ready"] {
        let (mut source, mut peer) = actor(policy).await;
        assert_eq!(message(&mut peer, 10).await, request(false, 16, 16));
        no_message(&mut peer).await; // Pending prevents duplicates after late ticks.
        peer.write_all(&[0, 0, 0, 0]).await.unwrap(); // Empty completion still counts.
        timeout(Duration::from_secs(2), source.frames.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message(&mut peer, 10).await, request(true, 16, 16));
        no_message(&mut peer).await;
        let mut resized = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 20, 0, 12];
        resized.extend_from_slice(&(-223i32).to_be_bytes());
        peer.write_all(&resized).await.unwrap();
        timeout(Duration::from_secs(2), source.frames.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message(&mut peer, 10).await, request(false, 20, 12));
        no_message(&mut peer).await;
        stop(source, peer).await;
    }
}

/// QEMU's audio format message, which the refresh hold repeats.
const AUDIO_FORMAT: [u8; 10] = [255, 1, 0, 2, 3, 2, 0, 0, 187, 128];

/// Long enough without a client message to call the connection quiet, with
/// room for a loaded machine to delay a hint.
const QUIET: Duration = Duration::from_millis(500);

/// What a client wrote before it went quiet. `audio` holds QEMU audio control
/// codes other than the format message: 0 enables capture, 1 disables it.
#[derive(Debug, Default, PartialEq)]
struct Written {
    requests: usize,
    formats: usize,
    keys: usize,
    audio: Vec<u8>,
}

async fn until_quiet(peer: &mut TcpStream) -> Written {
    let mut written = Written::default();
    while let Ok(kind) = timeout(QUIET, peer.read_u8()).await {
        match kind.unwrap() {
            3 => {
                assert_eq!(message(peer, 9).await, request(true, 16, 16)[1..]);
                written.requests += 1;
            }
            4 => {
                assert_eq!(message(peer, 7).await[1..], [0, 0, 0, 0, 0, 65]);
                written.keys += 1;
            }
            255 => match message(peer, 3).await[..] {
                [1, 0, 2] => {
                    assert_eq!(message(peer, 6).await, AUDIO_FORMAT[4..]);
                    written.formats += 1;
                }
                [1, 0, control] => written.audio.push(control),
                ref other => panic!("unexpected QEMU client message {other:?}"),
            },
            other => panic!("unexpected client message {other}"),
        }
    }
    written
}

#[tokio::test]
async fn real_actor_holds_refresh_for_a_second_after_activity_and_keeps_requesting() {
    let (source, mut peer) = actor("eager").await;
    assert_eq!(message(&mut peer, 10).await, request(false, 16, 16));
    // A server that has not acknowledged QEMU audio is only ever sent requests.
    peer.write_all(&[0, 0, 0, 0]).await.unwrap();
    let written = until_quiet(&mut peer).await;
    assert_eq!((written.requests, written.formats), (1, 0));

    // The acknowledgement arrives in a display update, which starts a hold.
    let mut acknowledged = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 16, 0, 16];
    acknowledged.extend_from_slice(&(-259i32).to_be_bytes());
    let started = std::time::Instant::now();
    peer.write_all(&acknowledged).await.unwrap();
    let written = until_quiet(&mut peer).await;
    let held = started.elapsed() - QUIET;
    assert_eq!(written.requests, 1, "one request stays pending");
    // About 40 hints at full speed; a loaded machine sends fewer, never more.
    assert!(
        (10..=41).contains(&written.formats),
        "{} hints in one hold",
        written.formats
    );
    assert!(
        (Duration::from_millis(900)..Duration::from_secs(3)).contains(&held),
        "hold lasted {held:?}"
    );

    // Input starts another hold. An update that completes during it is still
    // followed by the next request, and enabling sound during it enables
    // capture exactly once: the hints repeat the format, never the switch.
    for (down, id) in [(true, 1), (false, 2)] {
        let key = Input::Key {
            keysym: 65,
            down,
            id,
        };
        source.input.send(key).await.unwrap();
    }
    let sound = |enabled| json!({"type": "audio", "enabled": enabled, "codec": "pcm"});
    crate::audio::request(&source.audio.request, sound(true)).unwrap();
    peer.write_all(&[0, 0, 0, 0]).await.unwrap();
    let written = until_quiet(&mut peer).await;
    assert_eq!((written.requests, written.keys), (1, 2));
    assert_eq!(written.audio, [0]);
    assert!(
        (10..=42).contains(&written.formats),
        "{} hints in one hold",
        written.formats
    );

    // Turning sound off is not activity: one message, then quiet.
    crate::audio::request(&source.audio.request, sound(false)).unwrap();
    let written = until_quiet(&mut peer).await;
    assert_eq!((written.audio.as_slice(), written.formats), (&[1][..], 0));
    stop(source, peer).await;
}

#[tokio::test]
async fn real_actor_serializes_input_and_releases_held_inputs_while_pending() {
    let (source, mut peer) = actor("ready").await;
    assert_eq!(message(&mut peer, 10).await, request(false, 16, 16));
    source
        .input
        .send(Input::Key {
            keysym: 65,
            down: true,
            id: 7,
        })
        .await
        .unwrap();
    source
        .input
        .send(Input::Pointer {
            x: 3,
            y: 4,
            buttons: 1,
            id: 8,
        })
        .await
        .unwrap();
    assert_eq!(message(&mut peer, 8).await, vec![4, 1, 0, 0, 0, 0, 0, 65]);
    assert_eq!(message(&mut peer, 6).await, vec![5, 1, 0, 3, 0, 4]);
    let Source {
        input,
        mut shutdown,
        mut _tasks,
        ..
    } = source;
    drop(input); // Cancels the pending ready wait through the original select.
    assert_eq!(message(&mut peer, 8).await, vec![4, 0, 0, 0, 0, 0, 0, 65]);
    assert_eq!(message(&mut peer, 6).await, vec![5, 0, 0, 3, 0, 4]);
    timeout(Duration::from_secs(2), shutdown.take().unwrap())
        .await
        .unwrap()
        .unwrap();
    // Retain task ownership through the notice; then retire the same reader.
    _tasks.shutdown().await;
    let mut tail = Vec::new();
    timeout(Duration::from_secs(2), peer.read_to_end(&mut tail))
        .await
        .unwrap()
        .unwrap();
    assert!(tail.is_empty());
}

#[tokio::test]
async fn real_actor_disconnect_reports_error_and_pending_wait_cancels() {
    let (mut source, mut peer) = actor("ready").await;
    assert_eq!(message(&mut peer, 10).await, request(false, 16, 16));
    peer.shutdown().await.unwrap();
    let event = timeout(Duration::from_secs(2), source.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event["type"], "error");
    stop(source, peer).await;
}
