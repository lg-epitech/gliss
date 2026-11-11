//! Fuzzes the media window: any sequence of reservations, write completions,
//! skips, acknowledgements and resyncs, with any arguments.
//!
//! The model below keeps the same frames in a plain list and derives every
//! answer from the rules the window documents. After each operation the two
//! must agree on its result and on everything the window lets a caller see.

use std::time::{Duration, Instant};

use super::{ACK_TIMEOUT, Ack, RESYNC_INTERVAL, Window};
use crate::fuzz::support::{self, Rng};

struct Frame {
    generation: u64,
    sequence: u32,
    bytes: usize,
    written: Option<Instant>,
    acknowledged: bool,
}

struct Model {
    frame_limit: usize,
    byte_limit: usize,
    /// Frames whose bytes are still reserved, oldest first.
    frames: Vec<Frame>,
    generation: u64,
    resyncing: bool,
    /// The last sequence a resync retired.
    retired_through: Option<u32>,
    /// When the oldest frame was written that a resync retired before the
    /// browser acknowledged it, and that nothing has settled since.
    owed_since: Option<Instant>,
    resync_requested: bool,
    resync_after: Option<Instant>,
}

impl Model {
    fn reserved(&self) -> usize {
        self.frames.iter().map(|frame| frame.bytes).sum()
    }

    fn can_reserve(&self, bytes: usize) -> bool {
        bytes > 0
            && !self.resyncing
            && self.frames.len() < self.frame_limit
            // One packet larger than the whole budget may go alone.
            && (self.frames.is_empty()
                || self
                    .reserved()
                    .checked_add(bytes)
                    .is_some_and(|total| total <= self.byte_limit))
    }

    fn reserve(&mut self, sequence: u32, bytes: usize) -> bool {
        if !self.can_reserve(bytes) || self.find(None, sequence).is_some() {
            return false;
        }
        self.frames.push(Frame {
            generation: self.generation,
            sequence,
            bytes,
            written: None,
            acknowledged: false,
        });
        true
    }

    fn find(&self, generation: Option<u64>, sequence: u32) -> Option<usize> {
        self.frames.iter().position(|frame| {
            frame.sequence == sequence && generation.is_none_or(|it| it == frame.generation)
        })
    }

    fn owe(&mut self, written: Instant) {
        self.owed_since = Some(self.owed_since.map_or(written, |since| since.min(written)));
    }

    fn written(&mut self, generation: u64, sequence: u32, now: Instant) -> bool {
        let Some(at) = self.find(Some(generation), sequence) else {
            return false;
        };
        if self.frames[at].written.is_some() {
            return false;
        }
        self.frames[at].written = Some(now);
        if self.frames[at].acknowledged {
            self.frames.remove(at);
        } else if generation < self.generation {
            // Sent to the browser although a resync had retired it.
            self.owe(now);
        }
        true
    }

    fn skipped(&mut self, generation: u64, sequence: u32) -> bool {
        let Some(at) = self.find(Some(generation), sequence) else {
            return false;
        };
        if !self.resyncing || generation >= self.generation || self.frames[at].written.is_some() {
            return false;
        }
        self.frames.remove(at);
        true
    }

    /// Whether the sequence is one a resync retired: at or before the last
    /// retired one, counting around the end of the number range. Half the
    /// range is behind; the number exactly opposite counts as behind too.
    fn retired(&self, sequence: u32) -> bool {
        self.retired_through
            .is_some_and(|last| last.wrapping_sub(sequence) <= 1 << 31)
    }

    /// `Some(true)` for an acknowledgement that counts, `Some(false)` for one
    /// of a retired frame, `None` for one the browser may not send.
    fn acknowledge(&mut self, sequence: u32) -> Option<bool> {
        if self.retired(sequence) {
            return Some(false);
        }
        // Frames are acknowledged one by one, oldest first.
        let at = self
            .frames
            .iter()
            .position(|frame| frame.generation == self.generation && !frame.acknowledged)?;
        if self.frames[at].sequence != sequence {
            return None;
        }
        self.frames[at].acknowledged = true;
        self.owed_since = None;
        if self.frames[at].written.is_some() {
            self.frames.remove(at);
        }
        Some(true)
    }

