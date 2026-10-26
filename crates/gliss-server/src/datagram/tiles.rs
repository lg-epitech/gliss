//! Live absolute tiles. Input is owned by the independent parent receiver.
use super::*;
use crate::stage_trace::{
    self, Fields as TraceFields, FrameKey, Kind as TraceKind, Outcome as TraceOutcome,
};
use crate::tile_quantum::{self, Admission, FlushOrigin, FlushSummary, Sender, Stop, WakeHandle};
use crate::tile_send::{Policy, ReadyGate, Wake, wait_ready};
use gliss_tiles::{EncodedTile, Encoder as TileEncoder};
use std::collections::BTreeMap;

const PENDING_BYTES: usize = 4 * 1024 * 1024;
const MAX_TILES: usize = 8160;
const PENDING_TTL: Duration = Duration::from_millis(250);
const CODEC_MEMORY: usize = 128 * 1024 * 1024;

struct CancelOnDrop {
    barrier: Arc<AtomicU64>,
    queue: Arc<Mutex<Pending>>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        // Do not panic again during unwinding. A poisoned queue cannot admit
        // through insert_current; advancing the barrier still invalidates jobs.
        if invalidate_current(&self.queue, &self.barrier).is_err() {
            self.barrier.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn invalidate_current(queue: &Mutex<Pending>, barrier: &AtomicU64) -> Result<()> {
    let mut pending = queue
        .lock()
        .map_err(|_| anyhow::anyhow!("tile pending queue poisoned"))?;
    barrier.fetch_add(1, Ordering::AcqRel);
    pending.clear();
    Ok(())
}
#[cfg(test)]
fn insert_current<F>(
    queue: &Mutex<Pending>,
    barrier: &AtomicU64,
    expected: u64,
    tile: EncodedTile,
    before_insert: F,
) -> Result<bool>
where
    F: FnOnce(&EncodedTile) -> Result<()>,
{
    insert_current_traced(queue, barrier, expected, tile, before_insert, None)
}
fn insert_current_traced<F>(
    queue: &Mutex<Pending>,
    barrier: &AtomicU64,
    expected: u64,
    tile: EncodedTile,
    before_insert: F,
    generation: Option<u64>,
) -> Result<bool>
where
    F: FnOnce(&EncodedTile) -> Result<()>,
{
    let mut pending = queue
        .lock()
        .map_err(|_| anyhow::anyhow!("tile pending queue poisoned"))?;
    // This check, association, insertion and invalidate_current's clear share
    // one mutex. An old encode cannot pass the check, wait through a clear,
    // then insert its obsolete body after that clear.
    if barrier.load(Ordering::Acquire) != expected {
        stage_trace::keyed(
            &pending.trace,
            TraceKind::SequenceCanceled,
            generation.map(|generation| FrameKey {
                generation,
                epoch: tile.epoch,
                sequence: tile.sequence,
            }),
            1,
            TraceOutcome::Canceled,
        );
    }
    ensure!(
        barrier.load(Ordering::Acquire) == expected,
        "superseded tile encode generation"
    );
    before_insert(&tile)?;
    let key = generation.map(|generation| FrameKey {
        generation,
        epoch: tile.epoch,
        sequence: tile.sequence,
    });
    pending.insert_traced(tile, Instant::now(), key)
}

// The production admission loop and test fake use the same three operations.
// No modeled network, waiting-for-space or alternate batching path is added.
trait TileDatagrams {
    fn space(&self) -> usize;
    fn ceiling(&self) -> Option<usize>;
    fn admit(&self, packet: &[u8]) -> Result<()>;
}
impl TileDatagrams for Connection {
    fn space(&self) -> usize {
        self.quic_connection().datagram_send_buffer_space()
    }
    fn ceiling(&self) -> Option<usize> {
        self.max_datagram_size()
    }
    fn admit(&self, packet: &[u8]) -> Result<()> {
        self.send_datagram(packet)?;
        Ok(())
    }
}

fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

struct PendingTile {
    update: EncodedTile,
    queued: Instant,
    stage_key: Option<FrameKey>,
}

#[derive(Default)]
struct Pending {
    trace: stage_trace::Handle,
    updates: BTreeMap<usize, PendingTile>,
    bytes: usize,
    epoch: Option<u32>,
    dimensions: Option<(u16, u16)>,
    cursor: usize,
    generated: u64,
    replaced: u64,
    evicted: u64,
    expired: u64,
    superseded: u64,
    admitted: u64,
    datagrams: u64,
    body_bytes: u64,
    wire_bytes: u64,
    peak_bytes: usize,
    peak_count: usize,
    max_residence_ms: f64,
}

impl Pending {
    fn remove(&mut self, tile: usize) -> Option<PendingTile> {
        let removed = self.updates.remove(&tile)?;
        self.bytes -= removed.update.body.len();
        Some(removed)
    }
    fn clear(&mut self) {
        for old in self.updates.values() {
            stage_trace::keyed(
                &self.trace,
                TraceKind::SequenceCanceled,
                old.stage_key,
                1,
                TraceOutcome::Canceled,
            );
        }
        self.superseded += self.updates.len() as u64;
        self.updates.clear();
        self.bytes = 0;
    }
    fn expire(&mut self, now: Instant) {
        let stale: Vec<usize> = self
            .updates
            .iter()
            .filter(|(_, value)| now.saturating_duration_since(value.queued) >= PENDING_TTL)
            .map(|(tile, _)| *tile)
            .collect();
        for tile in stale {
            if let Some(old) = self.remove(tile) {
                stage_trace::keyed(
                    &self.trace,
                    TraceKind::SequenceExpired,
                    old.stage_key,
                    1,
                    TraceOutcome::Expired,
                );
                self.max_residence_ms = self
                    .max_residence_ms
                    .max(now.saturating_duration_since(old.queued).as_secs_f64() * 1000.);
                self.expired += 1;
            }
        }
    }
    #[cfg(test)]
    fn insert(&mut self, update: EncodedTile, now: Instant) -> Result<bool> {
        self.insert_traced(update, now, None)
    }
    fn insert_traced(
        &mut self,
        update: EncodedTile,
        now: Instant,
        stage_key: Option<FrameKey>,
    ) -> Result<bool> {
        ensure!(
            update.body.len() <= 4096 && update.tile < MAX_TILES,
            "tile queue body/index bound"
        );
        let dimensions = (update.width, update.height);
        if self.epoch != Some(update.epoch) {
            if self.epoch.is_some_and(|epoch| !newer(update.epoch, epoch)) {
                self.superseded += 1;
                stage_trace::keyed(
                    &self.trace,
                    TraceKind::SequenceCanceled,
                    stage_key,
                    1,
                    TraceOutcome::Canceled,
                );
                return Ok(false);
            }
            self.clear();
            self.epoch = Some(update.epoch);
            self.dimensions = Some(dimensions);
        }
        ensure!(
            self.dimensions == Some(dimensions),
            "tile queue epoch geometry differs"
        );
        self.generated += 1;
        self.expire(now);
        if let Some(old) = self.updates.get(&update.tile) {
            if !newer(update.sequence, old.update.sequence) {
                self.superseded += 1;
                stage_trace::keyed(
                    &self.trace,
                    TraceKind::SequenceCanceled,
                    stage_key,
                    1,
                    TraceOutcome::Canceled,
                );
                return Ok(false);
            }
            let old = self.remove(update.tile).unwrap();
            stage_trace::keyed(
                &self.trace,
                TraceKind::SequenceReplaced,
                old.stage_key,
                1,
                TraceOutcome::Replaced,
            );
            self.replaced += 1;
        }
        while self.bytes + update.body.len() > PENDING_BYTES {
            let victim = self
                .updates
                .iter()
                .min_by_key(|(id, item)| (item.queued, **id))
                .map(|(id, _)| *id)
                .context("tile queue accounting differs")?;
            let old = self.remove(victim).unwrap();
            stage_trace::keyed(
                &self.trace,
                TraceKind::SequenceEvicted,
                old.stage_key,
                1,
                TraceOutcome::Evicted,
            );
            self.evicted += 1;
        }
        self.bytes += update.body.len();
        self.updates.insert(
            update.tile,
            PendingTile {
                update,
                queued: now,
                stage_key,
            },
        );
        ensure!(
            self.updates.len() <= MAX_TILES,
            "tile queue geometry count bound"
        );
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        self.peak_count = self.peak_count.max(self.updates.len());
        Ok(true)
    }
    fn send(&mut self, connection: &impl TileDatagrams, stride: usize) -> Result<()> {
        let mut trace_visit = stage_trace::AdmissionVisit::new(&self.trace);
        self.expire(Instant::now());
        let Some((width, height)) = self.dimensions else {
            return Ok(());
        };
        let count = usize::from(width).div_ceil(32) * usize::from(height).div_ceil(32);
        ensure!(count <= MAX_TILES, "tile queue geometry bound");
        for _ in 0..count {
            let id = self.cursor % count;
            self.cursor = (id + 1) % count;
            let Some(pending) = self.updates.get(&id) else {
                continue;
            };
            let fragments = pending.update.body.len().div_ceil(stride);
            let charge = pending.update.body.len()
                + fragments * (32 + SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET);
            if connection.space() < charge {
                // Retry this bounded tile first next slot, rather than scanning
                // to the end and accidentally restarting every slot at tile0.
                self.cursor = id;
                break;
            }
            let ceiling = connection.ceiling().context("QUIC datagrams disabled")?;
            ensure!(
                ceiling >= stride + 32,
                "tile session path MTU decreased; reconnect required"
            );
            let pending = self.remove(id).unwrap();
            let packets = pending.update.fragments(stride)?;
            self.max_residence_ms = self
                .max_residence_ms
                .max(pending.queued.elapsed().as_secs_f64() * 1000.);
            let trace_started = self.trace.as_ref().and_then(|trace| trace.stamp());
            for (accepted, packet) in packets.iter().enumerate() {
                if let Err(error) = connection.admit(packet) {
                    stage_trace::keyed(
                        &self.trace,
                        TraceKind::SequenceSendError,
                        pending.stage_key,
                        accepted as u32,
                        TraceOutcome::Error,
                    );
                    return Err(error);
                }
            }
            trace_visit.observed(pending.stage_key, trace_started);
            if stage_trace::tile_marker(pending.update.width, pending.update.tile) {
                stage_trace::keyed(
                    &self.trace,
                    TraceKind::MarkerTileAdmitted,
                    pending.stage_key,
                    pending.update.tile as u32,
                    TraceOutcome::Accepted,
                );
            }
            self.admitted += 1;
            self.datagrams += packets.len() as u64;
            self.body_bytes += pending.update.body.len() as u64;
            self.wire_bytes += packets
                .iter()
                .map(|packet| packet.len() as u64)
                .sum::<u64>();
        }
        Ok(())
    }
    fn send_paced(
        &mut self,
        connection: &impl TileDatagrams,
        stride: usize,
        sender: &mut Sender,
        wake: WakeHandle,
    ) -> Result<Stop> {
        let mut trace_visit = stage_trace::AdmissionVisit::new(&self.trace);
        self.expire(Instant::now());
        let Some((width, height)) = self.dimensions else {
            return Ok(Stop::Empty);
        };
        let count = usize::from(width).div_ceil(32) * usize::from(height).div_ceil(32);
        ensure!(count <= MAX_TILES, "tile queue geometry bound");
        for _ in 0..count {
            let id = self.cursor % count;
            self.cursor = (id + 1) % count;
            let Some(pending) = self.updates.get(&id) else {
                continue;
            };
            let charge = tile_quantum::current_tile_charge(pending.update.body.len(), stride)?;
            let ticket = match sender.classify(wake, charge, connection.space())? {
                Admission::Ready(ticket) => ticket,
                Admission::BudgetBlocked(wait) => {
                    self.cursor = id;
                    return Ok(Stop::BudgetBlocked(wait));
                }
                Admission::QuinnBlocked => {
                    self.cursor = id;
                    return Ok(Stop::QuinnBlocked);
                }
            };
            let ceiling = connection.ceiling().context("QUIC datagrams disabled")?;
            ensure!(
                ceiling >= stride + 32,
                "tile session path MTU decreased; reconnect required"
            );
            // Construct and check every fragment before reserving or admitting
            // any of them. A send error after reservation closes this session;
            // there is no partial-tile retry or refunded quantum.
            let packets = pending.update.fragments(stride)?;
            let packet_bytes: usize = packets.iter().map(Vec::len).sum();
            ensure!(
                packets.len() == ticket.charge().datagrams() as usize
                    && packet_bytes
                        + packets.len() * (SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET)
                        == ticket.charge().bytes() as usize,
                "tile fragments differ from whole-tile quantum reservation"
            );
            sender.reserve(ticket)?;
            let pending = self.remove(id).unwrap();
            self.max_residence_ms = self
                .max_residence_ms
                .max(pending.queued.elapsed().as_secs_f64() * 1000.);
            let trace_started = self.trace.as_ref().and_then(|trace| trace.stamp());
            for (accepted, packet) in packets.iter().enumerate() {
                if let Err(error) = connection.admit(packet) {
                    stage_trace::keyed(
                        &self.trace,
                        TraceKind::SequenceSendError,
                        pending.stage_key,
                        accepted as u32,
                        TraceOutcome::Error,
                    );
                    return Err(error);
                }
            }
            trace_visit.observed(pending.stage_key, trace_started);
            if stage_trace::tile_marker(pending.update.width, pending.update.tile) {
                stage_trace::keyed(
                    &self.trace,
                    TraceKind::MarkerTileAdmitted,
                    pending.stage_key,
                    pending.update.tile as u32,
                    TraceOutcome::Accepted,
                );
            }
            self.admitted += 1;
            self.datagrams += packets.len() as u64;
            self.body_bytes += pending.update.body.len() as u64;
            self.wire_bytes += packet_bytes as u64;
        }
        Ok(Stop::Empty)
    }
    fn snapshot(&self) -> serde_json::Value {
        json!({"generated_tiles":self.generated,"pending_tiles":self.updates.len(),"pending_body_bytes":self.bytes,
            "peak_pending_tiles":self.peak_count,"peak_pending_body_bytes":self.peak_bytes,
            "pending_body_limit_bytes":PENDING_BYTES,"pending_slot_limit":MAX_TILES,"pending_ttl_ms":250,
            "replaced_tiles":self.replaced,"evicted_tiles":self.evicted,"expired_tiles":self.expired,
            "superseded_tiles":self.superseded,"admitted_tiles":self.admitted,"datagrams_enqueued":self.datagrams,
            "encoded_body_bytes_admitted":self.body_bytes,"application_datagram_bytes":self.wire_bytes,
            "max_observed_pending_residence_ms":self.max_residence_ms,
            "oldest_pending_age_ms":self.updates.values().map(|value|value.queued.elapsed().as_secs_f64()*1000.).fold(0.,f64::max)})
    }
}

struct Completed {
    encoder: TileEncoder,
    result: Result<gliss_tiles::EncodeSummary>,
    barrier: u64,
    observation: u64,
    elapsed: Duration,
}

fn listen_for_ready(policy: Policy, quantum: Option<&Sender>, legacy: &ReadyGate) -> bool {
    policy == Policy::Ready
        && match quantum {
            Some(sender) => sender.timer().is_none(),
            None => legacy.deadline().is_none(),
        }
}

pub(super) async fn run_media(
    connection: &Connection,
    app: &Arc<App>,
    source: &mut Source,
    outgoing: &mut SendStream,
    controls: &mut mpsc::Receiver<serde_json::Value>,
) -> Result<()> {
    let codec = app.args.datagram_codec.unwrap_or_default();
    let send_policy = app.args.tile_send_policy.unwrap_or_default();
    let ready_notify = Arc::new(tokio::sync::Notify::new());
    let mut ready_gate = ReadyGate::default();
    let policy = codec.policy().context("tile media requires tile policy")?;
    let stride = connection
        .max_datagram_size()
        .context("QUIC datagrams unavailable")?
        .min(1200)
        .saturating_sub(32);
    ensure!(
        (1024..=1168).contains(&stride),
        "tile datagram ceiling below1056 bytes"
    );
    let first = source
        .frames
        .wait_for(|value| value.is_some())
        .await
        .context("tile source closed before first frame")?
        .clone()
        .unwrap();
    let mut current = Some(first);
    let mut encoder = Some(TileEncoder::with_payload(1, policy, stride)?);
    let congestion = app
        .datagram
        .as_ref()
        .context("tile endpoint missing")?
        .congestion_control();
    let first_frame = &current.as_ref().unwrap().frame;
    write_control(outgoing,&with_stream_metadata(json!({"type":"hello","codec":codec.name(),"datagram_codec":codec.name(),
        "protocol":codec.protocol(),"datagram_protocol":"GLT2","transport":"webtransport-datagram","policy":policy,
        "payload_stride":stride,"max_datagram_bytes":stride+32,"tile_side":32,"initial_epoch":1,
        "width":first_frame.width,"height":first_frame.height,"independent_tiles":true,"independent_frames":false,
        "media_retransmission":false,"congestion_control":congestion,"fps_limit":app.args.fps,"refresh_hz":15,
        "source":if app.args.demo{"demo"}else if app.args.dbus_address.is_some(){"dbus"}else{"rfb"},
        "input_metadata":"Reliable forwarding; tile sequence is not a causal input ID.",
        "tile_send_policy":send_policy.name()}),app)).await?;
    let queue = Arc::new(Mutex::new(Pending {
        trace: app.stage_trace.clone(),
        ..Pending::default()
    }));
    let barrier = Arc::new(AtomicU64::new(0));
    let mut quantum = app
        .args
        .tile_send_quantum
        .map(|selected| Sender::new(selected, 0))
        .transpose()?;
    let _cancel_on_exit = CancelOnDrop {
        barrier: barrier.clone(),
        queue: queue.clone(),
    };
    let mut jobs = JoinSet::<Result<Completed>>::new();
    let mut observation = 1u64;
    let mut last_encoded = 0u64;
    let mut sequence = 0u32;
    let mut dirty = true;
    let mut force_all = false;
    let mut refresh_debt = 0usize;
    let mut refresh = tokio::time::interval(Duration::from_secs_f64(1. / 15.));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pacing = tokio::time::interval(Duration::from_secs_f64(1. / f64::from(app.args.fps)));
    pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut telemetry = tokio::time::interval(Duration::from_secs(1));
    telemetry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_resync = None::<Instant>;
    let mut encode_ns = 0u64;
    let mut updates = 0u64;
    let mut dirty_tiles = 0u64;
    let mut refresh_tiles = 0u64;
    let mut compared_tiles = 0u64;
    stage_trace::emit(&app.stage_trace, TraceKind::MediaObserved, || {
        stage_trace::fields(
            current.as_ref().and_then(|value| value.stage_generation),
            None,
            None,
        )
    });
    loop {
        if let Some(recorder) = &app.benchmark {
            recorder.check()?;
        }
        // Copy the identity before select. Resize/resync invalidation cancels
        // the gate's token; a stale ready future cannot revive old work.
        let quantum_timer = quantum.as_ref().and_then(Sender::timer);
        let quantum_deadline = quantum
            .as_ref()
            .map(Sender::deadline)
            .transpose()?
            .flatten();
        tokio::select! {
            _ = ready_notify.notified(), if listen_for_ready(send_policy, quantum.as_ref(), &ready_gate) => {
                if let Some(sender) = &mut quantum {
                    sender.notify_at(tokio::time::Instant::now(), barrier.load(Ordering::Acquire))?;
                } else {
                    ready_gate.arm(tokio::time::Instant::now());
                }
            },
            _ = wait_ready(quantum_deadline), if quantum_timer.is_some() => {
                let sender = quantum.as_mut().unwrap();
                let result = sender.timer_at(tokio::time::Instant::now(), quantum_timer.unwrap(), barrier.load(Ordering::Acquire))?;
                debug_assert!(!result.encode_slot);
                if let Some(wake) = result.handle {
                    let bytes = flush_pending_paced(&queue, connection, stride, sender, wake, FlushOrigin::Timer)?;
                    app.bytes.fetch_add(bytes, Ordering::Relaxed);
                }
            },
            _ = wait_ready(ready_gate.deadline()), if quantum.is_none() && send_policy == Policy::Ready && ready_gate.deadline().is_some() => {
                let result = ready_gate.on_wake(Wake::ReadyTimer, tokio::time::Instant::now(),
                    || flush_pending(&queue, connection, stride))?;
                debug_assert!(!result.encode_slot);
                app.bytes.fetch_add(result.bytes, Ordering::Relaxed);
            },
            _ = connection.closed() => break,
            Some(value) = controls.recv() => {
                if value.get("type").and_then(|v|v.as_str()) == Some("resync") {
                    if last_resync.is_none_or(|previous|previous.elapsed() >= Duration::from_secs(1)) {
                        last_resync = Some(Instant::now());
                        invalidate_current(&queue, &barrier)?;
                        if let Some(sender) = &mut quantum { sender.invalidate(barrier.load(Ordering::Acquire))?; }
                        force_all=true; dirty=true;
                        current=source.frames.borrow().clone();
                    }
                } else { write_control(outgoing,&value).await?; }
            },
            changed = source.frames.changed() => {
                if changed.is_err() { break; }
                let next=source.frames.borrow_and_update().clone();
                stage_trace::emit(&app.stage_trace,TraceKind::MediaObserved,||stage_trace::fields(next.as_ref().and_then(|value|value.stage_generation),None,None));
                let resized = match (&current,&next) {
                    (Some(a),Some(b)) => (a.frame.width,a.frame.height)!=(b.frame.width,b.frame.height),
                    _ => false,
                };
                if resized {
                    invalidate_current(&queue, &barrier)?;
                    if let Some(sender) = &mut quantum { sender.invalidate(barrier.load(Ordering::Acquire))?; }
                    force_all=true;
                }
                current=next; observation=observation.checked_add(1).context("tile capture counter exhausted")?; dirty=true;
            },
            Some(event) = source.events.recv() => {
                write_control(outgoing,&event).await?;
                if event.get("type").and_then(|v|v.as_str()) == Some("error") { break; }
            },
            _ = refresh.tick() => { refresh_debt=refresh_debt.saturating_add(1).min(15); },
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                let completed=result.context("tile encoding task failed")??;
                encoder=Some(completed.encoder);
                encode_ns=encode_ns.saturating_add(completed.elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
                if completed.barrier==barrier.load(Ordering::Acquire) {
                    let summary=completed.result?;
                    last_encoded=completed.observation;
                    updates+=1; dirty_tiles+=summary.dirty_tiles as u64;
                    refresh_tiles+=summary.refresh_tiles as u64; compared_tiles+=summary.source_tiles_compared as u64;
                    app.frames.fetch_add(1,Ordering::Relaxed);
                }
            },
            _ = pacing.tick() => {
                let result = if let Some(sender) = &mut quantum {
                    let result = sender.tick_at(tokio::time::Instant::now(), barrier.load(Ordering::Acquire))?;
                    let bytes = flush_pending_paced(&queue, connection, stride, sender,
                        result.handle.context("tile quantum tick lacks a wake")?, FlushOrigin::Tick)?;
                    crate::tile_send::WakeResult { bytes, encode_slot: result.encode_slot }
                } else {
                    ready_gate.on_wake(Wake::Tick, tokio::time::Instant::now(),
                        || flush_pending(&queue, connection, stride))?
                };
                app.bytes.fetch_add(result.bytes,Ordering::Relaxed);
                if result.encode_slot && jobs.is_empty() && current.is_some() && (dirty || force_all || refresh_debt>0) {
                    let captured=current.clone().unwrap();
                    ensure!(captured.frame.pixels.len().saturating_mul(3)+PENDING_BYTES+MAX_TILES*256+16384 <= CODEC_MEMORY,
                        "tile codec memory budget exceeded");
                    let count=usize::from(captured.frame.width).div_ceil(32)*usize::from(captured.frame.height).div_ceil(32);
                    let quota=count.div_ceil(15).saturating_mul(refresh_debt).min(count);
                    refresh_debt=0;
                    let mut next_encoder=encoder.take().context("tile encoder busy")?;
                    let force=force_all; force_all=false; dirty=false;
                    let source_is_new=observation!=last_encoded;
                    sequence=sequence.wrapping_add(1);
                    let packet_sequence=sequence;
                    let job_observation=observation;
                    let job_barrier=barrier.load(Ordering::Acquire);
                    let generation=app.benchmark.as_ref().map(|recorder|recorder.generation(&captured.frame)).transpose()?;
                    let queue=queue.clone(); let barrier=barrier.clone(); let permits=app.encoders.clone();
                    let recorder=app.benchmark.clone();
                    let ready_notify=ready_notify.clone();
                    let job_trace=app.stage_trace.clone();
                    let job_actor=stage_trace::lease(&job_trace)?;
                    let blocking_actor=stage_trace::lease(&job_trace)?;
                    let stage_generation=captured.stage_generation;
                    stage_trace::emit(&job_trace,TraceKind::EncodeSelected,||stage_trace::fields(stage_generation,None,Some(packet_sequence)));
                    jobs.spawn(async move {
                        let _trace_actor=job_actor;
                        let permit=permits.acquire_owned().await?;
                        stage_trace::emit(&job_trace,TraceKind::EncodePermit,||stage_trace::fields(stage_generation,None,Some(packet_sequence)));
                        tokio::task::spawn_blocking(move || {
                            let _trace_actor=blocking_actor;
                            let _permit=permit; let started=Instant::now();
                            stage_trace::emit(&job_trace,TraceKind::EncodeStarted,||stage_trace::fields(stage_generation,None,Some(packet_sequence)));
                            let mut associated=false;
                            let mut body_count=0u32; let mut last_body=None; let mut last_key=None;
                            let result=next_encoder.encode_with_refresh_budget(&captured.frame,packet_sequence,source_is_new,quota,force,|tile| {
                                let key=stage_generation.map(|generation|FrameKey { generation,epoch:tile.epoch,sequence:tile.sequence });
                                if let (Some(trace),Some(key))=(&job_trace,key) {
                                    if body_count==0 { stage_trace::keyed(&job_trace,TraceKind::FirstBodyReady,Some(key),0,TraceOutcome::Observed); }
                                    last_body=trace.stamp(); last_key=Some(key);
                                    if stage_trace::tile_marker(tile.width,tile.tile) { stage_trace::keyed(&job_trace,TraceKind::MarkerTileReady,Some(key),tile.tile as u32,TraceOutcome::Observed); }
                                }
                                body_count+=1;
                                let inserted = insert_current_traced(&queue, &barrier, job_barrier, tile, |tile| {
                                    if !associated {
                                        if let (Some(recorder),Some(generation))=(&recorder,generation) {
                                            recorder.associate(generation,u64::from(tile.epoch),packet_sequence,"GLT2")?;
                                        }
                                        associated=true;
                                    }
                                    Ok(())
                                },stage_generation)?;
                                // insert_current returned and released the mutex
                                // before posting this persistent/coalesced permit.
                                if inserted && send_policy == Policy::Ready { ready_notify.notify_one(); }
                                Ok(())
                            });
                            if let (Some(trace),Some(at),Some(key))=(&job_trace,last_body,last_key) {
                                trace.at(at,TraceKind::LastBodyReady,stage_trace::fields(Some(key.generation),Some(key.epoch),Some(key.sequence)));
                            }
                            stage_trace::emit(&job_trace,TraceKind::EncodeCompleted,||TraceFields { outcome:if result.is_ok(){TraceOutcome::Accepted}else{TraceOutcome::Error},
                                ..stage_trace::fields(stage_generation,result.as_ref().ok().map(|value|value.epoch),Some(packet_sequence)) });
                            stage_trace::emit(&job_trace,TraceKind::SequenceGenerated,||TraceFields { count:Some(body_count),
                                ..stage_trace::fields(stage_generation,result.as_ref().ok().map(|value|value.epoch).or(last_key.map(|key|key.epoch)),Some(packet_sequence)) });
                            Completed {encoder:next_encoder,result,barrier:job_barrier,observation:job_observation,
                                elapsed:started.elapsed()}
                        }).await.context("blocking tile encoder failed")
                    });
                }
            },
            _ = telemetry.tick(), if app.args.stream_stats => {
                let stats=connection.quic_connection().stats();
                let mut value=queue.lock().unwrap().snapshot();
                let object=value.as_object_mut().unwrap();
                for (key,item) in json!({"type":"stream_stats","transport":"webtransport-datagram","codec":codec.name(),
                    "protocol":codec.protocol(),"congestion_control":congestion,"payload_stride":stride,"max_datagram_bytes":stride+32,
                    "tile_send_policy":send_policy.name(),"tile_send_quantum":tile_quantum::selected_name(app.args.tile_send_quantum),
                    "tile_quantum":quantum.as_ref().map(Sender::snapshot),
                    "encoded_updates":updates,"dirty_tiles_selected":dirty_tiles,"refresh_tiles_selected":refresh_tiles,
                    "source_tiles_compared":compared_tiles,"encode_ms_total":encode_ns as f64/1_000_000.,
                    "quic_datagram_frames_sent":stats.frame_tx.datagram,"congestion_window_bytes":stats.path.cwnd,
                    "congestion_events":stats.path.congestion_events,"lost_bytes":stats.path.lost_bytes,"lost_packets":stats.path.lost_packets,
                    "quic_packets_sent":stats.path.sent_packets,"current_mtu":stats.path.current_mtu,
                    "udp_tx_datagrams":stats.udp_tx.datagrams,"udp_tx_bytes":stats.udp_tx.bytes,
                    "udp_rx_datagrams":stats.udp_rx.datagrams,"udp_rx_bytes":stats.udp_rx.bytes,
                    "datagram_send_buffer_space_bytes":connection.quic_connection().datagram_send_buffer_space(),
                    "send_queue_bytes":app.datagram.as_ref().unwrap().send_buffer,"rtt_ms":connection.rtt().as_secs_f64()*1000.}).as_object().unwrap() {
                    object.insert(key.clone(),item.clone());
                }
                write_control(outgoing,&value).await?;
            },
        }
    }
    Ok(())
}
fn flush_pending(
    queue: &Mutex<Pending>,
    connection: &impl TileDatagrams,
    stride: usize,
) -> Result<u64> {
    let mut pending = queue
        .lock()
        .map_err(|_| anyhow::anyhow!("tile pending queue poisoned"))?;
    let before = pending.wire_bytes;
    pending.send(connection, stride)?;
    Ok(pending.wire_bytes - before)
}

fn flush_pending_paced(
    queue: &Mutex<Pending>,
    connection: &impl TileDatagrams,
    stride: usize,
    sender: &mut Sender,
    wake: WakeHandle,
    origin: FlushOrigin,
) -> Result<u64> {
    let started = sender.begin_flush(wake, origin)?;
    let (reason, summary) = {
        let mut pending = queue
            .lock()
            .map_err(|_| anyhow::anyhow!("tile pending queue poisoned"))?;
        let before = (pending.admitted, pending.datagrams, pending.wire_bytes);
        let reason = pending.send_paced(connection, stride, sender, wake)?;
        (
            reason,
            FlushSummary {
                tiles: pending.admitted - before.0,
                datagrams: pending.datagrams - before.1,
                application_bytes: pending.wire_bytes - before.2,
            },
        )
    };
    sender.finish_flush(wake, reason, summary, started)?;
    Ok(summary.application_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        sync::{atomic::AtomicUsize, mpsc as std_channel},
        task::{Context as TaskContext, Poll, Waker},
    };
    // Release every blocked checkpoint before joining any owned thread. Drop
    // repeats this on parent assertion/recv/spawn failure without panicking.
    struct ThreadCleanup {
        releases: Vec<std_channel::Sender<()>>,
        threads: Vec<std::thread::JoinHandle<Result<()>>>,
    }
    impl ThreadCleanup {
        fn new(release: std_channel::Sender<()>) -> Self {
            Self {
                releases: vec![release],
                threads: Vec::with_capacity(2),
            }
        }
        fn spawn<F>(&mut self, work: F) -> Result<()>
        where
            F: FnOnce() -> Result<()> + Send + 'static,
        {
            self.threads.push(std::thread::Builder::new().spawn(work)?);
            Ok(())
        }
        fn cleanup(&mut self) -> Vec<String> {
            for release in self.releases.drain(..) {
                let _ = release.send(());
            }
            let mut failures = Vec::new();
            for handle in self.threads.drain(..) {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => failures.push(format!("owned test thread failed: {error:#}")),
                    Err(_) => failures.push("owned test thread panicked".to_owned()),
                }
            }
            failures
        }
        fn finish(mut self) -> Result<()> {
            let failures = self.cleanup();
            ensure!(failures.is_empty(), "{}", failures.join("; "));
            Ok(())
        }
    }
    impl Drop for ThreadCleanup {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }
    type FirstSendHook = Box<dyn FnOnce() + Send>;
    struct FakeDatagrams {
        available: AtomicUsize,
        packets: Mutex<Vec<Vec<u8>>>,
        first_send: Mutex<Option<FirstSendHook>>,
    }
    impl FakeDatagrams {
        fn new(bytes: usize) -> Self {
            Self {
                available: AtomicUsize::new(bytes),
                packets: Mutex::new(Vec::new()),
                first_send: Mutex::new(None),
            }
        }
    }
    impl TileDatagrams for FakeDatagrams {
        fn space(&self) -> usize {
            self.available.load(Ordering::Acquire)
        }
        fn ceiling(&self) -> Option<usize> {
            Some(1200)
        }
        fn admit(&self, packet: &[u8]) -> Result<()> {
            if let Some(hook) = self.first_send.lock().unwrap().take() {
                hook();
            }
            self.available.fetch_sub(
                packet.len() + SESSION_HEADER_BUDGET + QUINN_DATAGRAM_METADATA_BUDGET,
                Ordering::AcqRel,
            );
            self.packets.lock().unwrap().push(packet.to_vec());
            Ok(())
        }
    }
    fn permit_ready(notify: &tokio::sync::Notify) -> bool {
        let waiter = notify.notified();
        let mut waiter = pin!(waiter);
        let mut context = TaskContext::from_waker(Waker::noop());
        matches!(waiter.as_mut().poll(&mut context), Poll::Ready(()))
    }
    #[test]
    fn invalidation_before_insert_rejects_old_job_in_both_policies() {
        for _policy in [Policy::Tick, Policy::Ready] {
            let queue = Mutex::new(Pending::default());
            let barrier = AtomicU64::new(0);
            invalidate_current(&queue, &barrier).unwrap();
            assert!(
                insert_current(&queue, &barrier, 0, tile(0, 1), |_| panic!(
                    "stale job cannot associate"
                ))
                .is_err()
            );
            assert!(queue.lock().unwrap().updates.is_empty());
            assert!(insert_current(&queue, &barrier, 1, tile(0, 2), |_| Ok(())).unwrap());
        }
    }
    #[test]
    fn insert_linearizes_before_invalidation_and_clear_removes_it() {
        let queue = Arc::new(Mutex::new(Pending::default()));
        let barrier = Arc::new(AtomicU64::new(0));
        let (entered, at_association) = std_channel::channel();
        let (release, released) = std_channel::channel();
        let mut owned = ThreadCleanup::new(release);
        {
            let queue = queue.clone();
            let barrier = barrier.clone();
            owned
                .spawn(move || {
                    let inserted = insert_current(&queue, &barrier, 0, tile(0, 1), |_| {
                        entered.send(()).map_err(|_| {
                            anyhow::anyhow!("association checkpoint receiver closed")
                        })?;
                        released
                            .recv_timeout(Duration::from_secs(2))
                            .map_err(|_| anyhow::anyhow!("association checkpoint not released"))?;
                        Ok(())
                    })?;
                    ensure!(inserted, "test producer was not inserted");
                    Ok(())
                })
                .unwrap();
        }
        at_association.recv_timeout(Duration::from_secs(2)).unwrap();
        // Regression: the old recorder association ran outside the mutex.
        let held = matches!(queue.try_lock(), Err(std::sync::TryLockError::WouldBlock));
        {
            let queue = queue.clone();
            let barrier = barrier.clone();
            owned
                .spawn(move || invalidate_current(&queue, &barrier))
                .unwrap();
        }
        owned.finish().unwrap();
        assert!(held);
        assert_eq!(barrier.load(Ordering::Acquire), 1);
        assert!(queue.lock().unwrap().updates.is_empty());
        assert!(insert_current(&queue, &barrier, 0, tile(1, 2), |_| Ok(())).is_err());
    }
    #[test]
    fn insertion_during_flush_waits_then_its_notification_survives() {
        let queue = Arc::new(Mutex::new(Pending::default()));
        let barrier = Arc::new(AtomicU64::new(0));
        insert_current(&queue, &barrier, 0, tile(0, 1), |_| Ok(())).unwrap();
        let sink = Arc::new(FakeDatagrams::new(SEND_BUFFER));
        let notify = Arc::new(tokio::sync::Notify::new());
        let (entered, sending) = std_channel::channel();
        let (release, released) = std_channel::channel();
        let mut owned = ThreadCleanup::new(release);
        *sink.first_send.lock().unwrap() = Some(Box::new(move || {
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
        }));
        {
            let queue = queue.clone();
            let sink = sink.clone();
            owned
                .spawn(move || flush_pending(&queue, sink.as_ref(), 1024).map(|_| ()))
                .unwrap();
        }
        sending.recv_timeout(Duration::from_secs(2)).unwrap();
        let held = matches!(queue.try_lock(), Err(std::sync::TryLockError::WouldBlock));
        {
            let queue = queue.clone();
            let barrier = barrier.clone();
            let notify = notify.clone();
            owned
                .spawn(move || {
                    let inserted = insert_current(&queue, &barrier, 0, tile(1, 1), |_| Ok(()))?;
                    ensure!(inserted, "test producer was not inserted");
                    notify.notify_one();
                    Ok(())
                })
                .unwrap();
        }
        owned.finish().unwrap();
        assert!(held);
        assert_eq!(queue.lock().unwrap().updates.len(), 1);
        assert!(permit_ready(&notify));
        assert!(!permit_ready(&notify));
        flush_pending(&queue, sink.as_ref(), 1024).unwrap();
        assert_eq!(queue.lock().unwrap().admitted, 2);
        assert_eq!(sink.packets.lock().unwrap().len(), 8);
    }
    #[test]
    fn owned_test_threads_are_released_and_joined_during_parent_unwind() {
        let finished = Arc::new(AtomicUsize::new(0));
        let observed = finished.clone();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (release, released) = std_channel::channel();
            let mut owned = ThreadCleanup::new(release);
            owned
                .spawn(move || {
                    released
                        .recv_timeout(Duration::from_secs(2))
                        .map_err(|_| anyhow::anyhow!("cleanup did not release worker"))?;
                    observed.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                })
                .unwrap();
            panic!("intentional parent failure before explicit cleanup");
        }));
        assert!(panic.is_err());
        assert_eq!(finished.load(Ordering::Acquire), 1);
    }
    #[test]
    fn owned_test_cleanup_joins_remaining_workers_after_one_panics() {
        let finished = Arc::new(AtomicUsize::new(0));
        let observed = finished.clone();
        let (release, released) = std_channel::channel();
        let mut owned = ThreadCleanup::new(release);
        owned
            .spawn(|| -> Result<()> { panic!("intentional owned worker failure") })
            .unwrap();
        owned
            .spawn(move || {
                released
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| anyhow::anyhow!("cleanup did not release second worker"))?;
                observed.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
            .unwrap();
        assert!(owned.finish().is_err());
        assert_eq!(finished.load(Ordering::Acquire), 1);
    }
    #[test]
    fn ready_tick_coincidence_has_one_admission_and_one_encode_slot_in_either_order() {
        for order in [
            [Wake::Tick, Wake::ReadyTimer],
            [Wake::ReadyTimer, Wake::Tick],
        ] {
            let queue = Mutex::new(Pending::default());
            let barrier = AtomicU64::new(0);
            let sink = FakeDatagrams::new(SEND_BUFFER);
            insert_current(&queue, &barrier, 0, tile(0, 1), |_| Ok(())).unwrap();
            let mut gate = ReadyGate::default();
            let base = tokio::time::Instant::now();
            gate.arm(base);
            let mut encode_slots = 0;
            let mut bytes = 0;
            for wake in order {
                let result = gate
                    .on_wake(wake, base + Duration::from_millis(1), || {
                        flush_pending(&queue, &sink, 1024)
                    })
                    .unwrap();
                encode_slots += usize::from(result.encode_slot);
                bytes += result.bytes;
            }
            let pending = queue.lock().unwrap();
            assert_eq!(pending.admitted, 1);
            assert_eq!(pending.wire_bytes, bytes);
            assert_eq!(encode_slots, 1);
            assert_eq!(sink.packets.lock().unwrap().len(), 4);
        }
    }
    #[test]
    fn no_capacity_does_not_rearm_or_encode_and_existing_tick_recovers() {
        let queue = Mutex::new(Pending::default());
        let barrier = AtomicU64::new(0);
        let sink = FakeDatagrams::new(0);
        insert_current(&queue, &barrier, 0, tile(0, 1), |_| Ok(())).unwrap();
        let mut gate = ReadyGate::default();
        let base = tokio::time::Instant::now();
        gate.arm(base);
        let blocked = gate
            .on_wake(Wake::ReadyTimer, base + Duration::from_millis(1), || {
                flush_pending(&queue, &sink, 1024)
            })
            .unwrap();
        assert_eq!(blocked.bytes, 0);
        assert!(!blocked.encode_slot);
        assert!(gate.deadline().is_none());
        assert_eq!(queue.lock().unwrap().updates.len(), 1);
        assert!(sink.packets.lock().unwrap().is_empty());
        gate.on_wake(Wake::ReadyTimer, base + Duration::from_millis(2), || {
            panic!("blocked ready send cannot busy retry")
        })
        .unwrap();
        sink.available.store(SEND_BUFFER, Ordering::Release);
        let recovered = gate
            .on_wake(Wake::Tick, base + Duration::from_millis(17), || {
                flush_pending(&queue, &sink, 1024)
            })
            .unwrap();
        assert!(recovered.encode_slot);
        assert!(recovered.bytes > 0);
        assert_eq!(queue.lock().unwrap().admitted, 1);
    }
    #[test]
    fn late_notifications_after_resize_resync_or_close_never_restore_old_tiles() {
        for cause in ["resize", "resync", "close"] {
            let queue = Arc::new(Mutex::new(Pending::default()));
            let barrier = Arc::new(AtomicU64::new(0));
            let notify = tokio::sync::Notify::new();
            let sink = FakeDatagrams::new(SEND_BUFFER);
            insert_current(&queue, &barrier, 0, tile(0, 1), |_| Ok(())).unwrap();
            let mut gate = ReadyGate::default();
            let base = tokio::time::Instant::now();
            gate.arm(base);
            if cause == "close" {
                drop(CancelOnDrop {
                    queue: queue.clone(),
                    barrier: barrier.clone(),
                });
            } else {
                invalidate_current(&queue, &barrier).unwrap();
            }
            // A producer may notify after releasing the insertion mutex but
            // after the actor's later clear. It is only a bounded wake.
            notify.notify_one();
            assert!(permit_ready(&notify));
            let old_wake = gate
                .on_wake(Wake::ReadyTimer, base + Duration::from_millis(1), || {
                    flush_pending(&queue, &sink, 1024)
                })
                .unwrap();
            assert!(!old_wake.encode_slot);
            assert_eq!(old_wake.bytes, 0);
            assert!(queue.lock().unwrap().updates.is_empty());
            assert!(sink.packets.lock().unwrap().is_empty());
            assert!(insert_current(&queue, &barrier, 0, tile(1, 2), |_| Ok(())).is_err());
        }
    }
    fn tile(id: usize, sequence: u32) -> EncodedTile {
        EncodedTile {
            epoch: 1,
            sequence,
            width: 1280,
            height: 800,
            tile: id,
            kind: 0,
            policy: 0,
            raw: 4096,
            body: vec![7; 4096],
        }
    }
    fn sender_at(base: tokio::time::Instant) -> Sender {
        Sender::at(tile_quantum::Selection::Bounded, 0, base).unwrap()
    }
    fn paced_tick(
        queue: &Mutex<Pending>,
        sink: &FakeDatagrams,
        sender: &mut Sender,
        now: tokio::time::Instant,
        generation: u64,
    ) -> u64 {
        let wake = sender.tick_at(now, generation).unwrap();
        assert!(wake.encode_slot);
        flush_pending_paced(
            queue,
            sink,
            1024,
            sender,
            wake.handle.unwrap(),
            FlushOrigin::Tick,
        )
        .unwrap()
    }
    fn paced_timer(
        queue: &Mutex<Pending>,
        sink: &FakeDatagrams,
        sender: &mut Sender,
        now: tokio::time::Instant,
        generation: u64,
    ) -> u64 {
        let timer = sender.timer().unwrap();
        let wake = sender.timer_at(now, timer, generation).unwrap();
        assert!(!wake.encode_slot);
        flush_pending_paced(
            queue,
            sink,
            1024,
            sender,
            wake.handle.unwrap(),
            FlushOrigin::Timer,
        )
        .unwrap()
    }
    #[test]
    fn paced_ready_notifications_coalesce_while_armed_and_leave_one_later_permit() {
        let notify = tokio::sync::Notify::new();
        let queue = Mutex::new(Pending::default());
        let sink = FakeDatagrams::new(SEND_BUFFER);
        let legacy = ReadyGate::default();
        let base = tokio::time::Instant::now();
        let mut sender = sender_at(base);
        assert!(listen_for_ready(Policy::Ready, Some(&sender), &legacy));
        assert!(!listen_for_ready(Policy::Tick, Some(&sender), &legacy));
        notify.notify_one();
        assert!(permit_ready(&notify));
        sender.notify_at(base, 0).unwrap();
        let deadline = sender.deadline().unwrap();
        for _ in 0..100 {
            notify.notify_one();
            assert!(!listen_for_ready(Policy::Ready, Some(&sender), &legacy));
            assert_eq!(sender.deadline().unwrap(), deadline);
        }
        assert_eq!(
            paced_timer(
                &queue,
                &sink,
                &mut sender,
                base + Duration::from_millis(1),
                0
            ),
            0
        );
        assert!(listen_for_ready(Policy::Ready, Some(&sender), &legacy));
        assert!(permit_ready(&notify));
        assert!(!permit_ready(&notify));
        sender
            .notify_at(base + Duration::from_millis(1), 0)
            .unwrap();
        assert_eq!(
            sender.deadline().unwrap(),
            Some(base + Duration::from_millis(2))
        );
        assert_eq!(sender.snapshot()["whole_flush_calls_completed"], 1);
    }
    #[test]
    fn paced_flush_reserves_all_four_fragments_and_retries_the_current_head_next_slot() {
        let queue = Mutex::new(Pending::default());
        for id in 0..5 {
            queue
                .lock()
                .unwrap()
                .insert(tile(id, 1), Instant::now())
                .unwrap();
        }
        let sink = FakeDatagrams::new(SEND_BUFFER);
        let base = tokio::time::Instant::now();
        let mut sender = sender_at(base);
        assert_eq!(
            paced_tick(&queue, &sink, &mut sender, base, 0),
            3 * (4096 + 4 * 32)
        );
        assert_eq!(sink.packets.lock().unwrap().len(), 12);
        assert_eq!(queue.lock().unwrap().cursor, 3);
        assert_eq!(queue.lock().unwrap().updates.len(), 2);
        assert_eq!(
            sender.deadline().unwrap(),
            Some(base + Duration::from_millis(1))
        );
        // A new capture replaces an unsent tile. The follow-up reads current
        // pending state, rather than retaining the obsolete encoded body.
        queue
            .lock()
            .unwrap()
            .insert(tile(3, 2), Instant::now())
            .unwrap();
        assert_eq!(
            paced_timer(
                &queue,
                &sink,
                &mut sender,
                base + Duration::from_millis(1),
                0
            ),
            2 * (4096 + 4 * 32)
        );
        assert!(sender.timer().is_none());
        assert!(queue.lock().unwrap().updates.is_empty());
        let packets = sink.packets.lock().unwrap();
        assert_eq!(packets.len(), 20);
        for (id, chunks) in packets.chunks(4).enumerate() {
            for (index, packet) in chunks.iter().enumerate() {
                let header = gliss_tiles::parse_with_payload(packet, 1024).unwrap();
                assert_eq!((header.tile, header.index, header.count), (id, index, 4));
                assert_eq!(header.sequence, if id == 3 { 2 } else { 1 });
            }
        }
        let stats = sender.snapshot();
        assert_eq!(stats["whole_flush_calls_completed"], 2);
        assert_eq!(stats["application_datagrams_enqueued"], 20);
        assert_eq!(stats["charged_bytes_reserved"], 5 * 4768);
        assert_eq!(stats["byte_budget_blocks"], 1);
        assert_eq!(stats["max_budget_slot_charge_bytes"], 3 * 4768);
        assert_eq!(stats["later_slot_flush_calls"], 1);
    }
    #[test]
    fn paced_real_compressed_tiles_hit_datagram_limit_and_keep_exact_packets_and_pixels() {
        let source = Frame::new(640, 32).unwrap();
        let mut encoder = TileEncoder::with_payload(1, 0, 1024).unwrap();
        let mut originals = Vec::new();
        encoder
            .encode_with_refresh_budget(&source, 1, true, 0, true, |tile| {
                originals.push(tile);
                Ok(())
            })
            .unwrap();
        assert_eq!(originals.len(), 20);
        assert!(
            originals
                .iter()
                .all(|tile| tile.fragment_count(1024).unwrap() == 1)
        );
        let expected: Vec<Vec<u8>> = originals
            .iter()
            .flat_map(|tile| tile.fragments(1024).unwrap())
            .collect();
        let queue = Mutex::new(Pending::default());
        for tile in originals {
            queue.lock().unwrap().insert(tile, Instant::now()).unwrap();
        }
        let sink = FakeDatagrams::new(SEND_BUFFER);
        let base = tokio::time::Instant::now();
        let mut sender = sender_at(base);
        paced_tick(&queue, &sink, &mut sender, base, 0);
        assert_eq!(sink.packets.lock().unwrap().len(), 16);
        assert_eq!(queue.lock().unwrap().cursor, 16);
        assert!(
            sender.snapshot()["remaining_charge_bytes"]
                .as_i64()
                .unwrap()
                > 0
        );
        assert_eq!(sender.snapshot()["remaining_datagrams"], 0);
        paced_timer(
            &queue,
            &sink,
            &mut sender,
            base + Duration::from_millis(1),
            0,
        );
        let packets = sink.packets.lock().unwrap();
        assert_eq!(*packets, expected);
        let mut decoder = gliss_tiles::Decoder::with_payload(1024).unwrap();
        for packet in packets.iter() {
            decoder.ingest(packet, 0).unwrap();
        }
        assert_eq!(decoder.frame, Some(source));
        assert_eq!(sender.snapshot()["datagram_budget_blocks"], 1);
        assert_eq!(
            sender.snapshot()["max_budget_slot_application_datagrams"],
            16
        );
    }
    #[test]
    fn paced_tick_and_ready_share_one_budget_in_both_coincident_orders() {
        for ready_first in [false, true] {
            let queue = Mutex::new(Pending::default());
            for id in 0..7 {
                queue
                    .lock()
                    .unwrap()
                    .insert(tile(id, 1), Instant::now())
                    .unwrap();
            }
            let sink = FakeDatagrams::new(SEND_BUFFER);
            let base = tokio::time::Instant::now();
            let now = base + Duration::from_millis(1);
            let mut sender = sender_at(base);
            sender.notify_at(base, 0).unwrap();
            let original = sender.timer().unwrap();
            if ready_first {
                paced_timer(&queue, &sink, &mut sender, now, 0);
                paced_tick(&queue, &sink, &mut sender, now, 0);
            } else {
                paced_tick(&queue, &sink, &mut sender, now, 0);
                // Tick replaced the coalescing token with the later budget
                // token. The previous timer cannot duplicate the flush.
                assert!(sender.timer_at(now, original, 0).unwrap().handle.is_none());
            }
            assert_eq!(sink.packets.lock().unwrap().len(), 12);
            assert_eq!(queue.lock().unwrap().cursor, 3);
            assert_eq!(
                sender.deadline().unwrap(),
                Some(base + Duration::from_millis(2))
            );
            assert_eq!(sender.snapshot()["max_budget_slot_charge_bytes"], 14304);
            assert_eq!(
                sender.snapshot()["max_budget_slot_application_datagrams"],
                12
            );
        }
    }
    #[test]
    fn paced_quinn_block_clears_followup_without_spinning_and_tick_recovers() {
        let queue = Mutex::new(Pending::default());
        for id in 0..5 {
            queue
                .lock()
                .unwrap()
                .insert(tile(id, 1), Instant::now())
                .unwrap();
        }
        let sink = FakeDatagrams::new(SEND_BUFFER);
        let base = tokio::time::Instant::now();
        let mut sender = sender_at(base);
        paced_tick(&queue, &sink, &mut sender, base, 0);
        let old = sender.timer().unwrap();
        sink.available.store(0, Ordering::Release);
        assert_eq!(
            paced_timer(
                &queue,
                &sink,
                &mut sender,
                base + Duration::from_millis(1),
                0
            ),
            0
        );
        assert!(sender.timer().is_none());
        assert_eq!(queue.lock().unwrap().cursor, 3);
        for ms in 2..10 {
            let result = sender
                .timer_at(base + Duration::from_millis(ms), old, 0)
                .unwrap();
            assert!(!result.encode_slot);
            assert!(result.handle.is_none());
        }
        assert_eq!(sender.snapshot()["whole_flush_calls_completed"], 2);
        sink.available.store(SEND_BUFFER, Ordering::Release);
        paced_tick(
            &queue,
            &sink,
            &mut sender,
            base + Duration::from_millis(17),
            0,
        );
        assert!(queue.lock().unwrap().updates.is_empty());
        assert_eq!(sink.packets.lock().unwrap().len(), 20);
        assert_eq!(sender.snapshot()["quinn_blocked_flushes"], 1);
    }
    #[test]
    fn paced_empty_then_new_work_keeps_shared_credit_and_fixed_slot_boundary() {
        let queue = Mutex::new(Pending::default());
        queue
            .lock()
            .unwrap()
            .insert(tile(0, 1), Instant::now())
            .unwrap();
        let sink = FakeDatagrams::new(SEND_BUFFER);
        let base = tokio::time::Instant::now();
        let mut sender = sender_at(base);
        paced_tick(&queue, &sink, &mut sender, base, 0);
        assert!(sender.timer().is_none());
        for id in 1..5 {
            queue
                .lock()
                .unwrap()
                .insert(tile(id, 1), Instant::now())
                .unwrap();
        }
        paced_tick(
            &queue,
            &sink,
            &mut sender,
            base + Duration::from_micros(999),
            0,
        );
        assert_eq!(sink.packets.lock().unwrap().len(), 12);
        assert_eq!(
            sender.deadline().unwrap(),
            Some(base + Duration::from_millis(1))
        );
        paced_timer(
            &queue,
            &sink,
            &mut sender,
            base + Duration::from_millis(1),
            0,
        );
        assert_eq!(sink.packets.lock().unwrap().len(), 20);
        assert_eq!(sender.snapshot()["max_budget_slot_charge_bytes"], 14304);
        assert!(sender.timer().is_none());
        // New ready work after an empty flush gets a new fixed coalesced
        // wake, even if no frame tick happens first.
        queue
            .lock()
            .unwrap()
            .insert(tile(5, 1), Instant::now())
            .unwrap();
        sender
            .notify_at(base + Duration::from_millis(2), 0)
            .unwrap();
        sender
            .notify_at(base + Duration::from_micros(2500), 0)
            .unwrap();
        assert_eq!(
            sender.deadline().unwrap(),
            Some(base + Duration::from_millis(3))
        );
        paced_timer(
            &queue,
            &sink,
            &mut sender,
            base + Duration::from_millis(3),
            0,
        );
        assert!(queue.lock().unwrap().updates.is_empty());
    }
    #[test]
    fn paced_generation_cancel_rejects_old_jobs_and_timers_without_free_credit() {
        for cause in ["resize", "resync", "close"] {
            let queue = Arc::new(Mutex::new(Pending::default()));
            let barrier = Arc::new(AtomicU64::new(0));
            for id in 0..5 {
                insert_current(&queue, &barrier, 0, tile(id, 1), |_| Ok(())).unwrap();
            }
            let sink = FakeDatagrams::new(SEND_BUFFER);
            let base = tokio::time::Instant::now();
            let mut sender = sender_at(base);
            paced_tick(&queue, &sink, &mut sender, base, 0);
            let stale = sender.timer().unwrap();
            if cause == "close" {
                drop(CancelOnDrop {
                    queue: queue.clone(),
                    barrier: barrier.clone(),
                });
            } else {
                invalidate_current(&queue, &barrier).unwrap();
            }
            assert!(insert_current(&queue, &barrier, 0, tile(3, 2), |_| Ok(())).is_err());
            if cause == "close" {
                // The actor drops the sender and its select future on close;
                // it never schedules a fresh generation after that point.
                assert!(queue.lock().unwrap().updates.is_empty());
                assert_eq!(sink.packets.lock().unwrap().len(), 12);
                continue;
            }
            // Same-slot new state has the former 2080 bytes/4 datagrams of
            // credit. A new generation cannot buy another burst.
            let mut next = tile(3, 3);
            next.epoch = 2;
            if cause == "resize" {
                next.width = 640;
            }
            insert_current(&queue, &barrier, 1, next, |_| Ok(())).unwrap();
            paced_tick(
                &queue,
                &sink,
                &mut sender,
                base + Duration::from_micros(500),
                1,
            );
            assert_eq!(sink.packets.lock().unwrap().len(), 12);
            assert_eq!(sender.snapshot()["remaining_charge_bytes"], 2080);
            assert_eq!(sender.snapshot()["generation_cancellations"], 1);
            assert!(
                sender
                    .timer_at(base + Duration::from_millis(1), stale, 1)
                    .unwrap()
                    .handle
                    .is_none()
            );
            paced_timer(
                &queue,
                &sink,
                &mut sender,
                base + Duration::from_millis(1),
                1,
            );
            let packets = sink.packets.lock().unwrap();
            assert_eq!(packets.len(), 16);
            for packet in &packets[12..] {
                let header = gliss_tiles::parse_with_payload(packet, 1024).unwrap();
                assert_eq!((header.epoch, header.sequence, header.tile), (2, 3, 3));
            }
        }
    }
    #[test]
    fn paced_ttl_expiry_drops_stale_pending_and_does_not_widen_existing_limit() {
        let queue = Mutex::new(Pending::default());
        for id in 0..5 {
            queue
                .lock()
                .unwrap()
                .insert(tile(id, 1), Instant::now())
                .unwrap();
        }
        let sink = FakeDatagrams::new(SEND_BUFFER);
        let base = tokio::time::Instant::now();
        let mut sender = sender_at(base);
        paced_tick(&queue, &sink, &mut sender, base, 0);
        for value in queue.lock().unwrap().updates.values_mut() {
            value.queued = Instant::now() - PENDING_TTL;
        }
        paced_timer(
            &queue,
            &sink,
            &mut sender,
            base + Duration::from_millis(1),
            0,
        );
        assert_eq!(queue.lock().unwrap().expired, 2);
        assert_eq!(sink.packets.lock().unwrap().len(), 12);
        assert!(sender.timer().is_none());
    }
    #[test]
    fn unlimited_path_still_flushes_every_available_whole_tile_at_once() {
        let queue = Mutex::new(Pending::default());
        for id in 0..5 {
            queue
                .lock()
                .unwrap()
                .insert(tile(id, 1), Instant::now())
                .unwrap();
        }
        let sink = FakeDatagrams::new(SEND_BUFFER);
        assert_eq!(
            flush_pending(&queue, &sink, 1024).unwrap(),
            5 * (4096 + 4 * 32)
        );
        assert_eq!(sink.packets.lock().unwrap().len(), 20);
        assert!(queue.lock().unwrap().updates.is_empty());
    }
    #[test]
    fn thousand_complete_tiles_fit_without_the_partial_assembly_limit() {
        let mut queue = Pending::default();
        for id in 0..1000 {
            queue.insert(tile(id, 1), Instant::now()).unwrap();
        }
        assert_eq!(queue.updates.len(), 1000);
        assert_eq!(queue.evicted, 0);
        assert_eq!(queue.bytes, 4096000);
        queue.insert(tile(999, 2), Instant::now()).unwrap();
        assert_eq!(queue.updates[&999].update.sequence, 2);
        assert_eq!(queue.replaced, 1);
        queue.insert(tile(999, 1), Instant::now()).unwrap();
        assert_eq!(queue.updates[&999].update.sequence, 2);
    }
    #[test]
    fn byte_pressure_and_age_keep_complete_updates_bounded() {
        let mut queue = Pending::default();
        let then = Instant::now() - Duration::from_secs(1);
        queue.insert(tile(1, 1), then).unwrap();
        queue.expire(Instant::now());
        assert!(queue.updates.is_empty());
        assert_eq!(queue.expired, 1);
        for sequence in 1..4 {
            for id in 0..1000 {
                queue.insert(tile(id, sequence), Instant::now()).unwrap();
            }
        }
        assert!(queue.bytes <= PENDING_BYTES);
        assert_eq!(queue.updates.len(), 1000);
    }
}
