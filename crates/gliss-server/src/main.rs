mod audio;
mod audio_adpcm;
mod demo;
mod held;
mod http;
mod notices;
mod pacer;
mod profiling;
mod rfb_request;
mod scheduler;
mod tcp_listener;

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use gliss_core::{Frame, Input, MAX_CLIPBOARD};
use gliss_rfb::Event;
use gliss_video::{VideoEncoder, VideoTiming};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{Semaphore, broadcast, mpsc, oneshot, watch},
    task::JoinSet,
};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "CPU-only browser desktop streaming, bound to loopback by default"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,
    /// Fixed accepted HTTP/WebSocket TCP_NODELAY policy; verified before HTTP IO.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    tcp_nodelay: bool,
    /// Trusted audio packet duration (20 or 40 ms); does not change the freshness budget.
    #[arg(long, default_value_t = audio::PacketDuration::DEFAULT)]
    audio_packet_ms: audio::PacketDuration,
    /// Fixed upstream target. The browser cannot choose a VNC address.
    #[arg(long, conflicts_with = "demo", required_unless_present_any = ["demo", "export_web"])]
    vnc: Option<String>,
    /// When to ask the VNC server for the next update. The FPS cap limits encoding either way.
    #[arg(long, value_enum, default_value = "eager")]
    rfb_request_policy: rfb_request::Policy,
    #[arg(long)]
    demo: bool,
    #[arg(
        long,
        env = "GLISS_TOKEN",
        hide_env_values = true,
        required_unless_present = "export_web"
    )]
    token: Option<String>,
    /// Write the embedded browser modules to DIR and exit, so another web app can serve them.
    #[arg(long, value_name = "DIR")]
    export_web: Option<std::path::PathBuf>,
    #[arg(long, env = "GLISS_VNC_PASSWORD", hide_env_values = true)]
    vnc_password: Option<String>,
    /// Exact browser origin, required when listening outside loopback. Use TLS at your proxy.
    #[arg(long)]
    origin: Option<String>,
    #[arg(long, default_value = "h264", value_parser = ["h264"])]
    codec: String,
    #[arg(long, default_value_t = 4_000_000)]
    h264_bitrate: u32,
    /// H.264 periodic IDR interval in encoded frames; defaults to 3600.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=3600))]
    video_intra_period: Option<u32>,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Experimental media window; the stable default waits for each frame ACK.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=8))]
    in_flight_frames: u8,
    /// Queued and unacknowledged media bytes; one oversized packet may progress alone.
    #[arg(long, default_value_t = scheduler::DEFAULT_BYTES)]
    in_flight_bytes: usize,
    /// Emit bounded 1 Hz scheduler timing/capacity diagnostics.
    #[arg(long)]
    stream_stats: bool,
    #[arg(long, default_value_t = 8)]
    max_clients: usize,
    #[arg(long, default_value_t = 3600)]
    session_ttl: u64,
    #[arg(long, default_value = "static")]
    workload: String,
}

#[cfg(test)]
mod audio_option_tests {
    use super::*;

    #[test]
    fn duration_defaults_to_20_and_rejects_noncanonical_values_during_argument_parsing() {
        let parse = |extra: &[&str]| {
            Args::try_parse_from(
                [
                    "gliss-server",
                    "--demo",
                    "--token",
                    "01234567890123456789012345678901",
                ]
                .into_iter()
                .chain(extra.iter().copied()),
            )
        };
        assert_eq!(
            parse(&[]).unwrap().audio_packet_ms,
            audio::PacketDuration::Ms20
        );
        for (text, expected) in [
            ("20", audio::PacketDuration::Ms20),
            ("40", audio::PacketDuration::Ms40),
        ] {
            assert_eq!(
                parse(&["--audio-packet-ms", text]).unwrap().audio_packet_ms,
                expected
            );
        }
        for invalid in [
            "0", "10", "30", "60", "020", "040", "+20", "20.0", "20ms", " 20", "40 ", "",
        ] {
            assert!(
                parse(&["--audio-packet-ms", invalid]).is_err(),
                "{invalid:?}"
            );
        }
    }
}

struct App {
    args: Args,
    video_timing: VideoTiming,
    tcp_listener: Arc<tcp_listener::SocketPolicy>,
    worker_shutdown: watch::Sender<bool>,
    origins: Vec<String>,
    cookie_key: [u8; 32],
    clients: Arc<Semaphore>,
    encoders: Arc<Semaphore>,
    login_attempts: Mutex<(Instant, u32)>,
    login_body_timeout: Duration,
    bytes: AtomicU64,
    frames: AtomicU64,
    started: Instant,
}

impl App {
    /// `args` must already be validated.
    fn new(args: Args, video_timing: VideoTiming, origins: Vec<String>) -> Self {
        let (worker_shutdown, _) = watch::channel(false);
        Self {
            tcp_listener: tcp_listener::SocketPolicy::new(args.tcp_nodelay),
            video_timing,
            worker_shutdown,
            origins,
            cookie_key: rand::random(),
            clients: Arc::new(Semaphore::new(args.max_clients)),
            encoders: Arc::new(Semaphore::new(2)),
            login_attempts: Mutex::new((Instant::now(), 0)),
            login_body_timeout: LOGIN_BODY_TIMEOUT,
            bytes: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            started: Instant::now(),
            args,
        }
    }
}

/// Browser modules served at `/<path>`; `--export-web` writes the same files.
const WEB_ASSETS: &[(&str, &str, &str)] = &[
    (
        "profiling.js",
        "text/javascript",
        include_str!("../../../web/profiling.js"),
    ),
    (
        "viewer.js",
        "text/javascript",
        include_str!("../../../web/viewer.js"),
    ),
    (
        "client.js",
        "text/javascript",
        include_str!("../../../web/client.js"),
    ),
    (
        "audio-player.js",
        "text/javascript",
        include_str!("../../../web/audio-player.js"),
    ),
    (
        "audio-adpcm.js",
        "text/javascript",
        include_str!("../../../web/audio-adpcm.js"),
    ),
    (
        "video-decoder.js",
        "text/javascript",
        include_str!("../../../web/video-decoder.js"),
    ),
    (
        "probe.js",
        "text/javascript",
        include_str!("../../../web/probe.js"),
    ),
];

