//! Experimental, authenticated QUIC DATAGRAM video. Reliable QUIC streams carry
//! control; media is never retransmitted by the application. The default route
//! requires GLV1 IDRs. Explicit GOP4 uses bounded dependency-aware recovery.
use super::*;
use std::collections::HashMap;
use tokio::sync::OwnedSemaphorePermit;
use wtransport::endpoint::{IncomingSession, endpoint_side};
use wtransport::{Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig, VarInt};
mod predictive;
mod tiles;

const TICKET_TTL: Duration = Duration::from_secs(5);
const MAX_TICKETS: usize = 32;
const MAX_HANDSHAKES: usize = 32;
const MAX_CONTROL: usize = MAX_CLIPBOARD + 1024;
const MAX_PACKET: usize = 16 * 1024 * 1024 + 24;
const DATAGRAM_HEADER: usize = 20;
const DATAGRAM_PAYLOAD: usize = 1000;
// Maximum QUIC variable integer wrapping the WebTransport session ID.
const SESSION_HEADER_BUDGET: usize = 8;
// Pinned Quinn 0.11.19 also charges size_of::<frame::Datagram>() for each
// queued Bytes wrapper. 128 bytes conservatively covers that metadata on the
// supported x86_64 target; it is memory admission, never wire-byte accounting.
const QUINN_DATAGRAM_METADATA_BUDGET: usize = 128;
const SEND_BUFFER: usize = 1024 * 1024;
const MIN_SEND_BUFFER: usize = 8192;
const _: () = assert!(DATAGRAM_HEADER == crate::xor_fec::DATAGRAM_HEADER_BYTES);

pub(super) fn validate_send_buffer(bytes: usize) -> Result<()> {
    ensure!(
        (MIN_SEND_BUFFER..=SEND_BUFFER).contains(&bytes),
        "datagram-send-buffer-bytes must be8192..1048576"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(super) enum DatagramCodec {
    #[default]
    H264,
    H264PredictiveGop4,
    H264PredictiveGop4Head100,
    H264PredictiveGop4Compact100,
    H264PredictiveGop4Compact50,
    H264PredictiveGop4Compact50Xor8,
    Glt2Lossless,
    Glt2Gentle,
    Glt2Strong,
}

impl DatagramCodec {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H264PredictiveGop4 => "h264-predictive-gop4",
            Self::H264PredictiveGop4Head100 => "h264-predictive-gop4-head100",
            Self::H264PredictiveGop4Compact100 => "h264-predictive-gop4-compact100",
            Self::H264PredictiveGop4Compact50 => "h264-predictive-gop4-compact50",
            Self::H264PredictiveGop4Compact50Xor8 => "h264-predictive-gop4-compact50-xor8",
            Self::Glt2Lossless => "glt2-lossless",
            Self::Glt2Gentle => "glt2-gentle",
            Self::Glt2Strong => "glt2-strong",
        }
    }
    pub(super) fn policy(self) -> Option<u8> {
        match self {
            Self::H264
            | Self::H264PredictiveGop4
            | Self::H264PredictiveGop4Head100
            | Self::H264PredictiveGop4Compact100
            | Self::H264PredictiveGop4Compact50
            | Self::H264PredictiveGop4Compact50Xor8 => None,
            Self::Glt2Lossless => Some(0),
            Self::Glt2Gentle => Some(1),
            Self::Glt2Strong => Some(2),
        }
    }
    pub(super) fn protocol(self) -> &'static str {
        if self.policy().is_none() {
            "GLV1"
        } else {
            "GLT2-WT1-negotiated-stride"
        }
    }
    pub(super) fn predictive(self) -> bool {
        matches!(
            self,
            Self::H264PredictiveGop4
                | Self::H264PredictiveGop4Head100
                | Self::H264PredictiveGop4Compact100
                | Self::H264PredictiveGop4Compact50
                | Self::H264PredictiveGop4Compact50Xor8
        )
    }
    fn compact_head(self) -> bool {
        matches!(
            self,
            Self::H264PredictiveGop4Compact100
                | Self::H264PredictiveGop4Compact50
                | Self::H264PredictiveGop4Compact50Xor8
        )
    }
    fn xor_fec(self) -> bool {
        self == Self::H264PredictiveGop4Compact50Xor8
    }
    fn head_interval_ms(self) -> u64 {
        match self {
            Self::H264PredictiveGop4Compact50 | Self::H264PredictiveGop4Compact50Xor8 => 50,
            Self::H264PredictiveGop4Head100 | Self::H264PredictiveGop4Compact100 => 100,
            _ => 1000,
        }
    }
    fn statistics_interval_ms(self) -> u64 {
        if self == Self::H264PredictiveGop4Head100 {
            100
        } else {
            1000
        }
    }
    pub(super) fn decorate_video(self, value: &mut serde_json::Value) {
        if self.predictive() {
            let object = value.as_object_mut().unwrap();
            for (key, value) in json!({"media":"GLD1/GLV1-predictive-v1",
                "datagram_codec":self.name(),"protocol":"GLV1","datagram_protocol":"GLD1",
                "independent_frames":false,"reference_policy":"contiguous-since-idr",
                "gop_frames":4,"maximum_access_unit_bytes":1048576,
                "encoded_head_interval_ms":self.head_interval_ms(),"encoded_head_deadline_ms":250,
                "recovery_interval_ms":1000,"media_retransmission":false})
            .as_object()
            .unwrap()
            {
                object.insert(key.clone(), value.clone());
            }
            if self.compact_head() {
                object.insert(
                    "encoded_head_notification".into(),
                    json!("on-change-coalesced-v1"),
                );
                object.insert("statistics_interval_ms".into(), json!(1000));
                object.insert("encoded_head_max_body_bytes".into(), json!(128));
            }
            if self.xor_fec() {
                for (key, value) in json!({"fec":"xor-grouped-v1",
                    "fec_scope":"all-access-units","fec_data_fragments_per_group":8,
                    "fec_parity_fragments_per_group":1,"parity_datagram_protocol":"GLP1"})
                .as_object()
                .unwrap()
                {
                    object.insert(key.clone(), value.clone());
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(super) enum CongestionControl {
    #[default]
    Cubic,
    Bbr,
}

impl CongestionControl {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Cubic => "cubic",
            Self::Bbr => "bbr",
        }
    }

    fn factory(self) -> Arc<dyn wtransport::quinn::congestion::ControllerFactory + Send + Sync> {
        use wtransport::quinn::congestion::{BbrConfig, CubicConfig};
        match self {
            Self::Cubic => Arc::new(CubicConfig::default()),
            Self::Bbr => Arc::new(BbrConfig::default()),
        }
    }
}

struct Ticket {
    origin: String,
    cookie_expiry: u64,
    expires: tokio::time::Instant,
    _permit: OwnedSemaphorePermit,
    _media_budget: OwnedSemaphorePermit,
}

struct RegisteredConnection<'a> {
    connections: &'a Mutex<HashMap<usize, Connection>>,
    id: usize,
}

impl Drop for RegisteredConnection<'_> {
    fn drop(&mut self) {
        self.connections
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.id);
    }
}

pub(super) struct DatagramRuntime {
    endpoint: Endpoint<endpoint_side::Server>,
    address: SocketAddr,
    fingerprint: [u8; 32],
    advertise_port: Option<u16>,
    public_host: Option<String>,
    send_buffer: usize,
    congestion_control: CongestionControl,
    codec: DatagramCodec,
    tickets: Mutex<HashMap<String, Ticket>>,
    handshakes: Arc<Semaphore>,
    media_clients: Arc<Semaphore>,
    revoked: Arc<Mutex<bool>>,
    revocation_notice: watch::Sender<bool>,
    connections: Mutex<HashMap<usize, Connection>>,
}

impl DatagramRuntime {
    #[cfg(test)]
    pub(super) fn bind(
        address: SocketAddr,
        advertise_port: Option<u16>,
        congestion_control: CongestionControl,
    ) -> Result<Arc<Self>> {
        Self::bind_codec(
            address,
            advertise_port,
            congestion_control,
            DatagramCodec::H264,
        )
    }

    #[cfg(test)]
    pub(super) fn bind_codec(
        address: SocketAddr,
        advertise_port: Option<u16>,
        congestion_control: CongestionControl,
        codec: DatagramCodec,
    ) -> Result<Arc<Self>> {
        Self::bind_configured(
            address,
            advertise_port,
            congestion_control,
            codec,
            None,
            None,
        )
    }

