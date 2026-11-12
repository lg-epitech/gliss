//! What the browser's acknowledgements say about the link to it.
//!
//! The server writes each frame into a TCP connection that usually ends at a
//! relay, so it never feels the link through its own socket. All it sees is when
//! it wrote a frame and when that frame reached the browser: the browser
//! acknowledges a frame after painting it and says how long it held it. From
//! those two times this estimates how many bytes a second the link carries when
//! frames queue on it, keeps count of how long the frames already written will
//! occupy it, and says when another frame would no longer wait behind them.
//!
//! A link that keeps up yields no rate, and then nothing here limits the
//! stream: the frame window of the scheduler is all that applies. What reads
//! as a link that does not keep up is any delay that grows with the bytes in
//! flight and that the browser did not report as its own.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// How far back the quickest acknowledgement is looked for, and at how many.
const DELAY_WINDOW: Duration = Duration::from_secs(30);
const DELAYS: usize = 256;
/// How long frames may queue on the link before another one is held back.
const QUEUE: Duration = Duration::from_millis(30);
/// How much later than expected an acknowledgement may be before the link counts
/// as behind. It covers what varies on a link that keeps up: the browser paints
/// on a 16.7 ms clock and a large frame takes longer to decode.
const LATE: Duration = Duration::from_millis(50);
/// The least time over which bytes are counted for a rate. Shorter spans say more
/// about when the browser painted than about the link.
const MIN_SPAN: Duration = Duration::from_millis(100);
/// A run of queued frames is measured for this long, then a new one starts.
const MAX_SPAN: Duration = Duration::from_millis(400);
/// A lower rate is believed once two measurements agree that are at most this
/// far apart, or three times the gap between two acknowledgements if that is
/// longer: on a link so slow that a frame takes most of a second, measurements
/// are that far apart. One late frame is more often a pause on the way than a
/// slower link.
const CONFIRM: Duration = Duration::from_millis(500);
/// The encoder follows the rate for this long after the link last was behind or
/// held a frame back. A desktop that then changes only now and then never
/// queues, so nothing would show that the link became faster.
const RECENT: Duration = Duration::from_secs(10);
/// A measured rate stands for this long, or for two frames' time on the link if
/// that is longer. After that no frames have queued, the link may have become
/// faster, and the rate grows until they queue again.
const TRUST: Duration = Duration::from_millis(300);
/// That growth, for each acknowledgement that came on time while the rate was
/// holding frames back.
const PROBE: f64 = 1.05;
/// The same once no frame has been late for [`CLEAR`], or for four frames'
/// time on the link if that is longer: the link is then faster than the rate
/// by more than a little, and every frame held back meanwhile is a frame the
/// viewer does not get.
const PROBE_CLEAR: f64 = 1.15;
const CLEAR: Duration = Duration::from_secs(1);
/// And for each second of on-time acknowledgements while it was not.
const DRIFT: f64 = 1.1;
/// A rate above this, a gigabit a second, limits nothing: the link keeps up
/// again and the rate is forgotten.
const UNLIMITED: f64 = 125_000_000.0;
/// The longest the link is taken to be busy, whatever the rate says. A rate
/// that is far too low, from acknowledgements that came for some other reason
/// than the link, must not stop the stream: the next frame goes out after this
/// long at the latest, and its acknowledgement is a new measurement.
const MAX_BUSY: Duration = Duration::from_secs(2);

/// Frames that queued behind one another on the link: when the first of them
/// was acknowledged, and the bytes acknowledged since. The link carried those
/// bytes in that time, whatever held the first frame up.
struct Run {
    start: Instant,
    bytes: usize,
}

struct Delay {
    at: Instant,
    delay: Duration,
}

#[derive(Default)]
pub struct Link {
    /// The latest acknowledgements.
    delays: VecDeque<Delay>,
    /// Bytes a second the link carried when frames last queued on it.
    rate: Option<f64>,
    run: Option<Run>,
    last_ack: Option<Instant>,
    /// A rate below the current one that has been measured once.
    unconfirmed: Option<(Instant, f64)>,
    measured_at: Option<Instant>,
    late_at: Option<Instant>,
    /// When a frame last was late or was held back.
    behind_at: Option<Instant>,
    drifted: Option<Instant>,
    /// About how long the browser lately held a frame before painting it.
    held: Duration,
    /// When the link will have carried every frame written so far, were they
    /// written straight onto it: the delay to the link is the same for all of
    /// them and does not change how long they occupy it.
    busy_until: Option<Instant>,
}

