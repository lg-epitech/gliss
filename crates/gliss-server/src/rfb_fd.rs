//! Private, one-shot transport admission for a caller-owned Unix graphics FD.
//!
//! The Unix-only server entrypoint adopts the inherited FD before starting Tokio.
//! Direct libc/socket2 dependencies support admission. This does not authorize a VM,
//! authenticate a peer, adopt a libvirt lease or compare process generations.

use std::{
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream as StdUnixStream,
    },
    sync::Mutex,
};

use anyhow::{Context, Result, ensure};
use socket2::{Socket, Type};

/// Exactly one descriptor owner, with at most one successful stream claim.
pub struct RfbFd {
    stream: Mutex<Option<StdUnixStream>>,
}

impl RfbFd {
    /// Consume an exclusively owned descriptor. Rejection closes that owner.
    ///
    /// Domain/type/connectivity validation does not establish the VM identity.
    /// Call before publishing the worker's listening address or spawning other
    /// processes, so CLOEXEC is set before any unrelated exec can inherit it.
    pub fn from_owned(fd: OwnedFd) -> Result<Self> {
        ensure!(
            fd.as_raw_fd() > 2,
            "standard descriptors cannot be graphics FDs"
        );
        let socket = Socket::from(fd);
        // SO_TYPE rejects files/pipes before address queries. Neither query
        // consumes graphics bytes or changes the connection's peer.
        ensure!(
            socket.r#type().context("inspect graphics socket type")? == Type::STREAM,
            "graphics FD must be a stream socket"
        );
        ensure!(
            socket
                .local_addr()
                .context("inspect graphics socket domain")?
                .is_unix(),
            "graphics FD must be a Unix socket"
        );
        ensure!(
            socket
                .peer_addr()
                .context("graphics FD must be connected")?
                .is_unix(),
            "graphics FD peer must be Unix"
        );
        socket
            .set_cloexec(true)
            .context("set graphics FD CLOEXEC")?;
        socket
            .set_nonblocking(true)
            .context("set graphics FD nonblocking")?;
        let stream = StdUnixStream::from(OwnedFd::from(socket));
        Ok(Self {
            stream: Mutex::new(Some(stream)),
        })
    }

    /// Adopt a descriptor inherited from a trusted launcher, without dup/close.
    ///
    /// # Safety
    /// Any valid number greater than 2 must identify an exclusively owned
    /// descriptor inherited for this purpose. Invalid numbers are rejected by
    /// F_GETFD before constructing ownership. No other owner/thread may close,
    /// reuse or access a valid descriptor. The caller transfers ownership even
    /// on validation failure and must never adopt the number a second time. Perform this at
    /// controlled startup; an arbitrary integer from HTTP/WS is forbidden.
    /// Numbers 2 and below are always rejected before ownership is constructed.
    pub unsafe fn take_inherited(raw: RawFd) -> Result<Self> {
        ensure!(
            raw > 2,
            "standard/negative descriptors cannot be graphics FDs"
        );
        // SAFETY: F_GETFD accepts any integer and no pointer. It allocates no FD.
        // A successful probe does not prove exclusive ownership: startup must
        // precede Tokio/loggers/threads, and the launcher owns this one number.
        if unsafe { libc::fcntl(raw, libc::F_GETFD) } < 0 {
            return Err(std::io::Error::last_os_error()).context("invalid inherited graphics FD");
        }
        // SAFETY: the caller guarantees one valid exclusive inherited owner.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Self::from_owned(fd)
    }

    /// Consume the source only after Origin/authentication/client admission.
    ///
    /// Call inside a Tokio runtime with I/O enabled. Failed registration,
    /// canceled setup, EOF and disconnect never refill the slot or reconnect.
    /// Mutex ownership ends before runtime registration; no lock crosses await.
    pub fn claim(&self) -> Result<tokio::net::UnixStream> {
        let stream = self
            .stream
            .lock()
            .map_err(|_| anyhow::anyhow!("graphics FD slot poisoned"))?
            .take()
            .context("graphics FD already consumed")?;
        tokio::net::UnixStream::from_std(stream).context("register owned graphics stream")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        os::{fd::IntoRawFd, unix::net::UnixDatagram},
        sync::Arc,
        time::Duration,
    };
    use tokio::io::AsyncReadExt;

    fn closed(peer: &mut StdUnixStream) {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut byte = [0; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn stdin_stdout_stderr_and_negative_numbers_are_rejected_without_adoption() {
        for raw in [-1, 0, 1, 2] {
            // SAFETY: these values are rejected before ownership construction.
            assert!(unsafe { RfbFd::take_inherited(raw) }.is_err());
        }
    }

    #[test]
    fn closed_positive_inherited_fd_is_rejected_before_adoption() {
        const CHILD_ENV: &str = "GLISS_FD_TEST_CLOSED_OWNER";
        if std::env::var_os(CHILD_ENV).is_some() {
            let (client, _peer) = StdUnixStream::pair().unwrap();
            let number = client.into_raw_fd();
            assert!(number > 2);
            // Isolated child has no Tokio or other test threads allocating FDs.
            // SAFETY: into_raw_fd gave us the exclusive live owner to close.
            assert_eq!(unsafe { libc::close(number) }, 0);
            // SAFETY: invalid descriptors are explicitly allowed and rejected.
            let error = unsafe { RfbFd::take_inherited(number) }.err().unwrap();
            assert!(format!("{error:#}").contains("invalid inherited graphics FD"));
            assert_eq!(
                error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::EBADF)
            );
            return;
        }
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "rfb_fd::tests::closed_positive_inherited_fd_is_rejected_before_adoption"
                ])
                .env(CHILD_ENV, "yes")
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn nonsockets_datagrams_and_unconnected_unix_sockets_are_rejected() {
        let file = File::open("/dev/null").unwrap();
        assert!(RfbFd::from_owned(file.into()).is_err());
        let (datagram, _peer) = UnixDatagram::pair().unwrap();
        assert!(RfbFd::from_owned(datagram.into()).is_err());
        let socket = Socket::new(socket2::Domain::UNIX, Type::STREAM, None).unwrap();
        assert!(RfbFd::from_owned(socket.into()).is_err());
    }

