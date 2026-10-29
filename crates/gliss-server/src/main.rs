mod audio;
mod audio_adpcm;
mod benchmark_recorder;
mod datagram;
mod demo;
mod profiling;
#[cfg(unix)]
mod rfb_fd;
mod rfb_request;
mod rfb_teardown;
mod scheduler;
mod shared_capture;
mod stage_trace;
mod tcp_listener;
mod tile_quantum;
mod tile_send;
mod xor_fec;

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use gliss_codec::{Encoder, Mode};
use gliss_core::{Frame, Input, MAX_CLIPBOARD};
use gliss_rfb::Event;
use gliss_video::{VideoEncoder, VideoProfile, VideoTiming};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
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
    /// Trusted single-source loopback diagnostic. Binary trace exports only after actors stop.
    #[arg(long)]
    stage_trace: Option<std::path::PathBuf>,
    /// Fixed accepted HTTP/WebSocket TCP_NODELAY policy; verified before HTTP IO.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    tcp_nodelay: bool,
    /// Trusted audio packet duration (20 or 40 ms); does not change the freshness budget.
    #[arg(long, default_value_t = audio::PacketDuration::DEFAULT)]
    audio_packet_ms: audio::PacketDuration,
    /// Authenticated QUIC DATAGRAM media. Default scope remains loopback.
    #[arg(long)]
    datagram_bind: Option<SocketAddr>,
    /// Trusted externally routed UDP port; omission advertises the bound port.
    #[arg(long, requires = "datagram_bind", value_parser = clap::value_parser!(u16).range(1..))]
    datagram_advertise_port: Option<u16>,
    /// Trusted public media hostname or IP; requires a private managed-FD worker and HTTPS origin.
    #[arg(long, requires = "datagram_bind", value_parser = parse_datagram_public_host)]
    datagram_public_host: Option<String>,
    /// Restrict the datagram source to display, keyboard and pointer access.
    #[arg(long, requires = "datagram_bind")]
    datagram_display_input_only: bool,
    /// QUIC unsent-datagram memory budget, 8192..1048576 bytes; omission preserves 1 MiB.
    #[arg(long, requires = "datagram_bind")]
    datagram_send_buffer_bytes: Option<usize>,
    /// Experimental QUIC media controller. Omission preserves Cubic.
    #[arg(long, requires = "datagram_bind", value_enum)]
    datagram_congestion_control: Option<datagram::CongestionControl>,
    /// Trusted experimental UDP codec; the browser cannot select its own policy.
    #[arg(long, requires = "datagram_bind", value_enum)]
    datagram_codec: Option<datagram::DatagramCodec>,
    /// Trusted GLT2 sending policy; omission retains frame-tick sending.
    #[arg(long, requires = "datagram_bind", value_enum)]
    tile_send_policy: Option<tile_send::Policy>,
    /// Opt-in whole-tile GLT2 admission quantum; omission retains unlimited flushes.
    #[arg(long, requires = "datagram_bind", value_enum)]
    tile_send_quantum: Option<tile_quantum::Selection>,
    /// Trusted benchmark only: fresh private directory for canonical ROI references.
    #[arg(long)]
    benchmark_capture_dir: Option<std::path::PathBuf>,
    /// Fixed upstream target. The browser cannot choose a VNC address.
    #[arg(long, conflicts_with_all = ["demo", "rfb_fd"], required_unless_present_any = ["demo", "rfb_fd", "export_web"])]
    vnc: Option<String>,
    /// Opt-in single-private-RFB receiver lifetime experiment; default immediate.
    #[arg(long, value_enum)]
    rfb_close_policy: Option<rfb_teardown::ClosePolicy>,
    /// RFB request scheduling; ready rearms on completion within the FPS cap.
    #[arg(long, value_enum, default_value = "tick")]
    rfb_request_policy: rfb_request::Policy,
    /// Private one-use connected Unix graphics descriptor inherited from a trusted launcher.
    #[arg(long, conflicts_with_all = ["vnc", "demo", "dbus_address", "vm_uuid", "shared_dbus"], requires = "rfb_deadline_unix_ms", value_parser = clap::value_parser!(i32).range(3..))]
    rfb_fd: Option<i32>,
    /// Absolute source lease deadline; independent of login time, at most one hour ahead.
    #[arg(long, requires = "rfb_fd", conflicts_with_all = ["vnc", "demo", "dbus_address", "vm_uuid", "shared_dbus"])]
    rfb_deadline_unix_ms: Option<u64>,
    /// Experimental CPU capture from a private QEMU D-Bus; VNC still carries input.
    #[arg(long, requires = "vm_uuid", conflicts_with = "demo")]
    dbus_address: Option<String>,
    #[arg(long, requires = "dbus_address")]
    vm_uuid: Option<String>,
    #[arg(long, default_value_t = 0)]
    dbus_console: u32,
    /// Share one canonical D-Bus producer; inputs and encoders remain per viewer.
    #[arg(long, requires = "dbus_address", conflicts_with = "demo")]
    shared_dbus: bool,
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
    #[arg(long, default_value = "deflate")]
    codec: String,
    #[arg(long, default_value_t = 4_000_000)]
    h264_bitrate: u32,
    /// H.264 nominal rate-control FPS; defaults to capture FPS without changing capture cadence.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=60))]
    video_fps: Option<u32>,
    /// H.264 synthetic timestamp FPS; defaults to capture FPS.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=60))]
    video_timestamp_fps: Option<u32>,
    /// H.264 periodic IDR interval in encoded frames; defaults to twice capture FPS.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=3600))]
    video_intra_period: Option<u32>,
    /// Allow native H.264 to drop current pictures to meet its bitrate target (WebSocket only).
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    video_frame_skip: bool,
    /// Requested minimum H.264 QP; positive bounds avoid native default substitution.
    #[arg(long, requires = "video_qp_max", value_parser = clap::value_parser!(u8).range(1..=51))]
    video_qp_min: Option<u8>,
    /// Requested maximum H.264 QP; must accompany video-qp-min.
    #[arg(long, requires = "video_qp_min", value_parser = clap::value_parser!(u8).range(1..=51))]
    video_qp_max: Option<u8>,
    /// Explicit exact-base experiment for the Hybrid codecs.
    #[arg(long, default_value = "global", value_parser = ["global", "global-cached"])]
    hybrid_base: String,
    /// Explicit native usage/scene experiment for the Hybrid codecs.
    #[arg(long, default_value = "screen", value_parser = ["screen", "camera-scene-on", "camera-scene-off"])]
    hybrid_video_profile: String,
    /// GLH2/GLH4 mask edge; requires hybrid-grid or hybrid-continuity, defaults to 32.
    #[arg(long, value_parser = parse_mask_grid)]
    hybrid_mask_grid: Option<u16>,
    #[arg(long, default_value_t = 64)]
    tile_size: u16,
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
    mode: Option<Mode>,
    hybrid_config: gliss_hybrid::HybridConfig,
    hybrid_mask_grid: Option<u16>,
    video_timing: Option<VideoTiming>,
    datagram: Option<Arc<datagram::DatagramRuntime>>,
    benchmark: Option<Arc<benchmark_recorder::Recorder>>,
    stage_trace: stage_trace::Handle,
    tcp_listener: Arc<tcp_listener::SocketPolicy>,
    #[cfg(unix)]
    rfb_fd: Option<rfb_fd::RfbFd>,
    source_deadline: Option<tokio::time::Instant>,
    worker_shutdown: watch::Sender<bool>,
    origins: Vec<String>,
    cookie_key: [u8; 32],
    clients: Arc<Semaphore>,
    encoders: Arc<Semaphore>,
    login_attempts: Mutex<(Instant, u32)>,
    bytes: AtomicU64,
    frames: AtomicU64,
    started: Instant,
    shared_capture: Option<shared_capture::SharedCapture>,
}

/// Browser modules served at `/<path>`; `--export-web` writes the same files.
const WEB_ASSETS: &[(&str, &str, &str)] = &[
    (
        "video-presentation.js",
        "text/javascript",
        include_str!("../../../web/video-presentation.js"),
    ),
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
        "decoder.js",
        "text/javascript",
        include_str!("../../../web/decoder.js"),
    ),
    (
        "video-decoder.js",
        "text/javascript",
        include_str!("../../../web/video-decoder.js"),
    ),
    (
        "datagram-transport.js",
        "text/javascript",
        include_str!("../../../web/datagram-transport.js"),
    ),
    (
        "predictive-state.js",
        "text/javascript",
        include_str!("../../../web/predictive-state.js"),
    ),
    (
        "predictive-datagram.js",
        "text/javascript",
        include_str!("../../../web/predictive-datagram.js"),
    ),
    (
        "xor-fec.js",
        "text/javascript",
        include_str!("../../../web/xor-fec.js"),
    ),
    (
        "fec-predictive-state.js",
        "text/javascript",
        include_str!("../../../web/fec-predictive-state.js"),
    ),
    (
        "tile-connect.js",
        "text/javascript",
        include_str!("../../../web/tile-connect.js"),
    ),
    (
        "tile-transport.js",
        "text/javascript",
        include_str!("../../../web/tile-transport.js"),
    ),
    (
        "tile-painter.js",
        "text/javascript",
        include_str!("../../../web/tile-painter.js"),
    ),
    (
        "tile-worker.js",
        "text/javascript",
        include_str!("../../../web/tile-worker.js"),
    ),
    (
        "tile-decoder.js",
        "text/javascript",
        include_str!("../../../web/tile-decoder.js"),
    ),
    (
        "tile-inflate.js",
        "text/javascript",
        include_str!("../../../web/tile-inflate.js"),
    ),
    (
        "vendor/fflate-bounded-core.js",
        "text/javascript",
        include_str!("../../../web/vendor/fflate-bounded-core.js"),
    ),
    (
        "vendor/fflate-LICENSE",
        "text/plain",
        include_str!("../../../web/vendor/fflate-LICENSE"),
    ),
    (
        "vendor/fflate-provenance.json",
        "application/json",
        include_str!("../../../web/vendor/fflate-provenance.json"),
    ),
    (
        "hybrid-decoder.js",
        "text/javascript",
        include_str!("../../../web/hybrid-decoder.js"),
    ),
    (
        "hybrid-grid-decoder.js",
        "text/javascript",
        include_str!("../../../web/hybrid-grid-decoder.js"),
    ),
    (
        "hybrid-continuity-decoder.js",
        "text/javascript",
        include_str!("../../../web/hybrid-continuity-decoder.js"),
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
    // Reject unsupported sources before adopting any inherited descriptor.
    rfb_teardown::validate(&args)?;
    tile_send::validate(&args)?;
    let source_deadline = configured_source_deadline(&args)?;
    // SAFETY: a trusted launcher transfers this one exclusive owner. Adopt before
    // runtime/logging/threads can allocate another descriptor with that number.
    #[cfg(unix)]
    let rfb_fd = args
        .rfb_fd
        .map(|raw| unsafe { rfb_fd::RfbFd::take_inherited(raw) })
        .transpose()?;
    #[cfg(not(unix))]
    ensure!(args.rfb_fd.is_none(), "rfb-fd requires a Unix host");
    let stage_trace = stage_trace::prepare(&args)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    #[cfg(unix)]
    let result = runtime.block_on(run(args, source_deadline, rfb_fd, stage_trace.clone()));
    #[cfg(not(unix))]
    let result = runtime.block_on(run(args, source_deadline, stage_trace.clone()));
    let trace_result = if let Some(trace) = &stage_trace {
        runtime.block_on(trace.finish(result.is_ok()))
    } else {
        Ok(())
    };
    if source_deadline.is_some() || stage_trace.is_some() {
        // Main returning ends the private worker. A blocking task cannot keep
        // its source lease alive indefinitely during runtime shutdown.
        runtime.shutdown_timeout(Duration::from_secs(1));
    }
    result.and(trace_result)
}

fn configured_source_deadline(args: &Args) -> Result<Option<tokio::time::Instant>> {
    if args.rfb_fd.is_none() {
        ensure!(
            args.rfb_deadline_unix_ms.is_none(),
            "source deadline requires rfb-fd"
        );
        return Ok(None);
    }
    ensure!(
        args.bind.ip().is_loopback() && args.max_clients == 1 && args.dbus_console == 0,
        "rfb-fd requires loopback, max-clients1 and the default D-Bus console"
    );
    let anchor = Instant::now();
    let now = SystemTime::now();
    let expiry = UNIX_EPOCH
        .checked_add(Duration::from_millis(
            args.rfb_deadline_unix_ms
                .context("Missing graphics lease deadline")?,
        ))
        .context("Invalid graphics lease timestamp")?;
    let remaining = expiry
        .duration_since(now)
        .context("graphics lease deadline has expired")?;
    ensure!(
        !remaining.is_zero() && remaining <= Duration::from_secs(3600),
        "graphics lease deadline must be future and within one hour"
    );
    Ok(Some(tokio::time::Instant::from_std(anchor + remaining)))
}

