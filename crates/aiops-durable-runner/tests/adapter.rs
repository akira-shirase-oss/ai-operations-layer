//! The adapter over real HTTP, between a fake runner (its calls, its `GET /jobs/{id}`), a
//! fake handler and a fake collector: what the runner sees is unchanged, and what the
//! adapter reports names the runner's run, step and attempt.
#![expect(
    clippy::unwrap_used,
    reason = "integration tests: a failed setup step is a test failure"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aiops_domain::correlate::operation_for_run;
use aiops_durable_runner::adapter::{Adapter, router, watch};
use aiops_durable_runner::config::{Config, File};
use aiops_sdk::Client;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const KEY: &str = "aiops_ik_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const READ_KEY: &str = "read-key-0123456789abcdef0123456789abcdef";

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

type Batches = Arc<Mutex<Vec<Value>>>;

async fn collector() -> (String, Batches) {
    let batches: Batches = Arc::default();
    let app = Router::new()
        .route(
            "/v1/ingest",
            post(|State(b): State<Batches>, body: String| async move {
                let v: Value = serde_json::from_str(&body).unwrap();
                let n = v["events"].as_array().map_or(0, Vec::len);
                b.lock().unwrap().push(v);
                json!({"accepted": n, "duplicates": 0, "refused": [], "withheld_attributes": 0, "redactions": 0})
                    .to_string()
            }),
        )
        .with_state(batches.clone());
    (serve(app).await, batches)
}