    #[test]
    fn an_established_tcp_socket_is_not_a_unix_graphics_fd() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        assert!(RfbFd::from_owned(client.into()).is_err());
    }

    #[test]
    fn dropping_unclaimed_owner_closes_original_connection_without_duplication() {
        let (client, mut peer) = StdUnixStream::pair().unwrap();
        let number = client.as_raw_fd();
        let slot = RfbFd::from_owned(client.into()).unwrap();
        assert_eq!(
            slot.stream.lock().unwrap().as_ref().unwrap().as_raw_fd(),
            number
        );
        drop(slot);
        closed(&mut peer);
    }

    #[test]
    fn inherited_owner_keeps_exact_descriptor_and_nonblocking_read_does_not_consume_bytes() {
        let (client, mut peer) = StdUnixStream::pair().unwrap();
        let number = client.into_raw_fd();
        // SAFETY: into_raw_fd transferred the exclusive valid owner to us.
        let slot = unsafe { RfbFd::take_inherited(number) }.unwrap();
        {
            let mut guard = slot.stream.lock().unwrap();
            let stream = guard.as_mut().unwrap();
            assert_eq!(stream.as_raw_fd(), number);
            assert_eq!(
                stream.read(&mut [0; 1]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            peer.write_all(b"R").unwrap();
            let mut byte = [0; 1];
            stream.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"R");
        }
        drop(slot);
        closed(&mut peer);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_claims_have_one_owner_and_cannot_reopen_after_drop() {
        let (client, mut peer) = StdUnixStream::pair().unwrap();
        let slot = Arc::new(RfbFd::from_owned(client.into()).unwrap());
        let first = slot.clone();
        let second = slot.clone();
        let (a, b) = tokio::join!(
            tokio::spawn(async move { first.claim() }),
            tokio::spawn(async move { second.claim() })
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_ne!(a.is_ok(), b.is_ok());
        let stream = a.or(b).unwrap();
        assert!(slot.claim().is_err());
        drop(stream);
        assert!(slot.claim().is_err());
        closed(&mut peer);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn graphics_fd_does_not_survive_exec() {
        const INODE_ENV: &str = "GLISS_FD_TEST_SOCKET_INODE";
        const PRESENT_ENV: &str = "GLISS_FD_TEST_EXPECT_PRESENT";
        const OWNER_ENV: &str = "GLISS_FD_TEST_EXEC_OWNER";
        if let Ok(inode) = std::env::var(INODE_ENV) {
            let found = std::fs::read_dir("/proc/self/fd").unwrap().any(|entry| {
                entry
                    .ok()
                    .and_then(|entry| std::fs::read_link(entry.path()).ok())
                    .is_some_and(|link| link == std::path::Path::new(&inode))
            });
            assert_eq!(found, std::env::var(PRESENT_ENV).unwrap() == "yes");
            return;
        }
        if std::env::var_os(OWNER_ENV).is_none() {
            // Keep the deliberately inheritable positive-control socket out
            // of the parent harness, whose other tests can spawn concurrently.
            assert!(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "rfb_fd::tests::graphics_fd_does_not_survive_exec"
                    ])
                    .env(OWNER_ENV, "yes")
                    .status()
                    .unwrap()
                    .success()
            );
            return;
        }
        let (client, mut peer) = StdUnixStream::pair().unwrap();
        let socket = Socket::from(OwnedFd::from(client));
        socket.set_cloexec(false).unwrap();
        let inode = std::fs::read_link(format!("/proc/self/fd/{}", socket.as_raw_fd())).unwrap();
        let run_child = |expected| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "rfb_fd::tests::graphics_fd_does_not_survive_exec",
                ])
                .env(INODE_ENV, &inode)
                .env(PRESENT_ENV, expected)
                .status()
                .unwrap()
                .success()
        };
        // Positive control: the descriptor really was inheritable before admission.
        assert!(run_child("yes"));
        let slot = RfbFd::from_owned(socket.into()).unwrap();
        assert!(run_child("no"));
        drop(slot);
        closed(&mut peer);
    }

    #[tokio::test]
    async fn canceling_claimed_setup_closes_the_only_fd_and_preserves_empty_slot() {
        let (client, peer) = StdUnixStream::pair().unwrap();
        let slot = RfbFd::from_owned(client.into()).unwrap();
        let mut stream = slot.claim().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), async move {
                stream.read_exact(&mut [0; 12]).await
            })
            .await
            .is_err()
        );
        assert!(slot.claim().is_err());
        let mut peer = tokio::net::UnixStream::from_std({
            peer.set_nonblocking(true).unwrap();
            peer
        })
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), peer.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
