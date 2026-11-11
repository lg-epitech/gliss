//! Scheduling of RFB update requests and of QEMU's display refresh.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

/// QEMU's VNC server looks for changes on a timer that runs every 30 ms at
/// best. A tick that finds changed pixels but no pending update request sends
/// nothing and adds 50 ms to the timer, so one late request costs a 110 ms gap
/// instead of 30 ms. A request sent within 25 ms of the previous one is pending
/// before the next tick, unless the update itself took longer than that to
/// arrive.
const EAGER_SPACING: Duration = Duration::from_millis(25);

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// Request on a fixed clock at the FPS cap.
    Tick,
    /// Request when the previous update has arrived, at most once per FPS period.
    Ready,
    /// Request when the previous update has arrived, at most once per 25 ms.
    /// A higher FPS cap shortens that to its period; a cap below 20 lengthens
    /// it to half its period, so capture stays within twice the cap. The FPS
    /// cap still limits encoding.
    #[default]
    Eager,
}

impl Policy {
    /// Minimum time between two requests, or None for the fixed clock.
    pub fn spacing(self, period: Duration) -> Option<Duration> {
        match self {
            Self::Tick => None,
            Self::Ready => Some(period),
            Self::Eager => Some(period.min(EAGER_SPACING.max(period / 2))),
        }
    }
}

/// How long after the last input or display update the refresh timer is held.
const HOLD: Duration = Duration::from_millis(1000);
/// Below QEMU's 30 ms base period, so every tick is followed by a hint.
const HINT_PERIOD: Duration = Duration::from_millis(25);

/// Keeps QEMU's VNC refresh timer at its base rate while the desktop is in use.
///
/// QEMU slows that timer by 50 ms on every tick without a sent update, up to
/// three seconds, and only client messages other than update requests reset
/// it. A key press resets it, but the tick it triggers runs before the guest
/// has drawn anything, so the echo waits for the next tick 80 ms later. A hint
/// every 25 ms keeps the next tick 30 ms away instead. The hint is QEMU's
/// audio format message, so it is only sent to a server that has acknowledged
/// the audio extension; any other server keeps its own timing.
///
/// The hold ends one second after the last input or display update, so a
/// screen that changes at least once a second (a blinking caret, a clock with
/// seconds) is held for as long as it is viewed. That costs QEMU about 33
/// timer ticks a second on an otherwise idle display. Owned only by the
/// input/request actor.
pub struct RefreshHold {
    until: Instant,
    next: Instant,
}

impl RefreshHold {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            until: now,
            next: now,
        }
    }

    /// An input message was written; it reset the timer itself.
    pub fn input_sent(&mut self) {
        let now = Instant::now();
        self.until = now + HOLD;
        self.next = now + HINT_PERIOD;
    }

    /// A display update arrived.
    pub fn display_changed(&mut self) {
        self.until = Instant::now() + HOLD;
    }

    pub fn active(&self) -> bool {
        Instant::now() < self.until
    }

    /// When the next hint is due while the hold is active.
    pub fn deadline(&self) -> Instant {
        self.next
    }

    pub fn hint_sent(&mut self) {
        self.next = Instant::now() + HINT_PERIOD;
    }
}

/// Owned only by the input/request actor. A canceled wait changes no deadline.
pub struct Ready {
    period: Duration,
    last_invoked: Option<Instant>,
}

impl Ready {
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            last_invoked: None,
        }
    }

    pub async fn wait(&self, pending: &AtomicBool, completion: &Notify) {
        loop {
            // Exactly one waiter uses notify_one, which retains a permit even
            // if completion races the pending check or first future poll.
            let completed = completion.notified();
            if pending.load(Ordering::Acquire) {
                completed.await;
                continue;
            }
            if let Some(start) = self.last_invoked {
                tokio::time::sleep_until(start + self.period).await;
            }
            // A wake is only a hint. The caller also claims pending with the
            // original atomic swap immediately before issuing the request.
            if !pending.load(Ordering::Acquire) {
                return;
            }
        }
    }

    /// Call immediately before request_update, never at its planned deadline.
    /// This is local invocation/queue admission, not the socket's WriteStarted
    /// timestamp, peer receipt, or guest capture time.
    pub fn invoked(&mut self) {
        self.last_invoked = Some(Instant::now());
    }
}

#[cfg(test)]
#[path = "rfb_request_tests.rs"]
mod tests;
