//! Reset debt for the explicit GOP4 experiment. No media is retried.
use std::time::Duration;
use tokio::time::Instant;
#[derive(Default)]
pub(super) struct Recovery {
    debt: bool,
    retire: bool,
    last_reset: Option<Instant>,
    requested: u64,
    coalesced: u64,
    resets: u64,
    retired: u64,
    discarded: u64,
}
impl Recovery {
    pub(super) fn request(&mut self, busy: bool) {
        self.requested = self.requested.saturating_add(1);
        self.coalesced = self.coalesced.saturating_add(u64::from(self.debt));
        self.debt = true;
        self.retire |= busy;
    }
    pub(super) fn ready(&self, now: Instant) -> bool {
        !self.debt
            || self
                .last_reset
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
    }
    pub(super) fn take_reset(&mut self, now: Instant) -> bool {
        assert!(self.ready(now));
        let reset = self.debt;
        if reset {
            self.debt = false;
            self.retire = false;
            self.last_reset = Some(now);
            self.resets = self.resets.saturating_add(1);
        }
        reset
    }
    pub(super) fn retire_completed(&mut self) -> bool {
        let retire = self.retire;
        self.retire = false;
        if retire {
            self.debt = true;
            self.retired = self.retired.saturating_add(1);
        }
        retire
    }
    pub(super) fn discarded(&mut self) {
        self.debt = true;
        self.discarded = self.discarded.saturating_add(1);
    }
    pub(super) fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({"reset_debt":self.debt,"inflight_retirement_pending":self.retire,
            "resync_commands_received":self.requested,"resync_commands_coalesced":self.coalesced,
            "scheduler_reset_calls":self.resets,"encoded_outputs_retired":self.retired,
            "encoded_outputs_budget_discarded":self.discarded})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inflight_and_discarded_outputs_keep_debt_until_a_new_rate_bounded_encode() {
        let now = Instant::now();
        let mut recovery = Recovery::default();
        assert!(!recovery.take_reset(now));
        recovery.request(true);
        recovery.request(true);
        assert!(recovery.retire_completed());
        assert!(!recovery.retire_completed());
        assert!(recovery.take_reset(now));
        recovery.discarded();
        assert!(!recovery.ready(now + Duration::from_millis(999)));
        assert!(recovery.take_reset(now + Duration::from_secs(1)));
        assert!(!recovery.take_reset(now + Duration::from_secs(1)));
        assert_eq!(recovery.snapshot()["encoded_outputs_retired"], 1);
        assert_eq!(recovery.snapshot()["encoded_outputs_budget_discarded"], 1);
        assert_eq!(recovery.snapshot()["scheduler_reset_calls"], 2);
    }
    #[test]
    fn retiring_an_actual_p_frame_requires_a_new_reset_idr_for_the_same_source() {
        use gliss_video::{VideoEncoder, VideoProfile, VideoTiming, parse_packet};
        let frame = gliss_core::Frame::new(32, 32).unwrap();
        let mut encoder = VideoEncoder::with_timing(
            32,
            32,
            3_000_000,
            VideoProfile::Screen,
            VideoTiming::new(60, 60, 4).unwrap(),
        )
        .unwrap();
        assert!(
            parse_packet(&encoder.encode(&frame, 1, 0).unwrap())
                .unwrap()
                .keyframe
        );
        assert!(
            !parse_packet(&encoder.encode(&frame, 2, 0).unwrap())
                .unwrap()
                .keyframe
        );
        let mut recovery = Recovery::default();
        let now = Instant::now();
        recovery.request(true);
        assert!(
            !parse_packet(&encoder.encode(&frame, 3, 0).unwrap())
                .unwrap()
                .keyframe
        );
        assert!(recovery.retire_completed());
        assert!(recovery.take_reset(now));
        encoder.reset();
        let recovered = encoder.encode(&frame, 4, 0).unwrap();
        let parsed = parse_packet(&recovered).unwrap();
        assert_eq!(parsed.sequence, 4);
        assert!(parsed.keyframe);
    }
}
