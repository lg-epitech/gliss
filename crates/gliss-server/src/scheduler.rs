//! Bounded FIFO media reservations. An early ACK cannot release a write's bytes.

use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub const DEFAULT_BYTES: usize = 8 * 1024 * 1024;
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Least time between the starts of two resyncs. Each one costs a keyframe,
/// many times the size of the frame it replaces. A browser asks once for each
/// frame it could not decode, so a real decode error brings a short burst of
/// requests that one keyframe answers; four a second is more than recovery needs.
pub const RESYNC_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default, Serialize)]
struct Timing {
    count: u64,
    total_ms: f64,
    max_ms: f64,
}

impl Timing {
    fn observe(&mut self, duration: Duration) {
        let millis = duration.as_secs_f64() * 1000.0;
        self.count += 1;
        self.total_ms += millis;
        self.max_ms = self.max_ms.max(millis);
    }
}

#[derive(Default, Serialize)]
struct Stats {
    observed_capture_replacements: u64,
    encoded_frames: u64,
    written_frames: u64,
    acknowledged_frames: u64,
    discarded_generation_packets: u64,
    discarded_generation_encodes: u64,
    ignored_retired_acks: u64,
    ack_before_write_completion: u64,
    resyncs: u64,
    oversized_packets: u64,
    reserved_frames_high_water: usize,
    reserved_bytes_high_water: usize,
    encode_ms: Timing,
    encode_cpu_ms: Timing,
    source_age_ms: Timing,
    encode_worker_wait_ms: Timing,
    write_ms: Timing,
    enqueue_to_ack_ms: Timing,
    byte_capacity_wait_ms: Timing,
}

struct Reservation {
    generation: u64,
    sequence: u32,
    bytes: usize,
    queued: Instant,
    written: Option<Instant>,
    acknowledged: bool,
}

pub enum Ack {
    Accepted,
    Retired,
}

pub struct Window {
    frame_limit: usize,
    byte_limit: usize,
    reserved_bytes: usize,
    reservations: VecDeque<Reservation>,
    generation: u64,
    retired_through: Option<u32>,
    resyncing: bool,
    /// When the oldest frame was written that a resync retired before the
    /// browser acknowledged it. Its deadline passes to the frames that follow.
    retired_unacknowledged: Option<Instant>,
    resync_requested: bool,
    /// When the next resync may start.
    resync_after: Option<Instant>,
    stats: Stats,
}