async fn run(
    args: Args,
    source_deadline: Option<tokio::time::Instant>,
    #[cfg(unix)] rfb_fd: Option<rfb_fd::RfbFd>,
    stage_trace: stage_trace::Handle,
) -> Result<()> {
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
    let mode = if matches!(
        args.codec.as_str(),
        "h264" | "hybrid" | "hybrid-grid" | "hybrid-continuity"
    ) {
        None
    } else {
        Some(args.codec.parse::<Mode>()?)
    };
    let hybrid_config = configured_hybrid(&args)?;
    let hybrid_mask_grid = configured_mask_grid(&args)?;
    let video_timing = configured_video_timing(&args)?;
    let datagram_config = configured_datagram(&args, video_timing)?;
    if args.benchmark_capture_dir.is_some() {
        ensure!(
            args.bind.ip().is_loopback()
                && args.max_clients == 1
                && args.vnc.is_some()
                && args.rfb_fd.is_none()
                && !args.demo
                && args.dbus_address.is_none()
                && !args.shared_dbus
                && args.codec == "h264",
            "benchmark capture requires loopback, one client, fixed RFB and H264 base configuration"
        );
    }
    if let Some(mode) = mode {
        Encoder::new(mode, args.tile_size)?;
    } else if args.fps > 60 || !(1_000..=50_000_000).contains(&args.h264_bitrate) {
        bail!("software H264 requires fps <= 60 and bitrate 1000..50000000");
    }
    if matches!(
        args.codec.as_str(),
        "hybrid" | "hybrid-grid" | "hybrid-continuity"
    ) && args.tile_size != 64
    {
        bail!("the experimental hybrid exact base requires tile-size 64");
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
    let max_clients = args.max_clients;
    let shared_capture = if args.shared_dbus {
        Some(shared_capture::SharedCapture::new(
            shared_capture::FixedTarget {
                address: args.dbus_address.as_ref().unwrap().clone(),
                vm_uuid: args.vm_uuid.as_ref().unwrap().clone(),
                console: args.dbus_console,
                diagnostics: args.stream_stats,
                max_viewers: max_clients,
            },
        )?)
    } else {
        None
    };
    let (worker_shutdown, _) = watch::channel(false);
    let datagram = datagram_config
        .map(|(address, advertise_port)| {
            datagram::DatagramRuntime::bind_configured(
                address,
                advertise_port,
                args.datagram_congestion_control.unwrap_or_default(),
                args.datagram_codec.unwrap_or_default(),
                args.datagram_public_host.clone(),
                args.datagram_send_buffer_bytes,
            )
        })
        .transpose()?;
    let datagram_origin = datagram.as_ref().map(|runtime| runtime.advertised_origin());
    let tcp_listener = tcp_listener::SocketPolicy::new(args.tcp_nodelay);
    let benchmark = args
        .benchmark_capture_dir
        .clone()
        .map(benchmark_recorder::Recorder::create)
        .transpose()?;
    let app = Arc::new(App {
        tcp_listener: tcp_listener.clone(),
        args,
        mode,
        hybrid_config,
        hybrid_mask_grid,
        video_timing,
        datagram,
        benchmark,
        stage_trace,
        #[cfg(unix)]
        rfb_fd,
        source_deadline,
        worker_shutdown,
        origins,
        cookie_key: rand::random(),
        clients: Arc::new(Semaphore::new(max_clients)),
        encoders: Arc::new(Semaphore::new(2)),
        login_attempts: Mutex::new((Instant::now(), 0)),
        bytes: AtomicU64::new(0),
        frames: AtomicU64::new(0),
        started: Instant::now(),
        shared_capture,
    });
    let mut datagram_tasks = JoinSet::new();
    if let Some(runtime) = &app.datagram {
        let runtime = runtime.clone();
        let app = app.clone();
        datagram_tasks.spawn(async move { runtime.serve(app).await });
    }
    let mut router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../../../web/index.html")) }),
        )
        .route(
            "/datagram",
            get(|| async { Html(include_str!("../../../web/index.html")) }),
        );
    for &(path, content_type, body) in WEB_ASSETS {
        router = router.route(
            &format!("/{path}"),
            get(move || async move { ([(header::CONTENT_TYPE, content_type)], body) }),
        );
    }
    let router = router
        .route("/health", get(|| async { Json(json!({"status":"ok","version":env!("CARGO_PKG_VERSION")})) }))
        .route("/api/session", post(login).delete(logout))
        .route("/api/datagram-ticket", post(datagram_ticket))
        .route("/api/datagram-revoke", post(datagram_revoke))
        .route("/api/stats", get(stats))
        .route("/api/stage-clock", post(stage_clock).layer(DefaultBodyLimit::max(256)))
        .route("/ws", get(upgrade))
        .layer(DefaultBodyLimit::max(MAX_CLIPBOARD + 1024))
        .layer(axum::middleware::map_response(move |mut response: Response| {
            let datagram_origin = datagram_origin.clone();
            async move {
            let h = response.headers_mut();
            h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            let connect = datagram_origin.as_ref().map_or_else(|| "'self'".to_owned(), |origin| format!("'self' {origin}"));
            h.insert(header::CONTENT_SECURITY_POLICY, format!("default-src 'self'; script-src 'self'; connect-src {connect}; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'").parse().unwrap());
            h.insert("x-content-type-options", "nosniff".parse().unwrap());
            h.insert("referrer-policy", "no-referrer".parse().unwrap());
            response
        }}))
        .with_state(app.clone());
    let listener = tcp_listener::VerifiedListener::new(
        tokio::net::TcpListener::bind(bind).await?,
        tcp_listener,
    );
    println!("Gliss listening on http://{bind} (CPU-only)");
    let (shutdown_notice, shutdown_started) = oneshot::channel();
    let shutdown_signal = app.worker_shutdown.clone();
    let shutdown_trace = app.stage_trace.clone();
    let serving = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                signal_result = tokio::signal::ctrl_c() => { if let Some(trace) = &shutdown_trace {
                    if signal_result.is_ok() { trace.note_shutdown(1); } else { trace.note_shutdown(4); trace.failure(8); }
                } },
                signal_received = async {
                    #[cfg(unix)]
                    if shutdown_trace.is_some() {
                        return match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                            Ok(mut signal) => signal.recv().await.is_some(), Err(_) => false,
                        };
                    }
                    std::future::pending::<bool>().await
                } => { if let Some(trace) = &shutdown_trace {
                    if signal_received { trace.note_shutdown(3); } else { trace.note_shutdown(4); trace.failure(8); }
                } },
                _ = async {
                    if let Some(deadline) = source_deadline { tokio::time::sleep_until(deadline).await; }
                    else { std::future::pending::<()>().await; }
                } => { if let Some(trace) = &shutdown_trace { trace.note_shutdown(2); } },
            }
            shutdown_signal.send_replace(true);
            let _ = shutdown_notice.send(tokio::time::Instant::now());
        });
    let result = if source_deadline.is_some() {
        // Incomplete HTTP bodies must not extend an unused graphics lease.
        // Upgraded WebSocket callbacks are detached from Axum's drain barrier;
        // their existing client permits cover setup, stream and held releases.
        let mut serving = Box::pin(serving.into_future());
        let (result, hard_stop) = tokio::select! {
            result = &mut serving => (Ok(result), tokio::time::Instant::now() + Duration::from_secs(2)),
            started = shutdown_started => {
                let hard_stop = started.unwrap_or_else(|_| tokio::time::Instant::now()) + Duration::from_secs(2);
                (tokio::time::timeout_at(hard_stop, &mut serving).await, hard_stop)
            }
        };
        if tokio::time::timeout_at(hard_stop, app.clients.clone().acquire_owned())
            .await
            .is_err()
            && let Some(trace) = &app.stage_trace
        {
            trace.failure(16);
        }
        result.unwrap_or_else(|_| {
            if let Some(trace) = &app.stage_trace {
                trace.failure(16);
            }
            tracing::warn!("graphics lease shutdown grace elapsed");
            Ok(())
        })
    } else {
        serving.await
    };
    app.worker_shutdown.send_replace(true);
    while let Some(result) = datagram_tasks.join_next().await {
        result.context("datagram worker failed")??;
    }
    if let Some(capture) = &app.shared_capture {
        capture
            .shutdown()
            .await
            .context("shared capture shutdown")?;
    }
    if let Some(recorder) = &app.benchmark {
        recorder
            .finish()
            .context("benchmark evidence writer shutdown")?;
    }
    result?;
    Ok(())
}

fn parse_mask_grid(value: &str) -> Result<u16, String> {
    match value {
        "16" => Ok(16),
        "32" => Ok(32),
        _ => Err("hybrid-mask-grid must be 16 or 32".to_owned()),
    }
}

fn configured_mask_grid(args: &Args) -> Result<Option<u16>> {
    if matches!(args.codec.as_str(), "hybrid-grid" | "hybrid-continuity") {
        Ok(Some(args.hybrid_mask_grid.unwrap_or(32)))
    } else {
        ensure!(
            args.hybrid_mask_grid.is_none(),
            "hybrid-mask-grid requires --codec hybrid-grid or hybrid-continuity"
        );
        Ok(None)
    }
}

fn explicit_video_timing(args: &Args) -> bool {
    args.video_fps.is_some()
        || args.video_timestamp_fps.is_some()
        || args.video_intra_period.is_some()
}

fn configured_datagram(
    args: &Args,
    timing: Option<VideoTiming>,
) -> Result<Option<(SocketAddr, Option<u16>)>> {
    let Some(address) = args.datagram_bind else {
        ensure!(
            args.datagram_advertise_port.is_none()
                && args.datagram_congestion_control.is_none()
                && args.datagram_codec.is_none()
                && args.datagram_public_host.is_none()
                && !args.datagram_display_input_only
                && args.datagram_send_buffer_bytes.is_none(),
            "datagram options require datagram-bind"
        );
        return Ok(None);
    };
    if let Some(host) = &args.datagram_public_host {
        parse_datagram_public_host(host).map_err(anyhow::Error::msg)?;
        ensure!(
            args.bind.ip().is_loopback() && args.rfb_fd.is_some() && args.max_clients == 1,
            "public datagrams require private HTTP, a managed RFB FD and max-clients1"
        );
        let origin = args
            .origin
            .as_deref()
            .context("public datagrams require an exact HTTPS origin")?;
        let authority = origin
            .strip_prefix("https://")
            .context("public datagrams require an exact HTTPS origin")?;
        let uri: axum::http::Uri = origin.parse().context("invalid public browser origin")?;
        ensure!(
            !authority.is_empty()
                && !authority
                    .bytes()
                    .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b'@' | b'\\'))
                && uri.authority().is_some(),
            "public datagrams require an exact HTTPS origin without a path or userinfo"
        );
    } else {
        ensure!(
            address.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "experimental datagram bind must use 127.0.0.1; public UDP requires datagram-public-host"
        );
    }
    if let Some(bytes) = args.datagram_send_buffer_bytes {
        datagram::validate_send_buffer(bytes)?;
    }
    if args.datagram_codec.unwrap_or_default() == datagram::DatagramCodec::H264 {
        ensure!(
            args.codec == "h264" && timing.is_some_and(|timing| timing.intra_period() == 1),
            "datagram H264 media requires --codec h264 --video-intra-period 1"
        );
    } else if args.datagram_codec.is_some_and(|codec| codec.predictive()) {
        ensure!(
            args.codec == "h264"
                && timing.is_some_and(|timing| timing.intra_period() == 4)
                && args.stream_stats
                && args.max_clients == 1,
            "predictive GOP4 requires --codec h264 --video-intra-period 4 --stream-stats --max-clients 1"
        );
    } else {
        ensure!(
            args.datagram_congestion_control == Some(datagram::CongestionControl::Bbr),
            "experimental GLT2 requires explicit --datagram-congestion-control bbr"
        );
    }
    Ok(Some((address, args.datagram_advertise_port)))
}

