//! Experimental App-owned D-Bus pixels. Authorization, input and media state stay private.
use super::Captured;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{FutureExt, future::BoxFuture};
use gliss_core::Frame;
use gliss_dbus::{Cursor, DbusSource, Event};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
};

const MAX_VIEWERS: usize = 128;

/// Trusted startup configuration; no browser-supplied target or authorization.
pub(super) struct FixedTarget {
    pub address: String,
    pub vm_uuid: String,
    pub console: u32,
    pub diagnostics: bool,
    pub max_viewers: usize,
}

/// The producer can observe forwarding but receives no input sending capability.
#[derive(Clone)]
pub(super) struct ForwardedId(Arc<AtomicU32>);
impl ForwardedId {
    pub fn new(value: Arc<AtomicU32>) -> Self {
        Self(value)
    }
    fn load(&self) -> u32 {
        self.0.load(Ordering::Acquire)
    }
}

pub(super) struct ViewerChannels {
    pub forwarded_id: ForwardedId,
    pub dimensions: Arc<AtomicU32>,
    pub frames: watch::Sender<Option<Captured>>,
    pub events: mpsc::Sender<Value>,
}

trait EventSource: Send {
    /// Membership signals may cancel this wait; do not consume an event until
    /// the future completes. Native DbusSource and the mock receiver meet this.
    fn read_event(&mut self) -> BoxFuture<'_, Result<Event>>;
    fn close(self: Box<Self>) -> BoxFuture<'static, Result<()>>;
}
// Native registration is bounded at ten seconds; viewer cancellation never
// cancels this future. Failure does not prove partial peer cleanup, so it is
// terminal for this producer and forbids automatic replacement.
type Factory = Arc<dyn Fn() -> BoxFuture<'static, Result<Box<dyn EventSource>>> + Send + Sync>;
struct NativeSource(DbusSource);
impl EventSource for NativeSource {
    fn read_event(&mut self) -> BoxFuture<'_, Result<Event>> {
        self.0.read_event().boxed()
    }
    fn close(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        async move { self.0.close_checked().await }.boxed()
    }
}

#[derive(Clone)]
enum Phase {
    Idle,
    Starting,
    Running,
    Closing(Option<Arc<str>>),
    Failed(Arc<str>),
    Stopped(Option<Arc<str>>),
}
impl Phase {
    fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Closing(_) => "closing",
            Self::Failed(_) => "failed",
            Self::Stopped(_) => "stopped",
        }
    }
}
#[derive(Clone, Default)]
struct Controls {
    availability: Option<(u64, bool)>,
    cursor: Option<(u64, Arc<Value>)>,
    stats: Option<(u64, Arc<Value>)>,
    error: Option<Arc<Value>>,
}
struct Member {
    epoch: u64,
    forwarded_id: ForwardedId,
    dimensions: Arc<AtomicU32>,
    frames: watch::Sender<Option<Captured>>,
    controls: watch::Sender<Arc<Controls>>,
}
struct State {
    epoch: u64,
    next_member: u64,
    phase: Phase,
    shutdown: bool,
    members: BTreeMap<u64, Member>,
    latest: Option<Arc<Frame>>,
    controls: Controls,
    cursor: Option<Cursor>,
    cursor_initialized: bool,
    control_revision: u64,
    publications: u64,
    last_stats: Option<Instant>,
}
struct Inner {
    state: Mutex<State>,
    changed: Notify,
    phase: watch::Sender<Phase>,
    instance: String,
    diagnostics: bool,
    max_viewers: usize,
}
impl Inner {
    fn set_phase(&self, state: &mut State, phase: Phase) {
        state.phase = phase.clone();
        self.phase.send_replace(phase);
    }
    fn unregister(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        if state.members.remove(&id).is_some() {
            self.phase.send_replace(state.phase.clone());
            self.changed.notify_one();
        }
    }
    fn reset_snapshot(state: &mut State) {
        state.latest = None;
        state.controls = Controls::default();
        state.cursor = None;
        state.cursor_initialized = false;
        state.control_revision = 0;
        state.publications = 0;
        state.last_stats = None;
    }
    fn active_members(state: &State) -> bool {
        state
            .members
            .values()
            .any(|member| member.epoch == state.epoch)
    }
    fn terminal(&self, state: &mut State, message: &str, all: bool) {
        state.latest = None;
        state.controls.error = Some(Arc::new(
            json!({"type":"error","message":message,"capture_instance_id":self.instance,"capture_epoch":state.epoch}),
        ));
        let controls = Arc::new(state.controls.clone());
        for member in state
            .members
            .values()
            .filter(|member| all || member.epoch == state.epoch)
        {
            member.frames.send_replace(None);
            member.controls.send_replace(controls.clone());
        }
    }
    fn publish(&self, event: Event) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.shutdown || !matches!(state.phase, Phase::Running) {
            return Ok(());
        }
        let mut changed = false;
        state.control_revision = state
            .control_revision
            .checked_add(1)
            .context("control revision overflow")?;
        let revision = state.control_revision;
        let available = !event.disabled;
        if state
            .controls
            .availability
            .is_none_or(|(_, old)| old != available)
        {
            state.controls.availability = Some((revision, available));
            changed = true;
        }
        let cursor = if event.mouse.is_none_or(|(_, _, visible)| visible) {
            event.cursor.filter(|shape| {
                shape.width <= 128
                    && shape.height <= 128
                    && shape.argb.len() == usize::from(shape.width) * usize::from(shape.height)
            })
        } else {
            None
        };
        if !state.cursor_initialized || cursor != state.cursor {
            state.cursor_initialized = true;
            let value = match &cursor {
                Some(shape) => {
                    let pixels: Vec<u8> = shape
                        .argb
                        .iter()
                        .flat_map(|word| {
                            [
                                (word >> 16) as u8,
                                (word >> 8) as u8,
                                *word as u8,
                                (word >> 24) as u8,
                            ]
                        })
                        .collect();
                    json!({"type":"cursor","width":shape.width,"height":shape.height,"hotspot_x":shape.hotspot_x,"hotspot_y":shape.hotspot_y,"pixels":pixels})
                }
                None => {
                    json!({"type":"cursor","width":0,"height":0,"hotspot_x":0,"hotspot_y":0,"pixels":[]})
                }
            };
            state.cursor = cursor;
            state.controls.cursor = Some((revision, Arc::new(value)));
            changed = true;
        }
        if event.disabled {
            state.latest = None;
            for member in state
                .members
                .values()
                .filter(|member| member.epoch == state.epoch)
            {
                member.frames.send_replace(None);
            }
        } else if let Some(frame) = event.frame {
            ensure!(
                frame.is_valid(),
                "capture provider published an invalid frame"
            );
            let dimensions = (u32::from(frame.width) << 16) | u32::from(frame.height);
            let frame = Arc::new(frame);
            state.publications = state
                .publications
                .checked_add(1)
                .context("publication counter overflow")?;
            for member in state
                .members
                .values()
                .filter(|member| member.epoch == state.epoch)
            {
                member.dimensions.store(dimensions, Ordering::Release);
                member.frames.send_replace(Some(Captured {
                    stage_generation: None,
                    published_at: None,
                    frame: frame.clone(),
                    forwarded_input_id: member.forwarded_id.load(),
                }));
            }
            state.latest = Some(frame);
        }
        if self.diagnostics
            && state
                .last_stats
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
        {
            state.last_stats = Some(Instant::now());
            let value = json!({"type":"capture_stats","source":"dbus","shared":true,"capture_instance_id":self.instance,"capture_epoch":state.epoch,
                "source_publications":state.publications,"accepted_plane_bytes":event.stats.source_bytes,"scanouts":event.stats.scanouts,"updates":event.stats.updates,
                "normalization_ms_total":event.stats.normalization_ns as f64/1_000_000.0,"snapshot_ms":event.snapshot_ns as f64/1_000_000.0,
                "timing_scope":"shared source callback conversion/snapshot; count once per instance/epoch; not guest-write time"});
            state.controls.stats = Some((revision, Arc::new(value)));
            changed = true;
        }
        if changed {
            let controls = Arc::new(state.controls.clone());
            for member in state
                .members
                .values()
                .filter(|member| member.epoch == state.epoch)
            {
                member.controls.send_replace(controls.clone());
            }
        }
        Ok(())
    }
}