fn export_web(directory: &std::path::Path) -> Result<()> {
    for (path, _, body) in WEB_ASSETS {
        let target = directory.join(path);
        std::fs::create_dir_all(target.parent().unwrap_or(directory))?;
        std::fs::write(&target, body)
            .with_context(|| format!("cannot write {}", target.display()))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(directory) = &args.export_web {
        return export_web(directory);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(args))
}

fn router(app: Arc<App>) -> Router {
    let mut router = Router::new().route(
        "/",
        get(|| async { Html(include_str!("../../../web/index.html")) }),
    );
    for &(path, content_type, body) in WEB_ASSETS {
        router = router.route(
            &format!("/{path}"),
            get(move || async move { ([(header::CONTENT_TYPE, content_type)], body) }),
        );
    }
    router
        .route("/health", get(|| async { Json(json!({"status":"ok","version":env!("CARGO_PKG_VERSION")})) }))
        .route("/api/session", post(login).delete(logout))
        .route("/api/stats", get(stats))
        .route("/ws", get(upgrade))
        .layer(DefaultBodyLimit::max(LOGIN_BODY_LIMIT))
        .layer(axum::middleware::map_response(|mut response: Response| async move {
            let h = response.headers_mut();
            h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            h.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
            h.insert("x-content-type-options", "nosniff".parse().unwrap());
            h.insert("referrer-policy", "no-referrer".parse().unwrap());
            response
        }))
        .with_state(app)
}

async fn run(args: Args) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    if args.token.as_ref().is_none_or(|token| token.len() < 24) {
        bail!("GLISS_TOKEN must contain at least 24 characters");
    }
    if args.fps == 0 || args.fps > 120 || args.max_clients == 0 || args.max_clients > 128 {
        bail!("fps must be 1..120; max-clients must be 1..128");
    }
    if args.session_ttl == 0 || args.session_ttl > 3600 {
        bail!("session-ttl must be 1..3600 seconds");
    }
    if !(1024..=64 * 1024 * 1024).contains(&args.in_flight_bytes) {
        bail!("in-flight-bytes must be 1024..67108864");
    }
    if !args.bind.ip().is_loopback()
        && !args
            .origin
            .as_ref()
            .is_some_and(|o| o.starts_with("https://"))
    {
        bail!("non-loopback bind requires an exact HTTPS --origin and a TLS reverse proxy");
    }
    let video_timing = configured_video_timing(&args)?;
    if args.fps > 60 || !(1_000..=50_000_000).contains(&args.h264_bitrate) {
        bail!("software H264 requires fps <= 60 and bitrate 1000..50000000");
    }
    demo::validate_workload(&args.workload)?;
    let origins = match &args.origin {
        Some(o) => vec![o.trim_end_matches('/').to_owned()],
        None => vec![
            format!("http://{}", args.bind),
            format!("http://localhost:{}", args.bind.port()),
        ],
    };
    let bind = args.bind;
    let app = Arc::new(App::new(args, video_timing, origins));
    let tcp_listener = app.tcp_listener.clone();
    let router = router(app.clone());
    let listener = tcp_listener::VerifiedListener::new(
        tokio::net::TcpListener::bind(bind).await?,
        tcp_listener,
    );
    println!("Gliss listening on http://{bind} (CPU-only)");
    let shutdown_signal = app.worker_shutdown.clone();
    let limits = http::Limits::for_clients(app.args.max_clients);
    http::serve(listener, router, limits, async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_signal.send_replace(true);
    })
    .await;
    app.worker_shutdown.send_replace(true);
    Ok(())
}

