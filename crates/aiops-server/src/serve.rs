//! The collector's accept loop.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

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
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(10),
            max_connections: 2_048,
            drain_grace: Duration::from_secs(10),
        }
    }
}

/// Serves `app` on `listener` until `shutdown` is cancelled, then drains for at most
/// `limits.drain_grace` (review findings 9 and 16).
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
    let graceful = GracefulShutdown::new();
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
        let service = TowerToHyperService::new(app.clone());
        let conn = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(error = %e, "connection ended with an error");
            }
            drop(permit);
        });
    }
    if tokio::time::timeout(limits.drain_grace, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("drain deadline passed; closing connections with requests in flight");
    }
    Ok(())
}
