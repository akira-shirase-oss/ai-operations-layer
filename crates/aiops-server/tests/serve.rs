//! Review finding 9: connection-level bounds on the collector's accept loop. No database.
#![expect(
    clippy::unwrap_used,
    reason = "tests: a failed setup step is a test failure"
)]

use std::time::{Duration, Instant};

use aiops_server::serve::{ServeLimits, run};
use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

async fn server(limits: ServeLimits) -> (String, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    // `/big`: a 1 MiB response, larger than the socket buffers between server and client.
    let app = Router::new()
        .route("/", get(|| async { "ok" }))
        .route("/big", get(|| async { "x".repeat(1024 * 1024) }));
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move { run(listener, app, limits, t).await });
    (addr, token)
}

/// Seconds until the server closes the connection (EOF or reset); `None` if still open
/// after 8 s.
async fn seconds_until_closed(r: &mut (impl AsyncRead + Unpin)) -> Option<f64> {
    let started = Instant::now();
    let mut buf = [0u8; 1024];
    loop {
        match tokio::time::timeout(Duration::from_secs(8), r.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return Some(started.elapsed().as_secs_f64()),
            Ok(Ok(_)) => {}
            Err(_) => return None,
        }
    }
}

fn one_second() -> ServeLimits {
    ServeLimits {
        header_read_timeout: Duration::from_secs(1),
        ..ServeLimits::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_trickling_its_headers_is_disconnected() {
    let (addr, _token) = server(one_second()).await;
    let (mut r, mut w) = TcpStream::connect(&addr).await.unwrap().into_split();
    w.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    let trickle = tokio::spawn(async move {
        for _ in 0..16 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if w.write_all(b"X-Slow: 1\r\n").await.is_err() {
                break;
            }
        }
    });
    let closed = seconds_until_closed(&mut r).await;
    trickle.abort();
    assert!(
        closed.is_some_and(|t| t < 4.0),
        "slow-header client still open: {closed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_keep_alive_connection_is_closed() {
    let (addr, _token) = server(one_second()).await;
    let mut s = TcpStream::connect(&addr).await.unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    let closed = seconds_until_closed(&mut s).await;
    assert!(
        closed.is_some_and(|t| t < 4.0),
        "idle keep-alive still open: {closed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_beyond_the_cap_wait_until_one_is_free() {
    let (addr, _token) = server(ServeLimits {
        max_connections: 2,
        header_read_timeout: Duration::from_secs(2),
        ..ServeLimits::default()
    })
    .await;
    // Two silent connections hold both slots (until the header timeout frees them).
    let _a = TcpStream::connect(&addr).await.unwrap();
    let _b = TcpStream::connect(&addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut c = TcpStream::connect(&addr).await.unwrap(); // in the kernel's backlog only
    let started = Instant::now();
    c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(8), c.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let waited = started.elapsed();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    assert!(
        waited >= Duration::from_millis(1_500),
        "served at once ({waited:?}): no cap"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_an_in_flight_request_then_stops() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            "done"
        }),
    );
    let token = CancellationToken::new();
    let t = token.clone();
    let server = tokio::spawn(async move { run(listener, app, ServeLimits::default(), t).await });
    let mut s = TcpStream::connect(&addr).await.unwrap();
    s.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    token.cancel();
    let mut buf = [0u8; 256];
    let n = s.read(&mut buf).await.unwrap();
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("done"),
        "the in-flight request finished"
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_sends_nothing_is_closed() {
    let (addr, _token) = server(one_second()).await;
    let mut s = TcpStream::connect(&addr).await.unwrap();
    let closed = seconds_until_closed(&mut s).await;
    assert!(
        closed.is_some_and(|t| t < 4.0),
        "silent connection still open: {closed:?}"
    );
}

/// A connection with a small receive buffer, so a server writing to it stalls quickly.
async fn small_window(addr: &str) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.connect(addr.parse().unwrap()).await.unwrap()
}

async fn answered_within(addr: &str, limit: Duration) -> Option<f64> {
    let started = Instant::now();
    let attempt = async {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        buf.starts_with(b"HTTP/1.1 200")
    };
    match tokio::time::timeout(limit, attempt).await {
        Ok(true) => Some(started.elapsed().as_secs_f64()),
        _ => None,
    }
}

// Review N3: a client that never reads its responses is cut after the write-stall deadline
// and frees its slot; before, it held the slot indefinitely and new clients waited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_never_reads_is_cut_and_frees_its_slot() {
    let (addr, _token) = server(ServeLimits {
        max_connections: 1,
        write_stall_timeout: Duration::from_secs(1),
        ..ServeLimits::default()
    })
    .await;
    let mut stalled = small_window(&addr).await;
    let mut pipelined = Vec::new();
    for _ in 0..20 {
        pipelined.extend_from_slice(b"GET /big HTTP/1.1\r\nhost: x\r\n\r\n");
    }
    stalled.write_all(&pipelined).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let waited = answered_within(&addr, Duration::from_secs(6)).await;
    assert!(
        waited.is_some_and(|s| s < 4.0),
        "a new client waited behind a non-reading one: {waited:?}"
    );
    drop(stalled);
}

// Review N3: a connection serves a bounded number of requests, then closes, so one client
// cannot pipeline an unbounded stream through one slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_serves_a_bounded_number_of_requests() {
    let (addr, _token) = server(ServeLimits {
        max_requests_per_connection: 3,
        ..one_second()
    })
    .await;
    let mut s = TcpStream::connect(&addr).await.unwrap();
    let mut pipelined = Vec::new();
    for _ in 0..10 {
        pipelined.extend_from_slice(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n");
    }
    s.write_all(&pipelined).await.unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_millis(800), s.read_to_end(&mut buf)).await;
    assert!(
        read.is_ok(),
        "the connection was not closed after its last allowed request"
    );
    let answered = buf.windows(12).filter(|w| w == b"HTTP/1.1 200").count();
    assert_eq!(answered, 3, "{}", String::from_utf8_lossy(&buf));
}

// Review N3: a busy connection is closed after its lifetime (after the request in flight),
// so no client keeps one slot forever by never going idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_connection_is_closed_after_its_lifetime() {
    let (addr, _token) = server(ServeLimits {
        connection_lifetime: Duration::from_secs(1),
        ..ServeLimits::default()
    })
    .await;
    let mut s = TcpStream::connect(&addr).await.unwrap();
    let started = Instant::now();
    let mut buf = [0u8; 4096];
    let closed_after = loop {
        if started.elapsed() > Duration::from_secs(5) {
            break None;
        }
        if s.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .is_err()
        {
            break Some(started.elapsed());
        }
        match tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => break Some(started.elapsed()),
            Ok(Ok(_)) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(_) => break None,
        }
    };
    assert!(
        closed_after.is_some_and(|d| d < Duration::from_secs(3)),
        "a busy connection outlived its lifetime: {closed_after:?}"
    );
}
