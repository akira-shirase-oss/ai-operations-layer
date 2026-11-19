//! The control surface in-process: scope and adapter gating, reconcile against a stub
//! authority, retry decisions, replay identity, the executor's submit-and-observe drive
//! (re-drive on crash included), the dedicated control rate bucket, and the control
//! pool's isolation from a flooded query path. A3, rulings D-029..D-033.
//!
//! The stub reference host doubles as the authority-double the fixture's idempotent
//! publisher promises: the same business key can be submitted repeatedly and still
//! record one effect.
#![expect(
    clippy::unwrap_used,
    reason = "in-process tests: a failed call is a test failure"
)]

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aiops_server::auth::{TOKEN_PREFIX, generate, hash};
use aiops_server::control::{CONTROL_BURST, ControlAdapter, ReferenceHost, drive_resumable};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{
    Env, ScratchDb, batch, env_with, env_with_store, get as get_req, journey, now, op_hex,
    post_ingest, post_json, send,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// The stub authority: a publication registry keyed by business key. `submits` counts
// every submit call (including deduplicated retries); the registry itself only ever
// holds one effect per key — the fixture's idempotent-publish contract.
// ---------------------------------------------------------------------------

struct StubHost {
    publications: Mutex<HashMap<String, u64>>,
    submits: AtomicU64,
    /// `POST /jobs` submissions: the new `op_key` → the business key decoded out of
    /// the job input — the Durable Runner resubmit's record (D-031).
    jobs: Mutex<Vec<(String, String)>>,
}

impl StubHost {
    /// Submit calls that reached the authority for `key` — attempts, not effects.
    fn attempts(&self, key: &str) -> u64 {
        self.publications
            .lock()
            .unwrap()
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Distinct effects recorded: always one per business key, however often a
    /// resubmission landed (the fixture's idempotent-publish contract).
    fn effects(&self) -> usize {
        self.publications.lock().unwrap().len()
    }

    fn mark_published(&self, key: &str) {
        self.publications.lock().unwrap().insert(key.into(), 1);
    }

    /// Distinct resubmitted jobs — each a NEW `op_key` carrying a business key.
    fn job_submits(&self) -> Vec<(String, String)> {
        self.jobs.lock().unwrap().clone()
    }
}

async fn stub_publication(
    State(h): State<Arc<StubHost>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let n = h.attempts(&key);
    if n > 0 {
        Json(json!({"business_key": key, "times_published": 1, "times_attempted": n}))
            .into_response()
    } else {
        (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"published": false})),
        )
            .into_response()
    }
}

async fn stub_retry(State(h): State<Arc<StubHost>>, Json(body): Json<Value>) -> impl IntoResponse {
    let key = body["business_key"].as_str().unwrap_or("").to_owned();
    h.submits.fetch_add(1, Ordering::SeqCst);
    h.publications
        .lock()
        .unwrap()
        .entry(key.clone())
        .and_modify(|n| *n += 1)
        .or_insert(1);
    (
        axum::http::StatusCode::CREATED,
        Json(json!({"operation_id": "stub-op-new", "business_key": key})),
    )
        .into_response()
}

/// The stub Durable Runner: accepts a job, pulls the business key out of the job
/// input (`input_b64`), records the effect under it — the worker's idempotent
/// publish — and answers `run_id`. A new `op_key` each call; dedup is by key.
async fn stub_job_submit(
    State(h): State<Arc<StubHost>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let op_key = body["op_key"].as_str().unwrap_or("").to_owned();
    let b64 = body["spec"]["input_b64"].as_str().unwrap_or("");
    let input: Value = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    let key = input["business_key"].as_str().unwrap_or("").to_owned();
    h.submits.fetch_add(1, Ordering::SeqCst);
    h.jobs.lock().unwrap().push((op_key, key.clone()));
    if !key.is_empty() {
        h.publications
            .lock()
            .unwrap()
            .entry(key)
            .and_modify(|n| *n += 1)
            .or_insert(1);
    }
    let n = h.submits.load(Ordering::SeqCst);
    (
        axum::http::StatusCode::CREATED,
        Json(json!({"run_id": format!("run-{n}")})),
    )
        .into_response()
}

