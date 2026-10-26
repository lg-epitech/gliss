//! Apply the fixed operator socket policy before HTTP can observe an accepted IO.
use axum::serve::Listener;
use serde::Serialize;
use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct Counters {
    pub accepted_sockets: u64,
    pub verified_sockets: u64,
    pub rejected_sockets: u64,
    pub initial_true: u64,
    pub initial_false: u64,
    pub initial_read_errors: u64,
    pub set_errors: u64,
    pub readback_errors: u64,
    pub readback_mismatches: u64,
    pub verified_readback_true: u64,
    pub verified_readback_false: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Snapshot {
    pub requested_nodelay: bool,
    pub scope: &'static str,
    #[serde(flatten)]
    pub counters: Counters,
}

pub(crate) struct SocketPolicy {
    requested: bool,
    counters: Mutex<Counters>,
}

trait SocketOption {
    fn get_nodelay(&self) -> io::Result<bool>;
    fn apply_nodelay(&self, value: bool) -> io::Result<()>;
}
impl SocketOption for TcpStream {
    fn get_nodelay(&self) -> io::Result<bool> {
        self.nodelay()
    }
    fn apply_nodelay(&self, value: bool) -> io::Result<()> {
        self.set_nodelay(value)
    }
}
impl SocketPolicy {
    pub(crate) fn new(requested: bool) -> Arc<Self> {
        Arc::new(Self {
            requested,
            counters: Mutex::new(Counters::default()),
        })
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            requested_nodelay: self.requested,
            scope: "All accepted HTTP sockets, including upgraded WebSockets; actual option verified before HTTP IO; cumulative since worker start.",
            counters: *self.counters.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }
    fn configure(&self, socket: &impl SocketOption) -> io::Result<()> {
        // Synchronous option syscalls under one lock keep snapshots coherent.
        let mut c = self.counters.lock().unwrap_or_else(|p| p.into_inner());
        c.accepted_sockets += 1;
        let initial = match socket.get_nodelay() {
            Ok(value) => value,
            Err(error) => {
                c.initial_read_errors += 1;
                c.rejected_sockets += 1;
                return Err(error);
            }
        };
        if initial {
            c.initial_true += 1;
        } else {
            c.initial_false += 1;
        }
        if let Err(error) = socket.apply_nodelay(self.requested) {
            c.set_errors += 1;
            c.rejected_sockets += 1;
            return Err(error);
        }
        let actual = match socket.get_nodelay() {
            Ok(value) => value,
            Err(error) => {
                c.readback_errors += 1;
                c.rejected_sockets += 1;
                return Err(error);
            }
        };
        if actual != self.requested {
            c.readback_mismatches += 1;
            c.rejected_sockets += 1;
            return Err(io::Error::other("accepted TCP_NODELAY readback mismatch"));
        }
        c.verified_sockets += 1;
        if actual {
            c.verified_readback_true += 1;
        } else {
            c.verified_readback_false += 1;
        }
        Ok(())
    }
}

pub(crate) struct VerifiedListener {
    inner: TcpListener,
    policy: Arc<SocketPolicy>,
}
impl VerifiedListener {
    pub(crate) fn new(inner: TcpListener, policy: Arc<SocketPolicy>) -> Self {
        Self { inner, policy }
    }
}
impl Listener for VerifiedListener {
    type Io = TcpStream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, address)) => match self.policy.configure(&stream) {
                    Ok(()) => return (stream, address),
                    Err(error) => {
                        drop(stream); // A failed/unverified socket never reaches HTTP.
                        tracing::warn!(%error, "accepted TCP socket policy rejected connection");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "HTTP TCP accept failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Fake {
        reads: Cell<u8>,
        failure: u8,
        requested: Cell<Option<bool>>,
    }
    impl SocketOption for Fake {
        fn get_nodelay(&self) -> io::Result<bool> {
            let n = self.reads.get();
            self.reads.set(n + 1);
            if self.failure == n + 1 {
                return Err(io::Error::other("injected read failure"));
            }
            Ok(n > 0 && self.failure != 4)
        }
        fn apply_nodelay(&self, value: bool) -> io::Result<()> {
            self.requested.set(Some(value));
            if self.failure == 3 {
                Err(io::Error::other("injected set failure"))
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn each_socket_option_failure_is_rejected_and_accounted() {
        for (failure, error_field) in [
            (1, "initial_read_errors"),
            (2, "readback_errors"),
            (3, "set_errors"),
            (4, "readback_mismatches"),
        ] {
            let p = SocketPolicy::new(true);
            let f = Fake {
                reads: Cell::new(0),
                failure,
                requested: Cell::new(None),
            };
            assert!(p.configure(&f).is_err());
            let s = serde_json::to_value(p.snapshot()).unwrap();
            assert_eq!(s["accepted_sockets"], 1);
            assert_eq!(s["rejected_sockets"], 1);
            assert_eq!(s["verified_sockets"], 0);
            assert_eq!(s[error_field], 1);
            assert_eq!(
                f.requested.get(),
                if failure == 1 { None } else { Some(true) }
            );
        }
    }
    #[tokio::test]
    async fn real_accepts_apply_and_read_back_both_values_before_io() {
        for requested in [false, true] {
            let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = inner.local_addr().unwrap();
            let policy = SocketPolicy::new(requested);
            let mut listener = VerifiedListener::new(inner, policy.clone());
            let (client, accepted) =
                tokio::join!(TcpStream::connect(address), Listener::accept(&mut listener));
            let _client = client.unwrap();
            assert_eq!(accepted.0.nodelay().unwrap(), requested);
            let c = policy.snapshot().counters;
            assert_eq!(c.accepted_sockets, 1);
            assert_eq!(c.verified_sockets, 1);
            assert_eq!(c.rejected_sockets, 0);
            assert_eq!(c.initial_true + c.initial_false, 1);
            assert_eq!(c.verified_readback_true, u64::from(requested));
            assert_eq!(c.verified_readback_false, u64::from(!requested));
        }
    }
}