fn parse_datagram_public_host(value: &str) -> std::result::Result<String, String> {
    let bare = value
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(value);
    if let Ok(address) = bare.parse::<std::net::IpAddr>() {
        if address.is_unspecified() || address.is_multicast() {
            return Err("media IP must be a unicast destination".into());
        }
        return Ok(address.to_string());
    }
    if value.is_empty()
        || value.len() > 253
        || !value.is_ascii()
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(
            "media host must be a DNS hostname or literal IP without a scheme, port or path".into(),
        );
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(test)]
mod public_datagram_tests {
    use super::*;

    fn selected() -> Args {
        Args::try_parse_from([
            "gliss-server",
            "--token",
            "01234567890123456789012345678901",
            "--rfb-fd",
            "9",
            "--rfb-deadline-unix-ms",
            "1",
            "--max-clients",
            "1",
            "--codec",
            "batch",
            "--datagram-bind",
            "0.0.0.0:4443",
            "--datagram-public-host",
            "media.example",
            "--origin",
            "https://desktop.example",
            "--datagram-display-input-only",
            "--datagram-codec",
            "glt2-lossless",
            "--datagram-congestion-control",
            "bbr",
            "--datagram-send-buffer-bytes",
            "65536",
        ])
        .unwrap()
    }

    #[test]
    fn public_udp_requires_private_managed_source_and_exact_https_browser_origin() {
        let selected = selected();
        assert_eq!(
            configured_datagram(&selected, None).unwrap(),
            Some(("0.0.0.0:4443".parse().unwrap(), None))
        );
        assert_eq!(
            selected.datagram_public_host.as_deref(),
            Some("media.example")
        );
        for origin in [
            "http://desktop.example",
            "https://desktop.example/",
            "https://desktop.example/api",
            "https://user@desktop.example",
            "https://desktop.example?token=x",
            "https://desktop.example#fragment",
            "https://",
        ] {
            let mut invalid = self::selected();
            invalid.origin = Some(origin.to_owned());
            assert!(configured_datagram(&invalid, None).is_err(), "{origin}");
        }
        let mut invalid = self::selected();
        invalid.origin = None;
        assert!(configured_datagram(&invalid, None).is_err());
        let mut invalid = self::selected();
        invalid.rfb_fd = None;
        assert!(configured_datagram(&invalid, None).is_err());
        let mut invalid = self::selected();
        invalid.bind = "0.0.0.0:8080".parse().unwrap();
        assert!(configured_datagram(&invalid, None).is_err());
        let mut invalid = self::selected();
        invalid.max_clients = 2;
        assert!(configured_datagram(&invalid, None).is_err());
        let mut invalid = self::selected();
        invalid.datagram_public_host = None;
        assert!(configured_datagram(&invalid, None).is_err());
        let mut invalid = self::selected();
        invalid.datagram_send_buffer_bytes = Some(8191);
        assert!(configured_datagram(&invalid, None).is_err());
    }

    #[test]
    fn public_host_is_only_a_normalized_destination_not_an_arbitrary_url() {
        for (value, expected) in [
            ("Media.Example", "media.example"),
            ("203.0.113.7", "203.0.113.7"),
            ("2001:db8::7", "2001:db8::7"),
            ("[2001:db8::7]", "2001:db8::7"),
        ] {
            assert_eq!(parse_datagram_public_host(value).unwrap(), expected);
        }
        for value in [
            "",
            "https://media.example",
            "media.example:4443",
            "media.example/path",
            "user@media.example",
            "media.example?ticket=x",
            "media.example#x",
            "media.example; script-src *",
            "-media.example",
            "media..example",
            "0.0.0.0",
            "::",
            "ff02::1",
            "224.0.0.1",
            "media.example\n",
        ] {
            assert!(parse_datagram_public_host(value).is_err(), "{value}");
        }
    }

    #[test]
    fn scoped_source_does_not_publish_guest_clipboard_contents() {
        assert_eq!(clipboard_message("guest secret".to_owned(), true), None);
        assert_eq!(
            clipboard_message("guest text".to_owned(), false),
            Some(json!({"type":"clipboard", "text":"guest text"}))
        );
    }
}

#[cfg(test)]
mod datagram_option_tests {
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
    fn predictive_gop4_requires_its_own_trusted_capability_and_bounded_diagnostics() {
        let selected = args(&[
            "--codec",
            "h264",
            "--datagram-bind",
            "127.0.0.1:0",
            "--datagram-codec",
            "h264-predictive-gop4",
            "--video-intra-period",
            "4",
            "--stream-stats",
            "--max-clients",
            "1",
        ]);
        assert!(
            configured_datagram(&selected, configured_video_timing(&selected).unwrap()).is_ok()
        );
        for options in [
            vec![
                "--codec",
                "h264",
                "--datagram-bind",
                "127.0.0.1:0",
                "--video-intra-period",
                "4",
            ],
            vec![
                "--codec",
                "h264",
                "--datagram-bind",
                "127.0.0.1:0",
                "--datagram-codec",
                "h264-predictive-gop4",
                "--video-intra-period",
                "1",
                "--stream-stats",
                "--max-clients",
                "1",
            ],
            vec![
                "--codec",
                "h264",
                "--datagram-bind",
                "127.0.0.1:0",
                "--datagram-codec",
                "h264-predictive-gop4",
                "--video-intra-period",
                "4",
                "--max-clients",
                "1",
            ],
            vec![
                "--codec",
                "h264",
                "--datagram-bind",
                "127.0.0.1:0",
                "--datagram-codec",
                "h264-predictive-gop4",
                "--video-intra-period",
                "4",
                "--stream-stats",
                "--max-clients",
                "2",
            ],
        ] {
            let invalid = args(&options);
            assert!(
                configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err()
            );
        }
    }

    #[test]
    fn predictive_head100_changes_only_the_trusted_head_cadence() {
        let base = [
            "--codec",
            "h264",
            "--datagram-bind",
            "127.0.0.1:0",
            "--datagram-codec",
            "h264-predictive-gop4-head100",
            "--video-intra-period",
            "4",
            "--stream-stats",
            "--max-clients",
            "1",
        ];
        let selected = args(&base);
        let codec = selected.datagram_codec.unwrap();
        assert_eq!(codec, datagram::DatagramCodec::H264PredictiveGop4Head100);
        assert!(codec.predictive());
        assert_eq!(codec.policy(), None);
        assert_eq!(codec.protocol(), "GLV1");
        assert!(
            configured_datagram(&selected, configured_video_timing(&selected).unwrap()).is_ok()
        );
        let mut legacy = json!({});
        datagram::DatagramCodec::H264PredictiveGop4.decorate_video(&mut legacy);
        assert_eq!(
            legacy,
            json!({"media":"GLD1/GLV1-predictive-v1",
            "datagram_codec":"h264-predictive-gop4","protocol":"GLV1","datagram_protocol":"GLD1",
            "independent_frames":false,"reference_policy":"contiguous-since-idr","gop_frames":4,
            "maximum_access_unit_bytes":1048576,"encoded_head_interval_ms":1000,
            "encoded_head_deadline_ms":250,"recovery_interval_ms":1000,"media_retransmission":false})
        );
        let mut fast = json!({});
        codec.decorate_video(&mut fast);
        let mut expected = legacy;
        expected["datagram_codec"] = json!("h264-predictive-gop4-head100");
        expected["encoded_head_interval_ms"] = json!(100);
        assert_eq!(fast, expected);
        for (flag, value) in [
            ("--video-intra-period", "1"),
            ("--max-clients", "2"),
            ("--datagram-bind", "0.0.0.0:0"),
        ] {
            let mut changed = base.to_vec();
            let index = changed.iter().position(|item| *item == flag).unwrap() + 1;
            changed[index] = value;
            let invalid = args(&changed);
            assert!(
                configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err()
            );
        }
        let without_stats = base
            .into_iter()
            .filter(|item| *item != "--stream-stats")
            .collect::<Vec<_>>();
        let invalid = args(&without_stats);
        assert!(configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err());
    }

    #[test]
    fn compact_head_profile_keeps_the_same_trusted_gop4_admission_guards() {
        let base = [
            "--codec",
            "h264",
            "--datagram-bind",
            "127.0.0.1:0",
            "--datagram-codec",
            "h264-predictive-gop4-compact100",
            "--video-intra-period",
            "4",
            "--stream-stats",
            "--max-clients",
            "1",
        ];
        let selected = args(&base);
        assert_eq!(
            selected.datagram_codec,
            Some(datagram::DatagramCodec::H264PredictiveGop4Compact100)
        );
        assert!(
            configured_datagram(&selected, configured_video_timing(&selected).unwrap()).is_ok()
        );
        for (flag, value) in [
            ("--video-intra-period", "1"),
            ("--max-clients", "2"),
            ("--datagram-bind", "0.0.0.0:0"),
        ] {
            let mut changed = base.to_vec();
            let index = changed.iter().position(|item| *item == flag).unwrap() + 1;
            changed[index] = value;
            let invalid = args(&changed);
            assert!(
                configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err()
            );
        }
        let without_stats = base
            .into_iter()
            .filter(|item| *item != "--stream-stats")
            .collect::<Vec<_>>();
        let invalid = args(&without_stats);
        assert!(configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err());
    }

    #[test]
    fn compact50_keeps_trusted_predictive_profile_admission() {
        let base = [
            "--codec",
            "h264",
            "--datagram-bind",
            "127.0.0.1:0",
            "--datagram-codec",
            "h264-predictive-gop4-compact50",
            "--video-intra-period",
            "4",
            "--stream-stats",
            "--max-clients",
            "1",
        ];
        let selected = args(&base);
        assert_eq!(
            selected.datagram_codec,
            Some(datagram::DatagramCodec::H264PredictiveGop4Compact50)
        );
        assert!(
            configured_datagram(&selected, configured_video_timing(&selected).unwrap()).is_ok()
        );
        for (flag, value) in [
            ("--video-intra-period", "1"),
            ("--max-clients", "2"),
            ("--datagram-bind", "0.0.0.0:0"),
        ] {
            let mut changed = base.to_vec();
            let index = changed.iter().position(|item| *item == flag).unwrap() + 1;
            changed[index] = value;
            let invalid = args(&changed);
            assert!(
                configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err()
            );
        }
        let no_stats = base
            .into_iter()
            .filter(|item| *item != "--stream-stats")
            .collect::<Vec<_>>();
        let invalid = args(&no_stats);
        assert!(configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err());
    }

    #[test]
    fn xor8_keeps_trusted_predictive_profile_admission() {
        let base = [
            "--codec",
            "h264",
            "--datagram-bind",
            "127.0.0.1:0",
            "--datagram-codec",
            "h264-predictive-gop4-compact50-xor8",
            "--video-intra-period",
            "4",
            "--stream-stats",
            "--max-clients",
            "1",
        ];
        let selected = args(&base);
        assert_eq!(
            selected.datagram_codec,
            Some(datagram::DatagramCodec::H264PredictiveGop4Compact50Xor8)
        );
        assert!(
            configured_datagram(&selected, configured_video_timing(&selected).unwrap()).is_ok()
        );
        for (flag, value) in [
            ("--video-intra-period", "1"),
            ("--max-clients", "2"),
            ("--datagram-bind", "0.0.0.0:0"),
        ] {
            let mut changed = base.to_vec();
            let index = changed.iter().position(|item| *item == flag).unwrap() + 1;
            changed[index] = value;
            let invalid = args(&changed);
            assert!(
                configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err()
            );
        }
        let no_stats = base
            .into_iter()
            .filter(|item| *item != "--stream-stats")
            .collect::<Vec<_>>();
        let invalid = args(&no_stats);
        assert!(configured_datagram(&invalid, configured_video_timing(&invalid).unwrap()).is_err());
    }