fn events(b: &Batches) -> Vec<Value> {
    b.lock()
        .unwrap()
        .iter()
        .flat_map(|x| x["events"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// A handler that answers `status` after `delay`, and a runner whose run 5 is `state`.
async fn handler_and_runner(delay: Duration, status: u16, state: &'static str) -> (String, String) {
    let handler = Router::new().route(
        "/step",
        post(move || async move {
            tokio::time::sleep(delay).await;
            (StatusCode::from_u16(status).unwrap(), "{}")
        })
        .get(|| async { StatusCode::OK }),
    );
    let runner = Router::new().route(
        "/jobs/{id}",
        get(move || async move {
            json!({"id": 5, "key": "article/r-5", "state": state, "steps": 1, "finished_at": 1})
                .to_string()
        }),
    );
    (serve(handler).await, serve(runner).await)
}

async fn adapter(upstream: &str, runner: &str, collector: &str) -> (Arc<Adapter>, String) {
    let file: File = serde_json::from_value(json!({
        "listen": "127.0.0.1:0",
        "runner": {"label": "dr-test", "url": runner, "step_timeout_ms": 200},
        "workflow": "publish_article",
        "routes": [{"route": "publish", "kind": "tool", "external_effect": true, "upstream": format!("{upstream}/step")}],
        "poll_interval_ms": 50
    }))
    .unwrap();
    let mut c = aiops_sdk::Config::new(collector, KEY, "durable-runner-adapter");
    c.flush_interval = Duration::from_millis(20);
    c.include_error_messages = true;
    let sdk = Client::start(&c).unwrap();
    let config = Config {
        file,
        read_key: READ_KEY.into(),
        endpoint: Some(collector.into()),
        ingest_key: Some(KEY.into()),
        release: None,
    };
    config.validate().unwrap();
    let a = Adapter::new(config, sdk).unwrap();
    tokio::spawn(watch(a.clone(), CancellationToken::new()));
    let url = serve(router(a.clone())).await;
    (a, url)
}

/// How the runner calls a handler (its D-050 headers), giving up after `timeout`.
async fn runner_call(
    url: &str,
    attempt: u32,
    timeout: Duration,
) -> Result<reqwest::Response, reqwest::Error> {
    reqwest::Client::new()
        .post(format!("{url}/h/publish"))
        .header("x-idempotency-key", "article/r-5/0")
        .header("x-durable-run-id", "5")
        .header("x-durable-step", "0")
        .header("x-durable-attempt", attempt.to_string())
        .json(&json!({"input": [123, 125], "idem_key": "article/r-5/0"}))
        .timeout(timeout)
        .send()
        .await
}

async fn eventually(mut ok: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "not reached in 5 s: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_the_runner_hangs_up_on_is_still_reported_and_the_run_still_finishes() {
    // The runner gives up at its deadline and closes the connection; the handler answers
    // later. The attempt must still be reported (uncertain: the runner gave up), and the
    // run must still be finished from the runner's answer — the adapter must not wait for
    // a call it thinks is in flight for ever.
    let (collector_url, batches) = collector().await;
    let (upstream, runner) = handler_and_runner(Duration::from_millis(500), 200, "done").await;
    let (a, url) = adapter(&upstream, &runner, &collector_url).await;
    let err = runner_call(&url, 1, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(err.is_timeout(), "the runner gave up: {err}");
    eventually(
        || {
            events(&batches)
                .iter()
                .any(|e| e["type"] == "operation.finished")
        },
        "the operation's finish",
    )
    .await;
    let all = events(&batches);
    let op = operation_for_run("dr-test", 5).to_string();
    assert!(
        all.iter().all(|e| e["operation_id"] == op),
        "named after the run"
    );
    let finished = all.iter().find(|e| e["type"] == "step.finished").unwrap();
    assert_eq!(finished["attempt"], 1);
    assert_eq!(finished["outcome"], "uncertain");
    assert_eq!(finished["uncertain_reason"], "timeout_after_send");
    assert_eq!(finished["error_code"], "runner_deadline_passed");
    let fin = all
        .iter()
        .find(|e| e["type"] == "operation.finished")
        .unwrap();
    assert_eq!(fin["outcome"], "succeeded", "the runner said done");
    assert_eq!(
        fin["events_emitted"], 4,
        "start, step start and finish, finish"
    );
    assert_eq!(a.view().runs_tracked, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_runner_sees_the_handlers_answer_unchanged_and_the_finish_is_the_runners() {
    let (collector_url, batches) = collector().await;
    let (upstream, runner) = handler_and_runner(Duration::ZERO, 409, "failed:step:0").await;
    let (_a, url) = adapter(&upstream, &runner, &collector_url).await;
    for attempt in 1..=2 {
        let r = runner_call(&url, attempt, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(r.status(), 409, "the handler's own answer");
    }
    eventually(
        || {
            events(&batches)
                .iter()
                .any(|e| e["type"] == "operation.finished")
        },
        "the operation's finish",
    )
    .await;
    let all = events(&batches);
    let attempts: Vec<_> = all
        .iter()
        .filter(|e| e["type"] == "step.finished")
        .map(|e| {
            (
                e["attempt"].clone(),
                e["failure_class"].clone(),
                e["error_code"].clone(),
            )
        })
        .collect();
    assert_eq!(
        attempts,
        vec![
            (json!(1), json!("rejected"), json!("http_409")),
            (json!(2), json!("rejected"), json!("http_409")),
        ]
    );
    let steps: Vec<_> = all
        .iter()
        .filter(|e| e["type"] == "step.started")
        .map(|e| e["step_id"].clone())
        .collect();
    assert_eq!(steps[0], steps[1], "both attempts are one step");
    let fin = all
        .iter()
        .find(|e| e["type"] == "operation.finished")
        .unwrap();
    assert_eq!(fin["outcome"], "failed");
    assert_eq!(
        fin["failure_class"], "rejected",
        "the last failed attempt's class"
    );
    assert_eq!(fin["error_code"], "failed_at_step_0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_handler_connection_reaches_the_runner_as_an_unreadable_reply() {
    // The handler closes the connection without answering: the runner must see what it
    // would have seen (a reply it cannot read, its "lost"), never a status that would make
    // it retry without reconciling.
    use tokio::io::AsyncReadExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            drop(sock); // read the call, never answer
        }
    });
    let (collector_url, batches) = collector().await;
    let (_, runner) = handler_and_runner(Duration::ZERO, 200, "reconciling:step:0").await;
    let (_a, url) = adapter(&upstream, &runner, &collector_url).await;
    // Exactly how the runner classifies a reply (its HttpHandler): lost unless a readable
    // 2xx, a status (failed, retried without reconciling), a connect error or a timeout.
    let lost = match runner_call(&url, 1, Duration::from_secs(2)).await {
        Ok(r) if r.status().is_success() => r.bytes().await.is_err(),
        Ok(_) => false,
        Err(e) => !e.is_timeout() && !e.is_connect(),
    };
    assert!(lost, "the runner reads this call as lost and reconciles it");
    eventually(
        || {
            events(&batches)
                .iter()
                .any(|e| e["type"] == "step.finished")
        },
        "the attempt's finish",
    )
    .await;
    let finished = events(&batches)
        .into_iter()
        .find(|e| e["type"] == "step.finished")
        .unwrap();
    assert_eq!(finished["uncertain_reason"], "connection_lost_after_send");
}
