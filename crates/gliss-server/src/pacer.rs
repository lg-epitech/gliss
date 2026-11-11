//! Frame-rate cap for encode admission.

use std::time::Duration;
use tokio::time::Instant;

/// Caps encode starts at the configured rate by dropping frames from a faster
/// source rather than delaying them.
///
/// Starts follow a virtual schedule of one per period, with one period of
/// slack. A frame that arrives within the slack starts at once. A frame that
/// arrives sooner is held for one period, long enough for a faster source to
/// replace it with a newer frame, which is judged the same way on arrival. So
/// a source slightly faster than the cap loses a frame now and then instead
/// of having every frame wait: QEMU sends an update every 30 ms at best,
/// against a 33.3 ms period at 30 fps, and one frame in ten is dropped. A held
/// frame that nothing replaces starts when its hold ends, so the last frame of
/// a burst is shown, one period late at most.
///
/// The slack allows one start more than the cap in any span of time: two
/// starts can be closer than one period, and a second can hold eleven starts
/// at 10 fps.
pub struct Pacer {
    period: Duration,
    /// When the next start is due on the virtual schedule.
    due: Instant,
}

impl Pacer {
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            due: Instant::now(),
        }
    }

    /// When a frame captured now may start encoding.
    pub fn eligible(&self) -> Instant {
        let now = Instant::now();
        if now + self.period >= self.due {
            now
        } else {
            now + self.period
        }
    }

    /// Record an encode that starts now.
    pub fn started(&mut self) {
        self.due = self.due.max(Instant::now()) + self.period;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::advance;

    const PERIOD: Duration = Duration::from_millis(100);

    /// Feed captures at a fixed spacing; return each start as (capture, start) in ms.
    async fn run(spacing: Duration, captures: u32) -> Vec<(u128, u128)> {
        let origin = Instant::now();
        let mut pacer = Pacer::new(PERIOD);
        let mut held: Option<(Instant, Instant)> = None;
        let mut starts = Vec::new();
        for _ in 0..captures {
            let captured = Instant::now();
            held = Some((captured, pacer.eligible()));
            let next_capture = captured + spacing;
            if let Some((captured, eligible)) = held
                && eligible < next_capture
            {
                tokio::time::sleep_until(eligible).await;
                pacer.started();
                starts.push((
                    (captured - origin).as_millis(),
                    (Instant::now() - origin).as_millis(),
                ));
                held = None;
            }
            tokio::time::sleep_until(next_capture).await;
        }
        if let Some((captured, eligible)) = held {
            tokio::time::sleep_until(eligible).await;
            starts.push((
                (captured - origin).as_millis(),
                (Instant::now() - origin).as_millis(),
            ));
        }
        starts
    }

    #[tokio::test(start_paused = true)]
    async fn sparse_frames_start_at_once() {
        let mut pacer = Pacer::new(PERIOD);
        for _ in 0..5 {
            assert_eq!(pacer.eligible(), Instant::now());
            pacer.started();
            advance(PERIOD * 3 + Duration::from_millis(7)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_at_the_cap_is_never_delayed_or_dropped() {
        let starts = run(PERIOD, 200).await;
        assert_eq!(starts.len(), 200);
        assert!(starts.iter().all(|(captured, started)| captured == started));
    }

    #[tokio::test(start_paused = true)]
    async fn a_faster_source_loses_frames_instead_of_delaying_all_of_them() {
        // 90 ms captures against a 100 ms period: QEMU's 30 ms against 33.3 ms.
        let spacing = Duration::from_millis(90);
        let starts = run(spacing, 1000).await;
        let delayed = starts.iter().filter(|(c, s)| c != s).count();
        assert!(delayed <= 1, "{delayed} frames waited");
        // 1000 captures span 90 s: at most 900 starts plus the slack frame.
        assert!(starts.len() <= 901, "{} starts", starts.len());
        assert!(starts.len() >= 895, "{} starts", starts.len());
        let longest_gap = starts
            .windows(2)
            .map(|pair| pair[1].1 - pair[0].1)
            .max()
            .unwrap();
        assert_eq!(longest_gap, 180, "a dropped frame costs one capture");
    }

    #[tokio::test(start_paused = true)]
    async fn any_window_holds_at_most_the_cap_plus_one_frame() {
        for spacing in [1, 7, 30, 45, 90, 99] {
            let starts = run(Duration::from_millis(spacing), 3000 / spacing as u32).await;
            for (index, (_, first)) in starts.iter().enumerate() {
                let within = starts[index..]
                    .iter()
                    .take_while(|(_, started)| started - first < 1000)
                    .count();
                assert!(within <= 11, "{within} starts in 1 s at {spacing} ms");
            }
            assert!(
                starts.len() >= 3000 / 100 - 1,
                "cap is reached at {spacing} ms"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_frame_of_a_burst_is_held_one_period_then_shown() {
        let mut pacer = Pacer::new(PERIOD);
        pacer.started();
        pacer.started();
        // Two starts used the slack; a third capture right away is too soon.
        let captured = Instant::now();
        assert_eq!(pacer.eligible(), captured + PERIOD);
        tokio::time::sleep_until(pacer.eligible()).await;
        // Nothing replaced it, so it is due now.
        assert_eq!(pacer.eligible(), Instant::now());
    }
}
