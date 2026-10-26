//! Opt-in terminal receive draining of the already owned RFB stream.

use anyhow::{Result, bail};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::{mpsc, oneshot},
    task::AbortHandle,
    time::{Instant, sleep_until},
};

pub const MAX_DRAIN_DURATION: Duration = Duration::from_millis(250);
pub const MAX_DRAIN_BYTES: usize = 4 * 1024 * 1024;
pub const DRAIN_SCRATCH_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainTermination {
    PeerEof,
    Deadline,
    ByteBudget,
    ReadError,
}

#[derive(Clone, Debug)]
pub struct DrainReport {
    pub termination: DrainTermination,
    pub discarded_bytes: usize,
    pub accepted_at: Instant,
    pub finished_at: Instant,
}

pub(crate) struct DrainRequest {
    pub deadline: Instant,
    pub completed: oneshot::Sender<DrainReport>,
}

/// A terminal command handle; it cannot open or replace a display connection.
#[derive(Clone)]
pub struct ReaderDrainControl {
    commands: mpsc::Sender<DrainRequest>,
    requested: Arc<AtomicBool>,
    task: AbortHandle,
}

impl ReaderDrainControl {
    pub(crate) fn new(commands: mpsc::Sender<DrainRequest>, task: AbortHandle) -> Self {
        Self {
            commands,
            requested: Arc::new(AtomicBool::new(false)),
            task,
        }
    }

    /// One terminal transition, independent of the decoded-event sink. The
    /// absolute deadline is clamped before queueing and is never extended.
    pub fn request(&self, deadline: Instant) -> Result<oneshot::Receiver<DrainReport>> {
        if self.requested.swap(true, Ordering::AcqRel) {
            bail!("RFB terminal drain was already requested");
        }
        let deadline = deadline.min(Instant::now() + MAX_DRAIN_DURATION);
        let (completed, result) = oneshot::channel();
        self.commands
            .try_send(DrainRequest {
                deadline,
                completed,
            })
            .map_err(|_| anyhow::anyhow!("RFB terminal drain receiver is unavailable"))?;
        Ok(result)
    }

    /// Actual Tokio task completion, rather than a source-shutdown notice.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

pub(crate) async fn discard<R: AsyncRead + Unpin>(read: &mut R, deadline: Instant) -> DrainReport {
    let accepted_at = Instant::now();
    let mut discarded_bytes = 0;
    let mut scratch = [0u8; DRAIN_SCRATCH_BYTES];
    let termination = loop {
        if Instant::now() >= deadline {
            break DrainTermination::Deadline;
        }
        if discarded_bytes == MAX_DRAIN_BYTES {
            break DrainTermination::ByteBudget;
        }
        let remaining = (MAX_DRAIN_BYTES - discarded_bytes).min(scratch.len());
        let result = tokio::select! {
            biased;
            _ = sleep_until(deadline) => break DrainTermination::Deadline,
            result = read.read(&mut scratch[..remaining]) => result,
        };
        match result {
            Ok(bytes) => {
                discarded_bytes += bytes;
                if Instant::now() >= deadline {
                    break DrainTermination::Deadline;
                }
                if bytes == 0 {
                    break DrainTermination::PeerEof;
                }
            }
            Err(_) => break DrainTermination::ReadError,
        }
    };
    DrainReport {
        termination,
        discarded_bytes,
        accepted_at,
        finished_at: Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncWriteExt, ReadBuf};

    struct NeverRead;
    impl AsyncRead for NeverRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            panic!("expired drain must not poll the stream");
        }
    }

    #[tokio::test]
    async fn expired_deadline_never_touches_owned_stream() {
        let report = discard(&mut NeverRead, Instant::now() - Duration::from_millis(1)).await;
        assert_eq!(report.termination, DrainTermination::Deadline);
        assert_eq!(report.discarded_bytes, 0);
    }

    #[tokio::test]
    async fn byte_budget_is_exact_and_scratch_is_bounded() {
        struct CountRead {
            maximum: usize,
        }
        impl AsyncRead for CountRead {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                self.maximum = self.maximum.max(buffer.remaining());
                let count = buffer.remaining();
                buffer.put_slice(&vec![7; count]);
                Poll::Ready(Ok(()))
            }
        }
        let mut read = CountRead { maximum: 0 };
        let report = discard(&mut read, Instant::now() + MAX_DRAIN_DURATION).await;
        assert_eq!(report.termination, DrainTermination::ByteBudget);
        assert_eq!(report.discarded_bytes, MAX_DRAIN_BYTES);
        assert_eq!(read.maximum, DRAIN_SCRATCH_BYTES);
    }

    #[tokio::test]
    async fn silent_peer_hits_original_absolute_deadline() {
        let (mut read, _peer) = tokio::io::duplex(64);
        let deadline = Instant::now() + Duration::from_millis(15);
        let report = discard(&mut read, deadline).await;
        assert_eq!(report.termination, DrainTermination::Deadline);
        assert_eq!(report.discarded_bytes, 0);
        assert!(report.finished_at >= deadline);
    }

    #[tokio::test]
    async fn eof_and_read_error_are_distinct() {
        let (mut read, mut peer) = tokio::io::duplex(64);
        peer.write_all(b"remaining pixels").await.unwrap();
        peer.shutdown().await.unwrap();
        let report = discard(&mut read, Instant::now() + MAX_DRAIN_DURATION).await;
        assert_eq!(report.termination, DrainTermination::PeerEof);
        assert_eq!(report.discarded_bytes, 16);
        struct FailedRead;
        impl AsyncRead for FailedRead {
            fn poll_read(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Poll::Ready(Err(io::Error::other("test reset")))
            }
        }
        let report = discard(&mut FailedRead, Instant::now() + MAX_DRAIN_DURATION).await;
        assert_eq!(report.termination, DrainTermination::ReadError);
    }
}
