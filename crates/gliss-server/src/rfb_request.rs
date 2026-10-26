//! Optional completion rearming for the existing single RFB request actor.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    #[default]
    Tick,
    Ready,
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
