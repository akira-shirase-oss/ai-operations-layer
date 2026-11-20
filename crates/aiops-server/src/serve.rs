//! The collector's accept loop.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderValue, header};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tower::Service;

/// Connection-level bounds.
#[derive(Debug, Clone, Copy)]
pub struct ServeLimits {
    /// A client must finish sending a request's headers within this; it also closes an idle
    /// keep-alive connection that does not start its next request in time.
    pub header_read_timeout: Duration,
    /// Accepted connections at most; beyond it, new ones wait in the kernel's backlog.
    pub max_connections: usize,
    /// After shutdown starts, in-flight requests get this long to finish.
    pub drain_grace: Duration,
    /// A write (or read) that makes no progress for this long ends the connection: a client
    /// that never reads its responses cannot hold a slot (review N3).
    pub write_stall_timeout: Duration,
    /// Requests served on one connection at most; the last is answered with
    /// `Connection: close`, so pipelining cannot stream through one slot forever.
    pub max_requests_per_connection: u32,
    /// A connection is closed after this long even while busy. Its last `drain_grace`
    /// announces the end on the wire: every response in that window carries
    /// `Connection: close`, so the client learns from the answer it just read instead
    /// of losing a request it sent into an abrupt close (the keep-alive race).
    pub connection_lifetime: Duration,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(10),
            max_connections: 2_048,
            drain_grace: Duration::from_secs(10),
            write_stall_timeout: Duration::from_secs(10),
            max_requests_per_connection: 1_000,
            connection_lifetime: Duration::from_secs(300),
        }
    }
}

/// Serves `app` on `listener` until `shutdown` is cancelled, then drains for at most
/// `limits.drain_grace` (review findings 9 and 16). Each connection is bounded in time
/// (header read, write progress, lifetime) and in requests (review N3).
pub async fn run(
    listener: TcpListener,
    app: Router,
    limits: ServeLimits,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    // HTTP/1 only: hyper-util's auto mode first sniffs for an HTTP/2 preface with no timer,
    // so a connection that sends nothing would hold its slot forever. The SDK and browsers
    // speak HTTP/1.1 to the collector.
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout)
        // Each connection's read buffer is bounded (review N2): request headers fit, and a
        // body is pulled only as the handler reads it, which it does after authenticating.
        .max_buf_size(64 * 1024)
        .keep_alive(true);
    let builder = Arc::new(builder);
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    loop {
        // Take a slot before accepting: past the cap, connections wait in the backlog.
        let permit = tokio::select! {
            () = shutdown.cancelled() => break,
            p = slots.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let (stream, _peer) = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // Out of descriptors and the like: back off, never spin or exit.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let builder = builder.clone();
        let app = app.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            serve_one(&builder, stream, app, limits, shutdown).await;
            drop(permit);
        });
    }
    // Every connection task holds a slot: all slots back means every connection is done.
    let all = u32::try_from(limits.max_connections).unwrap_or(u32::MAX);
    if tokio::time::timeout(
        limits.drain_grace + Duration::from_secs(1),
        slots.acquire_many(all),
    )
    .await
    .is_err()
    {
        tracing::warn!("drain deadline passed; closing connections with requests in flight");
    }
    Ok(())
}

/// One connection: served until it ends, its lifetime passes or shutdown starts; in the last
/// two cases the request in flight may finish (up to `drain_grace`), then it is closed.
async fn serve_one(
    builder: &http1::Builder,
    stream: TcpStream,
    app: Router,
    limits: ServeLimits,
    shutdown: CancellationToken,
) {
    let served = Arc::new(AtomicU32::new(0));
    let max_requests = limits.max_requests_per_connection.max(1);
    // The last window is announced, not abrupt: a request arriving in the connection's
    // final `drain_grace` is answered with `Connection: close`, so the client is told on
    // the response it just read and stops sending — instead of having a request's bytes
    // vanish unparsed when the lifetime timer fires mid-send (the 5-minute EOF the 1M
    // control run hit at every 300 s tick). hyper only marks the close itself once the
    // shutdown is already draining; that is too late for a request about to be sent.
    let close_after = Instant::now()
        + limits
            .connection_lifetime
            .saturating_sub(limits.drain_grace);
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let n = served.fetch_add(1, Ordering::Relaxed) + 1;
        let last = n >= max_requests || Instant::now() >= close_after;
        // Router is always ready, so `call` without `poll_ready` is correct.
        let response = app.clone().call(req.map(axum::body::Body::new));
        async move {
            let mut response = match response.await {
                Ok(r) => r,
                Err(never) => match never {},
            };
            if last {
                // The last request this connection serves: hyper closes after answering, and
                // drops anything pipelined behind it.
                response
                    .headers_mut()
                    .insert(header::CONNECTION, HeaderValue::from_static("close"));
            }
            Ok::<_, Infallible>(response)
        }
    });
    let io = TokioIo::new(WriteDeadline::new(stream, limits.write_stall_timeout));
    let conn = builder.serve_connection(io, service);
    let mut conn = std::pin::pin!(conn);
    let ended = tokio::select! {
        r = conn.as_mut() => Some(r),
        () = tokio::time::sleep(limits.connection_lifetime) => None,
        () = shutdown.cancelled() => None,
    };
    let result = if let Some(r) = ended {
        r
    } else {
        conn.as_mut().graceful_shutdown();
        match tokio::time::timeout(limits.drain_grace, conn).await {
            Ok(r) => r,
            Err(_) => return, // dropped: the connection is closed
        }
    };
    if let Err(e) = result {
        tracing::debug!(error = %e, "connection ended with an error");
    }
}

/// A stream whose writes must make progress: a write (or flush) pending for longer than the
/// deadline fails with `TimedOut`, which ends the connection (review N3). Reads are bounded
/// by hyper's header-read timeout and the request timeout.
struct WriteDeadline<S> {
    inner: S,
    stall: Duration,
    pending_since: Option<Pin<Box<Sleep>>>,
}

impl<S> WriteDeadline<S> {
    fn new(inner: S, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            pending_since: None,
        }
    }

    /// Called when the inner write is pending: starts the stall clock, or fails once it ran out.
    fn stalled(&mut self, cx: &mut Context<'_>) -> Poll<io::Error> {
        let stall = self.stall;
        let deadline = self
            .pending_since
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(stall)));
        match deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(io::Error::new(
                io::ErrorKind::TimedOut,
                "the client stopped reading",
            )),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for WriteDeadline<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WriteDeadline<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(r) => {
                self.pending_since = None;
                Poll::Ready(r)
            }
            Poll::Pending => self.stalled(cx).map(Err),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Ready(r) => {
                self.pending_since = None;
                Poll::Ready(r)
            }
            Poll::Pending => self.stalled(cx).map(Err),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(r) => {
                self.pending_since = None;
                Poll::Ready(r)
            }
            Poll::Pending => self.stalled(cx).map(Err),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}