#[cfg(test)]
mod option_tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        Args::try_parse_from(
            [
                "gliss-server",
                "--demo",
                "--token",
                "01234567890123456789012345678901",
            ]
            .into_iter()
            .chain(extra.iter().copied()),
        )
        .unwrap()
    }

    #[test]
    fn export_web_writes_every_served_module_without_a_token_or_source() {
        let directory = std::env::temp_dir().join(format!("gliss-export-{}", std::process::id()));
        let args =
            Args::try_parse_from(["gliss-server", "--export-web", directory.to_str().unwrap()])
                .unwrap();
        assert_eq!(args.export_web.as_deref(), Some(directory.as_path()));
        export_web(&directory).unwrap();
        for (path, _, body) in WEB_ASSETS {
            assert_eq!(
                std::fs::read_to_string(directory.join(path)).unwrap(),
                *body,
                "{path}"
            );
        }
        for path in ["client.js", "video-decoder.js", "audio-player.js"] {
            assert!(
                WEB_ASSETS.iter().any(|(name, _, _)| *name == path),
                "{path}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn codec_defaults_to_h264_and_accepts_nothing_else() {
        assert_eq!(args(&[]).codec, "h264");
        assert_eq!(args(&["--codec", "h264"]).codec, "h264");
        let base = [
            "gliss-server",
            "--demo",
            "--token",
            "01234567890123456789012345678901",
        ];
        for other in ["deflate", "batch", "hybrid", "H264", ""] {
            assert!(Args::try_parse_from(base.into_iter().chain(["--codec", other])).is_err());
        }
    }

    #[test]
    fn version_flag_reports_the_package_version_without_other_arguments() {
        let error = Args::try_parse_from(["gliss-server", "--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string().trim(),
            concat!("gliss-server ", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn tcp_nodelay_is_explicit_boolean_and_defaults_true() {
        assert!(args(&[]).tcp_nodelay);
        assert!(!args(&["--tcp-nodelay", "false"]).tcp_nodelay);
        assert!(args(&["--tcp-nodelay", "true"]).tcp_nodelay);
        let base = [
            "gliss-server",
            "--demo",
            "--token",
            "01234567890123456789012345678901",
        ];
        for extra in [
            vec!["--tcp-nodelay"],
            vec!["--tcp-nodelay", "1"],
            vec!["--tcp-nodelay", "TRUE"],
            vec!["--tcp-nodelay", "true", "--tcp-nodelay", "false"],
        ] {
            assert!(Args::try_parse_from(base.into_iter().chain(extra)).is_err());
        }
    }
}

/// The longest interval the encoder accepts: two minutes of frames at 30 fps.
const DEFAULT_INTRA_PERIOD: u32 = 3600;

fn configured_video_timing(args: &Args) -> Result<VideoTiming> {
    // A WebSocket stream loses nothing, and its client asks for a keyframe after a
    // decode error. A periodic keyframe there only costs a burst of bytes and
    // resets the picture to a coarser quality, so it is as rare as the encoder allows.
    VideoTiming::new(
        args.fps,
        args.video_intra_period.unwrap_or(DEFAULT_INTRA_PERIOD),
    )
}

/// Clients are told the encoder timing only when the operator chose the keyframe interval.
fn with_video_metadata(
    mut value: serde_json::Value,
    args: &Args,
    timing: VideoTiming,
) -> serde_json::Value {
    if args.video_intra_period.is_some() {
        // Access units are timestamped at the rate-control rate.
        value["video_config"] = json!({
            "fps": timing.fps(),
            "timestamp_fps": timing.fps(),
            "intra_period": timing.intra_period(),
        });
    }
    value
}

fn with_stream_metadata(mut value: serde_json::Value, app: &App) -> serde_json::Value {
    value["tcp_listener"] = json!(app.tcp_listener.snapshot());
    value["audio_packet_ms"] = json!(app.args.audio_packet_ms.millis());
    with_video_metadata(value, &app.args, app.video_timing)
}

fn origin_allowed(app: &App, headers: &HeaderMap) -> bool {
    headers
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|v| app.origins.iter().any(|o| o == v))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn session_remaining_at(expires_at: u64, current: SystemTime) -> Duration {
    UNIX_EPOCH
        .checked_add(Duration::from_secs(expires_at))
        .and_then(|deadline| deadline.duration_since(current).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod session_tests {
    use super::*;
    #[test]
    fn fractional_wall_time_never_extends_the_signed_expiry() {
        let before = UNIX_EPOCH + Duration::from_millis(100_900);
        assert_eq!(
            session_remaining_at(110, before),
            Duration::from_millis(9_100)
        );
        assert_eq!(
            session_remaining_at(110, UNIX_EPOCH + Duration::from_secs(110)),
            Duration::ZERO
        );
        assert_eq!(
            session_remaining_at(110, UNIX_EPOCH + Duration::from_millis(110_001)),
            Duration::ZERO
        );
    }
}

#[cfg(test)]
mod video_timing_option_tests {
    use super::*;

    fn args(extra: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(
            [
                "gliss-server",
                "--demo",
                "--token",
                "01234567890123456789012345678901",
            ]
            .into_iter()
            .chain(extra.iter().copied()),
        )
    }

    #[test]
    fn keyframes_are_3600_encoded_frames_apart_at_any_fps() {
        for fps in ["15", "30", "60"] {
            let args = args(&["--codec", "h264", "--fps", fps]).unwrap();
            let timing = configured_video_timing(&args).unwrap();
            assert_eq!(timing.intra_period(), 3600);
        }
        let args = args(&["--codec", "h264", "--fps", "30"]).unwrap();
        let timing = configured_video_timing(&args).unwrap();
        let mut encoder = StreamEncoder {
            encoder: None,
            timing,
            bitrate: 2_000_000,
        };
        let frame = Frame::new(64, 48).unwrap();
        // Well past the former 240-frame interval, only the first frame is a keyframe.
        for sequence in 0..=300 {
            let bytes = encoder.encode(&frame, sequence, 0).unwrap();
            assert_eq!(
                gliss_video::parse_packet(&bytes).unwrap().keyframe,
                sequence == 0,
                "unexpected periodic keyframe at {sequence}"
            );
        }
        encoder.reset();
        let bytes = encoder.encode(&frame, 301, 0).unwrap();
        assert!(gliss_video::parse_packet(&bytes).unwrap().keyframe);
    }

    #[test]
    fn explicit_keyframe_intervals_are_preserved() {
        for period in ["1", "4", "60", "240", "3600"] {
            let args = args(&["--codec", "h264", "--video-intra-period", period]).unwrap();
            assert_eq!(
                configured_video_timing(&args).unwrap().intra_period(),
                period.parse::<u32>().unwrap()
            );
        }
    }

    #[test]
    fn default_rate_follows_capture_fps_and_metadata_stays_implicit() {
        for fps in ["15", "60"] {
            let args = args(&["--codec", "h264", "--fps", fps]).unwrap();
            let timing = configured_video_timing(&args).unwrap();
            assert_eq!(timing, VideoTiming::new(args.fps, 3600).unwrap());
            let original = json!({"codec": "h264", "fps_limit": args.fps});
            assert_eq!(
                with_video_metadata(original.clone(), &args, timing),
                original
            );
        }
    }

    #[test]
    fn an_explicit_keyframe_interval_is_described_to_the_client() {
        let args = args(&["--fps", "15", "--video-intra-period", "120"]).unwrap();
        let timing = configured_video_timing(&args).unwrap();
        assert_eq!(timing, VideoTiming::new(15, 120).unwrap());
        assert_eq!(
            with_video_metadata(json!({"fps_limit": args.fps}), &args, timing),
            json!({"fps_limit":15,"video_config":{
                "fps":15,"timestamp_fps":15,"intra_period":120
            }})
        );
    }

    #[test]
    fn keyframe_interval_cli_bounds_are_enforced() {
        for value in ["0", "3601", "4294967295", "-1", "x"] {
            assert!(args(&["--codec", "h264", "--video-intra-period", value]).is_err());
        }
    }

    #[test]
    fn lazy_video_encoder_retains_custom_timing_across_reset_and_resize() {
        let timing = VideoTiming::new(15, 120).unwrap();
        let mut encoder = StreamEncoder {
            encoder: None,
            timing,
            bitrate: 3_000_000,
        };
        encoder.reset();
        for (sequence, width, height) in [(1, 64, 48), (2, 64, 48), (3, 66, 50)] {
            let frame = Frame::new(width, height).unwrap();
            let bytes = encoder.encode(&frame, sequence, 2).unwrap();
            let packet = gliss_video::parse_packet(&bytes).unwrap();
            assert_eq!(packet.keyframe, sequence != 2);
            assert_eq!(encoder.encoder.as_ref().unwrap().timing(), timing);
        }
        encoder.reset();
        let bytes = encoder.encode(&Frame::new(66, 50).unwrap(), 4, 3).unwrap();
        assert!(gliss_video::parse_packet(&bytes).unwrap().keyframe);
    }
}

fn signed_cookie(app: &App) -> String {
    let payload = format!(
        "{}.{}",
        now() + app.args.session_ttl,
        hex::encode(rand::random::<[u8; 16]>())
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(&app.cookie_key).unwrap();
    mac.update(payload.as_bytes());
    format!("{payload}.{}", hex::encode(mac.finalize().into_bytes()))
}

fn authorized_until(app: &App, headers: &HeaderMap) -> Option<u64> {
    let value = headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| {
            s.split(';')
                .find_map(|c| c.trim().strip_prefix("gliss_session="))
        });
    let value = value?;
    if value.len() > 128 {
        return None;
    }
    let (payload, signature) = value.rsplit_once('.')?;
    let (expiry, nonce) = payload.split_once('.')?;
    let expiry = expiry.parse::<u64>().ok()?;
    if expiry <= now() || expiry > now() + app.args.session_ttl || nonce.len() != 32 {
        return None;
    }
    let signature = hex::decode(signature).ok()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&app.cookie_key).unwrap();
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature).is_ok().then_some(expiry)
}

/// A login body is `{"token":"..."}` and a key is something a person pastes.
const LOGIN_BODY_LIMIT: usize = 4096;
/// Time to send that body once the request head is in.
const LOGIN_BODY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct Login {
    token: String,
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim())
        .is_some_and(|kind| {
            let kind = kind.to_ascii_lowercase();
            kind == "application/json"
                || (kind.starts_with("application/") && kind.ends_with("+json"))
        })
}

async fn login(State(app): State<Arc<App>>, headers: HeaderMap, body: Body) -> Response {
    // Everything that needs no body comes first: a request that is refused
    // anyway cannot make the server wait for one.
    if !origin_allowed(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    {
        let mut rate = app.login_attempts.lock().unwrap();
        if rate.0.elapsed() > Duration::from_secs(60) {
            *rate = (Instant::now(), 0);
        }
        if rate.1 >= 30 {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        rate.1 += 1;
    }
    if !is_json(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let announced = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if announced.is_some_and(|length| length > LOGIN_BODY_LIMIT) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let read = axum::body::to_bytes(body, LOGIN_BODY_LIMIT);
    let bytes = match tokio::time::timeout(app.login_body_timeout, read).await {
        Ok(Ok(bytes)) => bytes,
        // A body without an announced length that outgrew the limit.
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        // hyper closes a connection whose request body was not read to its end.
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let body = match Json::<Login>::from_bytes(&bytes) {
        Ok(Json(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    if !bool::from(Sha256::digest(body.token.as_bytes()).ct_eq(&Sha256::digest(
        app.args.token.as_deref().unwrap_or_default().as_bytes(),
    ))) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let secure = if app.origins[0].starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    (
        [(
            header::SET_COOKIE,
            format!(
                "gliss_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}",
                signed_cookie(&app),
                app.args.session_ttl
            ),
        )],
        Json(json!({"ok":true})),
    )
        .into_response()
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !origin_allowed(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        [(
            header::SET_COOKIE,
            "gliss_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
        StatusCode::NO_CONTENT,
    )
        .into_response()
}

async fn stats(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if authorized_until(&app, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(with_stream_metadata(
        json!({"bytes":app.bytes.load(Ordering::Relaxed),"frames":app.frames.load(Ordering::Relaxed),"active_clients":app.args.max_clients-app.clients.available_permits(),"uptime_s":app.started.elapsed().as_secs_f64(),"codec":app.args.codec}),
        &app,
    ))
    .into_response()
}

async fn upgrade(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(expires_at) = authorized_until(&app, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(permit) = app.clients.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(MAX_CLIPBOARD + 1024)
        .max_frame_size(MAX_CLIPBOARD + 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            if let Err(error) = stream(socket, app, expires_at).await {
                tracing::warn!("stream ended: {error:#}");
            }
        })
        .into_response()
}

#[derive(Clone)]
struct Captured {
    frame: Arc<Frame>,
    forwarded_input_id: u32,
    published_at: Option<Instant>,
}

struct Source {
    audio: audio::Session,
    audio_counters: Arc<audio::Counters>,
    frames: watch::Receiver<Option<Captured>>,
    input: mpsc::Sender<Input>,
    notices: Arc<notices::Notices>,
    shutdown: Option<oneshot::Receiver<()>>,
    _tasks: JoinSet<()>,
}

async fn source(app: &App) -> Result<Source> {
    let (frame_tx, frames) = watch::channel(None);
    let (input, mut input_rx) = mpsc::channel::<Input>(128);
    let notices = Arc::new(notices::Notices::default());
    let (audio, mut audio_requests, audio_status) = audio::channel();
    let audio_config = audio::EncodeConfig::new(app.args.audio_packet_ms);
    let audio_counters = audio_config.counters.clone();
    let mut tasks = JoinSet::new();
    let mut shutdown = None;
    if app.args.demo {
        let workload = app.args.workload.clone();
        let fps = app.args.fps;
        tasks.spawn(async move {
            demo::run(&workload, fps, frame_tx, input_rx).await;
        });
    } else {
        let address = app
            .args
            .vnc
            .as_deref()
            .context("Missing fixed RFB source")?;
        let (mut reader, mut writer, info) = tokio::time::timeout(
            Duration::from_secs(10),
            gliss_rfb::connect(address, app.args.vnc_password.as_deref()),
        )
        .await
        .context("VNC connect timeout")??;
        let raw_audio = reader.audio_events();
        let mut audio_available = writer.audio_available();
        tasks.spawn(audio::encode_with(
            raw_audio,
            audio.request.subscribe(),
            audio.packets.clone(),
            audio_config,
        ));
        let input_id = Arc::new(AtomicU32::new(0));
        let dimensions = Arc::new(AtomicU32::new(
            (u32::from(info.width) << 16) | u32::from(info.height),
        ));
        let pending = Arc::new(AtomicBool::new(false));
        let refresh = Arc::new(AtomicBool::new(false));
        let period = Duration::from_secs_f64(1.0 / f64::from(app.args.fps));
        let spacing = app.args.rfb_request_policy.spacing(period);
        let completion = spacing.map(|_| Arc::new(tokio::sync::Notify::new()));
        let display_changed = Arc::new(tokio::sync::Notify::new());
        let reader_display_changed = display_changed.clone();
        let reader_completion = completion.clone();
        let reader_id = input_id.clone();
        let reader_size = dimensions.clone();
        let reader_pending = pending.clone();
        let reader_refresh = refresh.clone();
        let reader_notices = notices.clone();
        let input_notices = notices.clone();
        let profile_capture = app.args.stream_stats;
        tasks.spawn(async move {
            loop {
                let event = match reader.read_event().await {
                    Ok(event) => event,
                    Err(_) => {
                        reader_notices.failed("The VM display disconnected.");
                        break;
                    }
                };
                match event {
                    Event::Frame(frame) => {
                        let size = (u32::from(frame.width) << 16) | u32::from(frame.height);
                        if reader_size.swap(size, Ordering::AcqRel) != size {
                            reader_refresh.store(true, Ordering::Release);
                        }
                        reader_pending.store(false, Ordering::Release);
                        if let Some(completion) = &reader_completion {
                            completion.notify_one();
                        }
                        reader_display_changed.notify_one();
                        let frame = Arc::new(frame);
                        let forwarded_input_id = reader_id.load(Ordering::Acquire);
                        if frame_tx
                            .send(Some(Captured {
                                frame,
                                forwarded_input_id,
                                published_at: profile_capture.then(Instant::now),
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Event::Unchanged => {
                        // The request was answered, so the next one is due and the
                        // server is still active, but there is no picture to
                        // publish and nothing to encode.
                        reader_pending.store(false, Ordering::Release);
                        if let Some(completion) = &reader_completion {
                            completion.notify_one();
                        }
                        reader_display_changed.notify_one();
                    }
                    Event::Clipboard(text) => reader_notices.clipboard(text),
                    Event::Cursor(cursor) => reader_notices.cursor(cursor),
                    Event::Bell => reader_notices.bell(),
                    Event::ResizeResult {
                        width,
                        height,
                        status,
                    } => reader_notices.resize_result(width, height, status),
                }
            }
        });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown = Some(shutdown_rx);
        tasks.spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut ready = spacing.map(rfb_request::Ready::new);
            let mut hold = rfb_request::RefreshHold::new();
            let mut first = true;
            let mut held = held::Held::default();
            let mut audio_enabled = false;
            let initial_audio_request = *audio_requests.borrow_and_update();
            audio::update_capture(&mut writer, initial_audio_request, &mut audio_enabled, &audio_status).await;
            loop {
                tokio::select! {
                    changed = audio_requests.changed() => {
                        if changed.is_err() { break; }
                        let request = *audio_requests.borrow_and_update();
                        audio::update_capture(&mut writer, request, &mut audio_enabled, &audio_status).await;
                    }
                    changed = audio_available.changed() => {
                        if changed.is_err() { break; }
                        audio_available.borrow_and_update();
                        let request = *audio_requests.borrow();
                        audio::update_capture(&mut writer, request, &mut audio_enabled, &audio_status).await;
                    }
                    command = input_rx.recv() => {
                        let Some(command) = command else { break; };
                        if !held.admits(&command) { input_notices.input_error(held::TOO_MANY_KEYS.into()); continue; }
                        if let Err(error) = writer.send_input(&command).await { input_notices.input_error(error.to_string()); }
                        else { held.written(&command); input_id.store(command.id(), Ordering::Release); hold.input_sent(); }
                    }
                    _ = display_changed.notified() => {
                        hold.display_changed();
                    }
                    _ = tokio::time::sleep_until(hold.deadline()), if hold.active() && *audio_available.borrow() => {
                        if writer.hold_refresh().await.is_err() { break; }
                        hold.hint_sent();
                    }
                    _ = async {
                        if let (Some(ready), Some(completion)) = (&ready, &completion) {
                            ready.wait(&pending, completion).await;
                        } else {
                            tick.tick().await;
                        }
                    } => {
                        if !pending.swap(true, Ordering::AcqRel) {
                            let size = dimensions.load(Ordering::Relaxed);
                            let force_refresh = refresh.swap(false, Ordering::AcqRel);
                            if let Some(ready) = &mut ready { ready.invoked(); }
                            if writer.request_update(!first && !force_refresh, (size >> 16) as u16, size as u16).await.is_err() { break; }
                            first = false;
                        }
                    }
                }
            }
            if audio_enabled { let _ = writer.set_audio(false).await; }
            for release in held.releases() { let _ = writer.send_input(&release).await; }
            // The session learns its held input is released before the connection closes.
            let _ = shutdown_tx.send(());
            drop(writer);
        });
    }
    Ok(Source {
        audio,
        audio_counters,
        frames,
        input,
        notices,
        shutdown,
        _tasks: tasks,
    })
}

async fn stream(socket: WebSocket, app: Arc<App>, expires_at: u64) -> Result<()> {
    let mut worker_shutdown = app.worker_shutdown.subscribe();
    let remaining = session_remaining_at(expires_at, SystemTime::now());
    anyhow::ensure!(
        !remaining.is_zero(),
        "display session expired before source setup"
    );
    let deadline = tokio::time::Instant::now() + remaining;
    let mut source = tokio::select! {
        result = tokio::time::timeout_at(deadline, source(&app)) => result.context("display session expired during source setup")??,
        _ = worker_shutdown.wait_for(|stopped| *stopped) => bail!("worker stopped during source setup"),
    };
    let (sink, receiver) = socket.split();
    let result = tokio::select! {
        result = tokio::time::timeout_at(deadline, stream_session(sink, receiver, app.clone(), &mut source)) => result.context("display session expired").and_then(|result| result),
        _ = worker_shutdown.wait_for(|stopped| *stopped) => Err(anyhow::anyhow!("worker stopped during stream")),
    };
    drop(source.input);
    if let Some(shutdown) = source.shutdown.take() {
        let _ = tokio::time::timeout(Duration::from_secs(1), shutdown).await;
    }
    result
}

enum StreamCommand {
    Ack(u32),
    Resync,
    Reply(Message),
}

struct MediaPacket {
    generation: u64,
    sequence: u32,
    bytes: Vec<u8>,
    blocked_since: Option<Instant>,
}

struct ControlPacket {
    message: Message,
}

struct WriteComplete {
    generation: u64,
    sequence: u32,
    written: Option<Instant>,
    duration: Duration,
}

struct Encoded {
    encoder: StreamEncoder,
    generation: u64,
    sequence: u32,
    packet: Result<Vec<u8>>,
    work: Duration,
    worker_wait: Duration,
    cpu_work: Option<Duration>,
    source_age: Option<Duration>,
}

fn replace_latest(
    latest: &mut Option<Captured>,
    capture: Option<Captured>,
    window: &mut scheduler::Window,
) {
    let replaced = match (latest.as_ref(), capture.as_ref()) {
        (Some(old), Some(next)) => {
            old.forwarded_input_id != next.forwarded_input_id
                || !Arc::ptr_eq(&old.frame, &next.frame)
        }
        (Some(_), None) => true,
        _ => false,
    };
    if replaced {
        window.capture_replaced();
    }
    *latest = capture;
}

/// How long the browser gets to take the event that ends its session.
const LAST_NOTICE_WRITE: Duration = Duration::from_secs(2);

/// The clock of a session's deadlines. It is tokio's, which a test can pause,
/// so that every deadline of a session moves with the same clock.
fn session_now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// Sets aside a capture H.264 cannot encode and tells the browser, once for
/// each such display mode. Nothing is cropped or padded to make it fit: the
/// picture waits, and input keeps flowing so the mode can be changed.
fn hold_unencodable(
    latest: &mut Option<Captured>,
    unencodable: &mut Option<(u16, u16)>,
    window: &mut scheduler::Window,
    notices: &notices::Notices,
) {
    let Some(capture) = latest.as_ref() else {
        return;
    };
    let (width, height) = (capture.frame.width, capture.frame.height);
    if gliss_video::encodable(width, height) {
        if unencodable.take().is_some() {
            window.picture_resumed();
        }
        return;
    }
    *latest = None;
    if unencodable.replace((width, height)) != Some((width, height)) {
        notices.display_error(format!(
            "The VM display is {width}x{height}. Video needs an even width and height from 16x16 through 3840x2160, so the picture is paused until the VM changes its resolution."
        ));
    }
}

/// Runs one viewer over the two halves of its WebSocket.
async fn stream_session<Tx, Rx, E>(
    mut sink: Tx,
    mut receiver: Rx,
    app: Arc<App>,
    source: &mut Source,
) -> Result<()>
where
    Tx: Sink<Message, Error = E> + Unpin + Send + 'static,
    Rx: Stream<Item = Result<Message, E>> + Unpin + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let hello = Message::Text(with_stream_metadata(json!({"type":"hello","protocol":"GLV1","codec":app.args.codec,"source":if app.args.demo {"demo"} else {"rfb"},"fps_limit":app.args.fps,"input_metadata":"latest forwarded input; not proof of causal display"}),&app).to_string().into());
    let (command_tx, mut commands) = mpsc::channel(16);
    let (media_tx, mut media) =
        mpsc::channel::<MediaPacket>(usize::from(app.args.in_flight_frames));
    let (control_tx, mut controls) = mpsc::channel::<ControlPacket>(16);
    let (write_tx, mut writes) = mpsc::channel(16);
    let generation = Arc::new(AtomicU64::new(0));
    let mut io_tasks = JoinSet::<Result<()>>::new();
    let input = source.input.clone();
    let audio_request = source.audio.request.clone();
    let writer_audio_request = audio_request.subscribe();
    let mut audio_status = source.audio.status.clone();
    let mut audio_packets = source.audio.packets.subscribe();
    let writer_audio_counters = source.audio_counters.clone();
    let notices = source.notices.clone();
    // This task forwards input independently of semaphore waits, encoding and writes.
    // Dropping the session's JoinSet aborts it and releases its input sender clone.
    io_tasks.spawn(async move {
        while let Some(message) = receiver.next().await {
            let command = match message? {
                Message::Text(text) => {
                    let value: serde_json::Value = serde_json::from_str(&text)?;
                    match value.get("type").and_then(|value| value.as_str()) {
                        Some("ack") => {
                            let sequence = value.get("sequence").and_then(|value| value.as_u64()).and_then(|value| u32::try_from(value).ok()).context("invalid media acknowledgement")?;
                            Some(StreamCommand::Ack(sequence))
                        }
                        Some("resync") => Some(StreamCommand::Resync),
                        Some("audio") => { audio::request(&audio_request, value)?; None }
                        Some("ping") => Some(StreamCommand::Reply(Message::Text(json!({"type":"pong","id":value.get("id")}).to_string().into()))),
                        _ => {
                            let command: Input = serde_json::from_value(value)?;
                            if let Input::Clipboard { text, .. } = &command && text.len() > MAX_CLIPBOARD {
                                bail!("clipboard too large");
                            }
                            if matches!(command, Input::Resize { width, height, .. } if width < 16 || height < 16 || width > 3840 || height > 2160 || width % 2 != 0 || height % 2 != 0) {
                                Some(StreamCommand::Reply(Message::Text(json!({"type":"input_error","message":"Software H264 requires even desktop dimensions from 16x16 through 3840x2160."}).to_string().into())))
                            } else {
                                input.try_send(command).context("input queue full")?;
                                None
                            }
                        }
                    }
                }
                Message::Close(_) => return Ok(()),
                Message::Ping(data) => Some(StreamCommand::Reply(Message::Pong(data))),
                _ => None,
            };
            if let Some(command) = command { command_tx.send(command).await.context("stream command channel closed")?; }
        }
        Ok(())
    });
    let writer_generation = generation.clone();
    io_tasks.spawn(async move {
        // Keep hello first on the wire while the independent receiver forwards input.
        tokio::time::timeout(Duration::from_secs(10), sink.send(hello)).await.context("hello write timed out")??;
        let initial_audio_status = audio_status.borrow_and_update().message();
        tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Text(initial_audio_status.to_string().into()))).await.context("audio status write timed out")??;
        let mut audio_status_open = true;
        loop {
            let packet = tokio::select! {
                changed = audio_status.changed(), if audio_status_open => {
                    if changed.is_err() { audio_status_open = false; continue; }
                    let status = audio_status.borrow_and_update().message();
                    tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Text(status.to_string().into()))).await.context("audio status write timed out")??;
                    continue;
                }
                packet = audio_packets.recv() => {
                    match packet {
                        Ok(packet) => {
                            let request = *writer_audio_request.borrow();
                            if audio::admit_for_write(&packet, request, &writer_audio_counters) {
                                tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Binary(packet.bytes.to_vec().into()))).await.context("audio write timed out")??;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => writer_audio_counters.record_writer_lag(skipped),
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                    continue;
                }
                Some(control) = controls.recv() => {
                    tokio::time::timeout(Duration::from_secs(10), sink.send(control.message)).await.context("control write timed out")??;
                    continue;
                }
                // Taken only when it can be written next, so a browser that reads
                // slowly holds back one event here and the newest of each kind behind it.
                notice = notices.next() => {
                    let message = Message::Text(notice.text.into());
                    if notice.last {
                        // The display is gone: the session ends whether or not the browser reads this.
                        let _ = tokio::time::timeout(LAST_NOTICE_WRITE, sink.send(message)).await;
                        return Ok(());
                    }
                    tokio::time::timeout(Duration::from_secs(10), sink.send(message)).await.context("event write timed out")??;
                    continue;
                }
                Some(packet) = media.recv() => packet,
                else => return Ok(()),
            };
            if packet.generation != writer_generation.load(Ordering::Acquire) {
                write_tx.send(WriteComplete { generation: packet.generation, sequence: packet.sequence, written: None, duration: Duration::ZERO }).await.context("write completion channel closed")?;
                continue;
            }
            let started = session_now();
            tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Binary(packet.bytes.into()))).await.context("media write timed out")??;
            let completed = session_now();
            write_tx.send(WriteComplete { generation: packet.generation, sequence: packet.sequence, written: Some(completed), duration: completed.saturating_duration_since(started) }).await.context("write completion channel closed")?;
        }
    });
    let mut encoder = Some(StreamEncoder {
        encoder: None,
        timing: app.video_timing,
        bitrate: app.args.h264_bitrate,
    });
    let mut encoder_jobs = JoinSet::<Result<Encoded>>::new();
    let mut window = scheduler::Window::new(
        usize::from(app.args.in_flight_frames),
        app.args.in_flight_bytes,
    )?;
    let mut sequence = 0u32;
    let mut writer_jobs = 0usize;
    let mut waiting = None::<MediaPacket>;
    let mut latest = None::<Captured>;
    let mut pacer = pacer::Pacer::new(Duration::from_secs_f64(1.0 / f64::from(app.args.fps)));
    let mut eligible = tokio::time::Instant::now();
    let mut telemetry = tokio::time::interval(Duration::from_secs(1));
    telemetry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut source_ended = None::<tokio::time::Instant>;
    // The display mode that is being held back, if the current one cannot be encoded.
    let mut unencodable = None::<(u16, u16)>;
    loop {
        let now = session_now();
        if window.resync_due(now).is_some_and(|due| due <= now) {
            window.begin_resync(sequence, now)?;
            generation.store(window.generation(), Ordering::Release);
            if waiting.take().is_some() {
                window.discarded_encode();
            }
        }
        let resync_due = window.resync_due(now).map(tokio::time::Instant::from_std);
        if window.resyncing() && writer_jobs == 0 && encoder_jobs.is_empty() {
            // Old queued writes have been skipped, an already-started write finished,
            // and any old encode returned. Reset before admitting new dependencies.
            encoder
                .as_mut()
                .context("encoder missing at generation barrier")?
                .reset();
            window.finish_resync()?;
            replace_latest(&mut latest, source.frames.borrow().clone(), &mut window);
            hold_unencodable(&mut latest, &mut unencodable, &mut window, &source.notices);
            eligible = pacer.eligible();
        }
        if waiting
            .as_ref()
            .is_some_and(|packet| window.can_reserve(packet.bytes.len()))
        {
            let packet = waiting.take().unwrap();
            if let Some(blocked) = packet.blocked_since {
                window.waited_for_bytes(blocked.elapsed());
            }
            // Reserve before the writer sees the packet: ACK can race completion.
            window.reserve(packet.sequence, packet.bytes.len(), session_now())?;
            app.bytes
                .fetch_add(packet.bytes.len() as u64, Ordering::Relaxed);
            app.frames.fetch_add(1, Ordering::Relaxed);
            media_tx
                .try_send(packet)
                .context("bounded media writer queue full or closed")?;
            writer_jobs += 1;
        } else if let Some(packet) = &mut waiting {
            packet.blocked_since.get_or_insert_with(Instant::now);
        }
        let deadline = window.oldest_deadline();
        let sleep_deadline = tokio::time::Instant::from_std(
            deadline.unwrap_or_else(|| session_now() + Duration::from_secs(86400)),
        );
        tokio::select! {
            Some(result) = io_tasks.join_next() => { result.context("stream I/O task failed")??; break; }
            Some(command) = commands.recv() => {
                match command {
                    StreamCommand::Ack(sequence) => { window.acknowledge(sequence, session_now())?; }
                    // Started at the top of the loop, once the interval between resyncs allows.
                    StreamCommand::Resync => window.request_resync(),
                    StreamCommand::Reply(message) => { control_tx.try_send(ControlPacket { message }).context("bounded control writer queue full or closed")?; }
                }
            }
            Some(completed) = writes.recv() => {
                writer_jobs = writer_jobs.checked_sub(1).context("unexpected writer completion")?;
                if let Some(written) = completed.written {
                    window.written(completed.generation, completed.sequence, written, completed.duration)?;
                } else {
                    window.skipped(completed.generation, completed.sequence)?;
                }
            }
            Some(result) = encoder_jobs.join_next(), if !encoder_jobs.is_empty() => {
                let completed = result.context("encoder worker failed")??;
                window.encoded(completed.work, completed.worker_wait);
                window.profiled_encode(completed.cpu_work, completed.source_age);
                encoder = Some(completed.encoder);
                if completed.generation != window.generation() {
                    window.discarded_encode();
                } else {
                    waiting = Some(MediaPacket { generation: completed.generation, sequence: completed.sequence, bytes: completed.packet?, blocked_since: None });
                }
            }
            changed = source.frames.changed(), if source_ended.is_none() => {
                if changed.is_err() {
                    // The source says why before it stops, and the writer ends the
                    // session once the browser has that. This bound is for a writer
                    // that is still held up by an earlier message.
                    source_ended = Some(tokio::time::Instant::now() + LAST_NOTICE_WRITE);
                    continue;
                }
                replace_latest(&mut latest, source.frames.borrow_and_update().clone(), &mut window);
                hold_unencodable(&mut latest, &mut unencodable, &mut window, &source.notices);
                eligible = pacer.eligible();
            }
            _ = tokio::time::sleep_until(source_ended.unwrap_or(eligible)), if source_ended.is_some() => break,
            _ = tokio::time::sleep_until(resync_due.unwrap_or(eligible)), if resync_due.is_some() => {}
            _ = tokio::time::sleep_until(eligible), if window.can_encode() && waiting.is_none() && encoder_jobs.is_empty() && latest.is_some() => {
                pacer.started();
                let captured = latest.take().unwrap();
                let mut next_encoder = encoder.take().context("encoder is already busy")?;
                sequence = sequence.wrapping_add(1);
                let packet_sequence = sequence;
                let packet_generation = window.generation();
                let permits = app.encoders.clone();
                let queued = Instant::now();
                let profile_encode = app.args.stream_stats;
                encoder_jobs.spawn(async move {
                    let permit = permits.acquire_owned().await?;
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        let started = Instant::now();
                        let source_age = captured.published_at.map(|at| started.saturating_duration_since(at));
                        let cpu_started = profile_encode.then(profiling::thread_cpu_time).flatten();
                        let packet = next_encoder.encode(&captured.frame, packet_sequence, captured.forwarded_input_id);
                        let work = started.elapsed();
                        let cpu_work = cpu_started.and_then(|at| profiling::thread_cpu_time()?.checked_sub(at));
                        Encoded { encoder: next_encoder, generation: packet_generation, sequence: packet_sequence, packet, work, worker_wait: started.saturating_duration_since(queued), cpu_work, source_age }
                    }).await.context("blocking encoder worker failed")
                });
            }
            _ = telemetry.tick(), if app.args.stream_stats => {
                let mut stats = window.snapshot();
                stats["process_cpu"] = profiling::process_cpu(app.started.elapsed());
                stats["waiting_encoded_bytes"] = json!(waiting.as_ref().map_or(0, |packet| packet.bytes.len()));
                stats["encoder_busy"] = json!(!encoder_jobs.is_empty());
                stats["writer_jobs"] = json!(writer_jobs);
                stats["audio"] = json!({"packet_ms":app.args.audio_packet_ms.millis(),"counters":source.audio_counters.snapshot()});
                // Diagnostics never block input/media and have no unbounded history.
                let _ = control_tx.try_send(ControlPacket { message: Message::Text(stats.to_string().into()) });
            }
            _ = tokio::time::sleep_until(sleep_deadline), if deadline.is_some() => { bail!("browser did not acknowledge oldest frame within 10s"); }
        }
    }
    Ok(())
}

/// The native encoder is built from the first frame, whose size it needs.
struct StreamEncoder {
    encoder: Option<Box<VideoEncoder>>,
    timing: VideoTiming,
    bitrate: u32,
}

impl StreamEncoder {
    fn reset(&mut self) {
        if let Some(encoder) = &mut self.encoder {
            encoder.reset();
        }
    }

    fn encode(&mut self, frame: &Frame, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        if self.encoder.is_none() {
            self.encoder = Some(Box::new(VideoEncoder::with_timing(
                frame.width,
                frame.height,
                self.bitrate,
                self.timing,
            )?));
        }
        self.encoder
            .as_mut()
            .unwrap()
            .encode(frame, sequence, input_id)
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod source_tests;

#[cfg(test)]
mod viewer_tests;

#[cfg(test)]
mod http_tests;