    #[test]
    fn tile_codec_is_explicit_scoped_and_does_not_require_video_idr() {
        let base = [
            "gliss-server",
            "--demo",
            "--token",
            "01234567890123456789012345678901",
        ];
        for (codec, policy) in [("glt2-lossless", 0), ("glt2-gentle", 1), ("glt2-strong", 2)] {
            assert!(
                Args::try_parse_from(base.into_iter().chain(["--datagram-codec", codec])).is_err()
            );
            let selected = args(&[
                "--datagram-bind",
                "127.0.0.1:0",
                "--datagram-codec",
                codec,
                "--datagram-congestion-control",
                "bbr",
            ]);
            assert_eq!(selected.datagram_codec.unwrap().policy(), Some(policy));
            assert!(configured_datagram(&selected, None).unwrap().is_some());
            let cubic = args(&["--datagram-bind", "127.0.0.1:0", "--datagram-codec", codec]);
            assert!(configured_datagram(&cubic, None).is_err());
        }
        let ordinary = args(&[]);
        assert_eq!(ordinary.datagram_codec, None);
        assert!(configured_datagram(&ordinary, None).unwrap().is_none());
        assert!(
            Args::try_parse_from(base.into_iter().chain([
                "--datagram-bind",
                "127.0.0.1:0",
                "--datagram-codec",
                "automatic"
            ]))
            .is_err()
        );
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
        for path in ["client.js", "decoder.js", "vendor/fflate-bounded-core.js"] {
            assert!(
                WEB_ASSETS.iter().any(|(name, _, _)| *name == path),
                "{path}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
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

    #[test]
    fn congestion_controller_is_operator_selected_and_requires_udp_scope() {
        let base = [
            "gliss-server",
            "--demo",
            "--token",
            "01234567890123456789012345678901",
        ];
        for controller in ["cubic", "bbr"] {
            assert!(
                Args::try_parse_from(
                    base.into_iter()
                        .chain(["--datagram-congestion-control", controller])
                )
                .is_err()
            );
            let selected = args(&[
                "--codec",
                "h264",
                "--video-intra-period",
                "1",
                "--datagram-bind",
                "127.0.0.1:0",
                "--datagram-congestion-control",
                controller,
            ]);
            assert!(
                configured_datagram(&selected, configured_video_timing(&selected).unwrap())
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                selected.datagram_congestion_control.unwrap().name(),
                controller
            );
        }
        for invalid in ["reno", "BBR", "none", "", "gpu"] {
            assert!(
                Args::try_parse_from(base.into_iter().chain([
                    "--datagram-bind",
                    "127.0.0.1:0",
                    "--datagram-congestion-control",
                    invalid,
                ]))
                .is_err()
            );
        }
        let default = args(&[
            "--codec",
            "h264",
            "--video-intra-period",
            "1",
            "--datagram-bind",
            "127.0.0.1:0",
        ]);
        assert_eq!(default.datagram_congestion_control, None);
        assert_eq!(
            default.datagram_congestion_control.unwrap_or_default(),
            datagram::CongestionControl::Cubic
        );
        let mut unscoped = args(&[]);
        unscoped.datagram_congestion_control = Some(datagram::CongestionControl::Bbr);
        assert!(configured_datagram(&unscoped, None).is_err());
    }

    #[test]
    fn datagram_requires_loopback_and_independently_decodable_video() {
        let valid = args(&[
            "--codec",
            "h264",
            "--video-intra-period",
            "1",
            "--datagram-bind",
            "127.0.0.1:0",
            "--datagram-advertise-port",
            "4443",
        ]);
        assert_eq!(
            configured_datagram(&valid, configured_video_timing(&valid).unwrap()).unwrap(),
            Some(("127.0.0.1:0".parse().unwrap(), Some(4443)))
        );
        for bind in ["0.0.0.0:4443", "127.0.0.2:4443", "[::1]:4443"] {
            let args = args(&[
                "--codec",
                "h264",
                "--video-intra-period",
                "1",
                "--datagram-bind",
                bind,
            ]);
            assert!(configured_datagram(&args, configured_video_timing(&args).unwrap()).is_err());
        }
        for extra in [
            vec!["--codec", "h264", "--datagram-bind", "127.0.0.1:0"],
            vec!["--codec", "deflate", "--datagram-bind", "127.0.0.1:0"],
        ] {
            let args = args(&extra);
            assert!(configured_datagram(&args, configured_video_timing(&args).unwrap()).is_err());
        }
    }

    #[test]
    fn default_clients_need_no_datagram_endpoint_or_advertised_port() {
        for codec in ["deflate", "h264", "hybrid-grid"] {
            let args = args(&["--codec", codec]);
            assert!(
                configured_datagram(&args, configured_video_timing(&args).unwrap())
                    .unwrap()
                    .is_none()
            );
        }
        for extra in [
            vec!["--datagram-advertise-port", "4443"],
            vec![
                "--datagram-bind",
                "127.0.0.1:0",
                "--datagram-advertise-port",
                "0",
            ],
        ] {
            assert!(
                Args::try_parse_from(["gliss-server", "--demo"].into_iter().chain(extra)).is_err()
            );
        }
    }
}

fn configured_video_qp(args: &Args) -> Result<Option<gliss_video::QpBounds>> {
    match (args.video_qp_min, args.video_qp_max) {
        (None, None) => Ok(None),
        (Some(min), Some(max)) => Ok(Some(gliss_video::QpBounds::new(min, max)?)),
        _ => bail!("Both video-qp-min and video-qp-max are required"),
    }
}

fn configured_video_timing(args: &Args) -> Result<Option<VideoTiming>> {
    let qp = configured_video_qp(args)?;
    if args.video_frame_skip || qp.is_some() {
        ensure!(
            args.codec == "h264"
                && args.datagram_bind.is_none()
                && args.benchmark_capture_dir.is_none(),
            "video frame skipping and QP bounds require WebSocket h264 without native benchmark capture"
        );
    }
    if args.codec != "h264" {
        ensure!(
            !explicit_video_timing(args),
            "video-fps, video-timestamp-fps and video-intra-period require --codec h264"
        );
        return Ok(None);
    }
    Ok(Some(VideoTiming::new(
        args.video_fps.unwrap_or(args.fps),
        args.video_timestamp_fps.unwrap_or(args.fps),
        args.video_intra_period.unwrap_or(args.fps * 2),
    )?))
}

fn with_video_metadata(
    mut value: serde_json::Value,
    args: &Args,
    timing: Option<VideoTiming>,
) -> serde_json::Value {
    if explicit_video_timing(args)
        && let Some(timing) = timing
    {
        value["video_config"] = json!({
            "fps": timing.fps(),
            "timestamp_fps": timing.timestamp_fps(),
            "intra_period": timing.intra_period(),
        });
    }
    if let (Some(min), Some(max)) = (args.video_qp_min, args.video_qp_max) {
        value["video_qp_bounds"] = json!({"min":min,"max":max,"scope":"requested native bounds; not coded-QP attestation"});
    }
    if args.video_frame_skip {
        value["video_frame_skip"] = json!(true);
    }
    value
}

fn configured_hybrid(args: &Args) -> Result<gliss_hybrid::HybridConfig> {
    use gliss_hybrid::{BaseMode, HybridConfig, VideoProfile};
    let base = match args.hybrid_base.as_str() {
        "global" => BaseMode::Global,
        "global-cached" => BaseMode::GlobalCached,
        _ => bail!("hybrid-base must be global or global-cached"),
    };
    let video = match args.hybrid_video_profile.as_str() {
        "screen" => VideoProfile::Screen,
        "camera-scene-on" => VideoProfile::CameraSceneOn,
        "camera-scene-off" => VideoProfile::CameraSceneOff,
        _ => bail!("invalid hybrid-video-profile"),
    };
    let config = HybridConfig { base, video };
    if !matches!(
        args.codec.as_str(),
        "hybrid" | "hybrid-grid" | "hybrid-continuity"
    ) && config != HybridConfig::default()
    {
        bail!("nondefault hybrid options require --codec hybrid, hybrid-grid or hybrid-continuity");
    }
    Ok(config)
}

fn hybrid_metadata(codec: &str, config: gliss_hybrid::HybridConfig) -> Option<serde_json::Value> {
    matches!(codec, "hybrid" | "hybrid-grid" | "hybrid-continuity").then(|| {
        json!({"base_mode":config.base.name(),"video_profile":config.video.name(),"video_usage":config.video.usage_name(),"scene_change_detection":config.video.scene_change_detection()})
    })
}

fn with_hybrid_metadata(
    mut value: serde_json::Value,
    codec: &str,
    config: gliss_hybrid::HybridConfig,
) -> serde_json::Value {
    if let Some(metadata) = hybrid_metadata(codec, config) {
        value["hybrid_config"] = metadata;
    }
    value
}

fn with_stream_metadata(value: serde_json::Value, app: &App) -> serde_json::Value {
    let mut value = with_hybrid_metadata(value, &app.args.codec, app.hybrid_config);
    if let Some(grid) = app.hybrid_mask_grid {
        value["hybrid_config"]["mask_grid"] = json!(grid);
        value["hybrid_config"]["base_tile"] = json!(64);
    }
    value["tcp_listener"] = json!(app.tcp_listener.snapshot());
    value["audio_packet_ms"] = json!(app.args.audio_packet_ms.millis());
    if app.args.datagram_display_input_only {
        value["display_input_only"] = json!(true);
    }
    if app
        .args
        .datagram_codec
        .is_some_and(|codec| codec.policy().is_some())
    {
        value["tile_send_quantum"] = json!(tile_quantum::selected_name(app.args.tile_send_quantum));
        value["tile_quantum_configuration"] =
            tile_quantum::configuration(app.args.tile_send_quantum);
    }
    with_video_metadata(value, &app.args, app.video_timing)
}

fn clipboard_message(text: String, display_input_only: bool) -> Option<serde_json::Value> {
    (!display_input_only).then(|| json!({"type":"clipboard", "text":text}))
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
mod hybrid_option_tests {
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
    fn old_codec_and_hybrid_defaults_retain_global_screen() {
        let ordinary = args(&[]).unwrap();
        assert_eq!(ordinary.codec, "deflate");
        assert_eq!(
            configured_hybrid(&ordinary).unwrap(),
            gliss_hybrid::HybridConfig::default()
        );
        let hybrid = args(&["--codec", "hybrid"]).unwrap();
        assert_eq!(
            configured_hybrid(&hybrid).unwrap(),
            gliss_hybrid::HybridConfig::default()
        );
        assert!(hybrid_metadata(&ordinary.codec, configured_hybrid(&ordinary).unwrap()).is_none());
        let original = json!({"codec":"deflate","protocol":"GLS1"});
        assert_eq!(
            with_hybrid_metadata(
                original.clone(),
                &ordinary.codec,
                configured_hybrid(&ordinary).unwrap()
            ),
            original
        );
    }
    #[test]
    fn explicit_hybrid_options_select_and_describe_the_encoder_configuration() {
        use gliss_hybrid::{BaseMode, VideoProfile};
        for (name, profile) in [
            ("screen", VideoProfile::Screen),
            ("camera-scene-on", VideoProfile::CameraSceneOn),
            ("camera-scene-off", VideoProfile::CameraSceneOff),
        ] {
            let selected = args(&[
                "--codec",
                "hybrid",
                "--hybrid-base",
                "global-cached",
                "--hybrid-video-profile",
                name,
            ])
            .unwrap();
            let config = configured_hybrid(&selected).unwrap();
            assert_eq!(config.base, BaseMode::GlobalCached);
            assert_eq!(config.video, profile);
            let metadata = hybrid_metadata(&selected.codec, config).unwrap();
            assert_eq!(metadata["base_mode"], "global-cached");
            assert_eq!(metadata["video_profile"], name);
            assert_eq!(metadata["video_usage"], profile.usage_name());
            assert_eq!(
                metadata["scene_change_detection"],
                profile.scene_change_detection()
            );
        }
    }
    #[test]
    fn nondefault_hybrid_options_require_hybrid_but_explicit_defaults_are_compatible() {
        for codec in ["deflate", "global", "h264"] {
            for options in [
                ["--hybrid-base", "global-cached"],
                ["--hybrid-video-profile", "camera-scene-on"],
                ["--hybrid-video-profile", "camera-scene-off"],
            ] {
                let selected = args(&["--codec", codec, options[0], options[1]]).unwrap();
                assert!(configured_hybrid(&selected).is_err());
            }
            let selected = args(&[
                "--codec",
                codec,
                "--hybrid-base",
                "global",
                "--hybrid-video-profile",
                "screen",
            ])
            .unwrap();
            assert_eq!(
                configured_hybrid(&selected).unwrap(),
                gliss_hybrid::HybridConfig::default()
            );
        }
    }
    #[test]
    fn unsupported_profile_and_base_values_are_rejected_by_cli_admission() {
        for options in [
            ["--hybrid-base", "adaptive"],
            ["--hybrid-video-profile", "screen-scene-off"],
            ["--hybrid-video-profile", "camera"],
            ["--hybrid-video-profile", "gpu"],
        ] {
            assert!(args(&["--codec", "hybrid", options[0], options[1]]).is_err());
        }
    }

    #[test]
    fn fine_grid_requires_its_own_codec_and_rejects_unsupported_edges() {
        for edge in ["0", "17", "31", "64", "65536"] {
            assert!(args(&["--codec", "hybrid-grid", "--hybrid-mask-grid", edge]).is_err());
        }
        for codec in ["deflate", "hybrid", "h264"] {
            let selected = args(&["--codec", codec, "--hybrid-mask-grid", "16"]).unwrap();
            assert!(configured_mask_grid(&selected).is_err());
        }
    }

    #[test]
    fn fine_grid_defaults_and_explicit_axes_are_separate_from_the_old_hybrid() {
        let old = args(&["--codec", "hybrid"]).unwrap();
        assert_eq!(configured_mask_grid(&old).unwrap(), None);
        let new = args(&["--codec", "hybrid-grid"]).unwrap();
        assert_eq!(configured_mask_grid(&new).unwrap(), Some(32));
        assert_eq!(
            configured_hybrid(&new).unwrap(),
            gliss_hybrid::HybridConfig::default()
        );
        let explicit = args(&[
            "--codec",
            "hybrid-grid",
            "--hybrid-mask-grid",
            "16",
            "--hybrid-base",
            "global-cached",
            "--hybrid-video-profile",
            "camera-scene-off",
        ])
        .unwrap();
        assert_eq!(configured_mask_grid(&explicit).unwrap(), Some(16));
        assert_eq!(
            configured_hybrid(&explicit).unwrap(),
            gliss_hybrid::HybridConfig {
                base: gliss_hybrid::BaseMode::GlobalCached,
                video: gliss_hybrid::VideoProfile::CameraSceneOff
            }
        );
    }

    #[test]
    fn continuity_codec_has_separate_grid_default_and_explicit_profile_axes() {
        let selected = args(&["--codec", "hybrid-continuity"]).unwrap();
        assert_eq!(configured_mask_grid(&selected).unwrap(), Some(32));
        assert_eq!(
            configured_hybrid(&selected).unwrap(),
            gliss_hybrid::HybridConfig::default()
        );
        for grid in ["16", "32"] {
            for (name, profile) in [
                ("screen", gliss_hybrid::VideoProfile::Screen),
                ("camera-scene-on", gliss_hybrid::VideoProfile::CameraSceneOn),
                (
                    "camera-scene-off",
                    gliss_hybrid::VideoProfile::CameraSceneOff,
                ),
            ] {
                let explicit = args(&[
                    "--codec",
                    "hybrid-continuity",
                    "--hybrid-mask-grid",
                    grid,
                    "--hybrid-base",
                    "global-cached",
                    "--hybrid-video-profile",
                    name,
                ])
                .unwrap();
                assert_eq!(
                    configured_mask_grid(&explicit).unwrap(),
                    Some(grid.parse().unwrap())
                );
                let config = configured_hybrid(&explicit).unwrap();
                assert_eq!(config.base, gliss_hybrid::BaseMode::GlobalCached);
                assert_eq!(config.video, profile);
                let metadata = hybrid_metadata(&explicit.codec, config).unwrap();
                assert_eq!(metadata["base_mode"], "global-cached");
                assert_eq!(metadata["video_profile"], name);
                assert_eq!(
                    metadata["scene_change_detection"],
                    profile.scene_change_detection()
                );
            }
        }
    }

    #[test]
    fn continuity_cli_rejects_invalid_grid_and_preserves_other_codec_defaults() {
        for edge in ["0", "17", "31", "64", "65536"] {
            assert!(args(&["--codec", "hybrid-continuity", "--hybrid-mask-grid", edge]).is_err());
        }
        for options in [
            ["--hybrid-base", "adaptive"],
            ["--hybrid-video-profile", "screen-scene-off"],
        ] {
            assert!(args(&["--codec", "hybrid-continuity", options[0], options[1]]).is_err());
        }
        for codec in ["deflate", "hybrid", "h264"] {
            let unchanged = args(&["--codec", codec]).unwrap();
            assert_eq!(configured_mask_grid(&unchanged).unwrap(), None);
            assert_eq!(
                configured_hybrid(&unchanged).unwrap(),
                gliss_hybrid::HybridConfig::default()
            );
            let invalid = args(&["--codec", codec, "--hybrid-mask-grid", "16"]).unwrap();
            assert!(configured_mask_grid(&invalid).is_err());
        }
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
    fn legacy_video_rates_and_metadata_are_unchanged_without_explicit_options() {
        for fps in ["15", "60"] {
            let args = args(&["--codec", "h264", "--fps", fps]).unwrap();
            let timing = configured_video_timing(&args).unwrap().unwrap();
            assert_eq!(
                timing,
                VideoTiming::new(args.fps, args.fps, args.fps * 2).unwrap()
            );
            let original = json!({"codec": "h264", "fps_limit": args.fps});
            assert_eq!(
                with_video_metadata(original.clone(), &args, Some(timing)),
                original
            );
        }
        for codec in ["deflate", "hybrid", "hybrid-grid", "hybrid-continuity"] {
            let args = args(&["--codec", codec]).unwrap();
            assert!(configured_video_timing(&args).unwrap().is_none());
            let original = json!({"codec": codec});
            assert_eq!(with_video_metadata(original.clone(), &args, None), original);
        }
    }

    #[test]
    fn explicit_video_rates_are_independent_and_describe_resolved_native_policy() {
        let args = args(&["--codec", "h264", "--fps", "60", "--video-fps", "15"]).unwrap();
        let timing = configured_video_timing(&args).unwrap().unwrap();
        assert_eq!(args.fps, 60);
        assert_eq!(timing, VideoTiming::new(15, 60, 120).unwrap());
        let metadata = with_video_metadata(json!({"fps_limit": args.fps}), &args, Some(timing));
        assert_eq!(
            metadata,
            json!({"fps_limit":60,"video_config":{
                "fps":15,"timestamp_fps":60,"intra_period":120
            }})
        );
        for (option, value, expected) in [
            ("--video-fps", "60", VideoTiming::new(60, 15, 30).unwrap()),
            (
                "--video-timestamp-fps",
                "60",
                VideoTiming::new(15, 60, 30).unwrap(),
            ),
            (
                "--video-intra-period",
                "120",
                VideoTiming::new(15, 15, 120).unwrap(),
            ),
        ] {
            let args = self::args(&["--codec", "h264", "--fps", "15", option, value]).unwrap();
            assert_eq!(configured_video_timing(&args).unwrap(), Some(expected));
        }
    }

    #[test]
    fn video_timing_cli_bounds_and_codec_scope_are_enforced() {
        for option in ["--video-fps", "--video-timestamp-fps"] {
            for value in ["0", "61", "4294967295", "-1", "x"] {
                assert!(args(&["--codec", "h264", option, value]).is_err());
            }
        }
        for value in ["0", "3601", "4294967295", "-1", "x"] {
            assert!(args(&["--codec", "h264", "--video-intra-period", value]).is_err());
        }
        for codec in [
            "deflate",
            "global",
            "hybrid",
            "hybrid-grid",
            "hybrid-continuity",
        ] {
            for option in [
                "--video-fps",
                "--video-timestamp-fps",
                "--video-intra-period",
            ] {
                let args = args(&["--codec", codec, option, "60"]).unwrap();
                assert!(configured_video_timing(&args).is_err());
            }
        }
    }

    #[test]
    fn lazy_video_encoder_retains_custom_timing_across_reset_and_resize() {
        let timing = VideoTiming::new(15, 60, 120).unwrap();
        let mut encoder = StreamEncoder::Video {
            encoder: None,
            timing,
            bitrate: 3_000_000,
            options: gliss_video::VideoOptions::default(),
        };
        encoder.reset();
        for (sequence, width, height) in [(1, 64, 48), (2, 64, 48), (3, 66, 50)] {
            let frame = Frame::new(width, height).unwrap();
            let bytes = encoder.encode(&frame, sequence, 2).unwrap();
            let packet = gliss_video::parse_packet(&bytes).unwrap();
            assert_eq!(packet.keyframe, sequence != 2);
            let StreamEncoder::Video {
                encoder: Some(native),
                ..
            } = &encoder
            else {
                panic!("lazy encoder missing")
            };
            assert_eq!(native.timing(), timing);
        }
        encoder.reset();
        let bytes = encoder.encode(&Frame::new(66, 50).unwrap(), 4, 3).unwrap();
        assert!(gliss_video::parse_packet(&bytes).unwrap().keyframe);
    }

    #[test]
    fn frame_skip_is_opt_in_and_rejects_unimplemented_transports_and_recorders() {
        let default = args(&["--codec", "h264"]).unwrap();
        assert!(!default.video_frame_skip);
        let enabled = args(&["--codec", "h264", "--video-frame-skip", "true"]).unwrap();
        assert!(configured_video_timing(&enabled).is_ok());
        assert_eq!(
            with_video_metadata(
                json!({}),
                &enabled,
                configured_video_timing(&enabled).unwrap()
            )["video_frame_skip"],
            true
        );
        for extra in [
            vec!["--codec", "deflate", "--video-frame-skip", "true"],
            vec![
                "--codec",
                "h264",
                "--video-frame-skip",
                "true",
                "--datagram-bind",
                "127.0.0.1:0",
            ],
            vec![
                "--codec",
                "h264",
                "--video-frame-skip",
                "true",
                "--benchmark-capture-dir",
                "unused-test-directory",
            ],
        ] {
            let rejected = args(&extra).unwrap();
            assert!(configured_video_timing(&rejected).is_err());
        }
        assert!(args(&["--codec", "h264", "--video-frame-skip", "unknown"]).is_err());
    }
}

#[cfg(test)]
mod fd_option_tests {
    use super::*;

    fn args(extra: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(
            [
                "gliss-server",
                "--token",
                "01234567890123456789012345678901",
            ]
            .into_iter()
            .chain(extra.iter().copied()),
        )
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn fd_source_requires_its_absolute_deadline_and_excludes_other_sources() {
        assert!(args(&["--rfb-fd", "9"]).is_err());
        assert!(args(&["--demo", "--rfb-deadline-unix-ms", "1"]).is_err());
        let expiry = (now_ms() + 60_000).to_string();
        for other in [
            vec!["--demo"],
            vec!["--vnc", "127.0.0.1:5907"],
            vec!["--dbus-address", "unix:path=/tmp/bus", "--vm-uuid", "uuid"],
            vec!["--shared-dbus"],
        ] {
            let mut selected = vec!["--rfb-fd", "9", "--rfb-deadline-unix-ms", expiry.as_str()];
            selected.extend(other);
            assert!(args(&selected).is_err());
        }
        for raw in ["0", "1", "2"] {
            assert!(args(&["--rfb-fd", raw, "--rfb-deadline-unix-ms", &expiry]).is_err());
        }
    }

    #[test]
    fn fd_source_is_loopback_single_viewer_and_cannot_select_a_dbus_console() {
        let expiry = (now_ms() + 60_000).to_string();
        let base = ["--rfb-fd", "9", "--rfb-deadline-unix-ms", &expiry];
        assert!(configured_source_deadline(&args(&base).unwrap()).is_err());
        let selected = [&base[..], &["--max-clients", "1"]].concat();
        assert!(
            configured_source_deadline(&args(&selected).unwrap())
                .unwrap()
                .is_some()
        );
        for extra in [
            vec![
                "--bind",
                "0.0.0.0:8080",
                "--origin",
                "https://example.invalid",
            ],
            vec!["--dbus-console", "1"],
        ] {
            let mut selected = selected.clone();
            selected.extend(extra);
            assert!(configured_source_deadline(&args(&selected).unwrap()).is_err());
        }
    }

    #[test]
    fn expired_or_overlong_source_lease_is_rejected_before_adoption() {
        for expiry in [now_ms().saturating_sub(1), now_ms() + 3_700_000] {
            let expiry = expiry.to_string();
            assert!(
                configured_source_deadline(
                    &args(&[
                        "--rfb-fd",
                        "9",
                        "--rfb-deadline-unix-ms",
                        &expiry,
                        "--max-clients",
                        "1"
                    ])
                    .unwrap()
                )
                .is_err()
            );
        }
        assert!(
            configured_source_deadline(&args(&["--demo"]).unwrap())
                .unwrap()
                .is_none()
        );
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

#[derive(Deserialize)]
struct Login {
    token: String,
}

async fn login(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<Login>,
) -> Response {
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
    let mut value = with_stream_metadata(
        json!({"bytes":app.bytes.load(Ordering::Relaxed),"frames":app.frames.load(Ordering::Relaxed),"active_clients":app.args.max_clients-app.clients.available_permits(),"uptime_s":app.started.elapsed().as_secs_f64(),"codec":app.args.codec,"shared_capture":app.shared_capture.as_ref().map(|capture|capture.snapshot())}),
        &app,
    );
    if let Some(runtime) = &app.datagram {
        value["datagram_config"] = json!({
            "transport":"webtransport-datagram",
            "congestion_control":runtime.congestion_control(),
            "bound_udp":runtime.local_addr().to_string(),
            "advertised_udp_port":app.args.datagram_advertise_port.unwrap_or(runtime.local_addr().port()),
            "independent_frames":true,
            "media_retransmission":false,
        });
        if let Some(policy) = runtime.codec().policy() {
            let config = value["datagram_config"].as_object_mut().unwrap();
            config.insert("codec".into(), json!(runtime.codec().name()));
            config.insert("protocol".into(), json!(runtime.codec().protocol()));
            config.insert("policy".into(), json!(policy));
            config.insert(
                "tile_send_policy".into(),
                json!(app.args.tile_send_policy.unwrap_or_default().name()),
            );
            config.insert(
                "tile_send_quantum".into(),
                json!(tile_quantum::selected_name(app.args.tile_send_quantum)),
            );
            config.insert(
                "tile_quantum_configuration".into(),
                tile_quantum::configuration(app.args.tile_send_quantum),
            );
            config.insert("independent_frames".into(), json!(false));
            config.insert("independent_tiles".into(), json!(true));
        }
        runtime
            .codec()
            .decorate_video(&mut value["datagram_config"]);
    }
    if let Some(recorder) = &app.benchmark {
        value["benchmark_capture"] = recorder.snapshot();
    }
    Json(value).into_response()
}

async fn datagram_ticket(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    match &app.datagram {
        Some(runtime) => runtime.mint_ticket(&app, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn datagram_revoke(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(runtime) = &app.datagram else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !origin_allowed(&app, &headers) || authorized_until(&app, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // The irreversible admission/input gate changes before endpoint close or
    // shutdown notification. A revoked worker cannot be redeemed or reused.
    runtime.revoke();
    app.worker_shutdown.send_replace(true);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StageClockRequest {
    nonce: String,
    ordinal: u32,
}

impl StageClockRequest {
    fn valid(&self) -> bool {
        (1..=64).contains(&self.ordinal)
            && self.nonce.len() == 32
            && self
                .nonce
                .bytes()
                .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
    }
}

async fn stage_clock(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: std::result::Result<Json<StageClockRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(trace) = &app.stage_trace else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !origin_allowed(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if authorized_until(&app, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(Json(body)) = body else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !body.valid() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(_actor) = stage_trace::lease(&app.stage_trace) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match trace.clock_probe() {
        Ok((request_index, stamp, origin)) => Json(json!({
            "schema": "gliss-causal-native-clock-v1",
            "nonce": body.nonce,
            "ordinal": body.ordinal,
            "request_index": request_index,
            "pid": std::process::id(),
            "clock": "CLOCK_MONOTONIC",
            "domain_id": stamp.domain_id.to_string(),
            "origin_monotonic_ns": origin.to_string(),
            "delta_ns": stamp.delta_ns.to_string(),
        }))
        .into_response(),
        Err(stage_trace::ClockProbeError::Closed) => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(stage_trace::ClockProbeError::Full) => StatusCode::TOO_MANY_REQUESTS.into_response(),
        Err(stage_trace::ClockProbeError::Clock) => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn upgrade(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if app
        .stage_trace
        .as_ref()
        .is_some_and(|trace| trace.is_stopping())
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !origin_allowed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(expires_at) = authorized_until(&app, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if app
        .source_deadline
        .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(permit) = app.clients.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(trace_actor) = stage_trace::lease(&app.stage_trace) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(MAX_CLIPBOARD + 1024)
        .max_frame_size(MAX_CLIPBOARD + 1024)
        .on_upgrade(move |socket| async move {
            let _trace_actor = trace_actor;
            let _permit = permit;
            let trace = app.stage_trace.clone();
            if let Err(error) = stream(socket, app, expires_at).await {
                if error.downcast_ref::<WorkerStopped>().is_none()
                    && let Some(trace) = &trace
                {
                    trace.failure(4);
                }
                tracing::warn!("stream ended: {error:#}");
            }
        })
        .into_response()
}

#[derive(Clone)]
struct Captured {
    frame: Arc<Frame>,
    forwarded_input_id: u32,
    stage_generation: Option<u64>,
    published_at: Option<Instant>,
}

struct Source {
    audio: audio::Session,
    audio_counters: Arc<audio::Counters>,
    frames: watch::Receiver<Option<Captured>>,
    input: mpsc::Sender<Input>,
    events: mpsc::Receiver<serde_json::Value>,
    shutdown: Option<oneshot::Receiver<()>>,
    _tasks: JoinSet<()>,
    capture_lease: Option<shared_capture::CaptureLease>,
    _stage_lease: Option<stage_trace::Lease>,
}

async fn source(app: &App) -> Result<Source> {
    rfb_teardown::validate_source(app)?;
    let stage_lease = stage_trace::lease(&app.stage_trace)?;
    if let Some(trace) = &app.stage_trace {
        trace.claim_source()?;
    }
    let (frame_tx, frames) = watch::channel(None);
    let (input, mut input_rx) = mpsc::channel::<Input>(128);
    let (event_tx, events) = mpsc::channel(16);
    let (audio, mut audio_requests, audio_status) = audio::channel();
    let audio_config = audio::EncodeConfig::new(app.args.audio_packet_ms);
    let audio_counters = audio_config.counters.clone();
    let mut tasks = JoinSet::new();
    let mut shutdown = None;
    let mut capture_lease = None;
    if app.args.demo {
        let workload = app.args.workload.clone();
        let fps = app.args.fps;
        tasks.spawn(async move {
            demo::run(&workload, fps, frame_tx, input_rx).await;
        });
    } else {
        let dbus = if app.shared_capture.is_some() {
            None
        } else if let Some(address) = &app.args.dbus_address {
            Some(
                gliss_dbus::connect_private_bus(
                    address,
                    app.args.vm_uuid.as_deref().unwrap(),
                    app.args.dbus_console,
                )
                .await?,
            )
        } else {
            None
        };
        let dbus_capture = dbus.is_some() || app.shared_capture.is_some();
        let (mut reader, mut writer, info) = tokio::time::timeout(Duration::from_secs(10), async {
            #[cfg(unix)]
            if let Some(slot) = &app.rfb_fd {
                return gliss_rfb::connect_unix_traced(
                    slot.claim()?,
                    app.args.vnc_password.as_deref(),
                    stage_trace::rfb_observer(&app.stage_trace),
                )
                .await;
            }
            gliss_rfb::connect_traced(
                app.args
                    .vnc
                    .as_deref()
                    .context("Missing fixed RFB source")?,
                app.args.vnc_password.as_deref(),
                stage_trace::rfb_observer(&app.stage_trace),
            )
            .await
        })
        .await
        .context("VNC connect timeout")??;
        let teardown_control = reader.teardown_control();
        let raw_audio = reader.audio_events();
        let mut audio_available = writer.audio_available();
        tasks.spawn(audio::encode_with(
            raw_audio,
            audio.request.subscribe(),
            audio.packets.clone(),
            audio_config,
        ));
        let close_policy = app.args.rfb_close_policy.unwrap_or_default();
        let input_id = Arc::new(AtomicU32::new(0));
        let dimensions = Arc::new(AtomicU32::new(
            (u32::from(info.width) << 16) | u32::from(info.height),
        ));
        if let Some(capture) = &app.shared_capture {
            capture_lease = Some(
                capture
                    .attach(shared_capture::ViewerChannels {
                        forwarded_id: shared_capture::ForwardedId::new(input_id.clone()),
                        dimensions: dimensions.clone(),
                        frames: frame_tx.clone(),
                        events: event_tx.clone(),
                    })
                    .await?,
            );
        }
        let pending = Arc::new(AtomicBool::new(false));
        let refresh = Arc::new(AtomicBool::new(false));
        // D-Bus retains its original one-initial-request interval branch.
        let completion = (!dbus_capture
            && app.args.rfb_request_policy == rfb_request::Policy::Ready)
            .then(|| Arc::new(tokio::sync::Notify::new()));
        let reader_completion = completion.clone();
        let reader_id = input_id.clone();
        let reader_size = dimensions.clone();
        let reader_pending = pending.clone();
        let reader_refresh = refresh.clone();
        let reader_events = event_tx.clone();
        let rfb_frames = frame_tx.clone();
        let recorder = app.benchmark.clone();
        let reader_trace = app.stage_trace.clone();
        let reader_actor = stage_trace::lease(&reader_trace)?;
        let display_input_only = app.args.datagram_display_input_only;
        let profile_capture = app.args.stream_stats;
        tasks.spawn(async move {
            let _trace_actor = reader_actor;
            loop {
                let event = match reader.read_event().await { Ok(event) => event, Err(_) => { let _ = reader_events.send(json!({"type":"error","message":"The VM display disconnected."})).await; break; } };
                match event {
                    Event::Frame(frame) => {
                        let size = (u32::from(frame.width) << 16) | u32::from(frame.height);
                        if !dbus_capture && reader_size.swap(size, Ordering::AcqRel) != size { reader_refresh.store(true, Ordering::Release); }
                        reader_pending.store(false, Ordering::Release);
                        if let Some(completion) = &reader_completion { completion.notify_one(); }
                        if !dbus_capture {
                            let frame = Arc::new(frame);
                            if let Some(recorder) = &recorder && let Err(error) = recorder.capture(&frame) {
                                let _ = reader_events.try_send(json!({"type":"error","message":format!("Benchmark evidence invalid: {error:#}")}));
                                break;
                            }
                            let forwarded_input_id = reader_id.load(Ordering::Acquire);
                            let stage_frame = reader_trace.as_ref().map(|_|frame.clone());
                            let stage_generation = reader_trace.as_ref().map(|trace|trace.publication_started(reader.traced_update_ordinal()));
                            let sent = rfb_frames.send(Some(Captured { frame, forwarded_input_id,stage_generation,published_at:profile_capture.then(Instant::now) }));
                            if let (Some(trace),Some(frame),Some(generation)) = (&reader_trace,stage_frame,stage_generation) {
                                trace.publication_finished(generation,&frame,reader.traced_update_ordinal(),sent.is_ok());
                            }
                            if sent.is_err() { break; }
                        }
                    }
                    Event::Clipboard(text) => { if let Some(message) = clipboard_message(text, display_input_only) { let _ = reader_events.try_send(message); } }
                    Event::Cursor(cursor) => { if !dbus_capture { let _ = reader_events.try_send(json!({"type":"cursor","width":cursor.width,"height":cursor.height,"hotspot_x":cursor.hotspot_x,"hotspot_y":cursor.hotspot_y,"pixels":cursor.pixels})); } }
                    Event::Bell => { let _ = reader_events.try_send(json!({"type":"bell"})); }
                    Event::ResizeResult { width, height, status } => { let _ = reader_events.try_send(json!({"type":"resize_result","width":width,"height":height,"status":status})); }
                }
            }
        });
        if let Some(mut capture) = dbus {
            let capture_input = input_id.clone();
            let capture_dimensions = dimensions.clone();
            let capture_events = event_tx.clone();
            let diagnostics = app.args.stream_stats;
            tasks.spawn(async move {
                let mut cursor = None;
                let mut disabled = None;
                let mut telemetry = Instant::now();
                loop {
                    let event = match capture.read_event().await {
                        Ok(event) => event,
                        Err(error) => { let _ = capture_events.send(json!({"type":"error","message":format!("The VM CPU display disconnected: {error}")})).await; break; }
                    };
                    if disabled != Some(event.disabled) {
                        disabled = Some(event.disabled);
                        let _ = capture_events.try_send(json!({"type":"display_state","available":!event.disabled}));
                    }
                    if event.disabled { let _ = frame_tx.send(None); }
                    if let Some(frame) = event.frame {
                        capture_dimensions.store((u32::from(frame.width) << 16) | u32::from(frame.height), Ordering::Release);
                        if frame_tx.send(Some(Captured { frame:Arc::new(frame), forwarded_input_id:capture_input.load(Ordering::Acquire),stage_generation:None,published_at:diagnostics.then(Instant::now) })).is_err() { break; }
                    }
                    let next_cursor = if event.mouse.is_none_or(|(_,_,visible)| visible) { event.cursor } else { None };
                    if next_cursor != cursor {
                        match &next_cursor {
                            Some(shape) if shape.width<=128 && shape.height<=128 => {
                                let pixels:Vec<u8> = shape.argb.iter().flat_map(|word| [(word>>16) as u8, (word>>8) as u8, *word as u8, (word>>24) as u8]).collect();
                                let _ = capture_events.try_send(json!({"type":"cursor","width":shape.width,"height":shape.height,"hotspot_x":shape.hotspot_x,"hotspot_y":shape.hotspot_y,"pixels":pixels}));
                            }
                            _ => { let _ = capture_events.try_send(json!({"type":"cursor","width":0,"height":0,"hotspot_x":0,"hotspot_y":0,"pixels":[]})); }
                        }
                        cursor = next_cursor;
                    }
                    if diagnostics && telemetry.elapsed() >= Duration::from_secs(1) {
                        telemetry = Instant::now();
                        let _ = capture_events.try_send(json!({"type":"capture_stats","source":"dbus","accepted_plane_bytes":event.stats.source_bytes,"scanouts":event.stats.scanouts,"updates":event.stats.updates,"normalization_ms_total":event.stats.normalization_ns as f64/1_000_000.,"snapshot_ms":event.snapshot_ns as f64/1_000_000.,"timing_scope":"local callback conversion/snapshot; not guest-write time"}));
                    }
                }
                capture.close().await;
            });
        }
        let period = Duration::from_secs_f64(1.0 / f64::from(app.args.fps));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        shutdown = Some(shutdown_rx);
        let input_trace = app.stage_trace.clone();
        let input_actor = stage_trace::lease(&input_trace)?;
        tasks.spawn(async move {
            let _trace_actor = input_actor;
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut ready = completion.as_ref().map(|_| rfb_request::Ready::new(period));
            let mut first = true;
            let mut keys = HashSet::new();
            let mut pointer = None;
            let mut audio_enabled = false;
            let initial_audio_request = *audio_requests.borrow_and_update();
            audio::update_capture(&mut writer, initial_audio_request, &mut audio_enabled, &audio_status, !display_input_only).await;
            loop {
                tokio::select! {
                    changed = audio_requests.changed() => {
                        if changed.is_err() { break; }
                        let request = *audio_requests.borrow_and_update();
                        audio::update_capture(&mut writer, request, &mut audio_enabled, &audio_status, !display_input_only).await;
                    }
                    changed = audio_available.changed() => {
                        if changed.is_err() { break; }
                        audio_available.borrow_and_update();
                        let request = *audio_requests.borrow();
                        audio::update_capture(&mut writer, request, &mut audio_enabled, &audio_status, !display_input_only).await;
                    }
                    command = input_rx.recv() => {
                        let Some(command) = command else { break; };
                        stage_trace::emit(&input_trace,stage_trace::Kind::InputDequeued,||stage_trace::Fields { input_id:Some(u64::from(command.id())),..stage_trace::Fields::default() });
                        if let Input::Key { keysym, down, .. } = &command { if *down { keys.insert(*keysym); } else { keys.remove(keysym); } }
                        if let Input::Pointer { x, y, buttons, .. } = &command { pointer = Some((*x, *y, *buttons)); }
                        let result = writer.send_input(&command).await;
                        stage_trace::emit(&input_trace,stage_trace::Kind::InputSendCompleted,||stage_trace::Fields { input_id:Some(u64::from(command.id())),outcome:if result.is_ok(){stage_trace::Outcome::Accepted}else{stage_trace::Outcome::Error},..stage_trace::Fields::default() });
                        if let Err(error) = result { let _ = event_tx.try_send(json!({"type":"input_error","message":error.to_string()})); }
                        else { input_id.store(command.id(), Ordering::Release); }
                    }
                    // D-Bus capture requests one initial RFB update only, for cursor/
                    // resize negotiation. Later media arrives exclusively through D-Bus.
                    _ = async {
                        if let (Some(ready), Some(completion)) = (&ready, &completion) {
                            ready.wait(&pending, completion).await;
                        } else {
                            tick.tick().await;
                        }
                    }, if !dbus_capture || first => {
                        stage_trace::emit(&input_trace,stage_trace::Kind::CaptureTickServiced,stage_trace::Fields::default);
                        if !pending.swap(true, Ordering::AcqRel) {
                            let size = dimensions.load(Ordering::Relaxed);
                            let force_refresh = refresh.swap(false, Ordering::AcqRel);
                            if let Some(ready) = &mut ready { ready.invoked(); }
                            if writer.request_update(!first && !force_refresh, (size >> 16) as u16, size as u16).await.is_err() { break; }
                            first = false;
                        } else { stage_trace::emit(&input_trace,stage_trace::Kind::CaptureRequestSkipped,||stage_trace::Fields { outcome:stage_trace::Outcome::Skipped,..stage_trace::Fields::default() }); }
                    }
                }
            }
            if audio_enabled { let _ = writer.set_audio(false).await; }
            let release_start = tokio::time::Instant::now();
            let mut releases = rfb_teardown::Releases::default();
            for keysym in keys {
                releases.key_attempts += 1;
                if writer.send_input(&Input::Key { keysym, down: false, id: 0 }).await.is_ok() {
                    releases.key_successes += 1;
                }
            }
            if let Some((x, y, buttons)) = pointer && buttons != 0 {
                releases.pointer_attempted = true;
                releases.pointer_succeeded = writer.send_input(&Input::Pointer { x, y, buttons: 0, id: 0 }).await.is_ok();
            }
            rfb_teardown::finish(close_policy, writer, teardown_control, shutdown_tx, releases, release_start).await;
        });
    }
    Ok(Source {
        audio,
        audio_counters,
        frames,
        input,
        events,
        shutdown,
        _tasks: tasks,
        capture_lease,
        _stage_lease: stage_lease,
    })
}

/// Preserve the existing shutdown error result while distinguishing this exact
/// intentional cancellation from an authenticated session failure in evidence.
#[derive(Debug)]
struct WorkerStopped(&'static str);
impl std::fmt::Display for WorkerStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for WorkerStopped {}

async fn stream(socket: WebSocket, app: Arc<App>, expires_at: u64) -> Result<()> {
    let mut worker_shutdown = app.worker_shutdown.subscribe();
    let remaining = session_remaining_at(expires_at, SystemTime::now());
    anyhow::ensure!(
        !remaining.is_zero(),
        "display session expired before source setup"
    );
    let session_deadline = tokio::time::Instant::now() + remaining;
    let deadline = app
        .source_deadline
        .map_or(session_deadline, |source| source.min(session_deadline));
    ensure!(
        deadline > tokio::time::Instant::now(),
        "graphics source lease expired before setup"
    );
    let mut source = tokio::select! {
        result = tokio::time::timeout_at(deadline, source(&app)) => result.context("display session expired during source setup")??,
        _ = worker_shutdown.wait_for(|stopped| *stopped) => return Err(WorkerStopped("worker stopped during source setup").into()),
    };
    let result = tokio::select! {
        result = tokio::time::timeout_at(deadline, stream_session(socket, app.clone(), &mut source)) => result.context("display session expired").and_then(|result| result),
        _ = worker_shutdown.wait_for(|stopped| *stopped) => Err(WorkerStopped("worker stopped during stream").into()),
    };
    drop(source.input);
    if let Some(shutdown) = source.shutdown.take()
        && !matches!(
            tokio::time::timeout(Duration::from_secs(1), shutdown).await,
            Ok(Ok(()))
        )
        && let Some(trace) = &app.stage_trace
    {
        trace.failure(2);
    }
    // Release this viewer's held input before dropping shared membership.
    drop(source.capture_lease.take());
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
    flushed: Option<oneshot::Sender<()>>,
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
    packet: Result<Option<Vec<u8>>>,
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

async fn stream_session(socket: WebSocket, app: Arc<App>, source: &mut Source) -> Result<()> {
    let hello = Message::Text(with_stream_metadata(json!({"type":"hello","protocol":match app.mode {Some(Mode::Batch) => "GLB1", Some(Mode::Global | Mode::GlobalCached) => "GLB2", Some(Mode::Sparse) => "GLB3", Some(_) => "GLS1", None if app.args.codec == "hybrid" => "GLH1", None if app.args.codec == "hybrid-grid" => "GLH2", None if app.args.codec == "hybrid-continuity" => "GLH4", None => "GLV1"},"codec":app.args.codec,"source":if app.args.demo {"demo"} else if app.args.dbus_address.is_some() {"dbus"} else {"rfb"},"fps_limit":app.args.fps,"input_metadata":"latest forwarded input; not proof of causal display"}),&app).to_string().into());
    let (mut sink, mut receiver) = socket.split();
    let (command_tx, mut commands) = mpsc::channel(16);
    let (media_tx, mut media) =
        mpsc::channel::<MediaPacket>(usize::from(app.args.in_flight_frames));
    let (control_tx, mut controls) = mpsc::channel::<ControlPacket>(16);
    let (write_tx, mut writes) = mpsc::channel(16);
    let generation = Arc::new(AtomicU64::new(0));
    let mut io_tasks = JoinSet::<Result<()>>::new();
    let input = source.input.clone();
    let input_trace = app.stage_trace.clone();
    let input_actor = stage_trace::lease(&input_trace)?;
    let video = app.args.codec == "h264";
    let display_input_only = app.args.datagram_display_input_only;
    let audio_request = source.audio.request.clone();
    let writer_audio_request = audio_request.subscribe();
    let mut audio_status = source.audio.status.clone();
    let mut audio_packets = source.audio.packets.subscribe();
    let writer_audio_counters = source.audio_counters.clone();
    // This task forwards input independently of semaphore waits, encoding and writes.
    // Dropping the session's JoinSet aborts it and releases its input sender clone.
    io_tasks.spawn(async move {
        let _trace_actor = input_actor;
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
                        Some("audio") => { if !display_input_only { audio::request(&audio_request, value)?; } None }
                        Some("ping") => Some(StreamCommand::Reply(Message::Text(json!({"type":"pong","id":value.get("id")}).to_string().into()))),
                        _ => {
                            let command: Input = serde_json::from_value(value)?;
                            stage_trace::emit(&input_trace,stage_trace::Kind::ControlReceived,||stage_trace::Fields { input_id:Some(u64::from(command.id())),..stage_trace::Fields::default() });
                            if let Input::Clipboard { text, .. } = &command && text.len() > MAX_CLIPBOARD {
                                stage_trace::emit(&input_trace,stage_trace::Kind::InputRejected,||stage_trace::Fields { input_id:Some(u64::from(command.id())),outcome:stage_trace::Outcome::Rejected,..stage_trace::Fields::default() });
                                bail!("clipboard too large");
                            }
                            if video && matches!(command, Input::Resize { width, height, .. } if width < 16 || height < 16 || width > 3840 || height > 2160 || width % 2 != 0 || height % 2 != 0) {
                                stage_trace::emit(&input_trace,stage_trace::Kind::InputRejected,||stage_trace::Fields { input_id:Some(u64::from(command.id())),outcome:stage_trace::Outcome::Rejected,..stage_trace::Fields::default() });
                                Some(StreamCommand::Reply(Message::Text(json!({"type":"input_error","message":"Software H264 requires even desktop dimensions from 16x16 through 3840x2160."}).to_string().into())))
                            } else {
                                let id = command.id(); let sent = input.try_send(command);
                                stage_trace::emit(&input_trace,if sent.is_ok(){stage_trace::Kind::InputAdmitted}else{stage_trace::Kind::InputRejected},||stage_trace::Fields { input_id:Some(u64::from(id)),outcome:match &sent { Ok(())=>stage_trace::Outcome::Accepted,Err(mpsc::error::TrySendError::Full(_))=>stage_trace::Outcome::Full,Err(mpsc::error::TrySendError::Closed(_))=>stage_trace::Outcome::Error },..stage_trace::Fields::default() });
                                sent.context("input queue full")?;
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
    let writer_actor = stage_trace::lease(&app.stage_trace)?;
    io_tasks.spawn(async move {
        let _trace_actor = writer_actor;
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
                    if let Some(flushed) = control.flushed { let _ = flushed.send(()); }
                    continue;
                }
                Some(packet) = media.recv() => packet,
                else => return Ok(()),
            };
            if packet.generation != writer_generation.load(Ordering::Acquire) {
                write_tx.send(WriteComplete { generation: packet.generation, sequence: packet.sequence, written: None, duration: Duration::ZERO }).await.context("write completion channel closed")?;
                continue;
            }
            let started = Instant::now();
            tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Binary(packet.bytes.into()))).await.context("media write timed out")??;
            let completed = Instant::now();
            write_tx.send(WriteComplete { generation: packet.generation, sequence: packet.sequence, written: Some(completed), duration: completed.saturating_duration_since(started) }).await.context("write completion channel closed")?;
        }
    });
    let mut encoder = Some(match app.mode {
        Some(mode) => StreamEncoder::Desktop(Encoder::new(mode, app.args.tile_size)?),
        None if app.args.codec == "hybrid" => {
            StreamEncoder::Hybrid(Box::new(gliss_hybrid::HybridEncoder::with_config(
                app.args.fps,
                app.args.h264_bitrate,
                app.hybrid_config,
            )?))
        }
        None if app.args.codec == "hybrid-grid" => {
            use gliss_hybrid_grid::{BaseMode, HybridGridConfig, HybridGridEncoder};
            StreamEncoder::HybridGrid(Box::new(HybridGridEncoder::with_config(
                app.hybrid_mask_grid.context("Missing admitted GLH2 grid")?,
                app.args.fps,
                app.args.h264_bitrate,
                HybridGridConfig {
                    base: match app.hybrid_config.base {
                        gliss_hybrid::BaseMode::Global => BaseMode::Global,
                        gliss_hybrid::BaseMode::GlobalCached => BaseMode::GlobalCached,
                    },
                    video: app.hybrid_config.video,
                },
            )?))
        }
        None if app.args.codec == "hybrid-continuity" => {
            use gliss_hybrid_continuity::{
                BaseMode, HybridContinuityConfig, HybridContinuityEncoder,
            };
            StreamEncoder::HybridContinuity(Box::new(HybridContinuityEncoder::with_config(
                app.hybrid_mask_grid.context("Missing admitted GLH4 grid")?,
                app.args.fps,
                app.args.h264_bitrate,
                HybridContinuityConfig {
                    base: match app.hybrid_config.base {
                        gliss_hybrid::BaseMode::Global => BaseMode::Global,
                        gliss_hybrid::BaseMode::GlobalCached => BaseMode::GlobalCached,
                    },
                    video: app.hybrid_config.video,
                },
            )?))
        }
        None => StreamEncoder::Video {
            encoder: None,
            timing: app.video_timing.context("Missing admitted H.264 timing")?,
            bitrate: app.args.h264_bitrate,
            options: gliss_video::VideoOptions {
                frame_skip: app.args.video_frame_skip,
                qp_bounds: configured_video_qp(&app.args)?,
            },
        },
    });
    let mut encoder_jobs = JoinSet::<Result<Encoded>>::new();
    let mut window = scheduler::Window::new(
        usize::from(app.args.in_flight_frames),
        app.args.in_flight_bytes,
    )?;
    let mut sequence = 0u32;
    let mut writer_jobs = 0usize;
    let mut native_skipped_frames = 0_u64;
    let mut waiting = None::<MediaPacket>;
    let mut latest = None::<Captured>;
    let mut pacing = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(app.args.fps)));
    pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut telemetry = tokio::time::interval(Duration::from_secs(1));
    telemetry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if window.resyncing() && writer_jobs == 0 && encoder_jobs.is_empty() {
            // Old queued writes have been skipped, an already-started write finished,
            // and any old encode returned. Reset before admitting new dependencies.
            encoder
                .as_mut()
                .context("encoder missing at generation barrier")?
                .reset();
            window.finish_resync()?;
            replace_latest(&mut latest, source.frames.borrow().clone(), &mut window);
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
            window.reserve(packet.sequence, packet.bytes.len(), Instant::now())?;
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
            deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86400)),
        );
        tokio::select! {
            Some(result) = io_tasks.join_next() => { result.context("stream I/O task failed")??; break; }
            Some(command) = commands.recv() => {
                match command {
                    StreamCommand::Ack(sequence) => { window.acknowledge(sequence, Instant::now())?; }
                    StreamCommand::Resync => {
                        window.begin_resync(sequence)?;
                        generation.store(window.generation(), Ordering::Release);
                        if waiting.take().is_some() { window.discarded_encode(); }
                    }
                    StreamCommand::Reply(message) => { control_tx.try_send(ControlPacket { message, flushed: None }).context("bounded control writer queue full or closed")?; }
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
                    if let Some(packet) = completed.packet? {
                        if app.args.video_frame_skip { sequence = completed.sequence; }
                        waiting = Some(MediaPacket { generation: completed.generation, sequence: completed.sequence, bytes: packet, blocked_since: None });
                    } else {
                        ensure!(app.args.video_frame_skip, "Unexpected empty media output");
                        native_skipped_frames = native_skipped_frames.saturating_add(1);
                        window.discarded_encode();
                    }
                }
            }
            changed = source.frames.changed() => {
                if changed.is_err() { break; }
                replace_latest(&mut latest, source.frames.borrow_and_update().clone(), &mut window);
            }
            Some(event) = source.events.recv() => {
                let failed = event.get("type").and_then(|value| value.as_str()) == Some("error");
                let (flushed, done) = if failed { let (tx, rx) = oneshot::channel(); (Some(tx), Some(rx)) } else { (None, None) };
                control_tx.try_send(ControlPacket { message: Message::Text(event.to_string().into()), flushed }).context("bounded control writer queue full or closed")?;
                if let Some(done) = done { let _ = tokio::time::timeout(Duration::from_secs(2), done).await; break; }
            }
            _ = pacing.tick(), if window.can_encode() && waiting.is_none() && encoder_jobs.is_empty() && latest.is_some() => {
                let captured = latest.take().unwrap();
                let mut next_encoder = encoder.take().context("encoder is already busy")?;
                let packet_sequence = sequence.wrapping_add(1);
                // A rate-control skip must not create a predictive packet gap.
                // Preserve legacy allocation when the opt-in policy is disabled.
                if !app.args.video_frame_skip { sequence = packet_sequence; }
                let packet_generation = window.generation();
                if let Some(recorder) = &app.benchmark {
                    recorder.associate(recorder.generation(&captured.frame)?, packet_generation, packet_sequence, "GLV1")?;
                }
                let permits = app.encoders.clone();
                let queued = Instant::now();
                let profile_encode = app.args.stream_stats;
                let encode_actor = stage_trace::lease(&app.stage_trace)?;
                let blocking_actor = stage_trace::lease(&app.stage_trace)?;
                encoder_jobs.spawn(async move {
                    let _trace_actor = encode_actor;
                    let permit = permits.acquire_owned().await?;
                    tokio::task::spawn_blocking(move || {
                        let _trace_actor = blocking_actor;
                        let _permit = permit;
                        let started = Instant::now();
                        let source_age = captured.published_at.map(|at| started.saturating_duration_since(at));
                        let cpu_started = profile_encode.then(profiling::thread_cpu_time).flatten();
                        let packet = next_encoder.encode_optional(&captured.frame, packet_sequence, captured.forwarded_input_id);
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
                if app.args.video_frame_skip { stats["native_skipped_frames"] = json!(native_skipped_frames); }
                // Diagnostics never block input/media and have no unbounded history.
                let _ = control_tx.try_send(ControlPacket { message: Message::Text(stats.to_string().into()), flushed: None });
            }
            _ = tokio::time::sleep_until(sleep_deadline), if deadline.is_some() => { bail!("browser did not acknowledge oldest frame within 10s"); }
        }
    }
    Ok(())
}

enum StreamEncoder {
    Desktop(Encoder),
    Hybrid(Box<gliss_hybrid::HybridEncoder>),
    HybridGrid(Box<gliss_hybrid_grid::HybridGridEncoder>),
    HybridContinuity(Box<gliss_hybrid_continuity::HybridContinuityEncoder>),
    Video {
        encoder: Option<Box<VideoEncoder>>,
        timing: VideoTiming,
        bitrate: u32,
        options: gliss_video::VideoOptions,
    },
}

impl StreamEncoder {
    fn reset(&mut self) {
        match self {
            Self::Desktop(encoder) => encoder.reset(),
            Self::Hybrid(encoder) => encoder.reset(),
            Self::HybridGrid(encoder) => encoder.reset(),
            Self::HybridContinuity(encoder) => encoder.reset(),
            Self::Video {
                encoder: Some(encoder),
                ..
            } => encoder.reset(),
            _ => {}
        }
    }

    fn encode(&mut self, frame: &Frame, sequence: u32, input_id: u32) -> Result<Vec<u8>> {
        self.encode_optional(frame, sequence, input_id)?
            .context("Unexpected skipped output in a strict media path")
    }

    fn encode_optional(
        &mut self,
        frame: &Frame,
        sequence: u32,
        input_id: u32,
    ) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Desktop(encoder) => encoder.encode(frame, sequence, input_id).map(Some),
            Self::Hybrid(encoder) => encoder.encode(frame, sequence, input_id).map(Some),
            Self::HybridGrid(encoder) => encoder.encode(frame, sequence, input_id).map(Some),
            Self::HybridContinuity(encoder) => encoder.encode(frame, sequence, input_id).map(Some),
            Self::Video {
                encoder,
                timing,
                bitrate,
                options,
            } => {
                if encoder.is_none() {
                    *encoder = Some(Box::new(VideoEncoder::with_options(
                        frame.width,
                        frame.height,
                        *bitrate,
                        VideoProfile::Screen,
                        *timing,
                        *options,
                    )?));
                }
                encoder
                    .as_mut()
                    .unwrap()
                    .encode_optional(frame, sequence, input_id)
            }
        }
    }
}

#[cfg(test)]
mod causal_clock_request_tests {
    use super::StageClockRequest;

    #[test]
    fn request_has_only_a_bounded_nonce_and_ordinal() {
        for ordinal in [1, 64] {
            assert!(
                StageClockRequest {
                    nonce: "ab".repeat(16),
                    ordinal
                }
                .valid()
            );
        }
        for (nonce, ordinal) in [
            ("ab".repeat(15), 1),
            ("ab".repeat(17), 1),
            ("AB".repeat(16), 1),
            ("gg".repeat(16), 1),
            ("ab".repeat(16), 0),
            ("ab".repeat(16), 65),
        ] {
            assert!(!StageClockRequest { nonce, ordinal }.valid());
        }
        assert!(
            serde_json::from_str::<StageClockRequest>(
                r#"{"nonce":"abababababababababababababababab","ordinal":1,"extra":0}"#
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod video_qp_option_tests {
    use super::*;
    fn parse(extra: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(
            [
                "gliss-server",
                "--demo",
                "--token",
                "qp-test-token-0123456789abcdef",
            ]
            .into_iter()
            .chain(extra.iter().copied()),
        )
    }
    #[test]
    fn qp_bounds_are_paired_opt_in_and_scoped_before_startup() {
        let default = parse(&["--codec", "h264"]).unwrap();
        assert_eq!(configured_video_qp(&default).unwrap(), None);
        assert!(
            with_video_metadata(
                json!({}),
                &default,
                configured_video_timing(&default).unwrap()
            )
            .get("video_qp_bounds")
            .is_none()
        );
        let selected = parse(&[
            "--codec",
            "h264",
            "--video-qp-min",
            "26",
            "--video-qp-max",
            "51",
        ])
        .unwrap();
        assert!(configured_video_timing(&selected).is_ok());
        let qp = configured_video_qp(&selected).unwrap().unwrap();
        assert_eq!((qp.min(), qp.max()), (26, 51));
        let hello = with_video_metadata(
            json!({}),
            &selected,
            configured_video_timing(&selected).unwrap(),
        );
        assert_eq!(hello["video_qp_bounds"]["min"], 26);
        assert_eq!(hello["video_qp_bounds"]["max"], 51);
        for extra in [
            vec!["--codec", "h264", "--video-qp-min", "26"],
            vec!["--codec", "h264", "--video-qp-max", "51"],
            vec![
                "--codec",
                "h264",
                "--video-qp-min",
                "0",
                "--video-qp-max",
                "51",
            ],
            vec![
                "--codec",
                "h264",
                "--video-qp-min",
                "26",
                "--video-qp-max",
                "52",
            ],
        ] {
            assert!(parse(&extra).is_err());
        }
        for extra in [
            vec!["--codec", "deflate"],
            vec!["--codec", "h264", "--datagram-bind", "127.0.0.1:0"],
            vec![
                "--codec",
                "h264",
                "--benchmark-capture-dir",
                "unused-test-directory",
            ],
        ] {
            let mut args = extra;
            args.extend(["--video-qp-min", "26", "--video-qp-max", "51"]);
            assert!(configured_video_timing(&parse(&args).unwrap()).is_err());
        }
        assert!(
            configured_video_timing(
                &parse(&[
                    "--codec",
                    "h264",
                    "--video-qp-min",
                    "51",
                    "--video-qp-max",
                    "26"
                ])
                .unwrap()
            )
            .is_err()
        );
    }
}
