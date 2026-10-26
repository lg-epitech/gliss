//! Isolated atomic-slot trace experiment. No clocks, locks, allocation or IO in
//! append. Use static placement, never a4MiB temporary on a thread's stack.
#![forbid(unsafe_code)]

#[cfg(not(target_has_atomic = "64"))]
compile_error!("this experiment requires target-native64-bit atomics");

use std::io::{self, Write};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

pub const STORAGE_BUDGET_BYTES: usize = 4 * 1024 * 1024;
const BOOKKEEPING_RESERVATION_BYTES: usize = 16 * 1024;
pub const EVENT_BYTES: usize = 56;
pub const SLOT_BYTES: usize = 64;
pub const EVENT_CAPACITY: usize =
    (STORAGE_BUDGET_BYTES - BOOKKEEPING_RESERVATION_BYTES) / SLOT_BYTES;
pub const SCHEMA_VERSION: u32 = 1;
pub const MAGIC: [u8; 8] = *b"GLSTREC1";
pub const FIXED_HEADER_BYTES: usize = 144;
pub const LOSS_ROW_BYTES: usize = 32;

const NEW: u8 = 0;
const COLLECTING: u8 = 1;
const STOPPING: u8 = 2;
const FROZEN: u8 = 3;
const CLOSED: u64 = 1 << 63;
const ACTIVE: u64 = CLOSED - 1;
const HAS_INPUT: u8 = 1;
const HAS_GENERATION: u8 = 2;
const HAS_EPOCH: u8 = 4;
const HAS_SEQUENCE: u8 = 8;
const HAS_REQUEST: u8 = 16;
const HAS_SOURCE: u8 = 32;
const HAS_RESPONSE: u8 = 64;
const HAS_COUNT: u8 = 128;