impl Window {
    pub fn new(frame_limit: usize, byte_limit: usize) -> Result<Self> {
        ensure!(
            (1..=8).contains(&frame_limit),
            "in-flight-frames must be 1..8"
        );
        ensure!(byte_limit > 0, "in-flight byte limit must be positive");
        Ok(Self {
            frame_limit,
            byte_limit,
            reserved_bytes: 0,
            reservations: VecDeque::with_capacity(frame_limit),
            generation: 0,
            retired_through: None,
            resyncing: false,
            retired_unacknowledged: None,
            resync_requested: false,
            resync_after: None,
            stats: Stats::default(),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn resyncing(&self) -> bool {
        self.resyncing
    }

    /// One sequential encode may start only when no encoded packet is waiting.
    pub fn can_encode(&self) -> bool {
        !self.resyncing
            && self.reservations.len() < self.frame_limit
            && self.reserved_bytes < self.byte_limit
    }

    /// One oversized legal packet may make progress only in an empty window.
    pub fn can_reserve(&self, bytes: usize) -> bool {
        bytes > 0
            && !self.resyncing
            && self.reservations.len() < self.frame_limit
            && (self.reservations.is_empty()
                || self
                    .reserved_bytes
                    .checked_add(bytes)
                    .is_some_and(|total| total <= self.byte_limit))
    }

    pub fn reserve(&mut self, sequence: u32, bytes: usize, now: Instant) -> Result<()> {
        ensure!(
            self.can_reserve(bytes),
            "media reservation exceeds frame/byte capacity"
        );
        ensure!(
            !self
                .reservations
                .iter()
                .any(|entry| entry.sequence == sequence),
            "duplicate media sequence"
        );
        self.reserved_bytes = self
            .reserved_bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("media byte accounting overflow"))?;
        self.reservations.push_back(Reservation {
            generation: self.generation,
            sequence,
            bytes,
            queued: now,
            written: None,
            acknowledged: false,
        });
        self.stats.oversized_packets += u64::from(bytes > self.byte_limit);
        self.stats.reserved_frames_high_water = self
            .stats
            .reserved_frames_high_water
            .max(self.reservations.len());
        self.stats.reserved_bytes_high_water = self
            .stats
            .reserved_bytes_high_water
            .max(self.reserved_bytes);
        Ok(())
    }

    pub fn written(
        &mut self,
        generation: u64,
        sequence: u32,
        now: Instant,
        duration: Duration,
    ) -> Result<()> {
        let position = self
            .reservations
            .iter()
            .position(|entry| entry.generation == generation && entry.sequence == sequence)
            .ok_or_else(|| anyhow::anyhow!("write completed without media reservation"))?;
        let entry = &mut self.reservations[position];
        ensure!(entry.written.is_none(), "media write completed twice");
        entry.written = Some(now);
        self.stats.written_frames += 1;
        self.stats.write_ms.observe(duration);
        if entry.acknowledged {
            self.remove(position);
        } else if generation < self.generation {
            // Its write was under way when the resync retired it.
            self.retire_unacknowledged(now);
        }
        Ok(())
    }

    pub fn skipped(&mut self, generation: u64, sequence: u32) -> Result<()> {
        ensure!(
            self.resyncing && generation < self.generation,
            "only retired media may be skipped"
        );
        let position = self
            .reservations
            .iter()
            .position(|entry| entry.generation == generation && entry.sequence == sequence)
            .ok_or_else(|| anyhow::anyhow!("skipped packet lacked a reservation"))?;
        ensure!(
            self.reservations[position].written.is_none(),
            "cannot skip a completed write"
        );
        self.remove(position);
        self.stats.discarded_generation_packets += 1;
        Ok(())
    }

    fn remove(&mut self, position: usize) {
        let entry = self.reservations.remove(position).unwrap();
        self.reserved_bytes -= entry.bytes;
    }

    pub fn acknowledge(&mut self, sequence: u32, now: Instant) -> Result<Ack> {
        // Session TTL and FPS bound the stream far below a half-u32 sequence span.
        if self
            .retired_through
            .is_some_and(|last| sequence.wrapping_sub(last) as i32 <= 0)
        {
            self.stats.ignored_retired_acks += 1;
            return Ok(Ack::Retired);
        }
        let position = self
            .reservations
            .iter()
            .position(|entry| entry.generation == self.generation && !entry.acknowledged)
            .ok_or_else(|| anyhow::anyhow!("unexpected media acknowledgement"))?;
        let entry = &mut self.reservations[position];
        ensure!(
            entry.sequence == sequence,
            "media acknowledgements must match the oldest individual sequence"
        );
        entry.acknowledged = true;
        // The browser decodes again: what it left unacknowledged before is settled.
        self.retired_unacknowledged = None;
        self.stats.acknowledged_frames += 1;
        self.stats
            .enqueue_to_ack_ms
            .observe(now.saturating_duration_since(entry.queued));
        self.stats.ack_before_write_completion += u64::from(entry.written.is_none());
        // A writer completion can race a received ACK; retain bytes until both.
        if entry.written.is_some() {
            self.remove(position);
        }
        Ok(Ack::Accepted)
    }

    /// When the session ends unless the browser acknowledges a frame.
    ///
    /// There is a deadline only while the browser holds a frame it can still
    /// acknowledge. It counts from the oldest frame the browser was sent and
    /// has not acknowledged, including frames a resync has retired since:
    /// asking for a keyframe is not a way to keep a session without decoding.
    pub fn oldest_deadline(&self) -> Option<Instant> {
        if self.resyncing {
            return None;
        }
        let written = self
            .reservations
            .iter()
            .find(|entry| !entry.acknowledged && entry.written.is_some())
            .and_then(|entry| entry.written)?;
        let since = self
            .retired_unacknowledged
            .map_or(written, |retired| retired.min(written));
        Some(since + ACK_TIMEOUT)
    }

    fn retire_unacknowledged(&mut self, written: Instant) {
        self.retired_unacknowledged = Some(
            self.retired_unacknowledged
                .map_or(written, |oldest| oldest.min(written)),
        );
    }

    /// The picture is back after a time in which nothing could be encoded.
    /// The browser had nothing to acknowledge meanwhile, so frames retired
    /// before or during that time no longer count against it.
    pub fn picture_resumed(&mut self) {
        self.retired_unacknowledged = None;
    }

    /// The browser asked for a keyframe. Requests made before the resync
    /// starts are one request.
    pub fn request_resync(&mut self) {
        self.resync_requested = true;
    }

    /// When the requested resync may start: at `now`, or later if the last
    /// one started less than [`RESYNC_INTERVAL`] ago. None without a request.
    pub fn resync_due(&self, now: Instant) -> Option<Instant> {
        self.resync_requested
            .then(|| self.resync_after.map_or(now, |after| after.max(now)))
    }

    pub fn begin_resync(&mut self, last_sequence: u32, now: Instant) -> Result<()> {
        if let Some(written) = self
            .reservations
            .iter()
            .filter(|entry| !entry.acknowledged)
            .filter_map(|entry| entry.written)
            .min()
        {
            self.retire_unacknowledged(written);
        }
        self.resync_requested = false;
        self.resync_after = Some(now + RESYNC_INTERVAL);
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("media generation overflow"))?;
        self.retired_through = Some(last_sequence);
        self.resyncing = true;
        self.stats.resyncs += 1;
        Ok(())
    }

    /// Called only after the writer queue and current encoder job have settled.
    pub fn finish_resync(&mut self) -> Result<()> {
        ensure!(self.resyncing, "media generation barrier is not active");
        ensure!(
            self.reservations
                .iter()
                .all(|entry| entry.written.is_some()),
            "resync cannot release bytes while a write is outstanding"
        );
        self.reservations.clear();
        self.reserved_bytes = 0;
        self.resyncing = false;
        Ok(())
    }

    pub fn capture_replaced(&mut self) {
        self.stats.observed_capture_replacements += 1;
    }
    pub fn encoded(&mut self, work: Duration, wait: Duration) {
        self.stats.encoded_frames += 1;
        self.stats.encode_ms.observe(work);
        self.stats.encode_worker_wait_ms.observe(wait);
    }
    pub fn discarded_encode(&mut self) {
        self.stats.discarded_generation_encodes += 1;
    }
    pub fn profiled_encode(&mut self, cpu: Option<Duration>, source_age: Option<Duration>) {
        if let Some(cpu) = cpu {
            self.stats.encode_cpu_ms.observe(cpu);
        }
        if let Some(age) = source_age {
            self.stats.source_age_ms.observe(age);
        }
    }
    pub fn waited_for_bytes(&mut self, wait: Duration) {
        self.stats.byte_capacity_wait_ms.observe(wait);
    }

    pub fn snapshot(&self) -> Value {
        json!({"type":"stream_stats", "generation":self.generation, "in_flight_frames_limit":self.frame_limit, "in_flight_bytes_limit":self.byte_limit, "reserved_frames":self.reservations.len(), "unacknowledged_frames":self.reservations.iter().filter(|entry| !entry.acknowledged).count(), "reserved_bytes":self.reserved_bytes, "resyncing":self.resyncing, "capture_replacement_scope":"observed session replacements; watch coalescing is not counted", "counters":self.stats})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_waits_for_both_orderings_of_ack_and_write() {
        let now = Instant::now();
        for ack_first in [false, true] {
            let mut window = Window::new(1, 100).unwrap();
            window.reserve(1, 80, now).unwrap();
            if ack_first {
                window.acknowledge(1, now).unwrap();
            } else {
                window.written(0, 1, now, Duration::ZERO).unwrap();
            }
            assert!(!window.can_encode());
            if ack_first {
                window.written(0, 1, now, Duration::ZERO).unwrap();
            } else {
                window.acknowledge(1, now).unwrap();
            }
            assert!(window.can_encode());
            assert_eq!(window.reserved_bytes, 0);
        }
    }

    #[test]
    fn byte_budget_includes_queued_writes_and_oversize_progress_is_exclusive() {
        let now = Instant::now();
        let mut window = Window::new(8, 100).unwrap();
        window.reserve(1, 70, now).unwrap();
        assert!(!window.can_reserve(31));
        assert!(window.can_reserve(30));
        window.reserve(2, 30, now).unwrap();
        assert!(!window.can_encode());
        window.written(0, 1, now, Duration::ZERO).unwrap();
        window.acknowledge(1, now).unwrap();
        assert!(!window.can_reserve(101));
        window.written(0, 2, now, Duration::ZERO).unwrap();
        window.acknowledge(2, now).unwrap();
        window.reserve(3, 101, now).unwrap();
        assert!(!window.can_encode());
        assert!(!window.can_reserve(1));
    }

    #[test]
    fn acknowledgements_are_individual_ordered_and_never_grant_unknown_credit() {
        let now = Instant::now();
        let mut window = Window::new(3, 100).unwrap();
        window.reserve(1, 20, now).unwrap();
        window.reserve(2, 20, now).unwrap();
        assert!(window.acknowledge(2, now).is_err());
        assert!(window.acknowledge(99, now).is_err());
        assert_eq!(window.reserved_bytes, 40);
        window.acknowledge(1, now).unwrap();
        assert!(window.acknowledge(1, now).is_err());
        window.acknowledge(2, now).unwrap();
        assert_eq!(window.reserved_bytes, 40);
    }

    #[test]
    fn oldest_deadline_does_not_move_when_later_frames_are_written() {
        let now = Instant::now();
        let mut window = Window::new(2, 100).unwrap();
        window.reserve(1, 20, now).unwrap();
        window.reserve(2, 20, now).unwrap();
        assert!(window.oldest_deadline().is_none());
        window.written(0, 1, now, Duration::ZERO).unwrap();
        window
            .written(0, 2, now + Duration::from_secs(1), Duration::ZERO)
            .unwrap();
        assert_eq!(window.oldest_deadline(), Some(now + ACK_TIMEOUT));
        window.acknowledge(1, now).unwrap();
        assert_eq!(
            window.oldest_deadline(),
            Some(now + Duration::from_secs(1) + ACK_TIMEOUT)
        );
    }

    #[test]
    fn resync_retains_writes_until_barrier_and_retires_old_acknowledgements() {
        let now = Instant::now();
        let mut window = Window::new(2, 100).unwrap();
        window.reserve(1, 40, now).unwrap();
        window.reserve(2, 40, now).unwrap();
        window.begin_resync(2, now).unwrap();
        assert!(!window.can_encode());
        assert!(window.finish_resync().is_err());
        assert!(matches!(window.acknowledge(2, now).unwrap(), Ack::Retired));
        assert_eq!(window.reserved_bytes, 80);
        window.written(0, 1, now, Duration::ZERO).unwrap();
        window.skipped(0, 2).unwrap();
        window.finish_resync().unwrap();
        window.reserve(3, 50, now).unwrap();
        assert!(matches!(window.acknowledge(1, now).unwrap(), Ack::Retired));
        assert_eq!(window.reserved_bytes, 50);
        assert!(window.acknowledge(4, now).is_err());
    }

    #[test]
    fn resync_ack_boundary_handles_sequence_wrap() {
        let now = Instant::now();
        let mut window = Window::new(1, 100).unwrap();
        window.begin_resync(u32::MAX, now).unwrap();
        window.finish_resync().unwrap();
        window.reserve(0, 20, now).unwrap();
        assert!(matches!(
            window.acknowledge(u32::MAX, now).unwrap(),
            Ack::Retired
        ));
        assert!(matches!(window.acknowledge(0, now).unwrap(), Ack::Accepted));
    }

    /// A window of one frame whose single frame was written at `written`.
    fn outstanding(sequence: u32, written: Instant) -> Window {
        let mut window = Window::new(1, 100).unwrap();
        window.reserve(sequence, 20, written).unwrap();
        window
            .written(0, sequence, written, Duration::ZERO)
            .unwrap();
        window
    }

    #[test]
    fn a_resync_does_not_move_the_acknowledgement_deadline() {
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut window = outstanding(1, at(0));
        assert_eq!(window.oldest_deadline(), Some(at(0) + ACK_TIMEOUT));
        // The browser never acknowledges. It asks for a keyframe every three seconds.
        for sequence in 2..=4 {
            let now = at(3 * (u64::from(sequence) - 1));
            window.request_resync();
            window.begin_resync(sequence - 1, now).unwrap();
            window.finish_resync().unwrap();
            // Nothing it could acknowledge has been written yet.
            assert_eq!(window.oldest_deadline(), None);
            window.reserve(sequence, 20, now).unwrap();
            window
                .written(window.generation(), sequence, now, Duration::ZERO)
                .unwrap();
            assert_eq!(window.oldest_deadline(), Some(at(0) + ACK_TIMEOUT));
        }
        // One acknowledgement of a current frame settles every older one.
        assert!(matches!(
            window.acknowledge(4, at(9)).unwrap(),
            Ack::Accepted
        ));
        assert_eq!(window.oldest_deadline(), None);
        window.reserve(5, 20, at(9)).unwrap();
        window
            .written(window.generation(), 5, at(9), Duration::ZERO)
            .unwrap();
        assert_eq!(window.oldest_deadline(), Some(at(9) + ACK_TIMEOUT));
    }

    #[test]
    fn a_frame_still_being_written_at_the_resync_counts_from_its_write() {
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut window = Window::new(1, 100).unwrap();
        window.reserve(1, 20, at(0)).unwrap();
        window.request_resync();
        window.begin_resync(1, at(1)).unwrap();
        window.written(0, 1, at(2), Duration::ZERO).unwrap();
        window.finish_resync().unwrap();
        window.reserve(2, 20, at(5)).unwrap();
        window.written(1, 2, at(5), Duration::ZERO).unwrap();
        assert_eq!(window.oldest_deadline(), Some(at(2) + ACK_TIMEOUT));
    }

    #[test]
    fn a_resumed_picture_starts_the_acknowledgement_clock_again() {
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut window = outstanding(1, at(0));
        window.request_resync();
        window.begin_resync(1, at(1)).unwrap();
        window.finish_resync().unwrap();
        // Nothing could be encoded for a minute: the browser had nothing to acknowledge.
        window.picture_resumed();
        window.reserve(2, 20, at(60)).unwrap();
        window.written(1, 2, at(60), Duration::ZERO).unwrap();
        assert_eq!(window.oldest_deadline(), Some(at(60) + ACK_TIMEOUT));
    }

    #[test]
    fn resyncs_start_once_per_interval_and_requests_in_between_become_one() {
        let start = Instant::now();
        let mut window = Window::new(1, 100).unwrap();
        assert_eq!(window.resync_due(start), None);
        // After a quiet time the first request is served at once.
        window.request_resync();
        assert_eq!(window.resync_due(start), Some(start));
        window.begin_resync(0, start).unwrap();
        window.finish_resync().unwrap();
        assert_eq!(window.resync_due(start), None);
        let soon = start + Duration::from_millis(10);
        for _ in 0..100 {
            window.request_resync();
        }
        assert_eq!(window.resync_due(soon), Some(start + RESYNC_INTERVAL));
        let later = start + Duration::from_secs(1);
        assert_eq!(window.resync_due(later), Some(later));
        window.begin_resync(0, later).unwrap();
        window.finish_resync().unwrap();
        assert_eq!(window.resync_due(later), None);
        assert_eq!(window.stats.resyncs, 2);
        assert_eq!(window.generation(), 2);
    }
}
