//! Optional observations only. No clocks, payloads or recorder dependency here.
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    CommandQueued,
    WriteStarted,
    WriteCompleted,
    UpdateHeader,
    UpdateDecoded,
    CloneStarted,
    CloneCompleted,
    EventQueued,
    EventDequeued,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Observed,
    Accepted,
    Rejected,
    Error,
    Empty,
    Metadata,
    Pixels,
    Ambiguous,
    Canceled,
}
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub kind: Kind,
    /// Command ordinal for writes; independent update ordinal for update/frame events.
    /// RFB has no echoed request ID: these domains must never be equated.
    pub ordinal: u64,
    pub input_id: Option<u32>,
    pub count: u32,
    pub outcome: Outcome,
    pub capture_request: bool,
}
pub trait Observer: Send + Sync {
    fn record(&self, value: Record);
    fn actor_started(&self) -> bool;
    fn actor_stopped(&self);
}
pub type Sink = Option<Arc<dyn Observer>>;
pub(crate) fn emit(
    sink: &Sink,
    kind: Kind,
    ordinal: u64,
    input_id: Option<u32>,
    count: u32,
    outcome: Outcome,
) {
    if let Some(sink) = sink {
        sink.record(Record {
            kind,
            ordinal,
            input_id,
            count,
            outcome,
            capture_request: false,
        });
    }
}
/// Construct before spawning, then transfer into the future. Even a future
/// aborted before first poll drops this lease and closes its actor interval.
pub(crate) struct Actor(Sink);
impl Actor {
    pub fn new(sink: Sink) -> anyhow::Result<Self> {
        if let Some(value) = &sink {
            anyhow::ensure!(value.actor_started(), "trace actor admission closed");
        }
        Ok(Self(sink))
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        if let Some(value) = &self.0 {
            value.actor_stopped();
        }
    }
}

/// Cancellation may leave a partial TCP write; this reports cancellation,
/// never successful completion or a fabricated written-byte count.
pub(crate) struct Terminal {
    sink: Sink,
    record: Option<Record>,
}
pub const TERMINAL_GUARD_BYTES: usize = std::mem::size_of::<Terminal>();
pub const ACTOR_GUARD_BYTES: usize = std::mem::size_of::<Actor>();
impl Terminal {
    pub fn new(sink: &Sink, record: Record) -> Self {
        Self {
            sink: sink.clone(),
            record: sink.as_ref().map(|_| record),
        }
    }
    pub fn finish(mut self, outcome: Outcome) {
        if let (Some(sink), Some(mut record)) = (&self.sink, self.record.take()) {
            record.outcome = outcome;
            sink.record(record);
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let (Some(sink), Some(mut record)) = (&self.sink, self.record.take()) {
            record.outcome = Outcome::Canceled;
            sink.record(record);
        }
    }
}
