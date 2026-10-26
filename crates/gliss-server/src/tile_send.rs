//! Trusted GLT2 scheduling choice. No codec, trace, capture or encoder work.
use anyhow::{Result, ensure};
use tokio::time::Instant;

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Policy {
    #[default]
    Tick,
    Ready,
}
impl Policy {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Tick => "tick",
            Self::Ready => "ready",
        }
    }
}
pub(crate) fn validate(args: &super::Args) -> Result<()> {
    if args.tile_send_policy.is_none() && args.tile_send_quantum.is_none() {
        return Ok(());
    }
    ensure!(
        args.datagram_bind
            .is_some_and(|address| address.ip().is_loopback())
            && args
                .datagram_codec
                .is_some_and(|codec| codec.policy().is_some()),
        "explicit tile-send-policy or tile-send-quantum requires a loopback GLT2 datagram endpoint"
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum Wake {
    Tick,
    ReadyTimer,
}
pub(crate) struct WakeResult {
    pub(crate) bytes: u64,
    pub(crate) encode_slot: bool,
}
#[derive(Default)]
pub(crate) struct ReadyGate {
    deadline: Option<Instant>,
}
impl ReadyGate {
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub(crate) fn arm(&mut self, observed: Instant) {
        if self.deadline.is_none() {
            self.deadline = Some(observed + std::time::Duration::from_millis(1));
        }
    }
    pub(crate) fn on_wake<F>(&mut self, wake: Wake, now: Instant, flush: F) -> Result<WakeResult>
    where
        F: FnOnce() -> Result<u64>,
    {
        match wake {
            Wake::Tick => Ok(WakeResult {
                bytes: flush()?,
                encode_slot: true,
            }),
            Wake::ReadyTimer => {
                if self.deadline.is_none_or(|deadline| now < deadline) {
                    return Ok(WakeResult {
                        bytes: 0,
                        encode_slot: false,
                    });
                }
                // Consume before flushing. A blocked flush does not rearm a
                // timer or spin; existing ticks or new inserts provide retries.
                self.deadline = None;
                Ok(WakeResult {
                    bytes: flush()?,
                    encode_slot: false,
                })
            }
        }
    }
}
pub(crate) async fn wait_ready(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
        time::Duration,
    };
    use tokio::sync::Notify;
    #[test]
    fn deadline_is_fixed_and_a_ready_wake_never_creates_an_encode_slot() {
        let base = Instant::now();
        let mut gate = ReadyGate::default();
        let mut flushes = 0;
        gate.arm(base);
        gate.arm(base + Duration::from_micros(900));
        assert_eq!(gate.deadline(), Some(base + Duration::from_millis(1)));
        let early = gate
            .on_wake(Wake::ReadyTimer, base + Duration::from_micros(999), || {
                flushes += 1;
                Ok(0)
            })
            .unwrap();
        assert!(!early.encode_slot);
        assert_eq!(flushes, 0);
        let due = gate
            .on_wake(Wake::ReadyTimer, base + Duration::from_millis(1), || {
                flushes += 1;
                Ok(7)
            })
            .unwrap();
        assert_eq!(due.bytes, 7);
        assert!(!due.encode_slot);
        assert_eq!(flushes, 1);
        assert!(gate.deadline().is_none());
        for _ in 0..10 {
            assert!(
                !gate
                    .on_wake(Wake::ReadyTimer, base + Duration::from_secs(1), || panic!(
                        "no timer retry without rearm"
                    ))
                    .unwrap()
                    .encode_slot
            );
        }
    }
    #[test]
    fn a_notification_before_wait_persists_but_many_notifications_are_one_permit() {
        let notify = Notify::new();
        notify.notify_one();
        notify.notify_one();
        notify.notify_one();
        let mut context = Context::from_waker(Waker::noop());
        let first = notify.notified();
        let mut first = pin!(first);
        assert!(matches!(first.as_mut().poll(&mut context), Poll::Ready(())));
        let second = notify.notified();
        let mut second = pin!(second);
        assert!(second.as_mut().poll(&mut context).is_pending());
    }
    #[test]
    fn a_permit_posted_while_armed_survives_for_exactly_one_following_interval() {
        let notify = Notify::new();
        let mut gate = ReadyGate::default();
        let base = Instant::now();
        let mut flushes = 0;
        let take_permit = || {
            let waiter = notify.notified();
            let mut waiter = pin!(waiter);
            let mut context = Context::from_waker(Waker::noop());
            matches!(waiter.as_mut().poll(&mut context), Poll::Ready(()))
        };
        notify.notify_one();
        assert!(take_permit());
        gate.arm(base);
        // The actor does not poll Notify while armed. These permits coalesce
        // and must not change the first deadline or disappear during its flush.
        notify.notify_one();
        notify.notify_one();
        assert_eq!(gate.deadline(), Some(base + Duration::from_millis(1)));
        let first = gate
            .on_wake(Wake::ReadyTimer, base + Duration::from_millis(1), || {
                flushes += 1;
                Ok(0)
            })
            .unwrap();
        assert!(!first.encode_slot);
        assert_eq!(flushes, 1);
        assert!(gate.deadline().is_none());
        assert!(take_permit());
        assert!(!take_permit());
        let observed_again = base + Duration::from_millis(4);
        gate.arm(observed_again);
        assert_eq!(
            gate.deadline(),
            Some(observed_again + Duration::from_millis(1))
        );
        gate.on_wake(
            Wake::ReadyTimer,
            observed_again + Duration::from_micros(999),
            || panic!("second interval not yet due"),
        )
        .unwrap();
        let second = gate
            .on_wake(
                Wake::ReadyTimer,
                observed_again + Duration::from_millis(1),
                || {
                    flushes += 1;
                    Ok(0)
                },
            )
            .unwrap();
        assert!(!second.encode_slot);
        assert_eq!(flushes, 2);
        assert!(gate.deadline().is_none());
        assert!(!take_permit());
        gate.on_wake(
            Wake::ReadyTimer,
            observed_again + Duration::from_millis(2),
            || panic!("no third interval without a permit"),
        )
        .unwrap();
    }
    fn args(extra: &[&str]) -> Result<super::super::Args, clap::Error> {
        let mut values = vec![
            "gliss-server",
            "--demo",
            "--token",
            "012345678901234567890123",
            "--codec",
            "h264",
            "--video-intra-period",
            "1",
        ];
        values.extend_from_slice(extra);
        super::super::Args::try_parse_from(values)
    }
    #[test]
    fn selection_is_explicit_trusted_and_rejects_wrong_transport_codec_or_spelling() {
        let legacy = args(&[]).unwrap();
        assert_eq!(legacy.tile_send_policy, None);
        assert_eq!(legacy.tile_send_quantum, None);
        assert_eq!(
            crate::tile_quantum::selected_name(legacy.tile_send_quantum),
            "unlimited"
        );
        validate(&legacy).unwrap();
        for name in ["tick", "ready"] {
            assert!(args(&["--tile-send-policy", name]).is_err());
            let wrong_codec = args(&[
                "--datagram-bind",
                "127.0.0.1:9999",
                "--datagram-codec",
                "h264",
                "--tile-send-policy",
                name,
            ])
            .unwrap();
            assert!(validate(&wrong_codec).is_err());
            let valid = args(&[
                "--datagram-bind",
                "127.0.0.1:9999",
                "--datagram-codec",
                "glt2-lossless",
                "--tile-send-policy",
                name,
            ])
            .unwrap();
            validate(&valid).unwrap();
            assert_eq!(valid.tile_send_policy.unwrap().name(), name);
            let public = args(&[
                "--datagram-bind",
                "0.0.0.0:9999",
                "--datagram-codec",
                "glt2-lossless",
                "--tile-send-policy",
                name,
            ])
            .unwrap();
            assert!(validate(&public).is_err());
        }
        assert!(
            args(&[
                "--datagram-bind",
                "127.0.0.1:9999",
                "--tile-send-policy",
                "fast"
            ])
            .is_err()
        );
    }

    #[test]
    fn quantum_is_an_explicit_loopback_glt2_option_in_both_sending_policies() {
        assert!(args(&["--tile-send-quantum", "16k-16d-1ms"]).is_err());
        for codec in ["glt2-lossless", "glt2-gentle", "glt2-strong"] {
            for policy in ["tick", "ready"] {
                let value = args(&[
                    "--datagram-bind",
                    "127.0.0.1:9999",
                    "--datagram-codec",
                    codec,
                    "--tile-send-policy",
                    policy,
                    "--tile-send-quantum",
                    "16k-16d-1ms",
                ])
                .unwrap();
                validate(&value).unwrap();
                assert_eq!(
                    crate::tile_quantum::selected_name(value.tile_send_quantum),
                    "16k-16d-1ms"
                );
            }
        }
        // The quantum alone must validate too; it must not depend on also
        // providing an explicit policy to trigger the endpoint check.
        for (bind, codec) in [
            ("0.0.0.0:9999", "glt2-lossless"),
            ("127.0.0.1:9999", "h264"),
        ] {
            let value = args(&[
                "--datagram-bind",
                bind,
                "--datagram-codec",
                codec,
                "--tile-send-quantum",
                "16k-16d-1ms",
            ])
            .unwrap();
            assert!(validate(&value).is_err());
        }
        assert!(
            args(&[
                "--datagram-bind",
                "127.0.0.1:9999",
                "--datagram-codec",
                "glt2-lossless",
                "--tile-send-quantum",
                "unlimited"
            ])
            .is_err()
        );
        let omitted = args(&[
            "--datagram-bind",
            "127.0.0.1:9999",
            "--datagram-codec",
            "glt2-lossless",
        ])
        .unwrap();
        validate(&omitted).unwrap();
        assert_eq!(omitted.tile_send_quantum, None);
    }
}