impl Link {
    /// `bytes` written at `written` reached the browser at `now`, by its
    /// acknowledgement less the time it says it `held` them, and
    /// `unacknowledged` bytes written since are still out. `limited` says
    /// whether the rate held a frame back since the last call.
    pub fn acknowledged(
        &mut self,
        now: Instant,
        written: Instant,
        bytes: usize,
        unacknowledged: usize,
        limited: bool,
        held: Duration,
    ) {
        // The largest of the last few, fading: a frame is not overdue for a
        // decode that takes as long as the one before it did.
        self.held = held.max(self.held * 7 / 8);
        let delay = now.saturating_duration_since(written);
        if self.delays.len() == DELAYS {
            self.delays.pop_front();
        }
        self.delays.push_back(Delay { at: now, delay });
        // It queued if it was written early enough to reach the link before the
        // frame ahead of it had left, and came later than its own time on the
        // link explains. Then the link was busy from that frame's acknowledgement
        // to this one. Both have to hold: a frame paced by a rate that is too low
        // follows the last one closely and still finds the link idle.
        let base = self.base(now);
        let late = delay > base + self.time_for(bytes) + QUEUE / 2;
        let queued = late && self.last_ack.is_some_and(|last| written + base < last);
        let gap = self
            .last_ack
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last_ack = Some(now);
        match &mut self.run {
            Some(run) if queued => run.bytes += bytes,
            _ if queued => {
                self.run = Some(Run {
                    start: now,
                    bytes: 0,
                })
            }
            _ => self.run = None,
        }
        let span = self.run.as_ref().map_or(Duration::ZERO, |run| {
            now.saturating_duration_since(run.start)
        });
        if span >= MIN_SPAN {
            let measured = self.run.as_ref().unwrap().bytes as f64 / span.as_secs_f64();
            if span >= MAX_SPAN {
                self.run = Some(Run {
                    start: now,
                    bytes: 0,
                });
            }
            self.measured(now, measured, CONFIRM.max(3 * gap));
        }
        if late || limited {
            self.behind_at = Some(now);
        }
        if late {
            self.late_at = Some(now);
        } else {
            self.on_time(now, limited, self.time_for(bytes));
        }
        // This frame left the link `base` before a frame of no size written now
        // would reach it, and the link still has to carry what is out. If that
        // is later than the count of writes says, the rate was too high for a
        // while and frames have queued: wait for them.
        if self.rate.is_some() {
            let carried = (now + self.time_for(unacknowledged)).checked_sub(self.base(now));
            self.busy_until = self.busy_until.max(carried).min(Some(now + MAX_BUSY));
        }
    }

    /// The link carried `measured` bytes a second. A lower rate than the
    /// current one needs another like it within `confirm`.
    fn measured(&mut self, now: Instant, measured: f64, confirm: Duration) {
        self.measured_at = Some(now);
        if let Some(rate) = self.rate.as_mut().filter(|rate| measured >= **rate) {
            // Half way: when the browser painted two frames blurs each measurement.
            *rate += (measured - *rate) / 2.0;
            self.unconfirmed = None;
            return;
        }
        match self.unconfirmed.take() {
            Some((at, earlier)) if now.saturating_duration_since(at) <= confirm => {
                self.rate = Some(earlier.max(measured));
            }
            _ => self.unconfirmed = Some((now, measured)),
        }
    }

    /// A frame that took `own` on the link came on time.
    fn on_time(&mut self, now: Instant, limited: bool, own: Duration) {
        let trusted = self
            .measured_at
            .is_some_and(|at| now.saturating_duration_since(at) < TRUST.max(2 * own));
        let Some(rate) = self.rate.as_mut().filter(|_| !trusted) else {
            return;
        };
        if limited {
            let clear = self
                .late_at
                .is_none_or(|at| now.saturating_duration_since(at) >= CLEAR.max(4 * own));
            *rate *= if clear { PROBE_CLEAR } else { PROBE };
        } else {
            // A second of acknowledgements, not the first one after a pause.
            let second = Duration::from_secs(1);
            match self.drifted.map(|at| now.saturating_duration_since(at)) {
                Some(since) if since < second => {}
                Some(since) if since < 2 * second => {
                    *rate *= DRIFT;
                    self.drifted = Some(now);
                }
                _ => self.drifted = Some(now),
            }
        }
        if *rate > UNLIMITED {
            self.rate = None;
            self.busy_until = None;
            self.unconfirmed = None;
        }
    }