    fn begin_resync(&mut self, last_sequence: u32, now: Instant) {
        if let Some(oldest) = self.oldest_unacknowledged() {
            self.owe(oldest);
        }
        self.resync_requested = false;
        self.resync_after = Some(now + RESYNC_INTERVAL);
        self.generation += 1;
        self.retired_through = Some(last_sequence);
        self.resyncing = true;
    }

    fn finish_resync(&mut self) -> bool {
        if !self.resyncing || self.frames.iter().any(|frame| frame.written.is_none()) {
            return false;
        }
        self.frames.clear();
        self.resyncing = false;
        true
    }

    /// When the oldest frame was written that the browser was sent and has
    /// not acknowledged.
    fn oldest_unacknowledged(&self) -> Option<Instant> {
        self.frames
            .iter()
            .filter(|frame| !frame.acknowledged)
            .filter_map(|frame| frame.written)
            .min()
    }

    fn deadline(&self) -> Option<Instant> {
        if self.resyncing {
            return None;
        }
        let written = self.oldest_unacknowledged()?;
        Some(self.owed_since.map_or(written, |owed| owed.min(written)) + ACK_TIMEOUT)
    }

    fn resync_due(&self, now: Instant) -> Option<Instant> {
        self.resync_requested
            .then(|| self.resync_after.map_or(now, |after| after.max(now)))
    }
}

/// A byte count around the limits a reservation has, `left` being what the
/// budget still has room for.
fn size(rng: &mut Rng, byte_limit: usize, left: usize) -> usize {
    match rng.below(12) {
        0 => 0,
        1 => byte_limit,
        2 => byte_limit + 1,
        3 => usize::MAX,
        4 => usize::MAX / 2 + 1,
        5 | 6 => left,
        7 => left + 1,
        _ => 1 + rng.below(byte_limit),
    }
}