/// Stable wire values. Per-packet recording and key/text values are excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    ControlReceived = 0,
    InputAdmitted = 1,
    InputRejected = 2,
    InputDequeued = 3,
    RfbCommandQueued = 4,
    RfbWriteStarted = 5,
    RfbWriteCompleted = 6,
    InputSendCompleted = 7,
    CaptureTickDue = 8,
    CaptureTickServiced = 9,
    CaptureRequestSkipped = 10,
    CaptureRequestQueued = 11,
    UpdateHeader = 12,
    UpdateDecoded = 13,
    CloneStarted = 14,
    CloneCompleted = 15,
    SourceEventQueued = 16,
    SourceEventDequeued = 17,
    CapturePublished = 18,
    CaptureMarker = 19,
    MediaObserved = 20,
    EncodeSelected = 21,
    EncodePermit = 22,
    EncodeStarted = 23,
    EncodeCompleted = 24,
    FirstBodyReady = 25,
    LastBodyReady = 26,
    FirstAdmission = 27,
    LastAdmission = 28,
    SequenceGenerated = 29,
    SequenceAdmitted = 30,
    SequenceReplaced = 31,
    SequenceExpired = 32,
    SequenceEvicted = 33,
    SequenceCanceled = 34,
    SequenceSendError = 35,
    MarkerTileReady = 36,
    MarkerTileAdmitted = 37,
}
pub const KIND_COUNT: usize = EventKind::MarkerTileAdmitted as usize + 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Outcome {
    #[default]
    Observed = 0,
    Accepted = 1,
    Rejected = 2,
    Full = 3,
    Error = 4,
    Skipped = 5,
    Canceled = 6,
    Expired = 7,
    Replaced = 8,
    Evicted = 9,
    EmptyUpdate = 10,
    MetadataOnly = 11,
    PixelsChanged = 12,
    Ambiguous = 13,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Marker {
    #[default]
    Unobserved,
    Invalid,
    Mixed,
    Valid(u32),
}
impl Marker {
    fn wire(self) -> (u8, u32) {
        match self {
            Self::Unobserved => (0, 0),
            Self::Invalid => (1, 0),
            Self::Mixed => (2, 0),
            Self::Valid(value) => (3, value),
        }
    }
    fn decode(state: u8, value: u32) -> Self {
        match state {
            0 => Self::Unobserved,
            1 => Self::Invalid,
            2 => Self::Mixed,
            _ => Self::Valid(value),
        }
    }
}

/// Identifiers describe observed associations, not content causality. `count`
/// is a numeric per-update/sequence tally, never a keysym or text payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fields {
    pub input_id: Option<u64>,
    pub generation: Option<u64>,
    pub epoch: Option<u32>,
    pub sequence: Option<u32>,
    pub request_ordinal: Option<u64>,
    pub source_marker: Marker,
    pub response_marker: Marker,
    pub count: Option<u32>,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameKey {
    pub generation: u64,
    pub epoch: u32,
    pub sequence: u32,
}

/// The in-memory record is not serialized by casting its representation.
/// Explicit little-endian export has the same fixed 56-byte logical layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Event {
    delta_ns: u64,
    input_id: u64,
    generation: u64,
    request_ordinal: u64,
    epoch: u32,
    sequence: u32,
    source_marker: u32,
    response_marker: u32,
    count: u32,
    kind: u8,
    validity: u8,
    marker_states: u8,
    outcome: u8,
}
impl Event {
    fn new(stamp: Stamp, kind: EventKind, fields: Fields) -> Self {
        let (source_state, source_marker) = fields.source_marker.wire();
        let (response_state, response_marker) = fields.response_marker.wire();
        let present = |yes, mask| if yes { mask } else { 0 };
        let validity = present(fields.input_id.is_some(), HAS_INPUT)
            | present(fields.generation.is_some(), HAS_GENERATION)
            | present(fields.epoch.is_some(), HAS_EPOCH)
            | present(fields.sequence.is_some(), HAS_SEQUENCE)
            | present(fields.request_ordinal.is_some(), HAS_REQUEST)
            | present(source_state == 3, HAS_SOURCE)
            | present(response_state == 3, HAS_RESPONSE)
            | present(fields.count.is_some(), HAS_COUNT);
        Self {
            delta_ns: stamp.delta_ns,
            input_id: fields.input_id.unwrap_or(0),
            generation: fields.generation.unwrap_or(0),
            request_ordinal: fields.request_ordinal.unwrap_or(0),
            epoch: fields.epoch.unwrap_or(0),
            sequence: fields.sequence.unwrap_or(0),
            source_marker,
            response_marker,
            count: fields.count.unwrap_or(0),
            kind: kind as u8,
            validity,
            marker_states: source_state | (response_state << 2),
            outcome: fields.outcome as u8,
        }
    }
    pub fn delta_ns(&self) -> u64 {
        self.delta_ns
    }
    pub fn kind_number(&self) -> u8 {
        self.kind
    }
    pub fn fields(&self) -> Fields {
        Fields {
            input_id: (self.validity & HAS_INPUT != 0).then_some(self.input_id),
            generation: (self.validity & HAS_GENERATION != 0).then_some(self.generation),
            epoch: (self.validity & HAS_EPOCH != 0).then_some(self.epoch),
            sequence: (self.validity & HAS_SEQUENCE != 0).then_some(self.sequence),
            request_ordinal: (self.validity & HAS_REQUEST != 0).then_some(self.request_ordinal),
            source_marker: Marker::decode(self.marker_states & 3, self.source_marker),
            response_marker: Marker::decode((self.marker_states >> 2) & 3, self.response_marker),
            count: (self.validity & HAS_COUNT != 0).then_some(self.count),
            outcome: self.outcome(),
        }
    }
    pub fn frame_key(&self) -> Option<FrameKey> {
        let fields = self.fields();
        Some(FrameKey {
            generation: fields.generation?,
            epoch: fields.epoch?,
            sequence: fields.sequence?,
        })
    }
    pub fn outcome(&self) -> Outcome {
        match self.outcome {
            1 => Outcome::Accepted,
            2 => Outcome::Rejected,
            3 => Outcome::Full,
            4 => Outcome::Error,
            5 => Outcome::Skipped,
            6 => Outcome::Canceled,
            7 => Outcome::Expired,
            8 => Outcome::Replaced,
            9 => Outcome::Evicted,
            10 => Outcome::EmptyUpdate,
            11 => Outcome::MetadataOnly,
            12 => Outcome::PixelsChanged,
            13 => Outcome::Ambiguous,
            _ => Outcome::Observed,
        }
    }
    fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        for value in [
            self.delta_ns,
            self.input_id,
            self.generation,
            self.request_ordinal,
        ] {
            writer.write_all(&value.to_le_bytes())?;
        }
        for value in [
            self.epoch,
            self.sequence,
            self.source_marker,
            self.response_marker,
            self.count,
        ] {
            writer.write_all(&value.to_le_bytes())?;
        }
        writer.write_all(&[self.kind, self.validity, self.marker_states, self.outcome])
    }
    fn words(self) -> [u64; 7] {
        [
            self.delta_ns,
            self.input_id,
            self.generation,
            self.request_ordinal,
            u64::from(self.epoch) | (u64::from(self.sequence) << 32),
            u64::from(self.source_marker) | (u64::from(self.response_marker) << 32),
            u64::from(self.count)
                | (u64::from(self.kind) << 32)
                | (u64::from(self.validity) << 40)
                | (u64::from(self.marker_states) << 48)
                | (u64::from(self.outcome) << 56),
        ]
    }
    fn from_words(words: [u64; 7]) -> Self {
        Self {
            delta_ns: words[0],
            input_id: words[1],
            generation: words[2],
            request_ordinal: words[3],
            epoch: words[4] as u32,
            sequence: (words[4] >> 32) as u32,
            source_marker: words[5] as u32,
            response_marker: (words[5] >> 32) as u32,
            count: words[6] as u32,
            kind: (words[6] >> 32) as u8,
            validity: (words[6] >> 40) as u8,
            marker_states: (words[6] >> 48) as u8,
            outcome: (words[6] >> 56) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum ClockKind {
    RustInstant = 1,
    ClockMonotonic = 2,
}

/// Caller-observed metadata. Unknown resolution/origin stays unknown. A domain
/// ID names exactly one process/origin, not a cross-process synchronization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockDescription {
    pub domain_id: u64,
    pub process_id: u32,
    pub kind: ClockKind,
    pub origin_monotonic_ns: Option<u64>,
    pub resolution_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub domain_id: u64,
    pub delta_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidClock,
    InvalidLimit,
    AlreadyStarted,
    NotStarted,
    Busy,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Omission {
    Contended,
    Full,
    WrongClockDomain,
    Poisoned,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Append {
    Recorded { ordinal: usize },
    Omitted(Omission),
    Closed,
    Disabled,
}

struct Loss {
    count: AtomicU64,
    timed_count: AtomicU64,
    first_ns: AtomicU64,
    last_ns: AtomicU64,
}
impl Loss {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            timed_count: AtomicU64::new(0),
            first_ns: AtomicU64::new(u64::MAX),
            last_ns: AtomicU64::new(0),
        }
    }
}

struct Collection {
    clock: Option<ClockDescription>,
    limit: usize,
}

struct Slot {
    words: [AtomicU64; 7],
    published: AtomicU64,
}
impl Slot {
    const fn new() -> Self {
        Self {
            words: [const { AtomicU64::new(0) }; 7],
            published: AtomicU64::new(0),
        }
    }
    fn publish(&self, event: Event) {
        for (destination, word) in self.words.iter().zip(event.words()) {
            destination.store(word, Ordering::Relaxed);
        }
        self.published.store(1, Ordering::Release);
    }
    fn event(&self) -> Event {
        // Freeze independently verifies every reserved slot is published. This
        // acquire also makes this decoding contract explicit at the slot seam.
        let _published = self.published.load(Ordering::Acquire);
        Event::from_words(std::array::from_fn(|index| {
            self.words[index].load(Ordering::Relaxed)
        }))
    }
}

/// One inline allocation-free object. All collection and loss bookkeeping is
/// included in `size_of::<Recorder>()`; no Vec/Arc/Box owns extra storage.
pub struct Recorder {
    // Configuration/export mutex only: append never touches it.
    collection: Mutex<Collection>,
    phase: AtomicU8,
    admission: AtomicU64,
    next: AtomicUsize,
    limit: AtomicUsize,
    slots: [Slot; EVENT_CAPACITY],
    domain_id: AtomicU64,
    losses: [Loss; KIND_COUNT],
    contention: AtomicU64,
    overflow: AtomicU64,
    wrong_clock: AtomicU64,
    poison: AtomicU64,
    saturated: AtomicBool,
}

struct Attempt<'a>(&'a AtomicU64);
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}
impl Recorder {
    /// Use `static TRACE: Recorder = Recorder::new();`, not a large stack local.
    pub const fn new() -> Self {
        Self {
            collection: Mutex::new(Collection {
                clock: None,
                limit: EVENT_CAPACITY,
            }),
            phase: AtomicU8::new(NEW),
            admission: AtomicU64::new(CLOSED),
            next: AtomicUsize::new(0),
            limit: AtomicUsize::new(EVENT_CAPACITY),
            slots: [const { Slot::new() }; EVENT_CAPACITY],
            domain_id: AtomicU64::new(0),
            losses: [const { Loss::new() }; KIND_COUNT],
            contention: AtomicU64::new(0),
            overflow: AtomicU64::new(0),
            wrong_clock: AtomicU64::new(0),
            poison: AtomicU64::new(0),
            saturated: AtomicBool::new(false),
        }
    }
    pub fn start(&self, clock: ClockDescription) -> Result<(), Error> {
        self.start_with_limit(clock, EVENT_CAPACITY)
    }
    /// A smaller limit is useful for bounded diagnostics; it never changes the
    /// actual preallocated footprint and cannot increase the capacity.
    pub fn start_with_limit(&self, clock: ClockDescription, limit: usize) -> Result<(), Error> {
        if clock.domain_id == 0
            || clock.process_id == 0
            || clock.resolution_ns == Some(0)
            || (clock.kind == ClockKind::RustInstant && clock.origin_monotonic_ns.is_some())
        {
            return Err(Error::InvalidClock);
        }
        if limit == 0 || limit > EVENT_CAPACITY {
            return Err(Error::InvalidLimit);
        }
        let mut collection = self.collection.try_lock().map_err(|_| Error::Busy)?;
        if self.phase.load(Ordering::Acquire) != NEW {
            return Err(Error::AlreadyStarted);
        }
        collection.clock = Some(clock);
        collection.limit = limit;
        self.domain_id.store(clock.domain_id, Ordering::Relaxed);
        self.limit.store(limit, Ordering::Relaxed);
        // Publish all configuration before the phase admits any appender.
        self.admission.store(0, Ordering::Release);
        self.phase.store(COLLECTING, Ordering::Release);
        Ok(())
    }
    pub fn is_collecting(&self) -> bool {
        self.phase.load(Ordering::Acquire) == COLLECTING
            && self.admission.load(Ordering::Acquire) & CLOSED == 0
    }
    fn attempt(&self) -> Option<Attempt<'_>> {
        if self.phase.load(Ordering::Acquire) != COLLECTING {
            return None;
        }
        self.admission
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                // This single CAS word closes admission AND registers appenders.
                // An impossible active-count wrap never reopens admission.
                (value & CLOSED == 0 && value < ACTIVE).then(|| value + 1)
            })
            .ok()?;
        Some(Attempt(&self.admission))
    }
    fn increment(&self, counter: &AtomicU64) {
        let previous = counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            })
            .unwrap();
        if previous == u64::MAX {
            self.saturated.store(true, Ordering::Relaxed);
        }
    }
    fn omit(&self, kind: EventKind, at: Option<u64>, reason: Omission) -> Append {
        let counter = match reason {
            Omission::Contended => &self.contention,
            Omission::Full => &self.overflow,
            Omission::WrongClockDomain => &self.wrong_clock,
            Omission::Poisoned => &self.poison,
        };
        self.increment(counter);
        let loss = &self.losses[kind as usize];
        self.increment(&loss.count);
        if let Some(at) = at {
            self.increment(&loss.timed_count);
            loss.first_ns.fetch_min(at, Ordering::Relaxed);
            loss.last_ns.fetch_max(at, Ordering::Relaxed);
        }
        Append::Omitted(reason)
    }
    pub fn append(&self, stamp: Stamp, kind: EventKind, fields: Fields) -> Append {
        let Some(_attempt) = self.attempt() else {
            return Append::Closed;
        };
        if self.domain_id.load(Ordering::Relaxed) != stamp.domain_id {
            return self.omit(kind, None, Omission::WrongClockDomain);
        }
        let limit = self.limit.load(Ordering::Relaxed);
        let ordinal = match self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                (value < limit).then(|| value + 1)
            }) {
            Ok(ordinal) => ordinal,
            Err(_) => return self.omit(kind, Some(stamp.delta_ns), Omission::Full),
        };
        self.slots[ordinal].publish(Event::new(stamp, kind, fields));
        Append::Recorded { ordinal }
    }
    /// Stops future admission without waiting. A call already in flight may
    /// finish; try_freeze returns Busy until all such calls have finished.
    pub fn stop(&self) -> Result<(), Error> {
        match self
            .phase
            .compare_exchange(COLLECTING, STOPPING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(STOPPING | FROZEN) => {
                self.admission.fetch_or(CLOSED, Ordering::AcqRel);
                Ok(())
            }
            Err(_) => Err(Error::NotStarted),
        }
    }
    /// Call only after the source/input/media actors have actually quiesced.
    /// Nonblocking; after Busy retain custody and retry off the media path.
    pub fn try_freeze(&self) -> Result<Frozen<'_>, Error> {
        self.stop()?;
        if self.admission.load(Ordering::Acquire) != CLOSED {
            return Err(Error::Busy);
        }
        let (collection, poisoned) = match self.collection.try_lock() {
            Ok(value) => (value, false),
            Err(TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(TryLockError::Poisoned(value)) => (value.into_inner(), true),
        };
        let recorded = self.next.load(Ordering::Acquire);
        if self.slots[..recorded]
            .iter()
            .any(|slot| slot.published.load(Ordering::Acquire) != 1)
        {
            return Err(Error::Busy); // Never export a fabricated zero/unpublished slot.
        }
        self.phase.store(FROZEN, Ordering::Release);
        let losses = std::array::from_fn(|index| {
            let loss = &self.losses[index];
            let timed_count = loss.timed_count.load(Ordering::Relaxed);
            LossSummary {
                count: loss.count.load(Ordering::Relaxed),
                timed_count,
                first_ns: (timed_count != 0).then(|| loss.first_ns.load(Ordering::Relaxed)),
                last_ns: (timed_count != 0).then(|| loss.last_ns.load(Ordering::Relaxed)),
            }
        });
        let summary = Summary {
            clock: collection.clock.unwrap(),
            capacity: collection.limit,
            recorded,
            contention: self.contention.load(Ordering::Relaxed),
            overflow: self.overflow.load(Ordering::Relaxed),
            wrong_clock: self.wrong_clock.load(Ordering::Relaxed),
            poison: self.poison.load(Ordering::Relaxed),
            poisoned,
            counter_saturated: self.saturated.load(Ordering::Relaxed),
            losses,
        };
        Ok(Frozen {
            recorder: self,
            _collection: collection,
            summary,
        })
    }
}

