//! The HTTP listener: a bounded number of connections, none of which a peer
//! can keep open by doing nothing.
//!
//! `axum::serve` accepts without limit and gives hyper no timer, which leaves
//! hyper's header timeout switched off. This is the same accept loop with
//! both added. A request body is the handler's to bound: only the login
//! handler reads one.

use crate::tcp_listener::VerifiedListener;
use axum::{Extension, Router, extract::ConnectInfo, serve::Listener};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use std::{
    future::Future,
    io,
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::Sleep,
};

/// Connections a client uses for requests besides its stream: browsers open
/// at most six to one host.
const REQUEST_CONNECTIONS_PER_CLIENT: usize = 6;
/// Connections for whoever is not a streaming client: a health check, a
/// login page that is loading, a viewer about to be told every slot is taken.
const SPARE_CONNECTIONS: usize = 16;
/// Time to send a whole request head, from the accept or from the end of the
/// previous response, so also the longest a connection may sit idle. A head
/// fits in one packet and browsers reopen idle connections on their own. With
/// the five seconds a login body gets, a stalled request is over in ten.
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a peer may take none of what is being written to it: the limit
/// each WebSocket write of a stream already has.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Limits {
    /// Sockets open at once, upgraded streams included.
    pub connections: usize,
    pub request_head: Duration,
    pub write_stall: Duration,
}

impl Limits {
    /// Room for every stream `--max-clients` allows and for the requests of
    /// those clients, plus a few connections for everyone else.
    pub fn for_clients(max_clients: usize) -> Self {
        Self {
            connections: max_clients * (1 + REQUEST_CONNECTIONS_PER_CLIENT) + SPARE_CONNECTIONS,
            request_head: REQUEST_HEAD_TIMEOUT,
            write_stall: WRITE_STALL_TIMEOUT,
        }
    }
}

/// An accepted socket and its place among the allowed connections, which it
/// keeps until it is closed, also once it has become a WebSocket.
struct Connection {
    stream: TcpStream,
    write_stall: Duration,
    /// Runs while a write waits for the peer to take bytes.
    stalled: Option<Pin<Box<Sleep>>>,
    _slot: OwnedSemaphorePermit,
}

impl Connection {
    /// Fails a write that has waited `write_stall` without the peer taking a
    /// byte. No timeout covers reads: an idle stream is not a stalled one.
    fn progress<T>(
        &mut self,
        cx: &mut Context<'_>,
        written: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if written.is_ready() {
            self.stalled = None;
            return written;
        }
        let write_stall = self.write_stall;
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(write_stall)));
        match stalled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::ErrorKind::TimedOut.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.stream).poll_write(cx, bytes);
        this.progress(cx, written)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        slices: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.stream).poll_write_vectored(cx, slices);
        this.progress(cx, written)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// Serves `router` until `shutdown` completes, then lets open requests finish.
/// Handlers find the peer's address in `ConnectInfo<SocketAddr>`.
pub async fn serve(
    mut listener: VerifiedListener,
    router: Router,
    limits: Limits,
    shutdown: impl Future<Output = ()>,
) {
    let slots = Arc::new(Semaphore::new(limits.connections));
    let (closing, _) = watch::channel(false);
    // Every connection task holds a receiver: the sender sees the last one go.
    let (finished, unfinished) = watch::channel(());
    let mut shutdown = pin!(shutdown);
    loop {
        // Taken before the accept, so a connection over the limit waits in the
        // kernel's backlog and costs this process nothing until a slot is free.
        let slot = tokio::select! {
            slot = slots.clone().acquire_owned() => slot.expect("the semaphore is never closed"),
            () = &mut shutdown => break,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted,
            () = &mut shutdown => break,
        };
        let io = TokioIo::new(Connection {
            stream,
            write_stall: limits.write_stall,
            stalled: None,
            _slot: slot,
        });
        let service = TowerToHyperService::new(router.clone().layer(Extension(ConnectInfo(peer))));
        let mut closing = closing.subscribe();
        let unfinished = unfinished.clone();
        tokio::spawn(async move {
            let mut http = hyper::server::conn::http1::Builder::new();
            // The header timeout only runs with a timer.
            http.timer(TokioTimer::new())
                .header_read_timeout(limits.request_head);
            let mut connection = pin!(http.serve_connection(io, service).with_upgrades());
            tokio::select! {
                // Ends when the peer is done, a timeout fires, or the
                // connection is handed to a WebSocket.
                _ = connection.as_mut() => {}
                // The value read from the channel must not live across the await below.
                () = async { let _ = closing.wait_for(|closing| *closing).await; } => {
                    connection.as_mut().graceful_shutdown();
                    let _ = connection.await;
                }
            }
            drop(unfinished);
        });
    }
    closing.send_replace(true);
    drop(listener);
    drop(unfinished);
    finished.closed().await;
}
