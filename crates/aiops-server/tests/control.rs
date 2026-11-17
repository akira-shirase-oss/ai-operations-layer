//! Controlled recovery (A3, D-030) through the real router against real PostgreSQL, with a
//! fake Durable Runner and a fake publisher: a recovery is planned against the runner's
//! own state, blocked when the publisher has a record of the effect, submitted once as a
//! new run under a derived key when a person confirms, a no-op when replayed, and every
//! request, allowed or refused, is audited.
#![expect(
    clippy::unwrap_used,
    reason = "integration tests: a failed setup step is a test failure"
)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aiops_domain::correlate::operation_for_run;
use aiops_server::auth::{TOKEN_PREFIX, generate, hash};
use aiops_server::config::{AuthoritySource, ControlConfig};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use common::{Env, env_with, get as get_req, now, post_ingest, send};
use serde_json::{Value, json};
use sqlx::PgPool;

const READ_KEY: &str = "read-key-0123456789abcdef0123456789abcdef";
const API_KEY: &str = "api-key-0123456789abcdef0123456789abcdef";
const SPEC: &str =
    r#"{"version":1,"steps":[{"name":"publish","handler":"publish"}],"input_b64":"e30="}"#;

#[derive(Default)]
struct Fake {
    /// Every submit the runner received: (`op_key`, spec).
    submits: Mutex<Vec<(String, Value)>>,
    runs: Mutex<HashMap<String, u64>>,
}

fn authorized(h: &HeaderMap, key: &str) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {key}"))
}

async fn job(
    State(f): State<Arc<Fake>>,
    Path(id): Path<u64>,
    h: HeaderMap,
) -> (StatusCode, String) {
    if !authorized(&h, READ_KEY) && !authorized(&h, API_KEY) {
        return (StatusCode::UNAUTHORIZED, "{}".into());
    }
    let (key, state) = match id {
        7 => ("article/r-7".to_owned(), "failed:step:3"),
        8 => ("article/r-8".to_owned(), "done"),
        9 => ("article/r-9".to_owned(), "failed:step:3"),
        n if n >= 100 => {
            let runs = f.runs.lock().unwrap();
            match runs.iter().find(|(_, r)| **r == n) {
                Some((k, _)) => (k.clone(), "done"),
                None => return (StatusCode::NOT_FOUND, "{}".into()),
            }
        }
        _ => return (StatusCode::NOT_FOUND, "{}".into()),
    };
    let body = json!({"id": id, "key": key, "state": state, "steps": 4, "finished_at": 1});
    (StatusCode::OK, body.to_string())
}

async fn spec(Path(id): Path<u64>, h: HeaderMap) -> (StatusCode, String) {
    if !authorized(&h, API_KEY) {
        return (StatusCode::FORBIDDEN, "{}".into());
    }
    let spec: Value = serde_json::from_str(SPEC).unwrap();
    (
        StatusCode::OK,
        json!({"id": id, "key": format!("article/r-{id}"), "spec": spec}).to_string(),
    )
}

async fn submit(State(f): State<Arc<Fake>>, h: HeaderMap, body: String) -> (StatusCode, String) {
    if !authorized(&h, API_KEY) {
        return (StatusCode::UNAUTHORIZED, "{}".into());
    }
    let v: Value = serde_json::from_str(&body).unwrap();
    let key = v["op_key"].as_str().unwrap().to_owned();
    f.submits
        .lock()
        .unwrap()
        .push((key.clone(), v["spec"].clone()));
    let mut runs = f.runs.lock().unwrap();
    let next = 100 + runs.len() as u64;
    let id = *runs.entry(key).or_insert(next); // an op_key returns its first run
    (StatusCode::OK, json!({"run_id": id}).to_string())
}

/// The publisher's own record: only article/r-9 was published.
async fn publication(Path(key): Path<String>) -> StatusCode {
    if key == "article/r-9" {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn fakes() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/jobs", post(submit))
        .route("/jobs/{id}", get(job))
        .route("/jobs/{id}/spec", get(spec))
        .route("/publications/{key}", get(publication))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), fake)
}

