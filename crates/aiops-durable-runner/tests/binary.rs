//! The real `aiops-dr-adapter` binary: a SIGTERM with a call in flight ends the process
//! inside its systemd unit's `TimeoutStopSec`, so systemd never SIGKILLs it before the SDK
//! flushes what it holds (review R4).
#![expect(
    clippy::unwrap_used,
    reason = "integration tests: a failed setup step is a test failure"
)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::post;

/// The unit's stop timeout, read from the shipped unit file.
fn timeout_stop() -> Duration {
    let unit = include_str!("../../../deploy/aiops-dr-adapter.service");
    let secs = unit
        .lines()
        .find_map(|l| l.strip_prefix("TimeoutStopSec="))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap();
    Duration::from_secs(secs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sigterm_with_a_call_in_flight_ends_inside_the_units_stop_timeout() {
    // A handler that never answers within the step timeout (20 s, as the example config).
    let handler = Router::new().route(
        "/step",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(120)).await;
            "{}"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, handler).await });
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = std::env::temp_dir().join(format!("aiops-dr-adapter-r4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("adapter.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "listen": format!("127.0.0.1:{port}"),
            "runner": {"label": "dr-r4", "url": "http://127.0.0.1:9", "step_timeout_ms": 20_000},
            "workflow": "publish_article",
            "routes": [{"route": "publish", "kind": "tool", "external_effect": true,
                        "upstream": format!("http://{upstream}/step")}]
        })
        .to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aiops-dr-adapter"))
        .env("AIOPS_DR_ADAPTER_CONFIG", &config)
        .env("DURABLE_READ_KEY", "r".repeat(32))
        .env("AIOPS_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    let mut up = false;
    for _ in 0..200 {
        if http.get(format!("{base}/healthz")).send().await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(up, "the adapter did not start");
    // A call the runner made, in flight when the stop comes.
    let call = tokio::spawn(
        http.post(format!("{base}/h/publish"))
            .header("x-durable-run-id", "7")
            .header("x-durable-step", "0")
            .header("x-durable-attempt", "1")
            .json(&serde_json::json!({"input": [123, 125], "idem_key": "article/r-7/0"}))
            .timeout(Duration::from_secs(60))
            .send(),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stop = Instant::now();
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let limit = timeout_stop();
    let exited = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if stop.elapsed() > limit + Duration::from_secs(10) {
            let _ = child.kill();
            break None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let took = stop.elapsed();
    let _ = std::fs::remove_dir_all(&dir);
    call.abort();
    assert!(exited.is_some(), "still running {took:?} after SIGTERM");
    assert!(
        took < limit,
        "exited {took:?} after SIGTERM; systemd SIGKILLs at TimeoutStopSec = {limit:?}"
    );
}