/// None avoids invoking the closure: no clock, field construction or allocation
/// is performed by this API. Capture any expensive work inside the closure.
pub fn record_if_enabled(
    recorder: Option<&Recorder>,
    event: impl FnOnce() -> (Stamp, EventKind, Fields),
) -> Append {
    let Some(recorder) = recorder else {
        return Append::Disabled;
    };
    if !recorder.is_collecting() {
        return Append::Closed;
    }
    let (stamp, kind, fields) = event();
    recorder.append(stamp, kind, fields)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LossSummary {
    pub count: u64,
    pub timed_count: u64,
    pub first_ns: Option<u64>,
    pub last_ns: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub clock: ClockDescription,
    pub capacity: usize,
    pub recorded: usize,
    pub contention: u64,
    pub overflow: u64,
    pub wrong_clock: u64,
    pub poison: u64,
    pub poisoned: bool,
    pub counter_saturated: bool,
    pub losses: [LossSummary; KIND_COUNT],
}
impl Summary {
    pub fn complete(&self) -> bool {
        !self.poisoned && !self.counter_saturated && self.losses.iter().all(|loss| loss.count == 0)
    }
}
pub struct Frozen<'a> {
    recorder: &'a Recorder,
    _collection: MutexGuard<'a, Collection>,
    summary: Summary,
}
pub struct Events<'a> {
    slots: &'a [Slot],
    next: usize,
}
impl Iterator for Events<'_> {
    type Item = Event;
    fn next(&mut self) -> Option<Event> {
        let slot = self.slots.get(self.next)?;
        self.next += 1;
        Some(slot.event())
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.slots.len() - self.next;
        (remaining, Some(remaining))
    }
}
impl ExactSizeIterator for Events<'_> {}
impl Frozen<'_> {
    pub fn summary(&self) -> &Summary {
        &self.summary
    }
    /// Decode one immutable atomic slot at a time; no second large event array.
    pub fn events(&self) -> Events<'_> {
        Events {
            slots: &self.recorder.slots[..self.summary.recorded],
            next: 0,
        }
    }
    pub fn export_bytes(&self) -> usize {
        FIXED_HEADER_BYTES + KIND_COUNT * LOSS_ROW_BYTES + self.events().len() * EVENT_BYTES
    }
    /// Writes only after freeze, without an internal buffer/allocation. The
    /// caller owns an exclusive private destination, its deadline and any IO
    /// buffering. An IO error is a partial export, never a success receipt.
    pub fn write_binary(&self, writer: &mut impl Write) -> io::Result<usize> {
        let summary = self.summary();
        writer.write_all(&MAGIC)?;
        for value in [
            SCHEMA_VERSION,
            FIXED_HEADER_BYTES as u32,
            EVENT_BYTES as u32,
            KIND_COUNT as u32,
        ] {
            writer.write_all(&value.to_le_bytes())?;
        }
        for value in [
            STORAGE_BUDGET_BYTES as u64,
            size_of::<Recorder>() as u64,
            summary.capacity as u64,
            summary.recorded as u64,
            summary.clock.domain_id,
        ] {
            writer.write_all(&value.to_le_bytes())?;
        }
        writer.write_all(&summary.clock.process_id.to_le_bytes())?;
        writer.write_all(&(summary.clock.kind as u16).to_le_bytes())?;
        let clock_validity = u16::from(summary.clock.origin_monotonic_ns.is_some())
            | (u16::from(summary.clock.resolution_ns.is_some()) << 1);
        writer.write_all(&clock_validity.to_le_bytes())?;
        let flags = u64::from(!summary.complete())
            | (u64::from(summary.poisoned) << 1)
            | (u64::from(summary.counter_saturated) << 2);
        for value in [
            summary.clock.origin_monotonic_ns.unwrap_or(0),
            summary.clock.resolution_ns.unwrap_or(0),
            summary.contention,
            summary.overflow,
            summary.wrong_clock,
            summary.poison,
            flags,
            OWNED_FOOTPRINT_BOUND_BYTES as u64,
            0,
        ] {
            writer.write_all(&value.to_le_bytes())?;
        }
        for loss in &summary.losses {
            for value in [
                loss.count,
                loss.timed_count,
                loss.first_ns.unwrap_or(0),
                loss.last_ns.unwrap_or(0),
            ] {
                writer.write_all(&value.to_le_bytes())?;
            }
        }
        for event in self.events() {
            event.write(writer)?;
        }
        Ok(self.export_bytes())
    }
}

/// Includes the inline collection, frozen export view, one summary return/copy,
/// and conservative scalar export scratch. No allocator is used by this crate.
pub const OWNED_FOOTPRINT_BOUND_BYTES: usize = size_of::<Recorder>()
    + size_of::<Frozen<'static>>()
    + size_of::<Summary>()
    + size_of::<Events<'static>>()
    + 1024;
pub const MAX_EXPORT_BYTES: usize =
    FIXED_HEADER_BYTES + KIND_COUNT * LOSS_ROW_BYTES + EVENT_CAPACITY * EVENT_BYTES;
const _: () = assert!(size_of::<Event>() == EVENT_BYTES);
const _: () = assert!(size_of::<Slot>() == SLOT_BYTES);
const _: () = assert!(OWNED_FOOTPRINT_BOUND_BYTES <= STORAGE_BUDGET_BYTES);
const _: () = assert!(MAX_EXPORT_BYTES <= STORAGE_BUDGET_BYTES);

#[cfg(test)]
mod tests;