async fn setup(pool: PgPool) -> (Env, Arc<Fake>) {
    let (url, fake) = fakes().await;
    let e = env_with(pool, |s| {
        s.config.authority = Some(AuthoritySource {
            label: "dr-test".into(),
            url: url.clone(),
            read_key: READ_KEY.into(),
        });
        s.config.control = Some(ControlConfig {
            api_key: API_KEY.into(),
            effect_checks: vec![format!("{url}/publications/{{key}}")],
        });
    })
    .await;
    for run in [7_i64, 8, 9] {
        let op = operation_for_run("dr-test", u64::try_from(run).unwrap()).to_string();
        let batch = json!({
            "schema_version": 1,
            "source": {"instance_id": format!("{run:032x}"), "service": "durable-runner-adapter", "sdk": "test"},
            "events": [{
                "event_id": format!("{:032x}", 0xc000 + run), "type": "operation.started", "operation_id": op,
                "seq": 1, "op_seq": 1, "event_time": now().to_rfc3339(), "name": "publish_article",
                "attributes": {"job.runner": "dr-test", "job.id": run}
            }]
        });
        let r = send(&e.app, post_ingest(&e.ingest_key, batch.to_string())).await;
        assert_eq!(r.body["accepted"], 1, "{:?}", r.body);
    }
    (e, fake)
}

fn op(run: u64) -> String {
    operation_for_run("dr-test", run).to_string()
}

