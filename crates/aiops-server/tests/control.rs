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

use aiops_server::auth::{CredentialCache, TOKEN_PREFIX, generate, hash};
use aiops_server::control::{
    CONTROL_BURST, CONTROL_RATE, ControlAdapter, ReferenceHost, drive_resumable,
};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{
    Env, ScratchDb, audit_rows, batch, env_with, env_with_store, get as get_req, journey, now,
    op_hex, post_ingest, post_json, send,
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
    /// Every request path the stub saw — a business key that escaped
    /// `/publications/` shows up here.
    seen_paths: Mutex<Vec<String>>,
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
    match key.as_str() {
        // Headers land at once; the body arrives a KiB per 200 ms — a host that
        // answers but never finishes (white-hat S2-3).
        "article:slow" => {
            let (tx, rx) =
                tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(8);
            tokio::spawn(async move {
                for _ in 0..80 {
                    if tx
                        .send(Ok(axum::body::Bytes::from(vec![b'x'; 1024])))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
            return Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
                .into_response();
        }
        // A single body past the read cap (white-hat S2-3).
        "article:big" => {
            return Json(json!({"business_key": key, "blob": "x".repeat(100 * 1024)}))
                .into_response();
        }
        _ => {}
    }
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
    // The runner's own idempotence is the `op_key` (D-031): a re-POST under a
    // key it already accepted is acknowledged, not run again — no second job.
    let fresh = !h.jobs.lock().unwrap().iter().any(|(k, _)| k == &op_key);
    h.jobs.lock().unwrap().push((op_key, key.clone()));
    if !fresh {
        return (
            axum::http::StatusCode::OK,
            Json(json!({"run_id": "run-dup"})),
        )
            .into_response();
    }
    let slow = key == "article/slowjob";
    let hang = key == "article/hangwrite";
    if !key.is_empty() {
        h.publications
            .lock()
            .unwrap()
            .entry(key)
            .and_modify(|n| *n += 1)
            .or_insert(1);
    }
    let n = h.submits.load(Ordering::SeqCst);
    // The job was recorded above, then nothing: the write landed and the
    // answer never comes — the caller can only classify this Transient
    // (white-hat N5: the next drive must reuse the persisted `op_key`, not
    // mint a second job).
    if hang {
        std::future::pending::<()>().await;
    }
    // The runner accepted and recorded the job — its 201 headers land at once
    // but the body trickles and never finishes (white-hat N5: a slow 2xx is a
    // submit, not a transient).
    if slow {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(8);
        tokio::spawn(async move {
            loop {
                if tx
                    .send(Ok(axum::body::Bytes::from(vec![b'x'; 1024])))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        return (
            axum::http::StatusCode::CREATED,
            Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
        )
            .into_response();
    }
    (
        axum::http::StatusCode::CREATED,
        Json(json!({"run_id": format!("run-{n}")})),
    )
        .into_response()
}

async fn stub_fallback(State(h): State<Arc<StubHost>>, req: Request<Body>) -> impl IntoResponse {
    h.seen_paths
        .lock()
        .unwrap()
        .push(req.uri().path().to_owned());
    axum::http::StatusCode::NOT_FOUND
}

/// The stub reference host on an ephemeral port, on the test's own runtime.
async fn stub_host() -> (String, Arc<StubHost>) {
    let host = Arc::new(StubHost {
        publications: Mutex::new(HashMap::new()),
        submits: AtomicU64::new(0),
        jobs: Mutex::new(Vec::new()),
        seen_paths: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/publications/{*key}", get(stub_publication))
        .route("/articles/retry", post(stub_retry))
        .route("/jobs", post(stub_job_submit))
        .fallback(stub_fallback)
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

/// Every malformed-input arm answers 400 before any authority call: the operation id's
/// hex, the control id's uuid, the business key's shape and size, `expected_state`'s
/// shape and size — none of them reaches the adapter.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn malformed_control_requests_are_400(pool: PgPool) {
    let (url, _host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let op = op_path(&e, "reconcile").await;
    let op_id = op_hex(1);
    let good = body("article:req-x", &json!({"published": false}));

    for (what, path, b) in [
        (
            "non-hex op id",
            "/v1/projects/pilot/operations/nothex/reconcile".into(),
            good.clone(),
        ),
        (
            "key without prefix",
            op.clone(),
            body("req-x", &json!({"published": false})),
        ),
        (
            "key too long",
            op.clone(),
            body(
                &format!("article:{}", "k".repeat(300)),
                &json!({"published": false}),
            ),
        ),
        (
            "expected wrong shape",
            op.clone(),
            body("article:req-x", &json!({"published": "yes"})),
        ),
        (
            "expected too big",
            op.clone(),
            body(
                "article:req-x",
                &json!({"published": false, "pad": "p".repeat(5000)}),
            ),
        ),
    ] {
        let r = send(&e.app, post_json(&path, &e.no_read, &b)).await;
        assert_eq!(r.status, 400, "{what}: {:?}", r.body);
    }
    // The read paths validate their ids the same way.
    for path in [
        "/v1/projects/pilot/operations/nothex/controls".to_string(),
        format!("/v1/projects/pilot/operations/{op_id}/controls/not-a-uuid"),
    ] {
        let r = send(
            &e.app,
            Request::get(&path)
                .header("authorization", format!("Bearer {}", e.reader))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status, 400, "{path}: {:?}", r.body);
    }
}

/// White-hat S2-2: the key is `format!`-ed into `GET {host}/publications/{key}` —
/// a `..`, `?`, `#` or `/` in it escapes the publications path (reqwest normalizes
/// `..` before sending), and a normalized-away tail reads a DIFFERENT literal key's
/// answer, defeating reconcile-before-rerun. The id must be one URL segment.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_business_key_cannot_escape_the_publications_path(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "reconcile").await;

    for key in [
        "article:req-7/../../admin",
        "article:req-7/x/..",
        "article:..",
        "article:.",
        "article:req-7/../req-8",
        "article:req-7?debug=true",
        "article:req-7#frag",
        "article:req-7@evil",
        "article:req 7",
    ] {
        let r = send(
            &e.app,
            post_json(&path, &e.no_read, &body(key, &json!({"published": false}))),
        )
        .await;
        assert_eq!(r.status, 400, "{key}: {:?}", r.body);
    }
    assert!(
        host.seen_paths.lock().unwrap().is_empty(),
        "no forged key may reach the authority: {:?}",
        host.seen_paths.lock().unwrap()
    );
    assert!(
        host.publications.lock().unwrap().is_empty() && host.submits.load(Ordering::SeqCst) == 0,
        "no forged key may read or write an effect"
    );
    // The honest keys still reach the authority.
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:req-7.x_y-2", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
}

/// White-hat S2-3: an authority answer's body is size-bounded inside the read
/// deadline — a host that answers but trickles its body cannot hold a
/// control-gate slot open, and an oversized answer is not stored.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_authority_read_is_bounded_in_time_and_size(pool: PgPool) {
    let (url, _host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "reconcile").await;

    // 80 KiB at a KiB per 200 ms takes ~16 s to finish: the reconcile must end at
    // the 2 s read deadline instead of awaiting the body.
    let started = std::time::Instant::now();
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        send(
            &e.app,
            post_json(
                &path,
                &e.no_read,
                &body("article:slow", &json!({"published": false})),
            ),
        ),
    )
    .await
    .expect("a trickling body is cut at the deadline, not awaited");
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the read was cut at its deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(
        r.body["authority_read"]["answer"],
        Value::Null,
        "no unfinished body is stored: {:?}",
        r.body
    );

    // A body past the 64 KiB cap is not stored either.
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article:big", &json!({"published": true})),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(
        r.body["authority_read"]["answer"],
        Value::Null,
        "an oversized answer is not stored: {:?}",
        r.body
    );
}

/// `GET …/controls` pages newest-first through a `(created_at, control_id)` keyset:
/// a `next` pair on a full page, a capped page size, and a cursor that rows inserted
/// mid-walk can only move past, never duplicate (the audit page's shape).
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn controls_pages_follow_a_stable_keyset(pool: PgPool) {
    let (url, _host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    let op = op_hex(1);
    let project = e.store.project_by_slug("pilot").await.unwrap().unwrap();
    let op_uuid = Uuid::from_bytes(*aiops_domain::OperationId::parse(&op).unwrap().as_bytes());
    // 120 records, every (action, key) pair unique; reconcile rows on distinct keys.
    let expected = json!({"published": false});
    for i in 0..120 {
        e.store
            .control_insert(&aiops_store::NewControlRecord {
                project_id: project.id,
                operation_id: op_uuid,
                action: "reconcile",
                business_key: &format!("article:key-{i:03}"),
                expected_state: &expected,
                actor: "ana",
                token_id: Uuid::new_v4(),
                initial_status: "reconciled",
            })
            .await
            .unwrap();
    }
    let list = |q: &str| {
        get_req(
            &format!("/v1/projects/pilot/operations/{op}/controls{q}"),
            &e.reader,
        )
    };

    // Page 1: the cap applies and `next` is the [before_time, before_id] pair.
    let r = send(&e.app, list("?limit=50")).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let page1 = r.body["controls"].as_array().unwrap();
    assert_eq!(page1.len(), 50);
    let next = r.body["next"].as_array().unwrap();
    let (t, id) = (next[0].as_str().unwrap(), next[1].as_str().unwrap());

    // A row inserted mid-walk can only fall ahead of the cursor.
    for i in 120..123 {
        e.store
            .control_insert(&aiops_store::NewControlRecord {
                project_id: project.id,
                operation_id: op_uuid,
                action: "reconcile",
                business_key: &format!("article:key-{i:03}"),
                expected_state: &expected,
                actor: "ana",
                token_id: Uuid::new_v4(),
                initial_status: "reconciled",
            })
            .await
            .unwrap();
    }

    let mut seen: std::collections::HashSet<String> = page1
        .iter()
        .map(|c| c["control_id"].as_str().unwrap().to_owned())
        .collect();
    let mut cursor = format!("?limit=50&before_time={t}&before_id={id}");
    let mut pages = 1;
    loop {
        let r = send(&e.app, list(&cursor)).await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        let rows = r.body["controls"].as_array().unwrap();
        for c in rows {
            assert!(
                seen.insert(c["control_id"].as_str().unwrap().to_owned()),
                "a row repeated"
            );
        }
        match r.body["next"].as_array() {
            Some(n) => {
                cursor = format!(
                    "?limit=50&before_time={}&before_id={}",
                    n[0].as_str().unwrap(),
                    n[1].as_str().unwrap()
                );
                pages += 1;
            }
            None => break,
        }
        assert!(pages < 10, "the walk did not terminate");
    }
    assert_eq!(
        seen.len(),
        120,
        "the three mid-walk inserts stay ahead of the cursor"
    );

    // The cap bounds the page size; a half cursor is refused.
    let r = send(&e.app, list("?limit=200")).await;
    assert!(r.body["controls"].as_array().unwrap().len() <= 100);
    let r = send(&e.app, list("?before_time=2026-11-19T00:00:00Z")).await;
    assert_eq!(r.status, 400);
}

/// A store that cannot write the audit row must refuse the action, never perform it
/// unaudited (the `audit_unavailable`/`store_unavailable` 503s): the reconcile's
/// decision commit rolls back, the denial refuses the same way, and nothing but the
/// still-resumable intent record survives.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_failing_audit_store_refuses_the_action(pool: PgPool) {
    let (url, _host) = stub_host().await;
    let e = env_with_adapter(pool, &url).await;
    let path = op_path(&e, "reconcile").await;
    let req_body = body("article:req-x", &json!({"published": false}));

    e.store.fail_audit_writes(true);
    // The decision and its audit row commit in one transaction: a failed insert rolls
    // the transition back — 503, and the intent row stays resumable, never decided.
    let r = send(&e.app, post_json(&path, &e.no_read, &req_body)).await;
    assert_eq!(r.status, 503, "{:?}", r.body);
    // A denied call is denied only because it is audited: the same refusal, no row.
    let r = send(&e.app, post_json(&path, &e.reader, &req_body)).await;
    assert_eq!(r.status, 503, "{:?}", r.body);
    assert!(audit_rows(&e.store, 20).await.is_empty());
    let op = op_hex(1);
    let project = e.store.project_by_slug("pilot").await.unwrap().unwrap();
    let op_uuid = Uuid::from_bytes(*aiops_domain::OperationId::parse(&op).unwrap().as_bytes());
    let stranded = e
        .store
        .control_list(project.id, op_uuid, None, 10)
        .await
        .unwrap();
    assert_eq!(
        stranded.len(),
        1,
        "the committed intent is the crash-window record"
    );
    assert_eq!(stranded[0].status, "reconciling");

    // Healed, the boot re-drive finishes the reconcile and audits it — the designed
    // recovery for exactly this window.
    e.store.fail_audit_writes(false);
    assert_eq!(drive_resumable(&e.state).await, 1);
    let done = e
        .store
        .control_list(project.id, op_uuid, None, 10)
        .await
        .unwrap();
    assert_eq!(done[0].status, "reconciled");
    assert_eq!(audit_rows(&e.store, 20).await.len(), 1);
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
    let audit = audit_rows(&e.store, 20).await;
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
    let audit = audit_rows(&e.store, 20).await;
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
    // The bucket's refill is the test's own clock: frozen, a burst is exactly
    // CONTROL_BURST — a loaded machine cannot let a token trickle back between
    // two sequential calls (FLAKE = BUG: `limited > 0` raced the wall clock).
    let clock = common::TestClock::new();
    let installed = clock.clone();
    let adapter = ControlAdapter::ReferenceHost(ReferenceHost::new(&url));
    let e = env_with(pool, move |s| {
        s.adapters.insert("pilot".into(), adapter);
        installed.install(s);
    })
    .await;
    let path = op_path(&e, "reconcile").await;
    let req_body = body("article:none", &json!({"published": false}));
    let mut statuses = Vec::new();
    for _ in 0..CONTROL_BURST + 8 {
        let r = send(&e.app, post_json(&path, &e.no_read, &req_body)).await;
        statuses.push(r.status);
    }
    let limited = statuses.iter().filter(|s| **s == 429).count();
    assert_eq!(
        (statuses.iter().filter(|s| **s == 200).count(), limited),
        (CONTROL_BURST as usize, 8),
        "frozen clock: the burst then refusals: {statuses:?}"
    );

    // The refill is as deterministic as the drain: +1 s earns CONTROL_RATE.
    clock.advance(Duration::from_secs(1));
    let mut statuses = Vec::new();
    for _ in 0..=CONTROL_RATE {
        let r = send(&e.app, post_json(&path, &e.no_read, &req_body)).await;
        statuses.push(r.status);
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        CONTROL_RATE as usize,
        "one second refills the rate, no more: {statuses:?}"
    );
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
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

/// White-hat N5: a 2xx whose body never finishes is still a submitted job —
/// the runner took it. If the submit waited the body out under one deadline,
/// it would classify Transient and the next drive would land a SECOND job
/// under a fresh `op_key`.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_dr_retry_with_a_slow_2xx_body_is_still_one_job(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_runner(pool, &url, &url).await;
    let path = op_path(&e, "retry").await;
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article/slowjob", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.body["status"], "authorized", "{:?}", r.body);

    drive_resumable(&e.state).await;
    drive_resumable(&e.state).await;
    let jobs = host.job_submits();
    assert_eq!(
        jobs.len(),
        1,
        "a slow 2xx is a submit, not a transient — no second op_key: {jobs:?}"
    );
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
    // the observe) and drive again: the re-drive carries the SAME persisted
    // `op_key`, so the runner's own idempotence — not only the effect
    // authority's business-key dedup — keeps it one job (white-hat N5).
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
    assert_eq!(jobs.len(), 2, "the stranded record re-drives one submit");
    assert_eq!(
        jobs[0].0, jobs[1].0,
        "the persisted op_key is reused, not re-minted"
    );
    assert_eq!(
        host.attempts("article/lost"),
        1,
        "the runner ran it once — the op_key deduped"
    );
    assert_eq!(host.effects(), 1, "the authority saw one effect");
    let rec = e.store.control_get(id).await.unwrap().unwrap();
    assert_eq!(rec.status, "confirmed");
}

/// White-hat N5 residual: the write lands and the answer never comes — the
/// only honest verdict is Transient. The `op_key` was persisted BEFORE the
/// first POST, so the next drive resubmits under the SAME key and the runner
/// still sees one job, not a second mint.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_dr_retry_reuses_its_op_key_after_a_transient(pool: PgPool) {
    let (url, host) = stub_host().await;
    let e = env_with_runner(pool, &url, &url).await;
    let path = op_path(&e, "retry").await;
    let r = send(
        &e.app,
        post_json(
            &path,
            &e.no_read,
            &body("article/hangwrite", &json!({"published": false})),
        ),
    )
    .await;
    assert_eq!(r.body["status"], "authorized", "{:?}", r.body);

    // First drive: the POST lands, the answer never comes — Transient.
    drive_resumable(&e.state).await;
    // Second drive: resubmits under the persisted op_key — acknowledged, and
    // the observe then confirms the effect the first write caused.
    drive_resumable(&e.state).await;
    let jobs = host.job_submits();
    assert_eq!(jobs.len(), 2, "two POSTs — the transient lost the answer");
    assert_eq!(
        jobs[0].0, jobs[1].0,
        "the persisted op_key is reused, not re-minted"
    );
    assert_eq!(
        host.attempts("article/hangwrite"),
        1,
        "the runner ran it once — the op_key deduped"
    );
    assert_eq!(host.effects(), 1);
    let id = Uuid::parse_str(r.body["control_id"].as_str().unwrap()).unwrap();
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

/// The stale-cache fallback is the read path's flood resilience — a control or
/// admin call under a saturated lookup gate must refuse, never trust a record
/// past its TTL (white-hat S2-5: saturating the four lookup permits kept a
/// revoked record serving ~330 s past TTL). Since N6 the privileged lane must
/// be flooded too for the refusal — a stranger's flood alone no longer 503s.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_saturated_lookup_gate_fails_closed_on_controls(pool: PgPool) {
    let e = env_with(pool, |s| {
        s.tokens = CredentialCache::new(Duration::ZERO, Duration::ZERO, 10_000);
    })
    .await;
    let path = format!("/v1/projects/pilot/operations/{}/reconcile", op_hex(1));
    let req = body("article:x", &json!({"published": false}));
    // Warm the token cache: the first call resolves on a free permit, and with a
    // zero TTL its record is stale the instant it lands.
    let r = send(&e.app, post_json(&path, &e.no_read, &req)).await;
    assert_eq!(r.status, 403, "{:?}", r.body); // no adapter — audited denial, cache warm
    let r = send(&e.app, get_req("/v1/projects", &e.reader)).await;
    assert_eq!(r.status, 200, "{:?}", r.body); // the read token's record, too

    // Every lookup permit held on BOTH gates — no credential query can run.
    let held: Vec<_> = (0..4)
        .map(|_| e.state.lookup_gate.try_acquire().unwrap())
        .chain((0..2).map(|_| e.state.privileged_lookup_gate.try_acquire().unwrap()))
        .collect();

    // The read path still answers from the stale record (its documented trade).
    let r = send(&e.app, get_req("/v1/projects", &e.reader)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    // The control path fails closed: the stale record is not authority here.
    let r = send(&e.app, post_json(&path, &e.no_read, &req)).await;
    assert_eq!(r.status, 503, "{:?}", r.body);
    drop(held);
}

/// White-hat N6: a random-token flood holds every shared lookup permit — a
/// control or admin call rides the reserved lane, so its credential still
/// resolves fresh from PostgreSQL (and the denial it reaches is an honest 403,
/// never a flood-induced 503).
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_flooded_lookup_gate_cannot_starve_controls(pool: PgPool) {
    let e = env_with(pool, |s| {
        s.tokens = CredentialCache::new(Duration::ZERO, Duration::ZERO, 10_000);
    })
    .await;
    let path = format!("/v1/projects/pilot/operations/{}/reconcile", op_hex(1));
    let req = body("article:x", &json!({"published": false}));
    let r = send(&e.app, post_json(&path, &e.no_read, &req)).await;
    assert_eq!(r.status, 403, "{:?}", r.body);

    // The shared gate spent — as a random-token flood leaves it.
    let held: Vec<_> = (0..4)
        .map(|_| e.state.lookup_gate.try_acquire().unwrap())
        .collect();

    // The reserved lane resolves the control caller's token fresh: the call
    // reaches its own checks instead of shedding as Overloaded.
    let r = send(&e.app, post_json(&path, &e.no_read, &req)).await;
    assert_eq!(
        r.status, 403,
        "the reserved lane resolved the credential: {:?}",
        r.body
    );
    drop(held);
}

/// Review N6 (re-review): the lane is for digests the cache already holds as
/// `Stale` — a well-shaped stranger is a `Miss` and must ride the shared gate
/// like any other unknown key. Two of those on a control route can never hold
/// the two reserved permits an operator needs mid-incident.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_well_shaped_stranger_never_touches_the_reserved_lane(pool: PgPool) {
    let e = env_with(pool, |s| {
        s.tokens = CredentialCache::new(Duration::ZERO, Duration::ZERO, 10_000);
    })
    .await;
    let path = format!("/v1/projects/pilot/operations/{}/reconcile", op_hex(1));
    let req = body("article:x", &json!({"published": false}));
    // Warm the operator's token — its record goes Stale the instant it lands.
    let r = send(&e.app, post_json(&path, &e.no_read, &req)).await;
    assert_eq!(r.status, 403, "{:?}", r.body);

    // The shared gate spent — as a stranger's flood leaves it. The lane is
    // still free, but no stranger may reach it.
    let held: Vec<_> = (0..4)
        .map(|_| e.state.lookup_gate.try_acquire().unwrap())
        .collect();

    // A credential-shaped token the store has never seen: a Miss, not a
    // lane candidate — it sheds on the shared gate (a pre-fix build would
    // have ridden the free lane to a 401).
    let stranger = format!("{TOKEN_PREFIX}{}", "x".repeat(43));
    let r = send(&e.app, post_json(&path, &stranger, &req)).await;
    assert_eq!(
        r.status, 503,
        "a Miss must shed on the shared gate, not hold a reserved permit: {:?}",
        r.body
    );

    // The operator's Stale record still takes the lane — an honest 403.
    let r = send(&e.app, post_json(&path, &e.no_read, &req)).await;
    assert_eq!(r.status, 403, "{:?}", r.body);
    drop(held);
}
