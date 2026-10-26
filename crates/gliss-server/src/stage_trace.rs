//! Opt-in terminal-only stage evidence. Streaming appends never perform IO.
#[path = "recorder/mod.rs"]
#[allow(dead_code)] // The validated recorder has public standalone APIs unused by this adapter.
pub mod recorder;
use anyhow::{Context, Result, ensure};
use gliss_core::Frame;
pub use recorder::{EventKind as Kind, Fields, FrameKey, Marker, Outcome, Stamp};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

static RECORDER: recorder::Recorder = recorder::Recorder::new();
pub type Handle = Option<Arc<Trace>>;
const MAX_ACTORS: usize = 64;
const CLOSED: usize = 1usize << (usize::BITS - 1);
struct Files {
    binary: File,
    manifest: File,
}
pub struct Trace {
    origin: u64,
    domain: u64,
    files: Mutex<Option<Files>>,
    path: PathBuf,
    stopping: AtomicBool,
    claimed: AtomicBool,
    actors: AtomicUsize,
    peak_actors: AtomicUsize,
    late_actors: AtomicU64,
    clock_errors: AtomicU64,
    clock_probes: AtomicU64,
    next_generation: AtomicU64,
    shutdown: AtomicU64,
    failures: AtomicU64,
}
fn clock() -> Option<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0
        || value.tv_sec < 0
        || value.tv_nsec < 0
    {
        return None;
    }
    (value.tv_sec as u64)
        .checked_mul(1_000_000_000)?
        .checked_add(value.tv_nsec as u64)
}
fn create(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute()
            && !path.exists()
            && path.parent().context("trace parent")?.canonicalize()? == path.parent().unwrap(),
        "fresh canonical trace destination"
    );
    let info = path.parent().unwrap().metadata()?;
    ensure!(
        info.is_dir() && info.uid() == unsafe { libc::getuid() } && info.mode() & 0o077 == 0,
        "private owned trace parent"
    );
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}
pub fn prepare(args: &crate::Args) -> Result<Handle> {
    let Some(path) = &args.stage_trace else {
        return Ok(None);
    };
    ensure!(
        args.bind.ip().is_loopback()
            && args.max_clients == 1
            && !args.demo
            && args.dbus_address.is_none()
            && !args.shared_dbus
            && args.benchmark_capture_dir.is_none()
            && args
                .datagram_bind
                .is_none_or(|address| address.ip().is_loopback()),
        "stage tracing requires one fixed RFB source and loopback diagnostic listeners"
    );
    ensure!(
        args.rfb_fd.is_some()
            || args.vnc.as_ref().is_some_and(|value| value
                .parse::<std::net::SocketAddr>()
                .is_ok_and(|address| address.ip().is_loopback())),
        "stage trace requires a trusted fixed loopback or inherited RFB source"
    );
    let binary = create(path)?;
    let manifest = create(&path.with_extension("manifest.json"))?;
    let origin = clock().context("trace monotonic origin")?;
    let mut resolution = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC, &mut resolution) } == 0
            && resolution.tv_sec >= 0
            && resolution.tv_nsec >= 0,
        "actual trace clock resolution"
    );
    let domain = (u64::from(std::process::id()) << 32) | 1;
    RECORDER
        .start(recorder::ClockDescription {
            domain_id: domain,
            process_id: std::process::id(),
            kind: recorder::ClockKind::ClockMonotonic,
            origin_monotonic_ns: Some(origin),
            resolution_ns: Some(
                resolution.tv_sec as u64 * 1_000_000_000 + resolution.tv_nsec as u64,
            ),
        })
        .map_err(|error| anyhow::anyhow!("trace start: {error:?}"))?;
    Ok(Some(Arc::new(Trace {
        origin,
        domain,
        files: Mutex::new(Some(Files { binary, manifest })),
        path: path.clone(),
        stopping: AtomicBool::new(false),
        claimed: AtomicBool::new(false),
        actors: AtomicUsize::new(0),
        peak_actors: AtomicUsize::new(0),
        late_actors: AtomicU64::new(0),
        clock_errors: AtomicU64::new(0),
        clock_probes: AtomicU64::new(0),
        next_generation: AtomicU64::new(1),
        shutdown: AtomicU64::new(0),
        failures: AtomicU64::new(0),
    })))
}
pub fn emit(handle: &Handle, kind: Kind, fields: impl FnOnce() -> Fields) {
    if let Some(trace) = handle {
        trace.record(kind, fields());
    }
}
#[derive(Debug)]
pub enum ClockProbeError {
    Closed,
    Full,
    Clock,
}
pub struct Lease(Arc<Trace>);
pub fn lease(handle: &Handle) -> Result<Option<Lease>> {
    handle
        .as_ref()
        .map(|trace| {
            ensure!(
                trace.actor_started(),
                "trace actor admission closed or full"
            );
            Ok(Lease(trace.clone()))
        })
        .transpose()
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.actor_stopped();
    }
}
impl Trace {
    fn actor_started(&self) -> bool {
        let accepted = self
            .actors
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                if state & CLOSED != 0 || state >= MAX_ACTORS {
                    None
                } else {
                    Some(state + 1)
                }
            });
        match accepted {
            Ok(previous) => {
                self.peak_actors.fetch_max(previous + 1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.late_actors.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
    fn actor_stopped(&self) {
        self.actors.fetch_sub(1, Ordering::SeqCst);
    }
    fn close_admission(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.actors.fetch_or(CLOSED, Ordering::SeqCst);
    }
    pub fn claim_source(&self) -> Result<()> {
        ensure!(
            !self.stopping.load(Ordering::SeqCst) && !self.claimed.swap(true, Ordering::SeqCst),
            "one source claim per traced worker"
        );
        Ok(())
    }
    pub fn stamp(&self) -> Option<Stamp> {
        match clock().and_then(|now| now.checked_sub(self.origin)) {
            Some(delta_ns) => Some(Stamp {
                domain_id: self.domain,
                delta_ns,
            }),
            None => {
                self.clock_errors.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
    pub fn record(&self, kind: Kind, fields: Fields) {
        if let Some(stamp) = self.stamp() {
            self.at(stamp, kind, fields);
        }
    }
    pub fn at(&self, stamp: Stamp, kind: Kind, fields: Fields) {
        let _ = RECORDER.append(stamp, kind, fields);
    }
    pub fn publication_started(&self, update: Option<u64>) -> u64 {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        self.record(
            Kind::CapturePublished,
            Fields {
                generation: Some(generation),
                request_ordinal: update,
                outcome: Outcome::Observed,
                ..Fields::default()
            },
        );
        generation
    }
    pub fn publication_finished(
        &self,
        generation: u64,
        frame: &Frame,
        update: Option<u64>,
        accepted: bool,
    ) {
        self.record(
            Kind::CapturePublished,
            Fields {
                generation: Some(generation),
                request_ordinal: update,
                outcome: if accepted {
                    Outcome::Accepted
                } else {
                    Outcome::Rejected
                },
                ..Fields::default()
            },
        );
        let (source_marker, response_marker) = markers(frame);
        self.record(
            Kind::CaptureMarker,
            Fields {
                generation: Some(generation),
                source_marker,
                response_marker,
                ..Fields::default()
            },
        );
    }
    pub fn clock_probe(&self) -> std::result::Result<(u64, Stamp, u64), ClockProbeError> {
        if self.is_stopping() {
            return Err(ClockProbeError::Closed);
        }
        let previous = self
            .clock_probes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                (value < 64).then_some(value + 1)
            })
            .map_err(|_| ClockProbeError::Full)?;
        let stamp = self.stamp().ok_or(ClockProbeError::Clock)?;
        Ok((previous + 1, stamp, self.origin))
    }
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
    pub fn note_shutdown(&self, reason: u64) {
        self.shutdown.store(reason, Ordering::Relaxed);
    }
    pub fn failure(&self, reason: u64) {
        self.failures.fetch_or(reason, Ordering::Relaxed);
    }
    pub async fn finish(&self, run_ok: bool) -> Result<()> {
        self.close_admission();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while self.actors.load(Ordering::SeqCst) & !CLOSED != 0
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let active = self.actors.load(Ordering::SeqCst) & !CLOSED;
        let mut files = self
            .files
            .lock()
            .map_err(|_| anyhow::anyhow!("trace file lock poisoned"))?
            .take()
            .context("one terminal trace export")?;
        let mut complete = false;
        let mut exported = None;
        let mut recorder_summary = serde_json::Value::Null;
        let mut export_error = None;
        if active == 0 {
            let result = (|| -> Result<()> {
                let frozen = RECORDER
                    .try_freeze()
                    .map_err(|error| anyhow::anyhow!("trace freeze: {error:?}"))?;
                let summary = frozen.summary();
                complete = run_ok
                    && self.claimed.load(Ordering::SeqCst)
                    && summary.complete()
                    && self.clock_errors.load(Ordering::Relaxed) == 0
                    && self.failures.load(Ordering::Relaxed) == 0
                    && self.late_actors.load(Ordering::Relaxed) == 0
                    && self.actors.load(Ordering::SeqCst) == CLOSED;
                recorder_summary = serde_json::json!({"recorded":summary.recorded,"capacity":summary.capacity,"complete":summary.complete(),
                "contention":summary.contention,"overflow":summary.overflow,"wrong_clock":summary.wrong_clock,"poison":summary.poison,
                "losses":summary.losses.iter().map(|loss|serde_json::json!({"count":loss.count,"timed_count":loss.timed_count,"first_ns":loss.first_ns,"last_ns":loss.last_ns})).collect::<Vec<_>>()});
                exported = Some(frozen.write_binary(&mut files.binary)?);
                files.binary.sync_all()?;
                complete &= self.actors.load(Ordering::SeqCst) == CLOSED
                    && self.late_actors.load(Ordering::Relaxed) == 0;
                Ok(())
            })();
            if let Err(error) = result {
                complete = false;
                export_error = Some(format!("{error:#}"));
            }
        } else {
            export_error = Some(
                "tracked actors did not quiesce before the fixed1s export boundary".to_owned(),
            );
        }
        let actual_bytes = match files.binary.metadata() {
            Ok(info) => Some(info.len()),
            Err(error) => {
                complete = false;
                export_error = Some(format!("export metadata: {error}"));
                None
            }
        };
        if complete && actual_bytes != exported.map(|value| value as u64) {
            complete = false;
            export_error =
                Some("export file extent differs from completed binary write".to_owned());
        }
        let manifest = serde_json::json!({"schema":"gliss-native-stage-trace-v1","status":if complete{"complete"}else{"incomplete"},
            "path":self.path,"export_bytes":exported,"actual_file_bytes":actual_bytes,"export_error":export_error,"clock":"CLOCK_MONOTONIC process-relative ns","origin_monotonic_ns":self.origin,
            "domain_id":self.domain,"pid":std::process::id(),"run_returned_ok":run_ok,"shutdown_reason_code":self.shutdown.load(Ordering::Relaxed),
            "shutdown_reason_codes":{"0":"run returned without retained shutdown signal","1":"SIGINT/ctrl-c graceful path","2":"graphics lease expired","3":"SIGTERM graceful path","4":"signal registration/read failed"},
            "outer_owner_required":"workload complete, balanced inputs, normal browser close, exact exit0 and no escalation; graceful signal alone is not a failed trace",
            "active_actors":active,"peak_actors":self.peak_actors.load(Ordering::Relaxed),"late_or_overbound_actors":self.late_actors.load(Ordering::Relaxed),
            "clock_errors":self.clock_errors.load(Ordering::Relaxed),"recorder":recorder_summary,
            "clock_probe_requests":self.clock_probes.load(Ordering::Relaxed),"clock_probe_limit":64,
            "internal_failure_bits":self.failures.load(Ordering::Relaxed),"internal_failure_codes":{"1":"datagram session drain timeout/abort","2":"source input teardown timeout/canceled","4":"authenticated session returned error","8":"signal registration/read failed","16":"HTTP or detached viewer shutdown grace elapsed"},
            "storage":{"recorder_budget":recorder::STORAGE_BUDGET_BYTES,"recorder_owned_bound":recorder::OWNED_FOOTPRINT_BOUND_BYTES,
                "handle_bytes":std::mem::size_of::<Trace>(),"actor_guard_bytes":std::mem::size_of::<Lease>(),"maximum_actor_guards":MAX_ACTORS,
                "cloned_trace_path_capacity_bytes":self.path.capacity(),
                "trace_arc_header_allowance_bytes":2*std::mem::size_of::<usize>(),"rfb_observer_allocation_allowance_bytes":128,
                "rfb_actor_guard_bytes":gliss_rfb::trace::ACTOR_GUARD_BYTES,"rfb_terminal_guard_bytes":gliss_rfb::trace::TERMINAL_GUARD_BYTES,"maximum_simultaneous_rfb_terminal_guards":3,
                "pending_trace_keys_max_bytes":8160*std::mem::size_of::<Option<FrameKey>>(),"admission_visit_stack_bytes":std::mem::size_of::<AdmissionVisit>(),
                "scope":"recorder bound plus explicit Arc/observer/actor/queue wrappers; not process RSS"},
            "rfb_ordinals":"write command ordinals and framebuffer update ordinals are separate; no echoed request ID and no asserted one-to-one association",
            "rfb_pixel_outcome":"UpdateDecoded Observed0 means pixel-bearing RAW/CopyRect received, not a pixel-value difference; Empty10/Metadata11 remain distinct; Canceled6 never means completed decoding/writing",
            "markers":"fixed1280x800; source24,332 response464,332;32bits 8x24 double row;3x3 means, alpha255, luma<=64 or>=192, inverse row; mixed/invalid retained; no scanout atomicity claim",
            "media_coverage":"GLT2 source generation/selection/encode and whole-tile events where emitted; H264 media stages are unobserved; browser stages require separate future proof",
            "admission_groups":"FirstAdmission/LastAdmission bracket adjacent successful whole tiles within one flush; LastAdmission.count is this group only. Sum all groups per exact key; no SequenceAdmitted full-sequence assertion is emitted. Pre-encode epoch is unknown until actual body/completion evidence joins it.",
            "privacy":"typed identifiers/counts only; no key values, text, pixels, packets or credentials"});
        serde_json::to_writer(&mut files.manifest, &manifest)?;
        files.manifest.write_all(b"\n")?;
        files.manifest.sync_all()?;
        ensure!(
            complete,
            "native stage trace incomplete; retained terminal manifest"
        );
        Ok(())
    }
}
pub fn rfb_observer(handle: &Handle) -> gliss_rfb::trace::Sink {
    handle
        .as_ref()
        .map(|trace| Arc::new(RfbObserver(trace.clone())) as Arc<dyn gliss_rfb::trace::Observer>)
}
struct RfbObserver(Arc<Trace>);
impl gliss_rfb::trace::Observer for RfbObserver {
    fn actor_started(&self) -> bool {
        self.0.actor_started()
    }
    fn actor_stopped(&self) {
        self.0.actor_stopped();
    }
    fn record(&self, value: gliss_rfb::trace::Record) {
        use gliss_rfb::trace::{Kind as R, Outcome as O};
        let kind = match value.kind {
            R::CommandQueued => Kind::RfbCommandQueued,
            R::WriteStarted => Kind::RfbWriteStarted,
            R::WriteCompleted => Kind::RfbWriteCompleted,
            R::UpdateHeader => Kind::UpdateHeader,
            R::UpdateDecoded => Kind::UpdateDecoded,
            R::CloneStarted => Kind::CloneStarted,
            R::CloneCompleted => Kind::CloneCompleted,
            R::EventQueued => Kind::SourceEventQueued,
            R::EventDequeued => Kind::SourceEventDequeued,
        };
        let outcome = match value.outcome {
            O::Observed | O::Pixels => Outcome::Observed,
            O::Accepted => Outcome::Accepted,
            O::Rejected => Outcome::Rejected,
            O::Canceled => Outcome::Canceled,
            O::Error => Outcome::Error,
            O::Empty => Outcome::EmptyUpdate,
            O::Metadata => Outcome::MetadataOnly,
            O::Ambiguous => Outcome::Ambiguous,
        };
        self.0.record(
            kind,
            Fields {
                input_id: value.input_id.map(u64::from),
                request_ordinal: Some(value.ordinal),
                count: Some(value.count),
                outcome,
                ..Fields::default()
            },
        );
        if value.kind == R::CommandQueued && value.capture_request {
            self.0.record(
                Kind::CaptureRequestQueued,
                Fields {
                    request_ordinal: Some(value.ordinal),
                    count: Some(value.count),
                    outcome,
                    ..Fields::default()
                },
            );
        }
    }
}
fn word(frame: &Frame, x: usize) -> Marker {
    if frame.width != 1280 || frame.height != 800 || !frame.is_valid() {
        return Marker::Invalid;
    }
    let mut result = 0u32;
    for bit in 0..32 {
        let mut values = [0u8; 2];
        for (row, value) in values.iter_mut().enumerate() {
            let mut luma = 0.0;
            for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    let px = (x + bit * 8 + 4).checked_add_signed(dx).unwrap();
                    let py = (332 + row * 24 + 12usize).checked_add_signed(dy).unwrap();
                    let at = (py * 1280 + px) * 4;
                    let p = &frame.pixels[at..at + 4];
                    if p[3] != 255 {
                        return Marker::Invalid;
                    }
                    luma += 0.2126 * f64::from(p[0])
                        + 0.7152 * f64::from(p[1])
                        + 0.0722 * f64::from(p[2]);
                }
            }
            *value = if luma / 9.0 <= 64.0 {
                0
            } else if luma / 9.0 >= 192.0 {
                1
            } else {
                return Marker::Invalid;
            };
        }
        if values[0] == values[1] {
            return Marker::Mixed;
        }
        result |= u32::from(values[0]) << bit;
    }
    Marker::Valid(result)
}
pub fn markers(frame: &Frame) -> (Marker, Marker) {
    (word(frame, 24), word(frame, 464))
}
pub fn fields(generation: Option<u64>, epoch: Option<u32>, sequence: Option<u32>) -> Fields {
    Fields {
        generation,
        epoch,
        sequence,
        ..Fields::default()
    }
}

pub fn tile_marker(width: u16, tile: usize) -> bool {
    let columns = usize::from(width).div_ceil(32);
    if columns == 0 {
        return false;
    }
    let x = tile % columns * 32;
    let y = tile / columns * 32;
    x < 720 && x + 32 > 464 && y < 380 && y + 32 > 332
}
pub fn keyed(handle: &Handle, kind: Kind, key: Option<FrameKey>, count: u32, outcome: Outcome) {
    if let (Some(trace), Some(key)) = (handle, key) {
        trace.record(
            kind,
            Fields {
                count: Some(count),
                outcome,
                ..fields(Some(key.generation), Some(key.epoch), Some(key.sequence))
            },
        );
    }
}
/// Adjacent whole-tile groups within one flush visit. Repeated groups are
/// intentional: LastAdmission.count describes this group, never complete
/// sequence admission. Readers retain all groups and their disposition evidence.
pub struct AdmissionVisit {
    trace: Handle,
    current: Option<(FrameKey, Stamp, Stamp, u32)>,
}
impl AdmissionVisit {
    pub fn new(trace: &Handle) -> Self {
        Self {
            trace: trace.clone(),
            current: None,
        }
    }
    pub fn observed(&mut self, key: Option<FrameKey>, started: Option<Stamp>) {
        let (Some(trace), Some(key), Some(started)) = (&self.trace, key, started) else {
            return;
        };
        let Some(ended) = trace.stamp() else {
            return;
        };
        if self.current.as_ref().is_some_and(|value| value.0 != key) {
            self.flush();
        }
        match &mut self.current {
            Some(value) => {
                value.2 = ended;
                value.3 += 1;
            }
            None => self.current = Some((key, started, ended, 1)),
        }
    }
    fn flush(&mut self) {
        if let (Some(trace), Some((key, first, last, count))) = (&self.trace, self.current.take()) {
            let common = fields(Some(key.generation), Some(key.epoch), Some(key.sequence));
            trace.at(
                first,
                Kind::FirstAdmission,
                Fields {
                    outcome: Outcome::Observed,
                    ..common
                },
            );
            trace.at(
                last,
                Kind::LastAdmission,
                Fields {
                    count: Some(count),
                    outcome: Outcome::Accepted,
                    ..common
                },
            );
        }
    }
}
impl Drop for AdmissionVisit {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct OwnedTestDirectory(PathBuf);
    impl OwnedTestDirectory {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "gliss-stage-trace-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }
    impl Drop for OwnedTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("trace.bin"));
            let _ = std::fs::remove_file(self.0.join("trace.manifest.json"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    fn trace_without_recorder() -> Arc<Trace> {
        Arc::new(Trace {
            origin: 0,
            domain: 1,
            files: Mutex::new(None),
            path: PathBuf::new(),
            stopping: AtomicBool::new(false),
            claimed: AtomicBool::new(false),
            actors: AtomicUsize::new(0),
            peak_actors: AtomicUsize::new(0),
            late_actors: AtomicU64::new(0),
            clock_errors: AtomicU64::new(0),
            clock_probes: AtomicU64::new(0),
            next_generation: AtomicU64::new(1),
            shutdown: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        })
    }
    #[test]
    fn disabled_emit_does_not_evaluate_any_clock_or_fields() {
        emit(&None, Kind::ControlReceived, || {
            panic!("disabled field construction")
        });
    }
    #[test]
    fn closure_preserves_an_admitted_unstarted_actor_until_its_guard_drops() {
        let trace = trace_without_recorder();
        let handle = Some(trace.clone());
        let admitted = lease(&handle).unwrap().unwrap();
        // This represents a future queued with its guard but never polled.
        let queued = async move {
            let _guard = admitted;
            std::future::pending::<()>().await
        };
        trace.close_admission();
        assert!(trace.is_stopping());
        assert_eq!(trace.actors.load(Ordering::SeqCst), CLOSED | 1);
        assert!(lease(&handle).is_err());
        drop(queued);
        assert_eq!(trace.actors.load(Ordering::SeqCst), CLOSED);
        assert!(lease(&handle).is_err());
        assert_eq!(trace.actors.load(Ordering::SeqCst), CLOSED);
        assert_eq!(trace.late_actors.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn actor_capacity_refusal_never_mints_an_untracked_guard() {
        let trace = trace_without_recorder();
        let handle = Some(trace.clone());
        let guards = (0..MAX_ACTORS)
            .map(|_| lease(&handle).unwrap().unwrap())
            .collect::<Vec<_>>();
        assert!(lease(&handle).is_err());
        assert_eq!(trace.actors.load(Ordering::SeqCst), MAX_ACTORS);
        trace.close_admission();
        drop(guards);
        assert_eq!(trace.actors.load(Ordering::SeqCst), CLOSED);
        assert_eq!(trace.peak_actors.load(Ordering::Relaxed), MAX_ACTORS);
    }
    #[test]
    fn destination_is_exclusive_private_and_rejects_a_shared_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = OwnedTestDirectory::new();
        let path = dir.0.join("trace.bin");
        let file = create(&path).unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert!(create(&path).is_err());
        drop(file);
        std::fs::remove_file(&path).unwrap();
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(create(&path).is_err());
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn freeze_failure_retains_incomplete_manifest_and_real_file_extent() {
        let dir = OwnedTestDirectory::new();
        let binary = dir.0.join("trace.bin");
        let manifest = binary.with_extension("manifest.json");
        let mut trace = trace_without_recorder();
        let value = Arc::get_mut(&mut trace).unwrap();
        value.path = binary.clone();
        *value.files.lock().unwrap() = Some(Files {
            binary: create(&binary).unwrap(),
            manifest: create(&manifest).unwrap(),
        });
        // No recorder was started: this exercises the real freeze failure path.
        assert!(trace.finish(true).await.is_err());
        let result: serde_json::Value =
            serde_json::from_reader(File::open(manifest).unwrap()).unwrap();
        assert_eq!(result["status"], "incomplete");
        assert!(result["export_bytes"].is_null());
        assert_eq!(
            result["actual_file_bytes"],
            binary.metadata().unwrap().len()
        );
        assert!(
            result["export_error"]
                .as_str()
                .unwrap()
                .contains("NotStarted")
        );
    }
    #[test]
    fn causal_probe_cap_and_shutdown_keep_actual_counts() {
        let trace = trace_without_recorder();
        for expected in 1..=64 {
            let (index, stamp, origin) = trace.clock_probe().unwrap();
            assert_eq!(index, expected);
            assert_eq!(stamp.domain_id, trace.domain);
            assert_eq!(origin, trace.origin);
        }
        assert!(matches!(trace.clock_probe(), Err(ClockProbeError::Full)));
        assert_eq!(trace.clock_probes.load(Ordering::Relaxed), 64);
        trace.close_admission();
        assert!(matches!(trace.clock_probe(), Err(ClockProbeError::Closed)));
        assert_eq!(trace.clock_probes.load(Ordering::Relaxed), 64);
    }
    #[test]
    fn marker_validation_uses_every_sample_alpha_and_inverse_bit() {
        let mut frame = Frame::new(1280, 800).unwrap();
        let id = 0x8a54_1234u32;
        for x in [24, 464] {
            for bit in 0..32 {
                for row in 0..2 {
                    for dy in -1isize..=1 {
                        for dx in -1isize..=1 {
                            let value = if ((id >> bit) & 1) ^ row as u32 != 0 {
                                240
                            } else {
                                16
                            };
                            let px = (x + bit * 8 + 4usize).checked_add_signed(dx).unwrap();
                            let py = (332 + row * 24 + 12usize).checked_add_signed(dy).unwrap();
                            frame.pixels[(py * 1280 + px) * 4..(py * 1280 + px) * 4 + 4]
                                .copy_from_slice(&[value, value, value, 255]);
                        }
                    }
                }
            }
        }
        assert_eq!(markers(&frame), (Marker::Valid(id), Marker::Valid(id)));
        frame.pixels[(343 * 1280 + 27) * 4 + 3] = 0;
        assert_eq!(markers(&frame).0, Marker::Invalid);
        frame.pixels[(343 * 1280 + 27) * 4 + 3] = 255;
        for py in 367..=369 {
            for px in 27..=29 {
                frame.pixels[(py * 1280 + px) * 4..(py * 1280 + px) * 4 + 4]
                    .copy_from_slice(&[16, 16, 16, 255]);
            }
        }
        assert_eq!(markers(&frame).0, Marker::Mixed);
    }
}