/// Own this in App. Dropping a viewer never owns/aborts the producer task.
pub(super) struct SharedCapture {
    inner: Arc<Inner>,
    _supervisor: JoinHandle<()>,
}
pub(super) struct CaptureLease {
    inner: Weak<Inner>,
    id: u64,
    relay: JoinHandle<()>,
}
impl Drop for CaptureLease {
    fn drop(&mut self) {
        self.relay.abort();
        if let Some(inner) = self.inner.upgrade() {
            inner.unregister(self.id);
        }
    }
}
impl SharedCapture {
    pub fn new(target: FixedTarget) -> Result<Self> {
        ensure!(
            target.address.starts_with("unix:path=")
                && !target.address.contains(';')
                && !target.vm_uuid.is_empty()
                && target.vm_uuid.len() <= 128,
            "invalid fixed private D-Bus target"
        );
        let diagnostics = target.diagnostics;
        let max_viewers = target.max_viewers;
        let target = Arc::new(target);
        let factory: Factory = Arc::new(move || {
            let target = target.clone();
            async move {
                let source = gliss_dbus::connect_private_bus(
                    &target.address,
                    &target.vm_uuid,
                    target.console,
                )
                .await?;
                Ok(Box::new(NativeSource(source)) as Box<dyn EventSource>)
            }
            .boxed()
        });
        Self::with_factory(factory, diagnostics, max_viewers)
    }
    fn with_factory(factory: Factory, diagnostics: bool, max_viewers: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_VIEWERS).contains(&max_viewers),
            "shared viewer bound must be 1..128"
        );
        let (phase, _) = watch::channel(Phase::Idle);
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                epoch: 0,
                next_member: 0,
                phase: Phase::Idle,
                shutdown: false,
                members: BTreeMap::new(),
                latest: None,
                controls: Controls::default(),
                cursor: None,
                cursor_initialized: false,
                control_revision: 0,
                publications: 0,
                last_stats: None,
            }),
            changed: Notify::new(),
            phase,
            instance: format!("{}:{:032x}", std::process::id(), rand::random::<u128>()),
            diagnostics,
            max_viewers,
        });
        let worker = inner.clone();
        let supervisor = tokio::runtime::Handle::try_current()
            .context("shared capture requires a Tokio runtime")?
            .spawn(supervise(worker, factory));
        Ok(Self {
            inner,
            _supervisor: supervisor,
        })
    }
    /// Call only after viewer authorization/admission. Cancel this future on
    /// expiry; keep the returned lease through private input release cleanup.
    pub async fn attach(&self, viewer: ViewerChannels) -> Result<CaptureLease> {
        ensure!(
            viewer.frames.receiver_count() > 0 && !viewer.events.is_closed(),
            "viewer channels are already closed"
        );
        let mut phase = self.inner.phase.subscribe();
        let (id, controls, receiver) = {
            let mut state = self.inner.state.lock().unwrap();
            if let Phase::Stopped(Some(message)) = &state.phase {
                bail!("shared source stopped: {message}");
            }
            ensure!(
                !state.shutdown && !matches!(state.phase, Phase::Stopped(_)),
                "shared capture is shut down"
            );
            if let Phase::Failed(message) | Phase::Closing(Some(message)) = &state.phase
                && Inner::active_members(&state)
            {
                bail!("shared source failed: {message}");
            }
            ensure!(
                state.members.len() < self.inner.max_viewers,
                "shared viewer bound exceeded"
            );
            let pending = matches!(
                state.phase,
                Phase::Idle | Phase::Closing(_) | Phase::Failed(_)
            );
            let epoch = if pending {
                state
                    .epoch
                    .checked_add(1)
                    .context("source epoch overflow")?
            } else {
                state.epoch
            };
            if matches!(state.phase, Phase::Idle | Phase::Failed(_)) {
                state.epoch = epoch;
                Inner::reset_snapshot(&mut state);
                self.inner.set_phase(&mut state, Phase::Starting);
            }
            state.next_member = state
                .next_member
                .checked_add(1)
                .context("member ID overflow")?;
            let id = state.next_member;
            let initial = if epoch == state.epoch {
                state.controls.clone()
            } else {
                Controls::default()
            };
            let (controls, receiver) = watch::channel(Arc::new(initial));
            if epoch == state.epoch
                && let Some(frame) = &state.latest
            {
                viewer.dimensions.store(
                    (u32::from(frame.width) << 16) | u32::from(frame.height),
                    Ordering::Release,
                );
                viewer.frames.send_replace(Some(Captured {
                    stage_generation: None,
                    published_at: None,
                    frame: frame.clone(),
                    forwarded_input_id: viewer.forwarded_id.load(),
                }));
            }
            state.members.insert(
                id,
                Member {
                    epoch,
                    forwarded_id: viewer.forwarded_id,
                    dimensions: viewer.dimensions,
                    frames: viewer.frames.clone(),
                    controls: controls.clone(),
                },
            );
            (id, controls, receiver)
        };
        drop(controls);
        let weak = Arc::downgrade(&self.inner);
        let relay = tokio::spawn(relay_controls(
            weak.clone(),
            id,
            receiver,
            viewer.frames,
            viewer.events,
        ));
        let lease = CaptureLease {
            inner: weak,
            id,
            relay,
        };
        self.inner.changed.notify_one();
        loop {
            {
                let state = self.inner.state.lock().unwrap();
                if let Phase::Failed(message) | Phase::Stopped(Some(message)) = &state.phase {
                    bail!("shared source failed: {message}");
                }
                ensure!(!state.shutdown, "shared capture is shut down");
                let member = state
                    .members
                    .get(&id)
                    .context("viewer channels closed during attach")?;
                if matches!(state.phase, Phase::Running) && member.epoch == state.epoch {
                    return Ok(lease);
                }
            }
            phase
                .changed()
                .await
                .context("shared supervisor stopped during attach")?;
        }
    }
    pub fn snapshot(&self) -> Value {
        let state = self.inner.state.lock().unwrap();
        json!({"capture_instance_id":self.inner.instance,"capture_epoch":state.epoch,"phase":state.phase.name(),"viewers":state.members.len(),"source_publications":state.publications,"retained_frame":state.latest.is_some()})
    }
    /// Await while the Tokio runtime is alive. No other task can reopen afterward.
    pub async fn shutdown(&self) -> Result<()> {
        let mut phase = self.inner.phase.subscribe();
        {
            let mut state = self.inner.state.lock().unwrap();
            state.shutdown = true;
            // Wake provisional attaches even when the producer is completing
            // bounded registration before it can observe the shutdown signal.
            self.inner.phase.send_replace(state.phase.clone());
        }
        self.inner.changed.notify_one();
        loop {
            let current = phase.borrow().clone();
            if let Phase::Stopped(error) = current {
                if let Some(error) = error {
                    bail!("shared capture shutdown: {error}");
                }
                return Ok(());
            }
            phase
                .changed()
                .await
                .context("shared supervisor stopped without cleanup")?;
        }
    }
}
impl Drop for SharedCapture {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().shutdown = true;
        self.inner.changed.notify_one();
        // The detached supervisor owns Inner, not App/SharedCapture. It closes
        // the source explicitly; there is no App/task strong-reference cycle.
    }
}