/// The stub reference host on an ephemeral port, on the test's own runtime.
async fn stub_host() -> (String, Arc<StubHost>) {
    let host = Arc::new(StubHost {
        publications: Mutex::new(HashMap::new()),
        submits: AtomicU64::new(0),
        jobs: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/publications/{*key}", get(stub_publication))
        .route("/articles/retry", post(stub_retry))
        .route("/jobs", post(stub_job_submit))
        .with_state(host.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, host)
}

/// An env whose `pilot` project is controlled by the adapter pointed at `url`.
async fn env_with_adapter(pool: PgPool, url: &str) -> Env {
    let adapter = ControlAdapter::ReferenceHost(ReferenceHost::new(url));
    env_with(pool, move |s| {
        s.adapters.insert("pilot".into(), adapter);
    })
    .await
}

/// Ingest one operation (`journey(1)`) and return the control path prefix for it.
async fn op_path(e: &Env, suffix: &str) -> String {
    send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    format!("/v1/projects/pilot/operations/{}/{suffix}", op_hex(1))
}

fn body(key: &str, expected: &Value) -> Value {
    json!({"business_key": key, "expected_state": expected})
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn controls_require_operate_scope_and_a_configured_adapter(pool: PgPool) {
    let (url, _host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "reconcile").await;
    let req_body = body("article:req-x", &json!({"published": false}));

    // No token: rejected before any work.
    let r = send(
        &e.app,
        Request::post(&path)
            .header("content-type", "application/json")
            .body(Body::from(req_body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 401);
    // A reader without `operate`: denied and audited.
    let r = send(&e.app, post_json(&path, &e.reader, &req_body)).await;
    assert_eq!(r.status, 403);
    // An unknown operation id gets the same answer as one outside the token's view:
    // a denied control call, never a hint the op exists elsewhere.
    let missing = format!(
        "/v1/projects/pilot/operations/{}/reconcile",
        Uuid::new_v4().simple()
    );
    let r = send(&e.app, post_json(&missing, &e.no_read, &req_body)).await;
    assert_eq!(r.status, 403, "{:?}", r.body);

    // A project with no configured adapter denies by shape, not by guess: mint an
    // `operate` token on `other`, which has no adapter entry.
    let other_op = op_hex(2);
    send(
        &e.app,
        post_ingest(&e.other_ingest_key, batch(&journey(2, 0))),
    )
    .await;
    let t = generate(TOKEN_PREFIX).unwrap();
    let later = now().plus_micros(3_600_000_000);
    e.store
        .create_token(
            "eve-op",
            &["operate".into()],
            &["other".into()],
            &hash(&t),
            later,
        )
        .await
        .unwrap();
    let other = format!("/v1/projects/other/operations/{other_op}/reconcile");
    let r = send(&e.app, post_json(&other, &t, &req_body)).await;
    assert_eq!(r.status, 403, "{:?}", r.body);
    assert_eq!(r.body["error"]["code"], "forbidden");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn reconcile_reads_the_authority_and_stores_its_answer(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "reconcile").await;

    // Absent at the authority.
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:none", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "reconciled");
    assert_eq!(r.body["resolution"], "effect_absent");
    assert_eq!(r.body["authority_read"]["status"], 404);

    // Present at the authority.
    host.mark_published("article:seen");
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:seen", &json!({"published": true})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["resolution"], "effect_present");

    // Every call was audited.
    let audit = e.store.recent_audit(20).await.unwrap();
    let n = audit
        .iter()
        .filter(|r| r.action == "control.reconcile")
        .count();
    assert_eq!(n, 2, "{audit:?}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn reconcile_replays_and_conflicts_on_a_changed_intent(pool: PgPool) {
    let (url, _host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "reconcile").await;
    let req_body = body("article:req", &json!({"published": false}));

    let first = send(&e.app, post_json(&path, &e.no_read, &req_body)).await;
    assert_eq!(first.status, 200);
    let second = send(&e.app, post_json(&path, &e.no_read, &req_body)).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.body["control_id"], first.body["control_id"]);
    assert_eq!(second.body["replayed"], true);

    // Same replay identity, a different intent: a conflict, audited.
    let changed = body("article:req", &json!({"published": true}));
    let r = send(&e.app, post_json(&path, &e.no_read, &changed)).await;
    assert_eq!(r.status, 409, "{:?}", r.body);
    assert_eq!(r.body["error"]["code"], "conflicting_request");
    let audit = e.store.recent_audit(20).await.unwrap();
    assert!(
        audit
            .iter()
            .any(|r| r.action == "control.reconcile" && r.decision == "denied")
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn retry_denies_when_the_effect_already_happened(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "retry").await;
    host.mark_published("article:done");
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:done", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "denied");
    assert_eq!(r.body["denied_reason"], "effect_confirmed");
    assert_eq!(
        host.submits.load(Ordering::SeqCst),
        0,
        "a denied retry never submits"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn retry_denies_on_expected_state_mismatch_and_unreachable_authority(pool: PgPool) {
    let (url, _host) = stub_host().await;
    // One env, two adapters: `pilot` is served by the live stub, `other` points at an
    // authority that never answers.
    let live = ControlAdapter::ReferenceHost(ReferenceHost::new(&url));
    let dead = ControlAdapter::ReferenceHost(ReferenceHost::new("http://127.0.0.1:1"));
    let e = env_with(pool, move |s| {
        s.adapters.insert("pilot".into(), live);
        s.adapters.insert("other".into(), dead);
    })
    .await;
    let path = op_path(&e, "retry").await;

    // The caller expected publication but the authority says absent.
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:none", &json!({"published": true})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "denied");
    assert_eq!(r.body["denied_reason"], "expected_state_mismatch");

    // An authority that never answers: denied, never retried blind. The call needs
    // `operate` on `other`, minted like the adapter-denial test's.
    let t = generate(TOKEN_PREFIX).unwrap();
    let later = now().plus_micros(3_600_000_000);
    e.store
        .create_token(
            "eve-op",
            &["operate".into()],
            &["other".into()],
            &hash(&t),
            later,
        )
        .await
        .unwrap();
    send(
        &e.app,
        post_ingest(&e.other_ingest_key, batch(&journey(2, 0))),
    )
    .await;
    let dead_path = format!("/v1/projects/other/operations/{}/retry", op_hex(2));
    let r = send(
        &e.app,
        post_json(
            &dead_path,
            &t,
            &body("article:none", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "denied");
    assert_eq!(r.body["denied_reason"], "authority_unavailable");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn retry_authorizes_then_the_executor_submits_and_confirms(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "retry").await;
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:new", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "authorized", "{:?}", r.body);
    assert_eq!(
        host.submits.load(Ordering::SeqCst),
        0,
        "the commit precedes the submit"
    );

    // The executor's queue: the submit lands (the intermediate `submitted` commit is
    // inside the pass), then the bounded observe sees the effect — `confirmed`.
    drive_resumable(&e.state).await;
    let id = Uuid::parse_str(r.body["control_id"].as_str().unwrap()).unwrap();
    let rec = e.store.control_get(id).await.unwrap().unwrap();
    assert_eq!(rec.status, "confirmed");
    assert_eq!(rec.submitted_ref.as_deref(), Some("stub-op-new"));
    assert_eq!(host.submits.load(Ordering::SeqCst), 1);
    assert_eq!(host.effects(), 1);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_repeated_retry_is_one_submit_and_one_effect(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "retry").await;
    let req_body = body("article:dup", &json!({"published": false}));

    // Concurrent duplicates: one record, one authorization, one submit.
    let (a, b) = tokio::join!(
        send(&e.app, post_json(&path, &e.no_read, &req_body)),
        send(&e.app, post_json(&path, &e.no_read, &req_body)),
    );
    assert_eq!((a.status, b.status), (200, 200));
    assert_eq!(a.body["control_id"], b.body["control_id"]);
    drive_resumable(&e.state).await;
    assert_eq!(host.submits.load(Ordering::SeqCst), 1);
    assert_eq!(host.effects(), 1);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_crash_before_submit_redrives_exactly_once(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "retry").await;
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:crash", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["status"], "authorized");
    let id = Uuid::parse_str(r.body["control_id"].as_str().unwrap()).unwrap();

    // Crash between the commit and the submit: the record is authorized, the world
    // untouched. Boot's resumable scan picks it up — one submit, confirmed.
    drive_resumable(&e.state).await;
    assert_eq!(host.submits.load(Ordering::SeqCst), 1);
    let rec = e.store.control_get(id).await.unwrap().unwrap();
    assert_eq!(rec.status, "confirmed");

    // A second crash where the submit landed but the confirmation never committed —
    // the record is stranded `submitted`. The re-drive submits again; the authority
    // deduplicates the business key: two attempts, still one effect.
    let patch = aiops_store::ControlPatch {
        status: Some("submitted"),
        ..aiops_store::ControlPatch::default()
    };
    assert!(
        e.store
            .control_update(id, Some("confirmed"), &patch)
            .await
            .unwrap()
    );
    drive_resumable(&e.state).await;
    assert_eq!(host.submits.load(Ordering::SeqCst), 2);
    assert_eq!(host.effects(), 1, "the authority saw one effect");
    let rec = e.store.control_get(id).await.unwrap().unwrap();
    assert_eq!(rec.status, "confirmed");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn controls_have_their_own_rate_bucket(pool: PgPool) {
    let (url, _host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "reconcile").await;
    let req_body = body("article:none", &json!({"published": false}));
    let mut limited = 0;
    for _ in 0..CONTROL_BURST + 8 {
        let r = send(&e.app, post_json(&path, &e.no_read, &req_body)).await;
        if r.status == 429 {
            limited += 1;
        }
    }
    assert!(limited > 0, "the control bucket must bound the caller");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_flooded_query_path_does_not_starve_controls(pool: PgPool) {
    let _ = pool; // a real-pool store instead: sqlx::test's fixture pool is one DB too,
    // but the multi-pool Store needs its own connection URLs.
    let db = ScratchDb::new().await;
    let mut cfg = aiops_store::StoreConfig::new(&db.url);
    cfg.query_connections = 1;
    let store = aiops_store::Store::connect(&cfg).await.unwrap();
    let (url, _host) = stub_host().await;
    let adapter = ControlAdapter::ReferenceHost(ReferenceHost::new(&url));
    let e = env_with_store(store, move |s| {
        s.adapters.insert("pilot".into(), adapter);
    })
    .await;
    let path = op_path(&e, "reconcile").await;

    // Saturate the query pool: hold `events`, then a timeline read waits on the one
    // connection — queued query calls pile up behind it.
    let mut locker = db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE events IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *locker)
        .await
        .unwrap();
    let flooded = e.app.clone();
    let reader = e.reader.clone();
    let timeline = format!("/v1/projects/pilot/operations/{}", op_hex(1));
    let stuck = tokio::spawn(async move { send(&flooded, get_req(&timeline, &reader)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The control call still answers promptly on its own pool.
    let started = std::time::Instant::now();
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:x", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "control waited {:?} on a flooded query path",
        started.elapsed()
    );

    drop(locker);
    let _ = stuck.await.unwrap();
    db.drop().await;
}

// ---------------------------------------------------------------------------
// D-031: the Durable Runner adapter — a retry is a NEW job under a NEW `op_key`,
// the business key in its input; reconcile reads the effect's authority.
// ---------------------------------------------------------------------------

/// An env whose `pilot` project is controlled by the Durable Runner adapter pointed
/// at the stub: `runner` takes `POST /jobs`, `effect` answers `/publications/{key}`.
async fn env_with_runner(pool: PgPool, runner: &str, effect: &str) -> Env {
    let adapter = ControlAdapter::DurableRunner(aiops_server::control::DurableRunner::new(
        runner,
        effect,
        &"k".repeat(32),
    ));
    env_with(pool, move |s| {
        s.adapters.insert("pilot".into(), adapter);
    })
    .await
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_dr_retry_submits_a_new_job_with_the_key_in_its_input(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_runner(pool, &url, &url).await;
    let path = op_path(&e, "retry").await;
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article/req-dr", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["status"], "authorized", "{:?}", r.body);

    drive_resumable(&e.state).await;
    let jobs = host.job_submits();
    assert_eq!(jobs.len(), 1, "one resubmitted job");
    let (op_key, input_key) = &jobs[0];
    // D-031: a NEW op_key — not the business key — and the business key travels in
    // the job input. The record's submitted_ref is that op_key: the mapping kept.
    assert_ne!(op_key, "article/req-dr");
    assert!(op_key.starts_with("article/req-dr-r"), "{op_key}");
    assert_eq!(input_key, "article/req-dr");

    let id = Uuid::parse_str(r.body["control_id"].as_str().unwrap()).unwrap();
    let rec = e.store.control_get(id).await.unwrap().unwrap();
    assert_eq!(rec.status, "confirmed");
    assert_eq!(rec.submitted_ref.as_deref(), Some(op_key.as_str()));
    assert_eq!(host.effects(), 1);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_dr_retry_resubmitted_is_still_one_effect(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_runner(pool, &url, &url).await;
    let path = op_path(&e, "retry").await;
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article/lost", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.body["status"], "authorized");
    let id = Uuid::parse_str(r.body["control_id"].as_str().unwrap()).unwrap();

    // Drive, then strand the record at `submitted` (a crash between the submit and
    // the observe) and drive again: a second job lands — two RUNS, but the effect
    // authority dedups the business key. Exactly one effect (D-031's test).
    drive_resumable(&e.state).await;
    let patch = aiops_store::ControlPatch {
        status: Some("submitted"),
        ..aiops_store::ControlPatch::default()
    };
    assert!(
        e.store
            .control_update(id, Some("confirmed"), &patch)
            .await
            .unwrap()
    );
    drive_resumable(&e.state).await;
    let jobs = host.job_submits();
    assert_eq!(jobs.len(), 2, "each re-drive is a fresh job/op_key");
    assert_ne!(jobs[0].0, jobs[1].0, "distinct op_keys");
    assert_eq!(host.attempts("article/lost"), 2);
    assert_eq!(host.effects(), 1, "the authority saw one effect");
    let rec = e.store.control_get(id).await.unwrap().unwrap();
    assert_eq!(rec.status, "confirmed");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_dr_reconcile_reads_the_effect_authority_not_the_runner(pool: PgPool) {
    let (runner_url, runner_host) = stub_host().await;
    let (effect_url, effect_host) = stub_host().await;
    effect_host.mark_published("article/done");
    let e = env_with_runner(pool, &runner_url, &effect_url).await;
    let path = op_path(&e, "reconcile").await;
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article/done", &json!({"published": true})),
        ),
    )
    .await;
    assert_eq!(r.body["status"], "reconciled", "{:?}", r.body);
    assert_eq!(r.body["resolution"], "effect_present");
    // The effect authority answered; the runner itself saw no submit and no read.
    assert_eq!(runner_host.submits.load(Ordering::SeqCst), 0);
}