    pub(super) fn bind_configured(
        address: SocketAddr,
        advertise_port: Option<u16>,
        congestion_control: CongestionControl,
        codec: DatagramCodec,
        public_host: Option<String>,
        send_buffer: Option<usize>,
    ) -> Result<Arc<Self>> {
        ensure!(
            address.ip().is_loopback() || public_host.is_some(),
            "nonloopback datagrams require an explicitly configured public host"
        );
        let public_host = public_host
            .map(|host| parse_datagram_public_host(&host).map_err(anyhow::Error::msg))
            .transpose()?;
        let send_buffer = send_buffer.unwrap_or(SEND_BUFFER);
        validate_send_buffer(send_buffer)?;
        let identity = match &public_host {
            Some(host) => Identity::self_signed([host.as_str()])?,
            None => Identity::self_signed(["localhost", "127.0.0.1", "::1"])?,
        };
        let fingerprint = *identity.certificate_chain().as_slice()[0].hash().as_ref();
        let mut transport = wtransport::quinn::TransportConfig::default();
        transport.congestion_controller_factory(congestion_control.factory());
        // Quinn's nonblocking send_datagram discards oldest unsent datagrams
        // when this queue fills. No send_datagram_wait or application retry.
        transport.datagram_send_buffer_size(send_buffer);
        transport.datagram_receive_buffer_size(Some(4096));
        transport.max_concurrent_bidi_streams(4u8.into());
        transport.max_concurrent_uni_streams(4u8.into());
        transport.receive_window((2 * MAX_CONTROL as u32).into());
        transport.stream_receive_window((MAX_CONTROL as u32).into());
        let config = ServerConfig::builder()
            .with_bind_address(address)
            .with_custom_transport(identity, transport)
            .max_idle_timeout(Some(Duration::from_secs(15)))?
            .keep_alive_interval(Some(Duration::from_secs(3)))
            .allow_migration(false)
            .build();
        let endpoint = Endpoint::server(config)?;
        let address = endpoint.local_addr()?;
        Ok(Arc::new(Self {
            endpoint,
            address,
            fingerprint,
            advertise_port,
            public_host,
            send_buffer,
            congestion_control,
            codec,
            tickets: Mutex::new(HashMap::new()),
            handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
            media_clients: Arc::new(Semaphore::new(32)),
            revoked: Arc::new(Mutex::new(false)),
            revocation_notice: watch::channel(false).0,
            connections: Mutex::new(HashMap::new()),
        }))
    }