async fn close_source(
    inner: &Inner,
    source: Box<dyn EventSource>,
    failure: Option<Arc<str>>,
) -> bool {
    {
        let mut state = inner.state.lock().unwrap();
        inner.set_phase(&mut state, Phase::Closing(failure.clone()));
    }
    let closed = source.close().await;
    let mut state = inner.state.lock().unwrap();
    if let Err(error) = closed {
        let message: Arc<str> =
            format!("capture close failed; replacement forbidden: {error:#}").into();
        state.shutdown = true;
        inner.terminal(&mut state, &message, true);
        state.members.clear();
        inner.set_phase(&mut state, Phase::Stopped(Some(message)));
        return false;
    }
    if state.shutdown {
        inner.terminal(&mut state, "The shared display is shutting down.", true);
        state.members.clear();
        inner.set_phase(&mut state, Phase::Stopped(None));
        return false;
    }
    if let Some(message) = failure.filter(|_| Inner::active_members(&state)) {
        inner.set_phase(&mut state, Phase::Failed(message));
    } else {
        Inner::reset_snapshot(&mut state);
        inner.set_phase(&mut state, Phase::Idle);
    }
    true
}
async fn supervise(inner: Arc<Inner>, factory: Factory) {
    let mut source = None::<Box<dyn EventSource>>;
    loop {
        let (shutdown, active, phase) = {
            let state = inner.state.lock().unwrap();
            (
                state.shutdown,
                Inner::active_members(&state),
                state.phase.clone(),
            )
        };
        if shutdown {
            {
                let mut state = inner.state.lock().unwrap();
                inner.terminal(&mut state, "The shared display is shutting down.", true);
            }
            if let Some(source) = source.take() {
                close_source(&inner, source, None).await;
            } else {
                let mut state = inner.state.lock().unwrap();
                state.members.clear();
                inner.set_phase(&mut state, Phase::Stopped(None));
            }
            return;
        }
        if source.is_some() && !active {
            // Attach may race the earlier snapshot. Commit closing under the
            // same membership lock, before a new viewer chooses its epoch.
            let closing = {
                let mut state = inner.state.lock().unwrap();
                if !Inner::active_members(&state) {
                    inner.set_phase(&mut state, Phase::Closing(None));
                    true
                } else {
                    false
                }
            };
            if closing && !close_source(&inner, source.take().unwrap(), None).await {
                return;
            }
            continue;
        }
        if source.is_none() {
            if matches!(phase, Phase::Idle) {
                let mut state = inner.state.lock().unwrap();
                if matches!(state.phase, Phase::Idle) && !state.members.is_empty() {
                    state.epoch = state.epoch.checked_add(1).expect("checked by attach");
                    Inner::reset_snapshot(&mut state);
                    inner.set_phase(&mut state, Phase::Starting);
                }
            }
            let starting = {
                let state = inner.state.lock().unwrap();
                !state.shutdown && matches!(state.phase, Phase::Starting)
            };
            if starting {
                // Never cancel a half-registered native source when a viewer
                // leaves. Native startup is bounded; close the returned handle.
                match factory().await {
                    Ok(opened) => {
                        source = Some(opened);
                        let mut state = inner.state.lock().unwrap();
                        inner.set_phase(&mut state, Phase::Running);
                    }
                    Err(error) => {
                        let message: Arc<str> =
                            format!("capture registration failed; cleanup uncertain and replacement forbidden: {error:#}").into();
                        let mut state = inner.state.lock().unwrap();
                        state.shutdown = true;
                        inner.terminal(&mut state, &message, true);
                        state.members.clear();
                        inner.set_phase(&mut state, Phase::Stopped(Some(message)));
                        return;
                    }
                }
                continue;
            }
            // Failed state persists while any old viewer owns a lease. With
            // none left, a subsequent attach may start a fresh epoch.
            inner.changed.notified().await;
            continue;
        }
        let event = tokio::select! {
            biased;
            _=inner.changed.notified()=>continue,
            event=source.as_mut().unwrap().read_event()=>event,
        };
        let result = event.and_then(|event| inner.publish(event));
        if let Err(error) = result {
            let message: Arc<str> =
                format!("The shared VM CPU display disconnected: {error:#}").into();
            {
                let mut state = inner.state.lock().unwrap();
                inner.terminal(&mut state, &message, false);
                inner.set_phase(&mut state, Phase::Closing(Some(message.clone())));
            }
            if !close_source(&inner, source.take().unwrap(), Some(message)).await {
                return;
            }
        }
    }
}