    /// How long the browser takes to acknowledge a frame: the link both ways,
    /// the decode and the wait for a paint. It is the least recent delay, so it
    /// includes that frame's own time on the link and is too long by that much.
    /// It must not be too short: every frame would then look late, and a rate
    /// measured from frames that only look late is lower than the link's, which
    /// makes more frames look late. Taking each frame's own time off by the
    /// rate would do exactly that whenever the link is faster than the rate
    /// says. Zero before the first acknowledgement.
    fn base(&self, now: Instant) -> Duration {
        self.delays
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.at) <= DELAY_WINDOW)
            .map(|sample| sample.delay)
            .min()
            // An old figure says more than none.
            .or(self.delays.back().map(|sample| sample.delay))
            .unwrap_or_default()
    }

    /// How long the link takes for `bytes`, at most [`MAX_BUSY`].
    fn time_for(&self, bytes: usize) -> Duration {
        self.rate.map_or(Duration::ZERO, |rate| {
            Duration::try_from_secs_f64(bytes as f64 / rate)
                .map_or(MAX_BUSY, |time| time.min(MAX_BUSY))
        })
    }

    /// Bytes a second the link carries, once frames have queued on it.
    pub fn rate(&self) -> Option<f64> {
        self.rate
    }

    /// The same, while the link was behind or held a frame back within
    /// [`RECENT`]: what the encoder should fit its frames to.
    pub fn rate_to_fit(&self, now: Instant) -> Option<f64> {
        self.behind_at
            .filter(|at| now.saturating_duration_since(*at) <= RECENT)
            .and(self.rate)
    }

    /// `bytes` were written at `now`.
    pub fn written(&mut self, now: Instant, bytes: usize) {
        if self.rate.is_some() {
            let free = self.busy_until.map_or(now, |until| until.max(now));
            self.busy_until = Some((free + self.time_for(bytes)).min(now + MAX_BUSY));
        }
    }

    /// When a frame written would queue for no longer than [`QUEUE`]. None
    /// while the link has kept up.
    pub fn clear_at(&self) -> Option<Instant> {
        self.rate?;
        self.busy_until?.checked_sub(QUEUE)
    }

    /// How long a frame written with `flight` bytes in flight, itself included,
    /// may stay unacknowledged before the link counts as behind. None while the
    /// link has kept up: a late frame then is a browser that stalled, or a
    /// connection that starts slowly after a pause, and the frames behind it
    /// are not what holds it up.
    pub fn overdue_after(&self, now: Instant, flight: usize) -> Option<Duration> {
        self.rate?;
        Some(self.base(now) + self.time_for(flight) + self.held + LATE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link of `rate` bytes a second that takes `base` to carry a frame of no
    /// size there and its acknowledgement back. Frames of `size(n)` are offered
    /// every `period`; one is sent when `admit` allows it and dropped otherwise.
    /// Returns (sent at, acknowledged at, bytes) per frame sent and the link.
    fn carry(
        rate: impl Fn(Duration) -> f64,
        base: Duration,
        period: Duration,
        frames: u32,
        size: impl Fn(u32) -> usize,
        stall: impl Fn(u32) -> Duration,
    ) -> (Vec<(Duration, Duration, usize)>, Link) {
        let origin = Instant::now();
        let mut link = Link::default();
        // (written, acknowledged, bytes, bytes in flight when written)
        let mut flight: VecDeque<(Instant, Instant, usize, usize)> = VecDeque::new();
        let mut free = origin;
        let mut sent = Vec::new();
        let mut limited = false;
        for n in 0..frames {
            let now = origin + period * n;
            while flight.front().is_some_and(|(_, acked, _, _)| *acked <= now) {
                let (written, acked, bytes, _) = flight.pop_front().unwrap();
                let out = flight.iter().map(|(_, _, bytes, _)| bytes).sum();
                link.acknowledged(acked, written, bytes, out, limited, Duration::ZERO);
                limited = false;
            }
            // The scheduler's own window: eight frames.
            let open = flight.len() < 8 && link.clear_at().is_none_or(|at| now >= at);
            let on_time = flight.front().is_none_or(|(written, _, _, out)| {
                link.overdue_after(now, *out)
                    .is_none_or(|after| now.saturating_duration_since(*written) <= after)
            });
            if !(open && on_time) {
                limited |= flight.len() < 8;
                continue;
            }
            let bytes = size(n);
            link.written(now, bytes);
            let start = free.max(now);
            free = start + Duration::from_secs_f64(bytes as f64 / rate(start - origin));
            let acked = free + base + stall(n);
            let acked = flight
                .back()
                .map_or(acked, |(_, last, _, _)| acked.max(*last));
            let out = bytes + flight.iter().map(|(_, _, bytes, _)| bytes).sum::<usize>();
            flight.push_back((now, acked, bytes, out));
            sent.push((now - origin, acked - origin, bytes));
        }
        (sent, link)
    }

    const PERIOD: Duration = Duration::from_millis(33);
    const WAN: Duration = Duration::from_millis(130);
    const NO_STALL: fn(u32) -> Duration = |_| Duration::ZERO;

    fn worst_delay(sent: &[(Duration, Duration, usize)], from: usize) -> Duration {
        sent[from..]
            .iter()
            .map(|(written, acked, _)| *acked - *written)
            .max()
            .unwrap()
    }

    #[test]
    fn a_link_that_keeps_up_is_never_limited() {
        // 10 Mbit/s against 6 kB frames at 30 fps, with a 60 kB frame now and then.
        let size = |n: u32| if n % 40 == 5 { 60_000 } else { 6_000 };
        let (sent, link) = carry(|_| 1_250_000.0, WAN, PERIOD, 600, size, NO_STALL);
        assert_eq!(sent.len(), 600);
        assert!(link.rate().is_none());
    }

    #[test]
    fn one_stalled_browser_does_not_slow_the_stream() {
        let stall = |n: u32| Duration::from_millis(if n == 100 { 300 } else { 0 });
        let (sent, link) = carry(|_| 1_250_000.0, WAN, PERIOD, 400, |_| 6_000, stall);
        // 6 kB frames at 30 fps are 180 kB a second: a rate below that would hold them back.
        assert!(
            link.rate().is_none_or(|rate| rate >= 180_000.0),
            "{:?}",
            link.rate()
        );
        // Frames stop while the stalled one is overdue, and at no other time.
        assert!(sent.len() >= 400 - 12, "{} frames", sent.len());
    }

    #[test]
    fn frames_too_large_for_the_link_wait_instead_of_queueing() {
        // 1 Mbit/s against 7 kB frames at 30 fps: 56 ms each, 17.9 a second at best.
        // One frame in forty is small, as when a cursor moves.
        let size = |n: u32| if n % 40 == 3 { 500 } else { 7_000 };
        let (sent, link) = carry(|_| 125_000.0, WAN, PERIOD, 900, size, NO_STALL);
        let rate = link.rate().expect("a rate");
        assert!((100_000.0..160_000.0).contains(&rate), "{rate}");
        // Unlimited, eight frames would queue: 130 + 8 x 56 ms.
        let worst = worst_delay(&sent, sent.len() / 3);
        assert!(worst <= WAN + Duration::from_millis(250), "{worst:?}");
        let seconds = 900.0 * PERIOD.as_secs_f64();
        let carried = sent.iter().map(|(_, _, bytes)| bytes).sum::<usize>() as f64 / seconds;
        assert!(carried >= 0.85 * 125_000.0, "{carried} bytes a second");
    }

    #[test]
    fn large_frames_keep_the_link_busy() {
        // 16 kB frames take 128 ms each: waiting for each acknowledgement before
        // the next send would leave the link idle half of the time.
        let size = |n: u32| if n % 40 == 3 { 500 } else { 16_000 };
        let (sent, _) = carry(|_| 125_000.0, WAN, PERIOD, 900, size, NO_STALL);
        let seconds = 900.0 * PERIOD.as_secs_f64();
        let carried = sent.iter().map(|(_, _, bytes)| bytes).sum::<usize>() as f64 / seconds;
        assert!(carried >= 0.85 * 125_000.0, "{carried} bytes a second");
        // Unlimited, eight frames would queue: 130 + 8 x 128 ms. The base is
        // too long by such a frame's time on the link, and about that much more
        // queues than intended.
        let worst = worst_delay(&sent, sent.len() / 3);
        assert!(worst <= WAN + Duration::from_millis(128 * 4), "{worst:?}");
    }

    #[test]
    fn a_link_that_alternates_is_used_at_both_rates() {
        // 4 Mbit/s and 0.8 Mbit/s in turn, three seconds each, against 6 kB
        // frames at 30 fps: all of them fit the fast link and 16.7 a second the
        // slow one. The rate has to come back up within each fast phase.
        let fast = |at: Duration| (at.as_secs() / 3).is_multiple_of(2);
        let rate = |at: Duration| {
            if fast(at) { 500_000.0 } else { 100_000.0 }
        };
        let (sent, _) = carry(rate, WAN, PERIOD, 1800, |_| 6_000, NO_STALL);
        let on_fast = sent.iter().filter(|(at, _, _)| fast(*at)).count();
        let on_slow = sent.len() - on_fast;
        // 900 frames are offered in each kind of phase.
        assert!(on_fast >= 700, "{on_fast} frames on the fast link");
        assert!(on_slow >= 400, "{on_slow} frames on the slow link");
        let worst = worst_delay(&sent, sent.len() / 3);
        assert!(worst <= WAN + Duration::from_millis(400), "{worst:?}");
    }

    #[test]
    fn a_link_that_takes_most_of_a_second_for_a_frame_yields_a_rate() {
        // 0.5 Mbit/s against 40 kB frames: 640 ms each, so the measurements
        // that have to agree are that far apart.
        let (sent, link) = carry(|_| 62_500.0, WAN, PERIOD, 900, |_| 40_000, NO_STALL);
        let rate = link.rate().expect("a rate");
        assert!((50_000.0..80_000.0).contains(&rate), "{rate}");
        // Unlimited, eight frames would queue: 130 + 8 x 640 ms. With so few
        // acknowledgements the rate is slow to correct, and up to four do.
        let worst = worst_delay(&sent, sent.len() / 3);
        assert!(
            worst <= WAN + Duration::from_millis(640 * 4 + 30),
            "{worst:?}"
        );
    }

    #[test]
    fn the_encoder_follows_the_rate_only_while_the_link_is_behind() {
        let (sent, mut link) = carry(|_| 125_000.0, WAN, PERIOD, 300, |_| 7_000, NO_STALL);
        let end = Instant::now() + sent.last().unwrap().1;
        assert!(link.rate_to_fit(end).is_some());
        // Then one small frame every three seconds, each on time and none held
        // back: nothing queues, so nothing could show a link that became faster.
        for n in 1..=5u32 {
            let written = end + Duration::from_secs(3) * n;
            link.acknowledged(written + WAN, written, 1_000, 0, false, Duration::ZERO);
        }
        let now = end + Duration::from_secs(16);
        assert!(link.rate().is_some());
        assert!(link.rate_to_fit(now).is_none());
    }

    #[test]
    fn a_rate_that_is_far_too_low_does_not_stop_the_stream() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut link = Link::default();
        // Acknowledgements 300 ms apart for 100 bytes each, all written long
        // before: whatever delayed them, they read as 333 bytes a second.
        link.acknowledged(at(130), at(0), 100, 400, false, Duration::ZERO);
        for (written, acknowledged) in [(50, 600), (100, 1000), (200, 1300), (300, 1600)] {
            link.acknowledged(at(acknowledged), at(written), 100, 0, false, Duration::ZERO);
        }
        let rate = link.rate().expect("a rate");
        assert!(rate < 1_000.0, "{rate}");
        // A megabyte would take most of an hour at that rate.
        let now = at(1600);
        link.written(now, 1_000_000);
        assert!(link.clear_at().unwrap() <= now + MAX_BUSY);
        assert!(link.overdue_after(now, 1_000_000).unwrap() <= Duration::from_secs(3));
    }

    #[test]
    fn the_rate_follows_a_link_that_becomes_faster() {
        let origin_rate = 125_000.0;
        let (_, mut link) = carry(|_| origin_rate, WAN, PERIOD, 300, |_| 7_000, NO_STALL);
        assert!(link.rate().is_some());
        // The same stream on a link ten times as fast: every acknowledgement is on time.
        let start = Instant::now() + Duration::from_secs(60);
        let on_time = |link: &mut Link, n: u32| {
            let written = start + PERIOD * n;
            let arrived = written + WAN + Duration::from_millis(6);
            link.acknowledged(arrived, written, 7_000, 0, true, Duration::ZERO);
        };
        // Within a second the rate is past the link's.
        (0..30).for_each(|n| on_time(&mut link, n));
        let rate = link.rate().expect("still a rate");
        assert!(rate > 8.0 * origin_rate, "{rate}");
        // And it is forgotten once it limits nothing, as before any frame queued.
        (30..300).for_each(|n| on_time(&mut link, n));
        assert!(link.rate().is_none(), "{:?}", link.rate());
        assert!(link.clear_at().is_none());
    }
}