/// One window and its model through `steps` operations.
fn session(rng: &mut Rng, steps: usize) {
    let frame_limit = rng.between(1, 8);
    let byte_limit = rng.pick(&[1, 100, 1024, 8 * 1024 * 1024]);
    let mut window = Window::new(frame_limit, byte_limit).unwrap();
    let mut model = Model {
        frame_limit,
        byte_limit,
        frames: Vec::new(),
        generation: 0,
        resyncing: false,
        retired_through: None,
        owed_since: None,
        resync_requested: false,
        resync_after: None,
    };
    let mut now = Instant::now();
    // The server numbers its frames one after another; the first number is
    // anything, so that some sessions cross the end of the range.
    let anywhere = rng.next() as u32;
    let mut sequence = rng.pick(&[0, 5, u32::MAX - 3, 1 << 31, anywhere]);
    // The latest deadline that the resyncs since the last acknowledgement
    // may leave: none of them may give the browser more time.
    let mut bound = None::<Instant>;
    let mut last_resync = None::<Instant>;
    for _ in 0..steps {
        if !rng.chance(3) {
            now += Duration::from_millis(rng.pick(&[1, 10, 249, 250, 251, 3000, 9999]));
        }
        // A frame the window holds, the oldest one half the time, or one
        // that it does not hold.
        let known = |rng: &mut Rng, model: &Model, unwritten: bool| {
            let frames: Vec<&Frame> = model
                .frames
                .iter()
                .filter(|frame| !unwritten || frame.written.is_none())
                .collect();
            if frames.is_empty() || rng.chance(8) {
                let generation = model.generation.saturating_sub(rng.below(3) as u64);
                return (generation, rng.next() as u32);
            }
            let frame = frames[if rng.chance(2) {
                0
            } else {
                rng.below(frames.len())
            }];
            (frame.generation, frame.sequence)
        };
        match rng.below(20) {
            0..5 => {
                let left = byte_limit.saturating_sub(model.reserved());
                let bytes = size(rng, byte_limit, left);
                let number = if rng.chance(10) {
                    known(rng, &model, false).1
                } else {
                    sequence = sequence.wrapping_add(1);
                    sequence
                };
                assert_eq!(window.can_reserve(bytes), model.can_reserve(bytes));
                assert_eq!(
                    window.reserve(number, bytes, now).is_ok(),
                    model.reserve(number, bytes)
                );
            }
            5..9 => {
                let unwritten = !rng.chance(8);
                let (generation, number) = known(rng, &model, unwritten);
                assert_eq!(
                    window
                        .written(generation, number, now, Duration::ZERO)
                        .is_ok(),
                    model.written(generation, number, now)
                );
            }
            9 | 10 => {
                let unwritten = !rng.chance(4);
                let (generation, number) = known(rng, &model, unwritten);
                assert_eq!(
                    window.skipped(generation, number).is_ok(),
                    model.skipped(generation, number)
                );
            }
            11..15 => {
                // The browser chooses this number.
                let oldest = model
                    .frames
                    .iter()
                    .find(|frame| frame.generation == model.generation && !frame.acknowledged);
                let number = match (rng.below(8), oldest, model.retired_through) {
                    (0..5, Some(frame), _) => frame.sequence,
                    (5, _, Some(last)) => last.wrapping_sub(rng.below(3) as u32),
                    (6, _, Some(last)) => last.wrapping_add(rng.pick(&[1, 1 << 31, (1 << 31) - 1])),
                    _ => known(rng, &model, false).1,
                };
                let expected = model.acknowledge(number);
                match (window.acknowledge(number, now), expected) {
                    (Ok(Ack::Accepted), Some(true)) => bound = None,
                    (Ok(Ack::Retired), Some(false)) | (Err(_), None) => {}
                    (actual, expected) => panic!(
                        "acknowledging {number} gave {:?}, not {expected:?}",
                        actual.map(|ack| matches!(ack, Ack::Accepted))
                    ),
                }
            }
            15 => {
                window.request_resync();
                model.resync_requested = true;
            }
            16 | 17 => {
                // As the session does: a resync starts when one is due.
                let due = window.resync_due(now);
                assert_eq!(due, model.resync_due(now));
                if due.is_some_and(|due| due <= now) || rng.chance(6) {
                    if due.is_some_and(|due| due <= now) {
                        assert!(
                            last_resync.is_none_or(|last| now >= last + RESYNC_INTERVAL),
                            "two resyncs started within the interval"
                        );
                    }
                    if let Some(deadline) = window.oldest_deadline() {
                        bound = Some(bound.map_or(deadline, |bound| bound.min(deadline)));
                    }
                    window.begin_resync(sequence, now).unwrap();
                    model.begin_resync(sequence, now);
                    last_resync = Some(now);
                }
            }
            18 => assert_eq!(window.finish_resync().is_ok(), model.finish_resync()),
            _ => {
                window.picture_resumed();
                model.owed_since = None;
                bound = None;
            }
        }
        let seen = window.snapshot();
        assert_eq!(seen["reserved_frames"], model.frames.len());
        assert!(model.frames.len() <= frame_limit);
        assert_eq!(seen["reserved_bytes"], model.reserved());
        assert_eq!(
            seen["unacknowledged_frames"],
            model
                .frames
                .iter()
                .filter(|frame| !frame.acknowledged)
                .count()
        );
        assert_eq!(window.generation(), model.generation);
        assert_eq!(window.resyncing(), model.resyncing);
        assert_eq!(
            window.can_encode(),
            !model.resyncing && model.frames.len() < frame_limit && model.reserved() < byte_limit
        );
        assert_eq!(window.resync_due(now), model.resync_due(now));
        let deadline = window.oldest_deadline();
        assert_eq!(deadline, model.deadline());
        if let (Some(deadline), Some(bound)) = (deadline, bound) {
            assert!(deadline <= bound, "a resync gave the browser more time");
        }
    }
}

const DEFAULT_ITERATIONS: u64 = 400;

#[test]
fn fuzz_media_window() {
    support::run("server-media-window", DEFAULT_ITERATIONS, |rng, _| {
        let steps = rng.between(1, 60);
        session(rng, steps);
    });
}

/// Found by `fuzz_media_window`. The deadline was taken from the first
/// unacknowledged frame in the queue that had been written, which is the
/// oldest write only while writes are reported in the order of the queue.
/// The session's one writer does report them so; the window did not say it
/// relied on that.
#[test]
fn a_write_reported_after_a_newer_one_does_not_move_the_deadline() {
    let start = Instant::now();
    let mut window = Window::new(2, 100).unwrap();
    window.reserve(1, 20, start).unwrap();
    window.reserve(2, 20, start).unwrap();
    window.written(0, 2, start, Duration::ZERO).unwrap();
    assert_eq!(window.oldest_deadline(), Some(start + ACK_TIMEOUT));
    let later = start + Duration::from_secs(5);
    window.written(0, 1, later, Duration::ZERO).unwrap();
    assert_eq!(window.oldest_deadline(), Some(start + ACK_TIMEOUT));
}
