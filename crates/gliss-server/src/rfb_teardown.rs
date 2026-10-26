//! Operator-only receiver-lifetime experiment for a single private TCP source.

use anyhow::{Context, Result, ensure};
use gliss_rfb::{DrainReport, DrainTermination, ReaderDrainControl, RfbWriter};
use serde_json::json;
use tokio::{sync::oneshot, time::Instant};

static MONOTONIC_ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClosePolicy {
    #[default]
    Immediate,
    Drain,
}

impl ClosePolicy {
    pub fn name(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Drain => "drain",
        }
    }
}

pub fn validate(args: &super::Args) -> Result<()> {
    if args.rfb_close_policy != Some(ClosePolicy::Drain) {
        return Ok(());
    }
    ensure!(
        !args.demo
            && !args.shared_dbus
            && args.dbus_address.is_none()
            && args.vm_uuid.is_none()
            && args.dbus_console == 0
            && args.rfb_fd.is_none()
            && args.rfb_deadline_unix_ms.is_none()
            && args.max_clients == 1,
        "rfb-close-policy drain requires a private TCP VNC source and max-clients1; other source modes are excluded"
    );
    let address: std::net::SocketAddr = args
        .vnc
        .as_deref()
        .context("rfb-close-policy drain requires --vnc")?
        .parse()
        .context("rfb-close-policy drain requires a numeric loopback TCP address")?;
    ensure!(
        address.ip().is_loopback() && address.port() != 0,
        "rfb-close-policy drain requires a nonzero loopback TCP VNC port"
    );
    Ok(())
}

pub fn validate_source(app: &super::App) -> Result<()> {
    validate(&app.args)?;
    if app.args.rfb_close_policy == Some(ClosePolicy::Drain) {
        ensure!(
            app.shared_capture.is_none() && app.source_deadline.is_none(),
            "rfb-close-policy drain cannot own a shared display or graphics lease"
        );
        #[cfg(unix)]
        ensure!(
            app.rfb_fd.is_none(),
            "rfb-close-policy drain excludes inherited graphics sources"
        );
    }
    Ok(())
}

#[derive(Default)]
pub struct Releases {
    pub key_attempts: usize,
    pub key_successes: usize,
    pub pointer_attempted: bool,
    pub pointer_succeeded: bool,
}

