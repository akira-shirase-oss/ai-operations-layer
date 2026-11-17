//! The SDK never blocks or fails its host (D-021): against a dead port, a fake collector
//! that fails, refuses, hangs or recovers. Every drop is counted and turns into a gap
//! report; re-sends keep their event IDs.
#![expect(
    clippy::unwrap_used,
    reason = "tests: a failed setup step is a test failure"
)]

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aiops_sdk::{Client, Config, FailureClass, Finish, StepKind, UncertainReason};
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::{Value, json};

const KEY: &str = "aiops_ik_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

const ACCEPT: u8 = 0;
const FAIL_503: u8 = 1;
const REJECT_401: u8 = 2;
const HANG: u8 = 3;
/// Store the batch, then answer 503 (the response is "lost"): the SDK must re-send it
/// unchanged.
const STORE_THEN_503: u8 = 4;
/// Accept the batch but refuse its second event by name.
const REFUSE_SECOND: u8 = 5;

#[derive(Default)]
struct Fake {
    mode: AtomicU8,
    requests: AtomicUsize,
    batches: Mutex<Vec<Value>>,
}

async fn ingest(State(fake): State<Arc<Fake>>, body: String) -> (StatusCode, String) {
    fake.requests.fetch_add(1, Ordering::SeqCst);
    let batch: Value = serde_json::from_str(&body).unwrap();
    match fake.mode.load(Ordering::SeqCst) {
        FAIL_503 => (StatusCode::SERVICE_UNAVAILABLE, String::new()),
        REJECT_401 => (StatusCode::UNAUTHORIZED, String::new()),
        HANG => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            (StatusCode::OK, String::new())
        }
        REFUSE_SECOND => {
            let n = batch["events"].as_array().map_or(0, Vec::len);
            fake.batches.lock().unwrap().push(batch);
            fake.mode.store(ACCEPT, Ordering::SeqCst);
            (
                StatusCode::OK,
                json!({"accepted": n - 1, "duplicates": 0, "refused": [{"index": 1, "code": "invalid_value", "field": "kind"}],
                       "withheld_attributes": 0, "redactions": 0})
                    .to_string(),
            )
        }
        STORE_THEN_503 => {
            fake.batches.lock().unwrap().push(batch);
            fake.mode.store(ACCEPT, Ordering::SeqCst);
            (StatusCode::SERVICE_UNAVAILABLE, String::new())
        }
        _ => {
            let n = batch["events"].as_array().map_or(0, Vec::len);
            fake.batches.lock().unwrap().push(batch);
            (
                StatusCode::OK,
                json!({"accepted": n, "duplicates": 0, "refused": [], "withheld_attributes": 0, "redactions": 0})
                    .to_string(),
            )
        }
    }
}

async fn fake_collector() -> (String, Arc<Fake>) {
    fake_collector_with_limit(2 * 1024 * 1024).await
}

/// A fake with a real request-body cap (the collector's is 1 MiB): over it, 413.
async fn fake_collector_with_limit(max_body: usize) -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/ingest", post(ingest))
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), fake)
}

fn config(endpoint: &str) -> Config {
    let mut c = Config::new(endpoint, KEY, "reference-host");
    c.flush_interval = Duration::from_millis(20);
    c.retry_budget = Duration::from_millis(500);
    c.request_timeout = Duration::from_millis(500);
    c
}

fn all_events(fake: &Fake) -> Vec<Value> {
    fake.batches
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| b["events"].as_array().cloned().unwrap_or_default())
        .collect()
}