#[derive(Default)]
struct Sent {
    availability: u64,
    cursor: u64,
    stats: u64,
}
enum ControlKind {
    Availability(u64),
    Cursor(u64),
    Stats(u64),
    Error,
}
fn pending(controls: &Controls, sent: &Sent) -> Option<(ControlKind, Arc<Value>)> {
    if let Some(error) = &controls.error {
        return Some((ControlKind::Error, error.clone()));
    }
    if let Some((revision, available)) = controls.availability
        && revision != sent.availability
    {
        return Some((
            ControlKind::Availability(revision),
            Arc::new(json!({"type":"display_state","available":available})),
        ));
    }
    if let Some((revision, cursor)) = &controls.cursor
        && *revision != sent.cursor
    {
        return Some((ControlKind::Cursor(*revision), cursor.clone()));
    }
    if let Some((revision, stats)) = &controls.stats
        && *revision != sent.stats
    {
        return Some((ControlKind::Stats(*revision), stats.clone()));
    }
    None
}
async fn relay_controls(
    inner: Weak<Inner>,
    id: u64,
    mut controls: watch::Receiver<Arc<Controls>>,
    frames: watch::Sender<Option<Captured>>,
    events: mpsc::Sender<Value>,
) {
    let mut sent = Sent::default();
    let mut closed = false;
    loop {
        let snapshot = controls.borrow_and_update().clone();
        let has_pending = pending(&snapshot, &sent).is_some();
        if closed && !has_pending {
            break;
        }
        tokio::select! {
            biased;
            _=events.closed()=>break,
            _=frames.closed()=>break,
            changed=controls.changed(), if !closed=>{if changed.is_err() {closed=true;}},
            permit=events.reserve(), if has_pending=>{
                let Ok(permit)=permit else {break;};
                let snapshot=controls.borrow_and_update().clone();
                if let Some((kind,message))=pending(&snapshot,&sent) {
                    permit.send((*message).clone());
                    match kind {ControlKind::Availability(revision)=>sent.availability=revision,ControlKind::Cursor(revision)=>sent.cursor=revision,ControlKind::Stats(revision)=>sent.stats=revision,ControlKind::Error=>return}
                }
            }
        }
    }
    if let Some(inner) = inner.upgrade() {
        inner.unregister(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::{sync::Semaphore, time::timeout};

    #[derive(Clone, Default)]
    struct Counts {
        opens: Arc<AtomicUsize>,
        live: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
        fallback_drops: Arc<AtomicUsize>,
        close_error: Arc<std::sync::atomic::AtomicBool>,
        log: Arc<Mutex<Vec<String>>>,
    }
    struct Peer {
        events: mpsc::Sender<Result<Event>>,
    }
    struct MockSource {
        events: mpsc::Receiver<Result<Event>>,
        counts: Counts,
        index: usize,
        close_gate: Arc<Semaphore>,
        closing: mpsc::Sender<usize>,
        closed: bool,
    }
    impl EventSource for MockSource {
        fn read_event(&mut self) -> BoxFuture<'_, Result<Event>> {
            async move {
                self.events
                    .recv()
                    .await
                    .context("mock provider disconnected")?
            }
            .boxed()
        }
        fn close(mut self: Box<Self>) -> BoxFuture<'static, Result<()>> {
            async move {
                self.counts
                    .log
                    .lock()
                    .unwrap()
                    .push(format!("closing:{}", self.index));
                self.closing
                    .send(self.index)
                    .await
                    .context("mock close observer closed")?;
                self.close_gate.acquire().await?.forget();
                self.closed = true;
                self.counts.live.fetch_sub(1, Ordering::SeqCst);
                self.counts.closes.fetch_add(1, Ordering::SeqCst);
                self.counts
                    .log
                    .lock()
                    .unwrap()
                    .push(format!("closed:{}", self.index));
                ensure!(
                    !self.counts.close_error.load(Ordering::SeqCst),
                    "mock close uncertainty"
                );
                Ok(())
            }
            .boxed()
        }
    }
    impl Drop for MockSource {
        fn drop(&mut self) {
            if !self.closed {
                self.counts.live.fetch_sub(1, Ordering::SeqCst);
                self.counts.fallback_drops.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    struct Rig {
        factory: Factory,
        counts: Counts,
        opened: mpsc::Receiver<Peer>,
        closing: mpsc::Receiver<usize>,
        open_gate: Arc<Semaphore>,
        close_gate: Arc<Semaphore>,
    }
    impl Rig {
        fn new(hold_open: bool, hold_close: bool) -> Self {
            let counts = Counts::default();
            let (opened_tx, opened) = mpsc::channel(8);
            let (closing_tx, closing) = mpsc::channel(8);
            let open_gate = Arc::new(Semaphore::new(if hold_open { 0 } else { 8 }));
            let close_gate = Arc::new(Semaphore::new(if hold_close { 0 } else { 8 }));
            let factory: Factory = {
                let counts = counts.clone();
                let open_gate = open_gate.clone();
                let close_gate = close_gate.clone();
                Arc::new(move || {
                    let counts = counts.clone();
                    let opened = opened_tx.clone();
                    let closing = closing_tx.clone();
                    let open_gate = open_gate.clone();
                    let close_gate = close_gate.clone();
                    async move {
                        let index = counts.opens.fetch_add(1, Ordering::SeqCst) + 1;
                        counts.live.fetch_add(1, Ordering::SeqCst);
                        counts.log.lock().unwrap().push(format!("opened:{index}"));
                        let (events, receiver) = mpsc::channel(8);
                        let source = MockSource {
                            events: receiver,
                            counts,
                            index,
                            close_gate,
                            closing,
                            closed: false,
                        };
                        opened
                            .send(Peer { events })
                            .await
                            .context("mock open observer closed")?;
                        open_gate.acquire().await?.forget();
                        Ok(Box::new(source) as Box<dyn EventSource>)
                    }
                    .boxed()
                })
            };
            Self {
                factory,
                counts,
                opened,
                closing,
                open_gate,
                close_gate,
            }
        }
        fn hub(&self) -> Arc<SharedCapture> {
            Arc::new(SharedCapture::with_factory(self.factory.clone(), true, 4).unwrap())
        }
        async fn peer(&mut self) -> Peer {
            timeout(Duration::from_secs(2), self.opened.recv())
                .await
                .unwrap()
                .unwrap()
        }
        async fn closing(&mut self) -> usize {
            timeout(Duration::from_secs(2), self.closing.recv())
                .await
                .unwrap()
                .unwrap()
        }
    }
    struct Viewer {
        id: Arc<AtomicU32>,
        dimensions: Arc<AtomicU32>,
        frames: watch::Receiver<Option<Captured>>,
        events: Option<mpsc::Receiver<Value>>,
    }
    fn viewer(id: u32, capacity: usize) -> (ViewerChannels, Viewer) {
        let id = Arc::new(AtomicU32::new(id));
        let dimensions = Arc::new(AtomicU32::new(0));
        let (frames, receiver) = watch::channel(None);
        let (events, event_rx) = mpsc::channel(capacity);
        (
            ViewerChannels {
                forwarded_id: ForwardedId::new(id.clone()),
                dimensions: dimensions.clone(),
                frames,
                events,
            },
            Viewer {
                id,
                dimensions,
                frames: receiver,
                events: Some(event_rx),
            },
        )
    }
    impl Viewer {
        async fn frame(&mut self, color: u8) -> Captured {
            timeout(Duration::from_secs(2), async {
                loop {
                    let current = self.frames.borrow_and_update().clone();
                    if let Some(frame) = current
                        && frame.frame.pixels[0] == color
                    {
                        return frame;
                    }
                    self.frames.changed().await.unwrap();
                }
            })
            .await
            .unwrap()
        }
        async fn event(&mut self, kind: &str) -> Value {
            timeout(Duration::from_secs(2), async {
                loop {
                    let value = self.events.as_mut().unwrap().recv().await.unwrap();
                    if value["type"] == kind {
                        return value;
                    }
                }
            })
            .await
            .unwrap()
        }
    }
    fn event(color: u8, width: u16, height: u16) -> Event {
        Event {
            revision: u64::from(color),
            frame_revision: u64::from(color),
            frame: Some(Frame {
                width,
                height,
                pixels: [color, 17, 29, 255].repeat(usize::from(width) * usize::from(height)),
            }),
            disabled: false,
            cursor: Some(Cursor {
                width: 1,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0,
                argb: vec![0xff12_3456],
            }),
            mouse: None,
            stats: gliss_dbus::CaptureStats {
                source_bytes: 16,
                updates: 1,
                ..Default::default()
            },
            received_at: None,
            snapshot_ns: 41,
        }
    }
    async fn wait(hub: &SharedCapture, phase: &str, members: usize) {
        timeout(Duration::from_secs(2), async {
            loop {
                let state = hub.snapshot();
                if state["phase"] == phase && state["viewers"] == members {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn simultaneous_cold_attach_shares_one_provider_and_one_frame_arc() {
        let mut rig = Rig::new(true, false);
        let hub = rig.hub();
        let (channels_a, mut a) = viewer(11, 16);
        let (channels_b, mut b) = viewer(21, 16);
        let job_a = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.attach(channels_a).await })
        };
        let job_b = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.attach(channels_b).await })
        };
        let peer = rig.peer().await;
        wait(&hub, "starting", 2).await;
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        rig.open_gate.add_permits(1);
        let lease_a = job_a.await.unwrap().unwrap();
        let lease_b = job_b.await.unwrap().unwrap();
        peer.events.send(Ok(event(1, 2, 2))).await.unwrap();
        let frame_a = a.frame(1).await;
        let frame_b = b.frame(1).await;
        assert!(Arc::ptr_eq(&frame_a.frame, &frame_b.frame));
        assert_eq!(frame_a.forwarded_input_id, 11);
        assert_eq!(frame_b.forwarded_input_id, 21);
        let stats_a = a.event("capture_stats").await;
        let stats_b = b.event("capture_stats").await;
        assert_eq!(
            stats_a["capture_instance_id"],
            stats_b["capture_instance_id"]
        );
        assert_eq!(stats_a["capture_epoch"], stats_b["capture_epoch"]);
        assert_eq!(stats_a["source_publications"], 1);
        drop(lease_a);
        assert_eq!(hub.snapshot()["viewers"], 1);
        b.id.store(99, Ordering::Release);
        peer.events.send(Ok(event(2, 2, 2))).await.unwrap();
        assert_eq!(b.frame(2).await.forwarded_input_id, 99);
        assert_eq!(frame_a.forwarded_input_id, 11);
        assert_eq!(frame_a.frame.pixels[0], 1);
        drop(lease_b);
        hub.shutdown().await.unwrap();
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 1);
        assert_eq!(rig.counts.fallback_drops.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn canceled_attach_and_private_setup_failure_release_provisional_membership() {
        let mut rig = Rig::new(true, false);
        let hub = rig.hub();
        let (channels, _viewer) = viewer(0, 16);
        let job = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.attach(channels).await })
        };
        let _peer = rig.peer().await;
        job.abort();
        assert!(job.await.err().unwrap().is_cancelled());
        assert_eq!(hub.snapshot()["viewers"], 0);
        rig.open_gate.add_permits(1);
        assert_eq!(rig.closing().await, 1);
        wait(&hub, "idle", 0).await;
        assert_eq!(rig.counts.live.load(Ordering::SeqCst), 0);
        assert_eq!(rig.counts.fallback_drops.load(Ordering::SeqCst), 0);
        let (channels, _second) = viewer(7, 16);
        rig.open_gate.add_permits(1);
        let lease = hub.attach(channels).await.unwrap();
        let _peer = rig.peer().await;
        drop(lease); // Caller encountered private RFB setup failure after attach.
        assert_eq!(rig.closing().await, 2);
        wait(&hub, "idle", 0).await;
        hub.shutdown().await.unwrap();
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn late_static_join_retains_frame_cursor_availability_and_own_watermark() {
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let (channels, mut first) = viewer(1, 16);
        let lease = hub.attach(channels).await.unwrap();
        let peer = rig.peer().await;
        peer.events.send(Ok(event(4, 2, 3))).await.unwrap();
        let old = first.frame(4).await;
        let (channels, mut late) = viewer(73, 16);
        let late_lease = hub.attach(channels).await.unwrap();
        let retained = late.frame(4).await;
        assert!(Arc::ptr_eq(&old.frame, &retained.frame));
        assert_eq!(retained.forwarded_input_id, 73);
        assert_eq!(late.dimensions.load(Ordering::Acquire), (2 << 16) | 3);
        assert_eq!(late.event("display_state").await["available"], true);
        assert_eq!(
            late.event("cursor").await["pixels"],
            json!([18, 52, 86, 255])
        );
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        drop(late_lease);
        drop(lease);
        hub.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn full_control_queue_coalesces_and_does_not_block_other_frames_or_cleanup() {
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let (slow_channels, mut slow) = viewer(8, 1);
        slow_channels
            .events
            .try_send(json!({"type":"occupied"}))
            .unwrap();
        let slow_lease = hub.attach(slow_channels).await.unwrap();
        let peer = rig.peer().await;
        let (channels, mut fast) = viewer(9, 16);
        let fast_lease = hub.attach(channels).await.unwrap();
        peer.events.send(Ok(event(1, 2, 2))).await.unwrap();
        fast.frame(1).await;
        let mut disabled = event(2, 2, 2);
        disabled.disabled = true;
        disabled.frame = None;
        disabled.cursor = None;
        peer.events.send(Ok(disabled)).await.unwrap();
        let mut resized = event(3, 3, 2);
        resized.cursor.as_mut().unwrap().argb[0] = 0xffab_cdef;
        peer.events.send(Ok(resized)).await.unwrap();
        fast.frame(3).await;
        assert_eq!(fast.dimensions.load(Ordering::Acquire), (3 << 16) | 2);
        assert_eq!(slow.event("occupied").await["type"], "occupied");
        assert_eq!(slow.event("display_state").await["available"], true);
        assert_eq!(
            slow.event("cursor").await["pixels"],
            json!([171, 205, 239, 255])
        );
        drop(slow_lease);
        assert_eq!(hub.snapshot()["viewers"], 1);
        peer.events.send(Ok(event(5, 3, 2))).await.unwrap();
        fast.frame(5).await;
        drop(fast_lease);
        hub.shutdown().await.unwrap();
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disable_clears_frames_and_each_viewers_codec_resync_stays_private() {
        use gliss_codec::{Encoder, Mode};
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let (channels, mut a) = viewer(1, 16);
        let lease_a = hub.attach(channels).await.unwrap();
        let peer = rig.peer().await;
        let (channels, mut b) = viewer(2, 16);
        let lease_b = hub.attach(channels).await.unwrap();
        peer.events.send(Ok(event(1, 2, 2))).await.unwrap();
        let first_a = a.frame(1).await;
        let first_b = b.frame(1).await;
        let mut encoder_a = Encoder::new(Mode::Global, 16).unwrap();
        let mut encoder_b = Encoder::new(Mode::Global, 16).unwrap();
        encoder_a.encode(&first_a.frame, 1, 1).unwrap();
        encoder_b.encode(&first_b.frame, 1, 2).unwrap();
        peer.events.send(Ok(event(2, 2, 2))).await.unwrap();
        let next_a = a.frame(2).await;
        let next_b = b.frame(2).await;
        encoder_a.encode(&next_a.frame, 2, 1).unwrap();
        encoder_b.encode(&next_b.frame, 2, 2).unwrap();
        encoder_a.reset();
        assert_eq!(encoder_a.encode(&next_a.frame, 3, 1).unwrap()[18] & 1, 1);
        assert_eq!(encoder_b.encode(&next_b.frame, 3, 2).unwrap()[18] & 1, 0);
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&next_a.frame, &next_b.frame));
        let mut disabled = event(3, 2, 2);
        disabled.disabled = true;
        disabled.frame = None;
        peer.events.send(Ok(disabled)).await.unwrap();
        timeout(Duration::from_secs(2), async {
            loop {
                let current = a.frames.borrow_and_update().clone();
                if current.is_none() {
                    break;
                }
                a.frames.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(!hub.snapshot()["retained_frame"].as_bool().unwrap());
        peer.events.send(Ok(event(4, 4, 2))).await.unwrap();
        assert_eq!(b.frame(4).await.frame.width, 4);
        assert_eq!(b.dimensions.load(Ordering::Acquire), (4 << 16) | 2);
        drop(lease_a);
        drop(lease_b);
        hub.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn terminal_error_is_retained_and_prioritized_over_stale_controls() {
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let (channels, mut client) = viewer(1, 1);
        channels
            .events
            .try_send(json!({"type":"occupied"}))
            .unwrap();
        let lease = hub.attach(channels).await.unwrap();
        let peer = rig.peer().await;
        peer.events.send(Ok(event(1, 2, 2))).await.unwrap();
        client.frame(1).await;
        peer.events
            .send(Err(anyhow::anyhow!("wire lost")))
            .await
            .unwrap();
        assert_eq!(rig.closing().await, 1);
        wait(&hub, "failed", 1).await;
        let (channels, _late) = viewer(2, 16);
        assert!(
            hub.attach(channels)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("wire lost")
        );
        assert_eq!(client.event("occupied").await["type"], "occupied");
        assert!(
            client.event("error").await["message"]
                .as_str()
                .unwrap()
                .contains("wire lost")
        );
        assert_eq!(hub.snapshot()["viewers"], 1);
        assert!(!hub.snapshot()["retained_frame"].as_bool().unwrap());
        drop(lease);
        hub.shutdown().await.unwrap();
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn final_drop_then_attach_waits_for_complete_close_and_new_epoch() {
        let mut rig = Rig::new(false, true);
        let hub = rig.hub();
        let (channels, _first) = viewer(0, 16);
        let first = hub.attach(channels).await.unwrap();
        let _peer = rig.peer().await;
        let epoch = hub.snapshot()["capture_epoch"].as_u64().unwrap();
        drop(first);
        assert_eq!(rig.closing().await, 1);
        let (channels, mut second) = viewer(41, 16);
        let job = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.attach(channels).await })
        };
        wait(&hub, "closing", 1).await;
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        assert!(!job.is_finished());
        rig.close_gate.add_permits(1);
        let second_lease = job.await.unwrap().unwrap();
        let peer = rig.peer().await;
        assert_eq!(hub.snapshot()["capture_epoch"].as_u64().unwrap(), epoch + 1);
        assert_eq!(
            rig.counts.log.lock().unwrap().as_slice(),
            ["opened:1", "closing:1", "closed:1", "opened:2"]
        );
        peer.events.send(Ok(event(7, 2, 2))).await.unwrap();
        assert_eq!(second.frame(7).await.forwarded_input_id, 41);
        rig.close_gate.add_permits(1);
        drop(second_lease);
        hub.shutdown().await.unwrap();
        assert_eq!(rig.counts.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn closed_viewer_channels_and_expired_lease_cleanup_leave_other_viewer_live() {
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let (channels, mut expired) = viewer(0, 16);
        let expired_lease = hub.attach(channels).await.unwrap();
        let peer = rig.peer().await;
        let (channels, mut live) = viewer(1, 16);
        let live_lease = hub.attach(channels).await.unwrap();
        drop(expired.events.take());
        wait(&hub, "running", 1).await;
        drop(expired_lease);
        peer.events.send(Ok(event(5, 2, 2))).await.unwrap();
        live.frame(5).await;
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        drop(live_lease);
        hub.shutdown().await.unwrap();
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn uncertain_close_halts_replacement_and_shutdown_reports_the_reason() {
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let (channels, _viewer) = viewer(0, 16);
        let lease = hub.attach(channels).await.unwrap();
        let _peer = rig.peer().await;
        rig.counts.close_error.store(true, Ordering::SeqCst);
        drop(lease);
        assert_eq!(rig.closing().await, 1);
        wait(&hub, "stopped", 0).await;
        let (channels, _later) = viewer(1, 16);
        assert!(
            hub.attach(channels)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("replacement forbidden")
        );
        assert!(hub.shutdown().await.is_err());
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        assert_eq!(rig.counts.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn registration_failure_releases_attach_and_forbids_uncertain_replacement() {
        let rig = Rig::new(false, false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let factory: Factory = {
            let attempts = attempts.clone();
            let delegate = rig.factory.clone();
            Arc::new(move || {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    async { bail!("registration refused") }.boxed()
                } else {
                    delegate()
                }
            })
        };
        let hub = SharedCapture::with_factory(factory, true, 4).unwrap();
        let (channels, _first) = viewer(1, 16);
        assert!(
            hub.attach(channels)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("registration refused")
        );
        wait(&hub, "stopped", 0).await;
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 0);
        let failed_epoch = hub.snapshot()["capture_epoch"].as_u64().unwrap();
        let (channels, _second) = viewer(2, 16);
        assert!(hub.attach(channels).await.is_err());
        assert_eq!(
            hub.snapshot()["capture_epoch"].as_u64().unwrap(),
            failed_epoch
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(hub.shutdown().await.is_err());
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_attach_but_finishes_registration_and_explicit_close() {
        let mut rig = Rig::new(true, true);
        let hub = rig.hub();
        let (channels, _viewer) = viewer(1, 16);
        let attach = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.attach(channels).await })
        };
        let _peer = rig.peer().await;
        wait(&hub, "starting", 1).await;
        let shutdown = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.shutdown().await })
        };
        let result = timeout(Duration::from_secs(2), attach)
            .await
            .unwrap()
            .unwrap();
        assert!(result.err().unwrap().to_string().contains("shut down"));
        assert_eq!(hub.snapshot()["viewers"], 0);
        assert!(!shutdown.is_finished());
        assert_eq!(rig.counts.live.load(Ordering::SeqCst), 1);
        rig.open_gate.add_permits(1);
        assert_eq!(rig.closing().await, 1);
        let (channels, _later) = viewer(2, 16);
        assert!(hub.attach(channels).await.is_err());
        assert!(!shutdown.is_finished());
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        rig.close_gate.add_permits(1);
        shutdown.await.unwrap().unwrap();
        assert_eq!(rig.counts.live.load(Ordering::SeqCst), 0);
        assert_eq!(rig.counts.fallback_drops.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dropping_app_owner_explicitly_closes_without_an_inner_task_cycle() {
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let (channels, _viewer) = viewer(1, 16);
        let lease = hub.attach(channels).await.unwrap();
        let _peer = rig.peer().await;
        let inner = Arc::downgrade(&hub.inner);
        drop(hub);
        assert_eq!(rig.closing().await, 1);
        timeout(Duration::from_secs(2), async {
            while inner.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 1);
        assert_eq!(rig.counts.live.load(Ordering::SeqCst), 0);
        assert_eq!(rig.counts.fallback_drops.load(Ordering::SeqCst), 0);
        drop(lease);
    }

    #[tokio::test]
    async fn viewer_bound_rejects_extra_members_without_opening_another_provider() {
        let mut rig = Rig::new(false, false);
        let hub = rig.hub();
        let mut viewers = Vec::new();
        let mut leases = Vec::new();
        for id in 0..4 {
            let (channels, viewer) = viewer(id, 16);
            leases.push(hub.attach(channels).await.unwrap());
            viewers.push(viewer);
        }
        let _peer = rig.peer().await;
        let (channels, _rejected) = viewer(5, 16);
        assert!(
            hub.attach(channels)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("viewer bound")
        );
        assert_eq!(hub.snapshot()["viewers"], 4);
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        drop(leases);
        hub.shutdown().await.unwrap();
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 1);
        drop(viewers);
    }

    #[tokio::test]
    async fn canceling_attach_during_final_close_does_not_open_a_replacement() {
        let mut rig = Rig::new(false, true);
        let hub = rig.hub();
        let (channels, _first) = viewer(1, 16);
        let lease = hub.attach(channels).await.unwrap();
        let _peer = rig.peer().await;
        drop(lease);
        assert_eq!(rig.closing().await, 1);
        let (channels, _pending) = viewer(2, 16);
        let attach = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.attach(channels).await })
        };
        wait(&hub, "closing", 1).await;
        attach.abort();
        assert!(attach.await.err().unwrap().is_cancelled());
        assert_eq!(hub.snapshot()["viewers"], 0);
        rig.close_gate.add_permits(1);
        wait(&hub, "idle", 0).await;
        assert_eq!(rig.counts.opens.load(Ordering::SeqCst), 1);
        assert_eq!(rig.counts.closes.load(Ordering::SeqCst), 1);
        assert_eq!(rig.counts.live.load(Ordering::SeqCst), 0);
        hub.shutdown().await.unwrap();
    }
}