/// Called only after the original held-input writes have completed. The
/// immediate arm retains notice-before-writer-drop, without an added await.
pub async fn finish(
    policy: ClosePolicy,
    writer: RfbWriter,
    control: ReaderDrainControl,
    shutdown: oneshot::Sender<()>,
    releases: Releases,
    release_start: Instant,
) {
    let origin = *MONOTONIC_ORIGIN.get_or_init(Instant::now);
    let start = Instant::now();
    let mut requested = false;
    let mut report: Option<DrainReport> = None;
    let mut unavailable = false;
    let notice;
    if policy == ClosePolicy::Immediate {
        notice = shutdown.send(()).is_ok();
        drop(writer);
    } else {
        // Includes actor scheduling/transition. Never restart this deadline
        // after write-half drop, queue admission, or receipt of a report.
        let deadline = start + gliss_rfb::MAX_DRAIN_DURATION;
        let pending = control.request(deadline);
        requested = pending.is_ok();
        drop(writer);
        match pending {
            Ok(mut pending) => {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => {},
                    result = &mut pending => match result {
                        Ok(value) => report = Some(value),
                        Err(_) => unavailable = true,
                    },
                }
                // The actor drops the read half before reporting; prove its
                // actual task completion separately within the same cutoff.
                while report.is_some() && !control.is_finished() && Instant::now() < deadline {
                    tokio::task::yield_now().await;
                }
            }
            Err(_) => unavailable = true,
        }
        notice = shutdown.send(()).is_ok();
    }
    let termination = report.as_ref().map(|value| match value.termination {
        DrainTermination::PeerEof => "peer_eof",
        DrainTermination::Deadline => "deadline",
        DrainTermination::ByteBudget => "byte_budget",
        DrainTermination::ReadError => "read_error",
    });
    let trace = json!({
        "schema":"gliss-rfb-teardown-event-v1",
        "selected_policy":policy.name(),
        "release_key_attempts":releases.key_attempts,
        "release_key_successes":releases.key_successes,
        "release_key_failures":releases.key_attempts-releases.key_successes,
        "release_pointer_attempted":releases.pointer_attempted,
        "release_pointer_succeeded":releases.pointer_succeeded,
        "release_duration_ms":start.duration_since(release_start).as_secs_f64()*1000.,
        "terminal_drain_requested":requested,
        "terminal_drain_accepted":report.as_ref().map(|_|true),
        "writer_drop_requested":true,
        "drain_termination":termination,
        "discarded_bytes":report.as_ref().map(|value| value.discarded_bytes),
        "drain_report_unavailable":unavailable,
        "drain_wait_deadline_reached":policy==ClosePolicy::Drain && report.is_none() && !unavailable,
        "transition_after_start_ms":report.as_ref().map(|value| value.accepted_at.duration_since(start).as_secs_f64()*1000.),
        "drain_finished_after_start_ms":report.as_ref().map(|value| value.finished_at.duration_since(start).as_secs_f64()*1000.),
        "local_monotonic_start_ns":start.duration_since(origin).as_nanos().min(u128::from(u64::MAX)) as u64,
        "local_monotonic_end_ns":Instant::now().duration_since(origin).as_nanos().min(u128::from(u64::MAX)) as u64,
        "monotonic_origin":"First source teardown in this native process; not wall time or browser time",
        "read_task_finished":control.is_finished(),
        "source_shutdown_notice":notice,
        "input_task_scope_exiting":true,
        "task_scope_note":"Input task is at its final statement; read_task_finished is the actual Tokio task flag. Source JoinSet/browser/process cleanup is independently observed."
    });
    tracing::debug!(rfb_teardown = %trace, "RFB source teardown");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(extra: &[&str]) -> super::super::Args {
        let mut argv = vec![
            "gliss-server",
            "--vnc",
            "127.0.0.1:5901",
            "--token",
            "abcdefghijklmnopqrstuvwx",
            "--max-clients",
            "1",
        ];
        argv.extend_from_slice(extra);
        super::super::Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn explicit_drain_accepts_only_private_single_tcp_source() {
        assert!(validate(&args(&["--rfb-close-policy", "drain"])).is_ok());
        for extra in [
            vec!["--max-clients", "2"],
            vec!["--dbus-address", "unix:path=/private", "--vm-uuid", "test"],
            vec!["--dbus-console", "1"],
        ] {
            // Replace an existing value before parsing when necessary.
            let mut value = args(&["--rfb-close-policy", "drain"]);
            match extra[0] {
                "--max-clients" => value.max_clients = 2,
                "--dbus-console" => value.dbus_console = 1,
                _ => {
                    value.dbus_address = Some("unix:path=/private".into());
                    value.vm_uuid = Some("test".into());
                }
            }
            assert!(validate(&value).is_err());
        }
        for address in ["localhost:5901", "192.0.2.1:5901", "127.0.0.1:0"] {
            let mut value = args(&["--rfb-close-policy", "drain"]);
            value.vnc = Some(address.into());
            assert!(validate(&value).is_err());
        }
    }

    #[test]
    fn legacy_immediate_has_no_additional_source_restrictions() {
        for policy in [None, Some(ClosePolicy::Immediate)] {
            let mut value = args(&[]);
            value.rfb_close_policy = policy;
            value.demo = true;
            value.vnc = None;
            value.shared_dbus = true;
            value.max_clients = 8;
            value.rfb_fd = Some(7);
            value.rfb_deadline_unix_ms = Some(1);
            assert!(validate(&value).is_ok());
        }
    }

    #[test]
    fn excluded_sources_are_rejected_even_with_a_vnc_value() {
        for excluded in 0..6 {
            let mut value = args(&["--rfb-close-policy", "drain"]);
            match excluded {
                0 => value.demo = true,
                1 => value.shared_dbus = true,
                2 => value.rfb_fd = Some(7),
                3 => value.rfb_deadline_unix_ms = Some(1),
                4 => value.dbus_address = Some("private".into()),
                _ => value.vm_uuid = Some("uuid".into()),
            }
            assert!(validate(&value).is_err());
        }
    }
}