    pub(super) fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub(super) fn congestion_control(&self) -> &'static str {
        self.congestion_control.name()
    }

    pub(super) fn codec(&self) -> DatagramCodec {
        self.codec
    }

    pub(super) fn advertised_origin(&self) -> String {
        let port = self.advertise_port.unwrap_or(self.address.port());
        match &self.public_host {
            Some(host) if host.contains(':') => format!("https://[{host}]:{port}"),
            Some(host) => format!("https://{host}:{port}"),
            None => format!("https://127.0.0.1:{port}"),
        }
    }

    fn ticket_path(&self) -> &'static str {
        if self.public_host.is_some() {
            "/connect?ticket="
        } else {
            "/display?ticket="
        }
    }

    pub(super) fn revoke(&self) {
        // Match mint/admit/control lock ordering. No ticket can be inserted,
        // consumed or input admitted after this irreversible transition.
        let mut revoked = self
            .revoked
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *revoked = true;
        self.tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.revocation_notice.send_replace(true);
        for (_, connection) in self
            .connections
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain()
        {
            connection.close(VarInt::from_u32(0), b"display authorization revoked");
        }
        // The serving actor drops its pending accept before closing Endpoint:
        // wtransport 0.7.2 accept() panics if Endpoint is closed concurrently.
    }

    pub(super) fn mint_ticket(&self, app: &App, headers: &HeaderMap) -> Response {
        let revoked = self
            .revoked
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *revoked {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if !origin_allowed(app, headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let Some(cookie_expiry) = authorized_until(app, headers) else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        let current = tokio::time::Instant::now();
        if app
            .source_deadline
            .is_some_and(|deadline| deadline <= current)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let mut tickets = self.tickets.lock().unwrap();
        tickets.retain(|_, ticket| ticket.expires > current);
        if tickets.len() >= MAX_TICKETS {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let Ok(permit) = app.clients.clone().try_acquire_owned() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let Ok(media_budget) = self.media_clients.clone().try_acquire_owned() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let token = hex::encode(rand::random::<[u8; 32]>());
        let origin = headers
            .get(header::ORIGIN)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        tickets.insert(
            token.clone(),
            Ticket {
                origin,
                cookie_expiry,
                expires: current + TICKET_TTL,
                _permit: permit,
                _media_budget: media_budget,
            },
        );
        let mut body = json!({
            "transport":"webtransport-datagram", "url":format!("{}{}{token}",self.advertised_origin(),self.ticket_path()),
            "certificate_sha256":self.fingerprint, "ticket_ttl_ms":TICKET_TTL.as_millis(),
            "media":"GLD1/GLV1-all-IDR",
            "send_queue_bytes":self.send_buffer,
            "congestion_control":self.congestion_control(),
        });
        if app.args.datagram_display_input_only {
            body["display_input_only"] = json!(true);
        }
        if let Some(policy) = self.codec.policy() {
            let object = body.as_object_mut().unwrap();
            object.insert(
                "tile_send_policy".into(),
                json!(app.args.tile_send_policy.unwrap_or_default().name()),
            );
            object.insert(
                "tile_send_quantum".into(),
                json!(crate::tile_quantum::selected_name(
                    app.args.tile_send_quantum
                )),
            );
            object.insert(
                "tile_quantum_configuration".into(),
                crate::tile_quantum::configuration(app.args.tile_send_quantum),
            );
            for (key, value) in json!({"media":"GLT2-independent-tiles","codec":self.codec.name(),
                "datagram_codec":self.codec.name(),"protocol":self.codec.protocol(),"datagram_protocol":"GLT2",
                "policy":policy,"payload_stride_range":[1024,1168],"max_datagram_bytes":1200,
                "tile_side":32,"initial_epoch":1,"independent_tiles":true,"media_retransmission":false})
                .as_object().unwrap() {
                object.insert(key.clone(),value.clone());
            }
        }
        self.codec.decorate_video(&mut body);
        let mut response = Json(body).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        response
    }

    pub(super) async fn serve(self: Arc<Self>, app: Arc<App>) -> Result<()> {
        let mut stopped = app.worker_shutdown.subscribe();
        let mut revoked = self.revocation_notice.subscribe();
        let mut tasks = JoinSet::new();
        let mut sweep = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = revoked.wait_for(|value| *value) => break,
                _ = stopped.wait_for(|value| *value) => break,
                _ = sweep.tick() => {
                    let current = tokio::time::Instant::now();
                    self.tickets.lock().unwrap().retain(|_,ticket| ticket.expires > current);
                }
                Some(_) = tasks.join_next() => {},
                incoming = self.endpoint.accept() => {
                    let Ok(permit) = self.handshakes.clone().try_acquire_owned() else { continue; };
                    let runtime = self.clone();
                    let app = app.clone();
                    let trace_actor = crate::stage_trace::lease(&app.stage_trace)?;
                    tasks.spawn(async move {
                        let _trace_actor = trace_actor;
                        let result = runtime.accept(incoming, app, permit).await;
                        if let Err(error) = result {
                            // Never log an incoming path: it contains a one-use credential.
                            tracing::debug!("datagram session ended: {error:#}");
                        }
                    });
                }
            }
        }
        self.tickets.lock().unwrap().clear();
        self.endpoint.close(VarInt::from_u32(0), b"worker stopped");
        // Give active sessions time to release held input before aborting only
        // stalled handshakes or setup tasks. The endpoint has stopped accepting.
        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            if let Some(trace) = &app.stage_trace {
                trace.failure(1);
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        Ok(())
    }

    async fn accept(
        &self,
        incoming: IncomingSession,
        app: Arc<App>,
        handshake: OwnedSemaphorePermit,
    ) -> Result<()> {
        let request = tokio::time::timeout(Duration::from_secs(5), incoming)
            .await
            .context("QUIC handshake deadline")??;
        let ticket = {
            let revoked = self
                .revoked
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let token = request
                .path()
                .strip_prefix(self.ticket_path())
                .filter(|token| {
                    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
            if *revoked {
                None
            } else {
                token.and_then(|token| self.tickets.lock().unwrap().remove(token))
            }
        };
        let Some(ticket) = ticket else {
            request.forbidden().await;
            return Ok(());
        };
        if ticket.expires <= tokio::time::Instant::now()
            || request.origin() != Some(ticket.origin.as_str())
            || (self.public_host.is_none() && !request.remote_address().ip().is_loopback())
            || session_remaining_at(ticket.cookie_expiry, SystemTime::now()).is_zero()
            || app
                .source_deadline
                .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            request.forbidden().await;
            return Ok(());
        }
        let connection = tokio::time::timeout(Duration::from_secs(5), request.accept())
            .await
            .context("WebTransport accept deadline")??;
        drop(handshake);
        {
            let revoked = self
                .revoked
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *revoked {
                connection.close(VarInt::from_u32(0), b"display authorization revoked");
                return Ok(());
            }
            self.connections
                .lock()
                .unwrap()
                .insert(connection.stable_id(), connection.clone());
        }
        let _registration = RegisteredConnection {
            connections: &self.connections,
            id: connection.stable_id(),
        };
        let deadline = tokio::time::Instant::now()
            + session_remaining_at(ticket.cookie_expiry, SystemTime::now());
        let deadline = app
            .source_deadline
            .map_or(deadline, |source| source.min(deadline));
        if deadline <= tokio::time::Instant::now() {
            connection.close(VarInt::from_u32(0), b"display lease expired");
            bail!("datagram lease expired before control setup");
        }
        let session_trace = app.stage_trace.clone();
        let result = run_connection(&connection, app, deadline, self.revoked.clone()).await;
        if let Err(error) = &result
            && error.downcast_ref::<crate::WorkerStopped>().is_none()
            && let Some(trace) = &session_trace
        {
            trace.failure(4);
        }
        connection.close(VarInt::from_u32(0), b"display session ended");
        drop(ticket);
        result
    }
}

fn queued_frame_bytes(bytes: usize, fragments: usize) -> usize {
    bytes.saturating_add(
        fragments.saturating_mul(
            DATAGRAM_HEADER + SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET,
        ),
    )
}

#[cfg(test)]
fn frame_fits_send_admission(
    encoded_bytes: usize,
    queued_bytes: usize,
    datagrams: Option<usize>,
    predictive: bool,
) -> bool {
    frame_fits_send_budget(
        encoded_bytes,
        queued_bytes,
        datagrams,
        predictive,
        SEND_BUFFER,
    )
}

fn frame_fits_send_budget(
    encoded_bytes: usize,
    queued_bytes: usize,
    datagrams: Option<usize>,
    predictive: bool,
    send_buffer: usize,
) -> bool {
    queued_bytes <= send_buffer
        && datagrams.is_none_or(|count| count <= crate::xor_fec::BROWSER_DATAGRAM_HIGH_WATER_MARK)
        && (!predictive || encoded_bytes <= 1048576)
}

async fn write_control(stream: &mut SendStream, value: &serde_json::Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_CONTROL, "control message too large");
    tokio::time::timeout(Duration::from_secs(2), async {
        stream
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .await?;
        stream.write_all(&bytes).await
    })
    .await
    .context("control write deadline")??;
    Ok(())
}

async fn read_control(stream: &mut RecvStream) -> Result<serde_json::Value> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(
        (1..=MAX_CONTROL).contains(&length),
        "control message size out of bounds"
    );
    let mut bytes = vec![0u8; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

struct Completed {
    encoder: StreamEncoder,
    bytes: Result<Vec<u8>>,
    elapsed: Duration,
    reset: bool,
}

async fn run_connection(
    connection: &Connection,
    app: Arc<App>,
    deadline: tokio::time::Instant,
    revoked: Arc<Mutex<bool>>,
) -> Result<()> {
    let mut stopped = app.worker_shutdown.subscribe();
    ensure!(
        deadline > tokio::time::Instant::now(),
        "datagram lease expired before control setup"
    );
    let (mut outgoing, incoming) = tokio::time::timeout_at(
        deadline.min(tokio::time::Instant::now() + Duration::from_secs(5)),
        connection.accept_bi(),
    )
    .await
    .context("missing control stream")??;
    ensure!(
        deadline > tokio::time::Instant::now(),
        "datagram lease expired before source claim"
    );
    let mut source = tokio::select! {
        biased;
        _ = stopped.wait_for(|value| *value) => return Err(crate::WorkerStopped("worker stopped during datagram setup").into()),
        result = tokio::time::timeout_at(deadline, source(&app)) => result.context("datagram source setup lease expired")??,
    };
    let (control_tx, mut controls) = mpsc::channel(16);
    let mut input_tasks = JoinSet::new();
    let input = source.input.clone();
    let tile_mode = app
        .args
        .datagram_codec
        .unwrap_or_default()
        .policy()
        .is_some();
    let input_trace = app.stage_trace.clone();
    let scope = InputScope {
        tile_mode,
        display_input_only: app.args.datagram_display_input_only,
    };
    let input_actor = crate::stage_trace::lease(&input_trace)?;
    input_tasks.spawn(async move {
        let _trace_actor = input_actor;
        forward_control_traced(incoming, input, control_tx, scope, input_trace, revoked).await
    });
    let result = tokio::select! {
        biased;
        _ = stopped.wait_for(|value| *value) => Ok(()),
        result = tokio::time::timeout_at(deadline, async {
            if tile_mode { tiles::run_media(connection,&app,&mut source,&mut outgoing,&mut controls).await }
            else { run_media(connection,&app,&mut source,&mut outgoing,&mut controls).await }
        }) =>
            result.context("datagram session lease expired").and_then(|result| result),
        Some(result) = input_tasks.join_next() => result.context("datagram input task failed").and_then(|result|result),
        _ = connection.closed() => Ok(()),
    };
    connection.close(VarInt::from_u32(0), b"display session ended");
    // Reap the independent receiver before dropping the last source input
    // sender. Otherwise its clone could keep held guest keys/buttons alive.
    input_tasks.abort_all();
    while input_tasks.join_next().await.is_some() {}
    // Match the WebSocket source lifecycle: input task releases held keys and
    // buttons before the private source task and shared-capture lease are dropped.
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
    drop(source.capture_lease.take());
    result
}

async fn forward_control_traced(
    mut incoming: RecvStream,
    input: mpsc::Sender<Input>,
    controls: mpsc::Sender<serde_json::Value>,
    scope: InputScope,
    trace: crate::stage_trace::Handle,
    revoked: Arc<Mutex<bool>>,
) -> Result<()> {
    // This owned task keeps partial reliable reads across media/control writes.
    // Guest input bypasses the outgoing-control queue and its backpressure.
    loop {
        route_control_traced(
            read_control(&mut incoming).await?,
            &input,
            &controls,
            scope,
            &trace,
            &revoked,
        )?;
    }
}

#[cfg(test)]
fn route_control(
    value: serde_json::Value,
    input: &mpsc::Sender<Input>,
    controls: &mpsc::Sender<serde_json::Value>,
) -> Result<()> {
    route_control_for(value, input, controls, false)
}

#[cfg(test)]
fn route_control_for(
    value: serde_json::Value,
    input: &mpsc::Sender<Input>,
    controls: &mpsc::Sender<serde_json::Value>,
    tile_mode: bool,
) -> Result<()> {
    route_control_traced(
        value,
        input,
        controls,
        InputScope {
            tile_mode,
            display_input_only: false,
        },
        &None,
        &Mutex::new(false),
    )
}

#[derive(Clone, Copy)]
struct InputScope {
    tile_mode: bool,
    display_input_only: bool,
}

fn route_control_traced(
    value: serde_json::Value,
    input: &mpsc::Sender<Input>,
    controls: &mpsc::Sender<serde_json::Value>,
    scope: InputScope,
    trace: &crate::stage_trace::Handle,
    revoked: &Mutex<bool>,
) -> Result<()> {
    // Hold through nonblocking admission, serializing it against revoke().
    let revoked = revoked
        .lock()
        .map_err(|_| anyhow::anyhow!("datagram authorization gate poisoned"))?;
    ensure!(!*revoked, "datagram authorization revoked");
    match value.get("type").and_then(|value| value.as_str()) {
        Some("ack") => Ok(()),
        Some("resync") => controls
            .try_send(json!({"type":"resync"}))
            .context("control reply queue full or closed"),
        Some("ping") => controls
            .try_send(json!({"type":"pong","id":value.get("id")}))
            .context("control reply queue full or closed"),
        _ => {
            let command: Input = serde_json::from_value(value)?;
            ensure!(
                !scope.display_input_only
                    || !matches!(command, Input::Clipboard { .. } | Input::Resize { .. }),
                "datagram input capability rejected"
            );
            crate::stage_trace::emit(trace, crate::stage_trace::Kind::ControlReceived, || {
                crate::stage_trace::Fields {
                    input_id: Some(u64::from(command.id())),
                    ..crate::stage_trace::Fields::default()
                }
            });
            if let Input::Clipboard { text, .. } = &command {
                if text.len() > MAX_CLIPBOARD {
                    crate::stage_trace::emit(
                        trace,
                        crate::stage_trace::Kind::InputRejected,
                        || crate::stage_trace::Fields {
                            input_id: Some(u64::from(command.id())),
                            outcome: crate::stage_trace::Outcome::Rejected,
                            ..crate::stage_trace::Fields::default()
                        },
                    );
                }
                ensure!(text.len() <= MAX_CLIPBOARD, "clipboard too large");
            }
            if matches!(command,Input::Resize {width,height,..} if width<16||height<16||width>3840||height>2160||(!scope.tile_mode&&(width%2!=0||height%2!=0)))
            {
                crate::stage_trace::emit(trace, crate::stage_trace::Kind::InputRejected, || {
                    crate::stage_trace::Fields {
                        input_id: Some(u64::from(command.id())),
                        outcome: crate::stage_trace::Outcome::Rejected,
                        ..crate::stage_trace::Fields::default()
                    }
                });
                controls.try_send(json!({"type":"input_error","message":"Software H264 requires even desktop dimensions from 16x16 through 3840x2160."}))
                    .context("control reply queue full or closed")
            } else {
                let id = command.id();
                let sent = input.try_send(command);
                crate::stage_trace::emit(
                    trace,
                    if sent.is_ok() {
                        crate::stage_trace::Kind::InputAdmitted
                    } else {
                        crate::stage_trace::Kind::InputRejected
                    },
                    || crate::stage_trace::Fields {
                        input_id: Some(u64::from(id)),
                        outcome: match &sent {
                            Ok(()) => crate::stage_trace::Outcome::Accepted,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                crate::stage_trace::Outcome::Full
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                crate::stage_trace::Outcome::Error
                            }
                        },
                        ..crate::stage_trace::Fields::default()
                    },
                );
                sent.context("input queue full or closed")
            }
        }
    }
}

// One coalesced progress watermark; no per-sequence queue or idle timer.
#[derive(Default)]
struct CompactHead {
    last_written: Option<u32>,
    not_before: Option<tokio::time::Instant>,
    messages_written: u64,
    application_bytes_written: u64,
}
impl CompactHead {
    fn pending(&self, latest: Option<u32>) -> bool {
        latest.is_some() && latest != self.last_written
    }
    async fn wait(&self) {
        if let Some(deadline) = self.not_before {
            tokio::time::sleep_until(deadline).await;
        }
    }
    fn written(
        &mut self,
        sequence: u32,
        body_bytes: usize,
        now: tokio::time::Instant,
        interval_ms: u64,
    ) {
        self.last_written = Some(sequence);
        self.not_before = Some(now + Duration::from_millis(interval_ms));
        self.messages_written = self.messages_written.saturating_add(1);
        self.application_bytes_written = self
            .application_bytes_written
            .saturating_add(body_bytes as u64 + 4);
    }
}
fn compact_head_bytes(sequence: u32, interval_ms: u64) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&json!({"type":"encoded_head",
        "latest_encoded_sequence":sequence,"encoded_head_interval_ms":interval_ms}))?;
    ensure!(bytes.len() <= 128, "compact head body exceeds 128 bytes");
    Ok(bytes)
}
async fn write_compact_head(
    stream: &mut SendStream,
    sequence: u32,
    interval_ms: u64,
) -> Result<usize> {
    let bytes = compact_head_bytes(sequence, interval_ms)?;
    // Same framing/deadline as existing authenticated reliable control writes.
    tokio::time::timeout(Duration::from_secs(2), async {
        stream
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .await?;
        stream.write_all(&bytes).await
    })
    .await
    .context("compact head write deadline")??;
    Ok(bytes.len())
}