fn post_json(path: &str, token: &str, body: &Value) -> Request<Body> {
    Request::post(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn plan(
    e: &Env,
    token: &str,
    run: u64,
    state: &str,
    key: &str,
    request: &str,
) -> common::Reply {
    let body = json!({"request_key": request, "expected_state": state, "business_key": key});
    send(
        &e.app,
        post_json(
            &format!("/v1/projects/pilot/operations/{}/recoveries", op(run)),
            token,
            &body,
        ),
    )
    .await
}

async fn confirm(e: &Env, token: &str, id: &str, state: &str) -> common::Reply {
    let body = json!({"expected_state": state});
    send(
        &e.app,
        post_json(
            &format!("/v1/projects/pilot/recoveries/{id}/confirm"),
            token,
            &body,
        ),
    )
    .await
}

async fn audit(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT action, decision, reason FROM audit_log WHERE action LIKE 'recovery.%' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_failed_run_is_recovered_once_as_a_new_run_under_a_derived_key(pool: PgPool) {
    let (e, fake) = setup(pool.clone()).await;
    let operator = &e.no_read; // `operate` on pilot
    let r = plan(&e, operator, 7, "failed:step:3", "article/r-7", "plan-0001").await;
    assert_eq!(r.status, 201, "{:?}", r.body);
    assert_eq!(r.body["status"], "planned");
    assert_eq!(r.body["effect"], "not_landed");
    assert_eq!(r.body["derived_key"], "article/r-7#r1");
    let id = r.body["id"].as_str().unwrap().to_owned();
    // A replayed plan is the same plan.
    let again = plan(&e, operator, 7, "failed:step:3", "article/r-7", "plan-0001").await;
    assert_eq!(
        (again.status, again.body["id"].as_str()),
        (200, Some(id.as_str()))
    );
    assert!(
        fake.submits.lock().unwrap().is_empty(),
        "planning submits nothing"
    );
    // A confirm against a state the person did not see is refused.
    let stale = confirm(&e, operator, &id, "failed:step:2").await;
    assert_eq!(
        (stale.status, stale.body["error"]["code"].as_str()),
        (409, Some("state_changed"))
    );
    let done = confirm(&e, operator, &id, "failed:step:3").await;
    assert_eq!(done.status, 200, "{:?}", done.body);
    assert_eq!(done.body["status"], "submitted");
    assert_eq!(done.body["approved_by"], "bob");
    assert_eq!(done.body["new_run"]["run_id"], 100);
    assert_eq!(done.body["new_run"]["operation_id"], op(100));
    // Replayed confirms are no-ops: one submit, under the derived key, with the run's spec.
    let replay = confirm(&e, operator, &id, "failed:step:3").await;
    assert_eq!(
        (replay.status, replay.body["status"].as_str()),
        (200, Some("submitted"))
    );
    {
        let submits = fake.submits.lock().unwrap();
        assert_eq!(submits.len(), 1, "{submits:?}");
        assert_eq!(submits[0].0, "article/r-7#r1");
        assert_eq!(submits[0].1, serde_json::from_str::<Value>(SPEC).unwrap());
    }
    // Success is the runner's word, read when asked.
    let list = send(
        &e.app,
        get_req(
            &format!("/v1/projects/pilot/operations/{}/recoveries", op(7)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(list.status, 200, "{:?}", list.body);
    assert_eq!(list.body["configured"], true);
    assert_eq!(
        list.body["recoveries"][0]["new_run"]["runner_state"],
        "done"
    );
    // A second recovery of the same business key is numbered after the first.
    let second = plan(&e, operator, 7, "failed:step:3", "article/r-7", "plan-0002").await;
    assert_eq!(
        second.body["derived_key"], "article/r-7#r2",
        "{:?}",
        second.body
    );
    let rows = audit(&pool).await;
    let allowed: Vec<_> = rows
        .iter()
        .filter(|r| r.1 == "allowed")
        .map(|r| r.2.as_str())
        .collect();
    assert!(
        allowed
            .iter()
            .any(|r| r.starts_with("planned article/r-7#r1")),
        "{rows:?}"
    );
    assert!(allowed.contains(&"replayed"), "{rows:?}");
    assert!(
        allowed
            .iter()
            .any(|r| r.starts_with("submitted article/r-7#r1 as run 100")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.1 == "denied" && r.2 == "state_changed"),
        "{rows:?}"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn refusals_are_audited_and_nothing_is_submitted(pool: PgPool) {
    let (e, fake) = setup(pool.clone()).await;
    let operator = &e.no_read;
    // Without the operate scope, or outside the project: denied and audited.
    let r = plan(
        &e,
        &e.reader,
        7,
        "failed:step:3",
        "article/r-7",
        "deny-0001",
    )
    .await;
    assert_eq!(r.status, 403, "{:?}", r.body);
    let outsider = generate(TOKEN_PREFIX).unwrap();
    let hour = now().plus_micros(3_600_000_000);
    let scopes = ["operate".to_owned()];
    let projects = ["other".to_owned()];
    e.store
        .create_token("eve", &scopes, &projects, &hash(&outsider), hour)
        .await
        .unwrap();
    let r = plan(
        &e,
        &outsider,
        7,
        "failed:step:3",
        "article/r-7",
        "deny-0002",
    )
    .await;
    assert_eq!(r.status, 403, "{:?}", r.body);
    // The runner's state or key is not what the person saw.
    let r = plan(&e, operator, 7, "failed:step:1", "article/r-7", "deny-0003").await;
    assert_eq!(r.body["error"]["code"], "state_changed", "{:?}", r.body);
    let r = plan(
        &e,
        operator,
        7,
        "failed:step:3",
        "article/r-other",
        "deny-0004",
    )
    .await;
    assert_eq!(
        r.body["error"]["code"], "business_key_mismatch",
        "{:?}",
        r.body
    );
    // No generic run again: a run that finished has nothing to recover.
    let r = plan(&e, operator, 8, "done", "article/r-8", "deny-0005").await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (409, Some("done_nothing_to_recover"))
    );
    // The publisher has a record of article/r-9: the plan is blocked, and stays blocked.
    let r = plan(
        &e,
        operator,
        9,
        "failed:step:3",
        "article/r-9",
        "block-0001",
    )
    .await;
    assert_eq!(r.status, 201, "{:?}", r.body);
    assert_eq!(
        (r.body["status"].as_str(), r.body["effect"].as_str()),
        (Some("blocked"), Some("landed"))
    );
    let id = r.body["id"].as_str().unwrap().to_owned();
    let c = confirm(&e, operator, &id, "failed:step:3").await;
    assert_eq!(
        (c.status, c.body["error"]["code"].as_str()),
        (409, Some("blocked"))
    );
    assert!(
        fake.submits.lock().unwrap().is_empty(),
        "nothing was submitted"
    );
    let rows = audit(&pool).await;
    let denied: Vec<_> = rows
        .iter()
        .filter(|r| r.1 == "denied")
        .map(|r| r.2.as_str())
        .collect();
    for reason in [
        "scope_missing",
        "project_not_in_scope",
        "state_changed",
        "business_key_mismatch",
        "done_nothing_to_recover",
        "blocked",
    ] {
        assert!(denied.contains(&reason), "{reason} not audited: {rows:?}");
    }
}