async fn eventually(mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(Instant::now() < deadline, "condition not reached in 5 s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn run_operation(client: &Client, n: usize) {
    let op = client
        .operation("publish_article")
        .request_id(&format!("req-{n}"))
        .start();
    let s = op
        .step(StepKind::Model, "generate")
        .model("fake-model-1")
        .start();
    s.finish(
        Finish::succeeded()
            .usage(10, 20)
            .cost_estimate(1_000, "USD", "list"),
    );
    op.succeed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_collector_never_blocks_the_host_and_every_drop_is_counted() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cfg = config(&format!("http://127.0.0.1:{port}"));
    cfg.queue_capacity = 256;
    let client = Client::start(&cfg).unwrap();

    let started = Instant::now();
    for n in 0..5_000 {
        run_operation(&client, n); // 4 events each
    }
    let host_time = started.elapsed();
    // 20,000 emits; each is a non-blocking enqueue (debug build, shared machine: generous).
    assert!(
        host_time < Duration::from_secs(2),
        "host spent {host_time:?} emitting"
    );

    let s = client.stats();
    assert_eq!(s.emitted, 20_000);
    assert!(s.dropped_queue_full > 0, "{s:?}");
    let shut = Instant::now();
    let s = client.shutdown(Duration::from_millis(800)).await;
    assert!(
        shut.elapsed() < Duration::from_secs(2),
        "shutdown overran its deadline"
    );
    assert_eq!(s.accepted, 0);
    assert_eq!(
        s.dropped(),
        s.emitted,
        "every emitted event is accounted for as dropped: {s:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_arrive_numbered_with_the_emitted_count_on_the_finish() {
    let (endpoint, fake) = fake_collector().await;
    let client = Client::start(&config(&endpoint)).unwrap();
    let op = client
        .operation("publish_article")
        .external_trace("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        .attribute("http.route", "/articles")
        .attribute("prompt", "write about cats")
        .start();
    let step = op.step(StepKind::Model, "generate").start();
    let retry =
        step.finish(Finish::failed(FailureClass::Transport, "connect_refused").message("nope"));
    let step = retry.retry().start();
    assert_eq!(step.attempt(), 2);
    step.finish(Finish::succeeded());
    let publish = op
        .step(StepKind::Tool, "publish")
        .external_effect(true)
        .start();
    publish.finish(Finish::uncertain(
        UncertainReason::TimeoutAfterSend,
        "publish_timeout",
    ));
    op.finish(Finish::uncertain(
        UncertainReason::TimeoutAfterSend,
        "publish_timeout",
    ));
    op.succeed(); // idempotent: the first finish wins, nothing more is emitted
    let stats = client.shutdown(Duration::from_secs(2)).await;
    assert_eq!(stats.emitted, 8);
    assert_eq!(stats.accepted, 8);
    assert_eq!(
        stats.attributes_dropped_locally, 1,
        "a prompt never leaves the host"
    );

    let events = all_events(&fake);
    assert_eq!(events.len(), 8);
    assert_eq!(
        events[0]["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736",
        "the external trace is recorded"
    );
    assert_ne!(
        events[0]["operation_id"], "4bf92f3577b34da6a3ce929d0e0e4736",
        "but the operation is this service's own"
    );
    let op_seqs: Vec<u64> = events
        .iter()
        .map(|e| e["op_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(op_seqs, (1..=8).collect::<Vec<_>>());
    assert_eq!(events[7]["events_emitted"], 8);
    assert_eq!(events[2]["failure_class"], "transport");
    assert!(
        events[2].get("error_message").is_none(),
        "error text is off by default"
    );
    assert_eq!(events[3]["attempt"], 2);
    assert_eq!(
        events[3]["step_id"], events[1]["step_id"],
        "a retry keeps the step ID"
    );
    assert_eq!(events[5]["external_effect"], true);
    assert_eq!(events[6]["uncertain_reason"], "timeout_after_send");
    assert!(!serde_json::to_string(&events).unwrap().contains("cats"));
    let batch = &fake.batches.lock().unwrap()[0];
    assert!(
        batch["source"]["sdk"]
            .as_str()
            .unwrap()
            .starts_with("aiops-sdk-rust/")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_messages_are_opt_in_redacted_and_clipped() {
    let (endpoint, fake) = fake_collector().await;
    let mut cfg = config(&endpoint);
    cfg.include_error_messages = true;
    let client = Client::start(&cfg).unwrap();
    let op = client.operation("x").start();
    let long = format!("provider said Bearer abcdefgh12345678 {}", "z".repeat(600));
    op.step(StepKind::Model, "m")
        .start()
        .finish(Finish::failed(FailureClass::Rejected, "http_401").message(&long));
    client.shutdown(Duration::from_secs(2)).await;
    let events = all_events(&fake);
    let msg = events[2]["error_message"].as_str().unwrap();
    assert!(msg.len() <= 512);
    assert!(!msg.contains("abcdefgh12345678"));
    assert_eq!(events[2]["truncated"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drops_during_an_outage_are_reported_per_operation_when_the_collector_returns() {
    let (endpoint, fake) = fake_collector().await;
    fake.mode.store(FAIL_503, Ordering::SeqCst);
    let mut cfg = config(&endpoint);
    cfg.max_send_attempts = 2;
    cfg.retry_budget = Duration::from_millis(200);
    let client = Client::start(&cfg).unwrap();
    for n in 0..3 {
        run_operation(&client, n);
    }
    eventually(|| client.stats().dropped_send_failed == 12).await;
    fake.mode.store(ACCEPT, Ordering::SeqCst);
    run_operation(&client, 99);
    eventually(|| client.stats().gap_reports_sent == 3).await;
    let stats = client.shutdown(Duration::from_secs(2)).await;

    let events = all_events(&fake);
    let gaps: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "gap.reported")
        .collect();
    assert_eq!(gaps.len(), 3, "one report per operation that lost events");
    for g in &gaps {
        assert_eq!(g["dropped"], 4);
        assert_eq!(g["reason"], "send_failed");
        assert_eq!(g["op_seq"], 0);
    }
    assert_eq!(stats.accepted, 4 + 3);
    let batch = &fake.batches.lock().unwrap()[0];
    assert_eq!(
        batch["stats"]["dropped_send_failed"], 12,
        "cumulative counters travel with batches"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_response_is_re_sent_with_the_same_event_ids() {
    let (endpoint, fake) = fake_collector().await;
    fake.mode.store(STORE_THEN_503, Ordering::SeqCst);
    let client = Client::start(&config(&endpoint)).unwrap();
    run_operation(&client, 1);
    eventually(|| client.stats().batches_sent == 1).await;
    client.shutdown(Duration::from_secs(2)).await;
    let batches = fake.batches.lock().unwrap();
    assert_eq!(
        batches.len(),
        2,
        "stored once, then re-sent after the lost answer"
    );
    let ids = |b: &Value| -> Vec<String> {
        b["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["event_id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(&batches[0]), ids(&batches[1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_batch_is_dropped_counted_and_not_retried() {
    let (endpoint, fake) = fake_collector().await;
    fake.mode.store(REJECT_401, Ordering::SeqCst);
    let client = Client::start(&config(&endpoint)).unwrap();
    run_operation(&client, 1);
    eventually(|| client.stats().dropped_rejected == 4).await;
    assert_eq!(
        fake.requests.load(Ordering::SeqCst),
        1,
        "401 is never retried"
    );
    client.shutdown(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_respects_its_deadline_against_a_hanging_collector() {
    let (endpoint, fake) = fake_collector().await;
    fake.mode.store(HANG, Ordering::SeqCst);
    let mut cfg = config(&endpoint);
    cfg.request_timeout = Duration::from_secs(10);
    cfg.retry_budget = Duration::from_secs(30);
    let client = Client::start(&cfg).unwrap();
    run_operation(&client, 1);
    eventually(|| fake.requests.load(Ordering::SeqCst) == 1).await;
    let started = Instant::now();
    let s = client.shutdown(Duration::from_millis(300)).await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        s.dropped_shutdown, 4,
        "the in-flight batch is counted as lost: {s:?}"
    );
}

#[tokio::test]
async fn a_disabled_client_is_a_no_op_with_the_same_api() {
    let client = Client::disabled();
    run_operation(&client, 1);
    assert_eq!(client.stats().emitted, 0);
    assert!(client.operation("x").start().id().is_none());
    client.shutdown(Duration::from_millis(10)).await;
}

#[test]
fn start_outside_a_runtime_is_an_error_not_a_panic() {
    let err = Client::start(&Config::new("http://127.0.0.1:1", KEY, "x")).unwrap_err();
    assert!(matches!(err, aiops_sdk::SdkError::NoRuntime));
}

#[tokio::test]
async fn bad_configuration_is_refused() {
    assert!(Client::start(&Config::new("not a url", KEY, "x")).is_err());
    assert!(Client::start(&Config::new("http://127.0.0.1:1", "sk-wrong", "x")).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_continued_operation_contributes_steps_but_never_its_own_start_or_finish() {
    let (endpoint, fake) = fake_collector().await;
    let client = Client::start(&config(&endpoint)).unwrap();
    let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let op = client
        .continue_operation(header)
        .expect("a valid traceparent");
    op.step(StepKind::Tool, "render")
        .start()
        .finish(Finish::succeeded());
    op.finish(Finish::uncertain(UncertainReason::TimeoutAfterSend, "x")); // a no-op here
    assert!(client.continue_operation("garbage").is_none());
    client.shutdown(Duration::from_secs(2)).await;
    let events = all_events(&fake);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        ["step.started", "step.finished"],
        "only the owning service declares the outcome"
    );
    assert!(
        events
            .iter()
            .all(|e| e["operation_id"] == "4bf92f3577b34da6a3ce929d0e0e4736")
    );
    assert_eq!(
        events[0]["parent_step_id"], "00f067aa0ba902b7",
        "steps hang under the caller's span"
    );
}

// Review finding 6: batches must fit the collector's body cap; a 413 must not lose a batch.
const ALLOWED: [&str; 15] = [
    "business_key",
    "cache.hit",
    "content.kind",
    "content.length",
    "error.type",
    "http.method",
    "http.route",
    "http.status_code",
    "provider",
    "provider.region",
    "queue.name",
    "retry.reason",
    "sampled",
    "tool.name",
    "validation.rule",
];

fn big_operation(client: &Client, n: usize) {
    let mut b = client
        .operation("publish_article")
        .request_id(&format!("big-{n}"));
    for k in ALLOWED {
        b = b.attribute(k, "v".repeat(250).as_str());
    }
    let op = b.start();
    op.succeed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_events_are_batched_under_the_collectors_body_cap() {
    let (endpoint, fake) = fake_collector_with_limit(1024 * 1024).await;
    let client = Client::start(&config(&endpoint)).unwrap();
    for n in 0..400 {
        big_operation(&client, n);
    }
    let s = client.shutdown(Duration::from_secs(10)).await;
    assert_eq!(s.emitted, 800);
    assert_eq!(s.dropped(), 0, "{s:?}");
    assert_eq!(s.accepted, 800, "{s:?}");
    let _ = fake;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_413_splits_the_batch_and_resends_instead_of_dropping_it() {
    // The collector's cap is far below the SDK's batch size.
    let (endpoint, fake) = fake_collector_with_limit(64 * 1024).await;
    let client = Client::start(&config(&endpoint)).unwrap();
    for n in 0..100 {
        big_operation(&client, n);
    }
    let s = client.shutdown(Duration::from_secs(10)).await;
    assert_eq!(s.dropped(), 0, "{s:?}");
    assert_eq!(s.accepted, 200, "{s:?}");
    // ~450 KB of events cannot pass a 64 KiB cap in fewer than 8 accepted batches: the
    // SDK's 900 KiB batch was refused 413 and split until each half fitted.
    assert!(s.batches_sent >= 8, "{s:?}");
    assert_eq!(s.batches_failed, 0, "{s:?}");
    assert_eq!(fake.batches.lock().unwrap().len() as u64, s.batches_sent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_over_8_kib_is_cut_in_the_sdk_and_marked() {
    let (endpoint, fake) = fake_collector().await;
    let mut cfg = config(&endpoint);
    cfg.opt_in_attributes = (0..32).map(|i| format!("custom.k{i:02}")).collect();
    let client = Client::start(&cfg).unwrap();
    let mut b = client.operation("x");
    for i in 0..32 {
        b = b.attribute(&format!("custom.k{i:02}"), "w".repeat(256).as_str());
    }
    b.start().succeed();
    client.shutdown(Duration::from_secs(5)).await;
    let batches = fake.batches.lock().unwrap();
    let started = batches
        .iter()
        .flat_map(|b| b["events"].as_array().cloned().unwrap_or_default())
        .find(|e| e["type"] == "operation.started")
        .unwrap();
    assert!(
        started.to_string().len() <= 8 * 1024,
        "{} bytes",
        started.to_string().len()
    );
    assert!(
        started["truncated"].as_u64().unwrap() >= 1,
        "the cut is marked"
    );
}

// Review finding 13: an event the collector refuses is reported as a gap, with its reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_the_collector_refuses_is_reported_as_a_rejected_gap() {
    let (endpoint, fake) = fake_collector().await;
    fake.mode.store(REFUSE_SECOND, Ordering::SeqCst);
    let client = Client::start(&config(&endpoint)).unwrap();
    run_operation(&client, 1);
    eventually(|| client.stats().refused == 1).await;
    run_operation(&client, 2); // a later batch carries the report
    eventually(|| client.stats().gap_reports_sent >= 1).await;
    client.shutdown(Duration::from_secs(2)).await;
    let events = all_events(&fake);
    let refused_op = events[1]["operation_id"].clone();
    let gap = events
        .iter()
        .find(|e| e["type"] == "gap.reported")
        .expect("a gap report for the refused event");
    assert_eq!(gap["reason"], "rejected");
    assert_eq!(gap["dropped"], 1);
    assert_eq!(gap["operation_id"], refused_op);
}