async fn run_media(
    connection: &Connection,
    app: &Arc<App>,
    source: &mut Source,
    outgoing: &mut SendStream,
    controls: &mut mpsc::Receiver<serde_json::Value>,
) -> Result<()> {
    ensure!(app.args.codec == "h264", "datagram requires H264");
    let timing = app.video_timing.context("missing datagram video policy")?;
    let codec = app.args.datagram_codec.unwrap_or_default();
    let predictive = codec.predictive();
    let xor_enabled = codec.xor_fec();
    if xor_enabled {
        let fixed_bytes = DATAGRAM_HEADER + SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET;
        ensure!(
            crate::xor_fec::admitted_datagram_upper_bound(
                app.datagram.as_ref().unwrap().send_buffer,
                256,
                fixed_bytes,
            ) < crate::xor_fec::BROWSER_DATAGRAM_HIGH_WATER_MARK,
            "XOR send admission exceeds browser datagram high-water mark"
        );
    }
    ensure!(
        (!predictive && timing.intra_period() == 1) || (predictive && timing.intra_period() == 4),
        "datagram requires independently decodable frames"
    );
    ensure!(
        !predictive || (app.args.stream_stats && app.args.max_clients == 1),
        "predictive media requires bounded head telemetry and one viewer"
    );
    let congestion_control = app
        .datagram
        .as_ref()
        .context("missing admitted datagram endpoint")?
        .congestion_control();
    let send_buffer = app.datagram.as_ref().unwrap().send_buffer;
    let mut hello = json!({
        "type":"hello", "codec":"h264", "protocol":"GLV1",
        "transport":"webtransport-datagram", "datagram_protocol":"GLD1",
        "media_retransmission":false, "independent_frames":true,
        "congestion_control":congestion_control,
        "fps_limit":app.args.fps,
        "source":if app.args.demo {"demo"} else if app.args.dbus_address.is_some() {"dbus"} else {"rfb"},
        "input_metadata":"latest forwarded input; not proof of causal display",
    });
    codec.decorate_video(&mut hello);
    write_control(outgoing, &with_stream_metadata(hello, app)).await?;
    let mut encoder = Some(StreamEncoder::Video {
        frame_skip: false,
        encoder: None,
        timing,
        bitrate: app.args.h264_bitrate,
    });
    let mut jobs = JoinSet::<Result<Completed>>::new();
    let mut latest = source.frames.borrow().clone();
    let mut sequence = 0u32;
    let mut pacing = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(app.args.fps)));
    pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut telemetry =
        tokio::time::interval(Duration::from_millis(codec.statistics_interval_ms()));
    let mut compact_head = CompactHead::default();
    let mut encoded = 0u64;
    let mut datagrams = 0u64;
    let mut original_datagrams_enqueued = 0u64;
    let mut original_application_bytes_enqueued = 0u64;
    let mut parity_datagrams_enqueued = 0u64;
    let mut parity_application_bytes_enqueued = 0u64;
    let mut xor_groups_enqueued = 0u64;
    let mut xor_payload_bytes_read = 0u64;
    let mut xor_scratch_bytes_allocated_total = 0u64;
    let mut xor_scratch_bytes_peak = 0u64;
    let mut encode_ns = 0u64;
    let mut oversized_frames_dropped = 0u64;
    let mut latest_encoded_sequence = None::<u32>;
    let mut recovery = predictive::Recovery::default();
    loop {
        tokio::select! {
            _ = connection.closed() => break,
            Some(value) = controls.recv() => {
                if value.get("type").and_then(|value|value.as_str()) == Some("resync") {
                    if predictive { recovery.request(!jobs.is_empty()); }
                    // Re-encode the current source state with a fresh sequence;
                    // static desktops may have no further capture notification.
                    // This does not retransmit an old packet or dependency.
                    latest = source.frames.borrow().clone();
                } else { write_control(outgoing,&value).await?; }
            },
            changed = source.frames.changed() => {
                if changed.is_err() { break; }
                latest = source.frames.borrow_and_update().clone();
            },
            Some(event) = source.events.recv() => {
                write_control(outgoing,&event).await?;
                if event.get("type").and_then(|value|value.as_str()) == Some("error") { break; }
            },
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                let completed = result.context("datagram encoder task failed")??;
                encoder = Some(completed.encoder);
                let bytes = completed.bytes?;
                let packet = gliss_video::parse_packet(&bytes)?;
                ensure!(predictive || packet.keyframe,"dependent video frame forbidden on all-IDR datagram transport");
                ensure!(!completed.reset || packet.keyframe,"resync reset did not produce actual IDR");
                ensure!(bytes.len() <= MAX_PACKET,"datagram video frame too large");
                latest_encoded_sequence = Some(packet.sequence);
                encoded += 1;
                encode_ns = encode_ns.saturating_add(completed.elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
                // An in-flight encoder has advanced its references even when
                // its result is retired. Recover the same encoder before reset.
                if predictive && recovery.retire_completed() {
                    latest = source.frames.borrow().clone();
                    continue;
                }
                let stride = connection.max_datagram_size().context("peer does not support QUIC datagrams")?
                    .saturating_sub(DATAGRAM_HEADER).min(DATAGRAM_PAYLOAD);
                ensure!(stride >= 256,"QUIC datagram path size too small");
                let chunks = bytes.len().div_ceil(stride);
                ensure!(chunks <= usize::from(u16::MAX),"too many video datagrams");
                let xor_layout = if xor_enabled {
                    Some(crate::xor_fec::frame_layout(bytes.len(),stride,SESSION_HEADER_BUDGET,
                        QUINN_DATAGRAM_METADATA_BUDGET).context("invalid XOR frame layout")?)
                } else { None };
                let queued_bytes = xor_layout.map_or_else(
                    || queued_frame_bytes(bytes.len(),chunks),
                    |layout| layout.queued_bytes,
                );
                if let Some(layout) = xor_layout {
                    debug_assert_eq!(layout.data_datagrams,chunks);
                    debug_assert_eq!(layout.queued_bytes,
                        layout.data_application_bytes
                            .saturating_add(layout.parity_application_bytes)
                            .saturating_add(layout.datagrams().saturating_mul(
                                SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET)));
                }
                if !frame_fits_send_budget(bytes.len(),queued_bytes,
                    xor_layout.map(|layout|layout.datagrams()),predictive,send_buffer) {
                    oversized_frames_dropped += 1;
                    if predictive {
                        recovery.discarded();
                        latest = source.frames.borrow().clone();
                    }
                    continue;
                }
                let mut xor_scratch = xor_enabled.then(|| {
                    xor_scratch_bytes_allocated_total = xor_scratch_bytes_allocated_total
                        .saturating_add(stride as u64);
                    xor_scratch_bytes_peak = xor_scratch_bytes_peak.max(stride as u64);
                    vec![0u8;stride]
                });
                for first in (0..chunks).step_by(crate::xor_fec::DATA_FRAGMENTS_PER_GROUP) {
                    for index in first..(first + crate::xor_fec::DATA_FRAGMENTS_PER_GROUP).min(chunks) {
                        let offset = index * stride;
                        let payload = &bytes[offset..bytes.len().min(offset + stride)];
                        let mut datagram = Vec::with_capacity(DATAGRAM_HEADER + payload.len());
                        datagram.extend_from_slice(b"GLD1");
                        datagram.extend_from_slice(&packet.sequence.to_le_bytes());
                        datagram.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                        datagram.extend_from_slice(&(index as u16).to_le_bytes());
                        datagram.extend_from_slice(&(chunks as u16).to_le_bytes());
                        datagram.extend_from_slice(&(stride as u16).to_le_bytes());
                        datagram.extend_from_slice(&0u16.to_le_bytes());
                        datagram.extend_from_slice(payload);
                        connection.send_datagram(&datagram)?;
                        datagrams += 1;
                        if xor_enabled {
                            original_datagrams_enqueued = original_datagrams_enqueued.saturating_add(1);
                            original_application_bytes_enqueued = original_application_bytes_enqueued
                                .saturating_add(datagram.len() as u64);
                        }
                    }
                    if let Some(scratch) = xor_scratch.as_mut() {
                        let parity = crate::xor_fec::parity_datagram(
                            &bytes,packet.sequence,stride,first,scratch,
                        ).context("invalid XOR parity geometry")?;
                        connection.send_datagram(&parity.bytes)?;
                        datagrams += 1;
                        parity_datagrams_enqueued = parity_datagrams_enqueued.saturating_add(1);
                        parity_application_bytes_enqueued = parity_application_bytes_enqueued
                            .saturating_add(parity.bytes.len() as u64);
                        xor_groups_enqueued = xor_groups_enqueued.saturating_add(1);
                        xor_payload_bytes_read = xor_payload_bytes_read
                            .saturating_add(parity.payload_bytes_read as u64);
                    }
                }
                app.bytes.fetch_add(bytes.len() as u64,Ordering::Relaxed);
                app.frames.fetch_add(1,Ordering::Relaxed);
            },
            _ = pacing.tick(), if jobs.is_empty() && latest.is_some() => {
                if predictive && !recovery.ready(tokio::time::Instant::now()) { continue; }
                let captured = latest.take().unwrap();
                let mut next_encoder = encoder.take().context("datagram encoder busy")?;
                let reset = predictive && recovery.take_reset(tokio::time::Instant::now());
                if reset { next_encoder.reset(); }
                sequence = sequence.wrapping_add(1);
                let packet_sequence = sequence;
                if let Some(recorder) = &app.benchmark {
                    recorder.associate(recorder.generation(&captured.frame)?, 0, packet_sequence, "GLV1")?;
                }
                let permits = app.encoders.clone();
                let encode_actor = crate::stage_trace::lease(&app.stage_trace)?;
                let blocking_actor = crate::stage_trace::lease(&app.stage_trace)?;
                jobs.spawn(async move {
                    let _trace_actor = encode_actor;
                    let permit = permits.acquire_owned().await?;
                    tokio::task::spawn_blocking(move || {
                        let _trace_actor = blocking_actor;
                        let _permit = permit;
                        let started = Instant::now();
                        let bytes = next_encoder.encode(&captured.frame,packet_sequence,captured.forwarded_input_id);
                        Completed { encoder:next_encoder, bytes, elapsed:started.elapsed(), reset }
                    }).await.context("blocking datagram encoder failed")
                });
            },
            _ = compact_head.wait(), if codec.compact_head() && compact_head.pending(latest_encoded_sequence) => {
                let sequence = latest_encoded_sequence.context("compact head missing current encoded sequence")?;
                let body_bytes = write_compact_head(outgoing, sequence, codec.head_interval_ms()).await?;
                compact_head.written(sequence, body_bytes, tokio::time::Instant::now(), codec.head_interval_ms());
            },
            _ = telemetry.tick(), if app.args.stream_stats => {
                let stats = connection.quic_connection().stats();
                let send_buffer_space = connection.quic_connection().datagram_send_buffer_space();
                let mut message = json!({
                    "type":"stream_stats","transport":"webtransport-datagram",
                    "congestion_control":congestion_control,
                    "quic_datagram_frames_sent":stats.frame_tx.datagram,
                    "congestion_window_bytes":stats.path.cwnd,
                    "congestion_events":stats.path.congestion_events,
                    "lost_bytes":stats.path.lost_bytes,
                    "quic_packets_sent":stats.path.sent_packets,
                    "current_mtu":stats.path.current_mtu,
                    "datagram_send_buffer_space_bytes":send_buffer_space,
                    "datagram_send_buffer_used_bytes":send_buffer.saturating_sub(send_buffer_space),
                    "datagram_send_buffer_used_scope":"Conservative queue memory bound: limit minus next-send payload capacity; includes next-datagram metadata allowance, not queue age.",
                    "datagram_metadata_budget_bytes":QUINN_DATAGRAM_METADATA_BUDGET,
                    "encoded_frames":encoded,"datagrams_enqueued":datagrams,
                    "latest_encoded_sequence":latest_encoded_sequence,
                    "encoded_head_interval_ms":codec.head_interval_ms(),
                    "predictive_recovery":recovery.snapshot(),
                    "encode_ms_total":encode_ns as f64 / 1_000_000.0,
                    "rtt_ms":connection.rtt().as_secs_f64()*1000.,
                    "udp_tx_datagrams":stats.udp_tx.datagrams,
                    "udp_tx_bytes":stats.udp_tx.bytes,
                    "udp_rx_datagrams":stats.udp_rx.datagrams,
                    "udp_rx_bytes":stats.udp_rx.bytes,
                    "lost_packets":stats.path.lost_packets,
                    "send_queue_bytes":send_buffer,
                    "oversized_frames_dropped":oversized_frames_dropped,
                });
                if codec.compact_head() {
                    message["compact_head_messages_written"] = json!(compact_head.messages_written);
                    message["compact_head_application_bytes_written"] = json!(compact_head.application_bytes_written);
                    message["statistics_interval_ms"] = json!(1000);
                }
                if xor_enabled {
                    message["original_datagrams_enqueued"] = json!(original_datagrams_enqueued);
                    message["original_application_bytes_enqueued"] = json!(original_application_bytes_enqueued);
                    message["parity_datagrams_enqueued"] = json!(parity_datagrams_enqueued);
                    message["parity_application_bytes_enqueued"] = json!(parity_application_bytes_enqueued);
                    message["xor_groups_enqueued"] = json!(xor_groups_enqueued);
                    message["xor_payload_bytes_read"] = json!(xor_payload_bytes_read);
                    message["xor_scratch_bytes_allocated_total"] = json!(xor_scratch_bytes_allocated_total);
                    message["xor_scratch_bytes_peak"] = json!(xor_scratch_bytes_peak);
                }
                write_control(outgoing,&message).await?;
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authenticated_headers(app: &App) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("gliss_session={}", signed_cookie(app))
                .parse()
                .unwrap(),
        );
        headers.insert(header::ORIGIN, app.origins[0].parse().unwrap());
        headers
    }

    #[test]
    fn datagram_buffer_bounds_allow_a_complete_maximum_tile_and_preserve_legacy_budget() {
        let tile_charge = 4096 + 4 * (32 + SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET);
        assert!(tile_charge < MIN_SEND_BUFFER);
        for bytes in [MIN_SEND_BUFFER, 65536, SEND_BUFFER] {
            validate_send_buffer(bytes).unwrap();
        }
        for bytes in [0, MIN_SEND_BUFFER - 1, SEND_BUFFER + 1] {
            assert!(validate_send_buffer(bytes).is_err());
        }
        assert!(frame_fits_send_admission(
            900_000,
            queued_frame_bytes(900_000, 900),
            None,
            false
        ));
        assert!(!frame_fits_send_budget(
            900_000,
            queued_frame_bytes(900_000, 900),
            None,
            false,
            65536
        ));
    }

    #[tokio::test]
    async fn scoped_datagram_input_allows_keys_and_pointer_but_denies_clipboard_and_resize() {
        let scope = InputScope {
            tile_mode: true,
            display_input_only: true,
        };
        let revoked = Mutex::new(false);
        let (input, mut received) = mpsc::channel(4);
        let (controls, mut replies) = mpsc::channel(4);
        for value in [
            json!({"type":"clipboard","text":"private"}),
            json!({"type":"resize","width":1280,"height":800}),
        ] {
            assert!(
                route_control_traced(value, &input, &controls, scope, &None, &revoked).is_err()
            );
        }
        assert!(received.try_recv().is_err());
        assert!(replies.try_recv().is_err());
        route_control_traced(
            json!({"type":"key","keysym":0xff0d,"down":true,"id":1}),
            &input,
            &controls,
            scope,
            &None,
            &revoked,
        )
        .unwrap();
        assert!(matches!(
            received.recv().await,
            Some(Input::Key {
                keysym: 0xff0d,
                down: true,
                id: 1
            })
        ));
        route_control_traced(
            json!({"type":"pointer","x":1,"y":2,"buttons":0,"id":2}),
            &input,
            &controls,
            scope,
            &None,
            &revoked,
        )
        .unwrap();
        assert!(matches!(
            received.recv().await,
            Some(Input::Pointer {
                x: 1,
                y: 2,
                buttons: 0,
                id: 2
            })
        ));
        route_control_traced(
            json!({"type":"ack","sequence":1}),
            &input,
            &controls,
            scope,
            &None,
            &revoked,
        )
        .unwrap();
        assert!(replies.try_recv().is_err());
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn private_revoke_authenticates_before_consuming_tickets_and_permanently_blocks_input() {
        let runtime =
            DatagramRuntime::bind("127.0.0.1:0".parse().unwrap(), None, CongestionControl::Bbr)
                .unwrap();
        let mut fixture = app(1);
        fixture.datagram = Some(runtime.clone());
        let app = Arc::new(fixture);
        let headers = authenticated_headers(&app);
        assert_eq!(runtime.mint_ticket(&app, &headers).status(), StatusCode::OK);
        assert_eq!(app.clients.available_permits(), 0);
        for unauthorized in [HeaderMap::new(), {
            let mut headers = headers.clone();
            headers.insert(header::ORIGIN, "https://other.example".parse().unwrap());
            headers
        }] {
            assert_eq!(
                datagram_revoke(State(app.clone()), unauthorized)
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert!(!*runtime.revoked.lock().unwrap());
            assert_eq!(runtime.tickets.lock().unwrap().len(), 1);
        }
        assert_eq!(
            datagram_revoke(State(app.clone()), headers.clone())
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert!(*runtime.revoked.lock().unwrap());
        assert!(*app.worker_shutdown.borrow());
        assert!(runtime.tickets.lock().unwrap().is_empty());
        assert_eq!(app.clients.available_permits(), 1);
        assert_eq!(
            runtime.mint_ticket(&app, &headers).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            datagram_revoke(State(app.clone()), headers).await.status(),
            StatusCode::NO_CONTENT
        );
        let (input, mut received) = mpsc::channel(1);
        let (controls, mut replies) = mpsc::channel(1);
        let scope = InputScope {
            tile_mode: true,
            display_input_only: true,
        };
        for value in [
            json!({"type":"key","keysym":1,"down":true}),
            json!({"type":"pointer","x":0,"y":0,"buttons":1}),
            json!({"type":"resync"}),
            json!({"type":"ack","sequence":1}),
        ] {
            assert!(
                route_control_traced(value, &input, &controls, scope, &None, &runtime.revoked)
                    .is_err()
            );
        }
        assert!(received.try_recv().is_err() && replies.try_recv().is_err());
    }

    #[tokio::test]
    async fn public_descriptor_uses_configured_media_host_scope_and_queue_bound() {
        for (host, expected) in [
            ("media.example", "https://media.example:4443"),
            ("203.0.113.7", "https://203.0.113.7:4443"),
            ("2001:db8::7", "https://[2001:db8::7]:4443"),
        ] {
            let runtime = DatagramRuntime::bind_configured(
                "127.0.0.1:0".parse().unwrap(),
                Some(4443),
                CongestionControl::Bbr,
                DatagramCodec::Glt2Lossless,
                Some(host.to_owned()),
                Some(65536),
            )
            .unwrap();
            let mut fixture = app(1);
            fixture.args.datagram_display_input_only = true;
            fixture.origins = vec!["https://desktop.example".to_owned()];
            let response = runtime.mint_ticket(&fixture, &authenticated_headers(&fixture));
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                descriptor["url"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("{expected}/connect?ticket="))
            );
            assert_eq!(runtime.advertised_origin(), expected);
            assert_eq!(descriptor["send_queue_bytes"], 65536);
            assert_eq!(descriptor["display_input_only"], true);
            assert_eq!(descriptor["datagram_codec"], "glt2-lossless");
            assert_eq!(
                descriptor["certificate_sha256"].as_array().unwrap().len(),
                32
            );
            runtime.revoke();
        }
    }

    #[tokio::test]
    async fn revocation_stops_the_accept_actor_without_closing_under_its_pending_accept() {
        let runtime = DatagramRuntime::bind(
            "127.0.0.1:0".parse().unwrap(),
            None,
            CongestionControl::Cubic,
        )
        .unwrap();
        let app = Arc::new(app(1));
        let serving = tokio::spawn(runtime.clone().serve(app));
        tokio::task::yield_now().await;
        runtime.revoke();
        tokio::time::timeout(Duration::from_secs(3), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn compact_head_coalesces_progress_without_idle_work_or_a_sequence_queue() {
        const {
            assert!(std::mem::size_of::<CompactHead>() <= 64);
        }
        let now = tokio::time::Instant::now();
        let mut head = CompactHead::default();
        assert!(!head.pending(None));
        assert!(head.pending(Some(u32::MAX)));
        head.written(u32::MAX, 100, now, 100);
        assert!(!head.pending(Some(u32::MAX)));
        assert!(head.pending(Some(0))); // Native u32 sequence wrap is still progress.
        assert!(head.pending(Some(7))); // Only the actor's latest value will be written.
        assert_eq!(head.not_before, Some(now + Duration::from_millis(100)));
        head.written(7, 100, now + Duration::from_millis(150), 100);
        assert_eq!(head.not_before, Some(now + Duration::from_millis(250)));
        assert!(!head.pending(Some(7)));
        assert_eq!(head.messages_written, 2);
        assert_eq!(head.application_bytes_written, 208);
    }

    #[test]
    fn compact_head_body_is_bounded_and_existing_profile_capabilities_are_unchanged() {
        for sequence in [0, u32::MAX] {
            let bytes = compact_head_bytes(sequence, 100).unwrap();
            assert!(bytes.len() <= 128);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                json!({"type":"encoded_head","latest_encoded_sequence":sequence,"encoded_head_interval_ms":100})
            );
        }
        for (codec, interval) in [
            (DatagramCodec::H264PredictiveGop4, 1000),
            (DatagramCodec::H264PredictiveGop4Head100, 100),
        ] {
            let mut value = json!({});
            codec.decorate_video(&mut value);
            assert_eq!(value["encoded_head_interval_ms"], interval);
            assert_eq!(value["encoded_head_deadline_ms"], 250);
            assert_eq!(value["recovery_interval_ms"], 1000);
            assert!(value.get("encoded_head_notification").is_none());
            assert_eq!(codec.statistics_interval_ms(), interval);
        }
        let codec = DatagramCodec::H264PredictiveGop4Compact100;
        let mut value = json!({});
        codec.decorate_video(&mut value);
        assert!(codec.predictive());
        assert_eq!(codec.statistics_interval_ms(), 1000);
        assert_eq!(value["encoded_head_interval_ms"], 100);
        assert_eq!(value["statistics_interval_ms"], 1000);
        assert_eq!(value["encoded_head_notification"], "on-change-coalesced-v1");
        assert_eq!(value["encoded_head_max_body_bytes"], 128);
        assert_eq!(value["encoded_head_deadline_ms"], 250);
        assert_eq!(value["recovery_interval_ms"], 1000);
    }

    #[test]
    fn compact50_only_changes_the_selected_coalescing_interval() {
        let codec = DatagramCodec::H264PredictiveGop4Compact50;
        assert_eq!(codec.name(), "h264-predictive-gop4-compact50");
        assert!(codec.predictive() && codec.compact_head());
        assert_eq!(codec.policy(), None);
        assert_eq!(codec.protocol(), "GLV1");
        assert_eq!(codec.head_interval_ms(), 50);
        assert_eq!(codec.statistics_interval_ms(), 1000);
        let mut old = json!({});
        let mut new = json!({});
        DatagramCodec::H264PredictiveGop4Compact100.decorate_video(&mut old);
        codec.decorate_video(&mut new);
        new["datagram_codec"] = old["datagram_codec"].clone();
        new["encoded_head_interval_ms"] = old["encoded_head_interval_ms"].clone();
        assert_eq!(new, old);
        for sequence in [0, u32::MAX] {
            let old_bytes = compact_head_bytes(sequence, 100).unwrap();
            let expected_old = format!(
                "{{\"encoded_head_interval_ms\":100,\"latest_encoded_sequence\":{sequence},\"type\":\"encoded_head\"}}"
            );
            assert_eq!(old_bytes, expected_old.as_bytes());
            let bytes = compact_head_bytes(sequence, 50).unwrap();
            assert!(bytes.len() <= 128);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                json!({"type":"encoded_head","latest_encoded_sequence":sequence,"encoded_head_interval_ms":50})
            );
            assert_eq!(bytes.len() + 1, old_bytes.len());
        }
    }

    #[test]
    fn xor8_adds_only_the_explicit_default_off_capability() {
        let off = DatagramCodec::H264PredictiveGop4Compact50;
        let on = DatagramCodec::H264PredictiveGop4Compact50Xor8;
        assert_eq!(DatagramCodec::default(), DatagramCodec::H264);
        assert_eq!(on.name(), "h264-predictive-gop4-compact50-xor8");
        assert!(on.predictive() && on.compact_head() && on.xor_fec());
        assert_eq!(on.policy(), None);
        assert_eq!(on.protocol(), "GLV1");
        assert_eq!(on.head_interval_ms(), 50);
        assert_eq!(on.statistics_interval_ms(), 1000);
        let fields = [
            "fec",
            "fec_scope",
            "fec_data_fragments_per_group",
            "fec_parity_fragments_per_group",
            "parity_datagram_protocol",
        ];
        for codec in [
            DatagramCodec::H264,
            DatagramCodec::H264PredictiveGop4,
            DatagramCodec::H264PredictiveGop4Head100,
            DatagramCodec::H264PredictiveGop4Compact100,
            DatagramCodec::H264PredictiveGop4Compact50,
            DatagramCodec::Glt2Lossless,
            DatagramCodec::Glt2Gentle,
            DatagramCodec::Glt2Strong,
        ] {
            let mut value = json!({});
            codec.decorate_video(&mut value);
            for field in fields {
                assert!(
                    value.get(field).is_none(),
                    "{} exposed {field}",
                    codec.name()
                );
            }
        }
        let mut off_value = json!({});
        let mut on_value = json!({});
        off.decorate_video(&mut off_value);
        on.decorate_video(&mut on_value);
        let mut expected = off_value;
        expected["datagram_codec"] = json!("h264-predictive-gop4-compact50-xor8");
        expected["fec"] = json!("xor-grouped-v1");
        expected["fec_scope"] = json!("all-access-units");
        expected["fec_data_fragments_per_group"] = json!(8);
        expected["fec_parity_fragments_per_group"] = json!(1);
        expected["parity_datagram_protocol"] = json!("GLP1");
        assert_eq!(on_value, expected);
    }

    #[test]
    fn compact50_keeps_one_persistent_post_write_deadline_and_wrap_accounting() {
        let now = tokio::time::Instant::now();
        for interval in [50, 100] {
            let mut head = CompactHead::default();
            assert!(!head.pending(None));
            assert_eq!(head.not_before, None);
            head.written(u32::MAX, 82, now, interval);
            let deadline = Some(now + Duration::from_millis(interval));
            assert!(!head.pending(Some(u32::MAX)));
            assert!(head.pending(Some(0)));
            assert!(head.pending(Some(17)));
            assert_eq!(head.not_before, deadline); // Observing newer heads cannot rearm it.
            let completed = now + Duration::from_millis(interval + 7);
            head.written(17, 83, completed, interval);
            assert!(!head.pending(Some(17)));
            assert_eq!(
                head.not_before,
                Some(completed + Duration::from_millis(interval))
            );
            assert_eq!(head.messages_written, 2);
            assert_eq!(head.application_bytes_written, 173);
        }
    }

    fn app(max_clients: usize) -> App {
        let args = Args::try_parse_from([
            "gliss-server",
            "--demo",
            "--token",
            "01234567890123456789012345678901",
            "--codec",
            "h264",
            "--video-intra-period",
            "1",
        ])
        .unwrap();
        let video_timing = configured_video_timing(&args).unwrap();
        App {
            args,
            mode: None,
            hybrid_config: gliss_hybrid::HybridConfig::default(),
            hybrid_mask_grid: None,
            video_timing,
            datagram: None,
            benchmark: None,
            stage_trace: None,
            tcp_listener: crate::tcp_listener::SocketPolicy::new(false),
            #[cfg(unix)]
            rfb_fd: None,
            source_deadline: None,
            worker_shutdown: watch::channel(false).0,
            origins: vec!["http://127.0.0.1:8080".to_owned()],
            cookie_key: [7; 32],
            clients: Arc::new(Semaphore::new(max_clients)),
            encoders: Arc::new(Semaphore::new(2)),
            login_attempts: Mutex::new((Instant::now(), 0)),
            bytes: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            started: Instant::now(),
            shared_capture: None,
        }
    }

    #[tokio::test]
    async fn tile_resize_admits_odd_dimensions_without_weakening_video_rules() {
        let (input, mut received) = mpsc::channel(4);
        let (controls, mut replies) = mpsc::channel(4);
        let value = json!({"type":"resize","width":1279,"height":799});
        route_control_for(value.clone(), &input, &controls, false).unwrap();
        assert_eq!(replies.recv().await.unwrap()["type"], "input_error");
        assert!(received.try_recv().is_err());
        route_control_for(value, &input, &controls, true).unwrap();
        assert!(matches!(
            received.recv().await,
            Some(Input::Resize {
                width: 1279,
                height: 799,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn input_progresses_while_outgoing_control_is_backpressured() {
        let (input, mut received) = mpsc::channel(4);
        let (controls, _blocked_writer) = mpsc::channel(1);
        controls.try_send(json!({"type":"cursor"})).unwrap();
        for down in [true, false] {
            route_control(
                json!({"type":"key","keysym":0xff0d,"down":down,"id":7}),
                &input,
                &controls,
            )
            .unwrap();
            assert!(
                matches!(received.recv().await,Some(Input::Key {keysym:0xff0d,down:actual,id:7}) if actual==down)
            );
        }
        assert!(route_control(json!({"type":"ping"}), &input, &controls).is_err());
        assert!(
            route_control(
                json!({"type":"key","keysym":"invalid","down":true}),
                &input,
                &controls
            )
            .is_err()
        );
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn recovery_requests_current_source_state_without_media_ack_dependencies() {
        let (input, mut received) = mpsc::channel(1);
        let (controls, mut commands) = mpsc::channel(1);
        route_control(json!({"type":"ack","sequence":19}), &input, &controls).unwrap();
        assert!(commands.try_recv().is_err());
        route_control(json!({"type":"resync"}), &input, &controls).unwrap();
        assert_eq!(commands.recv().await.unwrap(), json!({"type":"resync"}));
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn selected_controller_constructs_the_actual_locked_quinn_implementation() {
        use wtransport::quinn::congestion::{Bbr, Cubic};
        assert!(
            CongestionControl::Cubic
                .factory()
                .build(Instant::now(), 1200)
                .into_any()
                .is::<Cubic>()
        );
        assert!(
            CongestionControl::Bbr
                .factory()
                .build(Instant::now(), 1200)
                .into_any()
                .is::<Bbr>()
        );
        assert_eq!(CongestionControl::default(), CongestionControl::Cubic);
    }

    #[test]
    fn admission_counts_transport_wrapping_and_queue_metadata_before_sending() {
        //1020 full fragments really fit the old application+session-only
        // guard by16 bytes, but Quinn metadata would evict their own head.
        let fragments = 1020;
        let bytes = fragments * DATAGRAM_PAYLOAD;
        assert!(bytes + fragments * (DATAGRAM_HEADER + SESSION_HEADER_BUDGET) <= SEND_BUFFER);
        assert!(queued_frame_bytes(bytes, fragments) > SEND_BUFFER);
        assert!(queued_frame_bytes(900_000, 900) <= SEND_BUFFER);
        assert_eq!(queued_frame_bytes(usize::MAX, usize::MAX), usize::MAX);
    }

    #[test]
    fn xor_admission_charges_whole_parity_frame_before_any_send() {
        let off_bytes = queued_frame_bytes(900_000, 900);
        let oversized = crate::xor_fec::frame_layout(
            900_000,
            1000,
            SESSION_HEADER_BUDGET,
            QUINN_DATAGRAM_METADATA_BUDGET,
        )
        .unwrap();
        assert!(frame_fits_send_admission(900_000, off_bytes, None, true));
        let fits = frame_fits_send_admission(
            900_000,
            oversized.queued_bytes,
            Some(oversized.datagrams()),
            true,
        );
        assert!(!fits);
        let mut recovery = predictive::Recovery::default();
        if !fits {
            recovery.discarded();
        }
        assert_eq!(recovery.snapshot()["reset_debt"], true);
        assert_eq!(recovery.snapshot()["encoded_outputs_budget_discarded"], 1);
        let admitted = crate::xor_fec::frame_layout(
            700_000,
            1000,
            SESSION_HEADER_BUDGET,
            QUINN_DATAGRAM_METADATA_BUDGET,
        )
        .unwrap();
        assert!(frame_fits_send_admission(
            700_000,
            admitted.queued_bytes,
            Some(admitted.datagrams()),
            true
        ));
        assert!(admitted.datagrams() <= crate::xor_fec::BROWSER_DATAGRAM_HIGH_WATER_MARK);
        let bound = crate::xor_fec::admitted_datagram_upper_bound(
            SEND_BUFFER,
            256,
            DATAGRAM_HEADER + SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET,
        );
        assert_eq!(bound, 2546);
        assert!(bound < crate::xor_fec::BROWSER_DATAGRAM_HIGH_WATER_MARK);
        assert!(!frame_fits_send_admission(
            700_000,
            admitted.queued_bytes,
            Some(crate::xor_fec::BROWSER_DATAGRAM_HIGH_WATER_MARK + 1),
            true
        ));
    }

    #[tokio::test]
    async fn authenticated_metadata_reports_runtime_controller_even_if_args_are_not_reused() {
        for selected in [CongestionControl::Cubic, CongestionControl::Bbr] {
            let runtime =
                DatagramRuntime::bind("127.0.0.1:0".parse().unwrap(), None, selected).unwrap();
            let mut fixture = app(1);
            fixture.datagram = Some(runtime.clone());
            // Metadata must come from the configured endpoint, never an absent
            // optional CLI field defaulting differently at the reporting site.
            assert_eq!(fixture.args.datagram_congestion_control, None);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::COOKIE,
                format!("gliss_session={}", signed_cookie(&fixture))
                    .parse()
                    .unwrap(),
            );
            headers.insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
            let response = runtime.mint_ticket(&fixture, &headers);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let ticket: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(ticket["congestion_control"], selected.name());
            assert_eq!(ticket.as_object().unwrap().len(), 7);
            assert!(ticket.get("codec").is_none());
            let response = stats(State(Arc::new(fixture)), headers).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let actual: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                actual["datagram_config"]["congestion_control"],
                selected.name()
            );
            assert_eq!(
                actual["datagram_config"],
                json!({"transport":"webtransport-datagram",
                "congestion_control":selected.name(),"bound_udp":runtime.local_addr().to_string(),
                "advertised_udp_port":runtime.local_addr().port(),"independent_frames":true,"media_retransmission":false})
            );
            runtime
                .endpoint
                .close(VarInt::from_u32(0), b"test complete");
        }
    }

    #[tokio::test]
    async fn ticket_authentication_origin_expiry_and_capacity_precede_capture() {
        let runtime = DatagramRuntime::bind(
            "127.0.0.1:0".parse().unwrap(),
            None,
            CongestionControl::Cubic,
        )
        .unwrap();
        let mut app = app(2);
        let mut headers = HeaderMap::new();
        assert_eq!(
            runtime.mint_ticket(&app, &headers).status(),
            StatusCode::UNAUTHORIZED
        );
        headers.insert(
            header::COOKIE,
            format!("gliss_session={}", signed_cookie(&app))
                .parse()
                .unwrap(),
        );
        headers.insert(header::ORIGIN, "https://untrusted.invalid".parse().unwrap());
        assert_eq!(
            runtime.mint_ticket(&app, &headers).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(app.clients.available_permits(), 2);
        headers.insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        let response = runtime.mint_ticket(&app, &headers);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let descriptor: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            descriptor["certificate_sha256"].as_array().unwrap().len(),
            32
        );
        assert!(
            descriptor["url"]
                .as_str()
                .unwrap()
                .starts_with(&format!("https://{}/display?ticket=", runtime.local_addr()))
        );
        assert_eq!(app.clients.available_permits(), 1);
        assert_eq!(runtime.mint_ticket(&app, &headers).status(), StatusCode::OK);
        assert_eq!(
            runtime.mint_ticket(&app, &headers).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(app.clients.available_permits(), 0);
        for ticket in runtime.tickets.lock().unwrap().values_mut() {
            ticket.expires = tokio::time::Instant::now() - Duration::from_millis(1);
        }
        assert_eq!(runtime.mint_ticket(&app, &headers).status(), StatusCode::OK);
        assert_eq!(app.clients.available_permits(), 1);
        app.source_deadline = Some(tokio::time::Instant::now());
        assert_eq!(
            runtime.mint_ticket(&app, &headers).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(app.clients.available_permits(), 1);
        runtime
            .endpoint
            .close(VarInt::from_u32(0), b"test complete");
    }

    #[tokio::test]
    async fn advertised_endpoint_is_operator_selected_and_nonloopback_bind_is_rejected() {
        assert!(
            DatagramRuntime::bind("0.0.0.0:0".parse().unwrap(), None, CongestionControl::Cubic)
                .is_err()
        );
        let runtime = DatagramRuntime::bind(
            "127.0.0.1:0".parse().unwrap(),
            Some(54321),
            CongestionControl::Cubic,
        )
        .unwrap();
        let app = app(1);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("gliss_session={}", signed_cookie(&app))
                .parse()
                .unwrap(),
        );
        headers.insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        let response = runtime.mint_ticket(&app, &headers);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let descriptor: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            descriptor["url"]
                .as_str()
                .unwrap()
                .starts_with("https://127.0.0.1:54321/display?ticket=")
        );
        runtime
            .endpoint
            .close(VarInt::from_u32(0), b"test complete");
    }
}
