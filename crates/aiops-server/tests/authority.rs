//! The runner's own answer beside a timeline (A2, D-027), in-process against real
//! PostgreSQL and a fake Durable Runner: shown only for a link that verifies, labeled with
//! the time it was read, and every way the runner can fail to answer is said by name.
#![expect(
    clippy::unwrap_used,
    reason = "integration tests: a failed setup step is a test failure"
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aiops_domain::correlate::operation_for_run;
use aiops_server::config::AuthoritySource;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use common::{env_with, get as get_req, now, post_ingest, send};
use serde_json::{Value, json};
use sqlx::PgPool;

const READ_KEY: &str = "read-key-0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct FakeRunner {
    slow: AtomicBool,
}

async fn job(
    State(f): State<Arc<FakeRunner>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {READ_KEY}"))
    {
        return (
            StatusCode::UNAUTHORIZED,
            r#"{"code":"unauthorized"}"#.into(),
        );
    }
    if f.slow.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    match id {
        7 => (
            StatusCode::OK,
            json!({"id": 7, "key": "article/r-7", "state": "failed:step:1", "steps": 4, "finished_at": 1_794_900_000_000_u64})
                .to_string(),
        ),
        _ => (StatusCode::NOT_FOUND, r#"{"code":"not_found"}"#.into()),
    }
}

async fn fake_runner() -> (String, Arc<FakeRunner>) {
    let fake = Arc::new(FakeRunner::default());
    let app = Router::new()
        .route("/jobs/{id}", get(job))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), fake)
}

/// An operation's start, as the adapter reports it: linked to `(runner, run)`.
fn linked_start(op: &str, runner: &str, run: i64, n: u64) -> String {
    json!({
        "schema_version": 1,
        "source": {"instance_id": format!("{n:032x}"), "service": "durable-runner-adapter", "sdk": "test"},
        "events": [{
            "event_id": format!("{:032x}", 0xa000 + n), "type": "operation.started", "operation_id": op,
            "seq": 1, "op_seq": 1, "event_time": now().to_rfc3339(), "name": "publish_article",
            "attributes": {"business_key": "article/r-7", "job.runner": runner, "job.id": run}
        }]
    })
    .to_string()
}

async fn authority(app: &Router, token: &str, op: &str) -> (u16, Value) {
    let r = send(
        app,
        get_req(
            &format!("/v1/projects/pilot/operations/{op}/authority"),
            token,
        ),
    )
    .await;
    (r.status, r.body)
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_runner_answers_only_for_a_link_that_verifies(pool: PgPool) {
    let (url, fake) = fake_runner().await;
    let e = env_with(pool, |s| {
        s.config.authority = Some(AuthoritySource {
            label: "dr-test".into(),
            url: url.clone(),
            read_key: READ_KEY.into(),
        });
    })
    .await;
    let real = operation_for_run("dr-test", 7).to_string();
    let r = send(
        &e.app,
        post_ingest(&e.ingest_key, linked_start(&real, "dr-test", 7, 1)),
    )
    .await;
    assert_eq!(
        r.body["accepted"], 1,
        "job.runner and job.id are allowed by default: {:?}",
        r.body
    );
    assert_eq!(r.body["withheld_attributes"], 0, "{:?}", r.body);

    let (status, a) = authority(&e.app, &e.reader, &real).await;
    assert_eq!(status, 200, "{a}");
    assert_eq!(a["state"], "answered", "{a}");
    assert_eq!(a["source"], "durable-runner");
    assert_eq!(a["run_id"], 7);
    assert_eq!(a["answer"]["state"], "failed:step:1");
    assert_eq!(a["answer"]["steps"], 4);
    assert!(
        a["read_at"].is_string(),
        "the answer says when it was read: {a}"
    );

    // A forged link: an operation whose ID does not derive from the run it names.
    let forged = "f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0";
    send(
        &e.app,
        post_ingest(&e.ingest_key, linked_start(forged, "dr-test", 7, 2)),
    )
    .await;
    let (_, a) = authority(&e.app, &e.reader, forged).await;
    assert_eq!(a["state"], "link_unverified", "{a}");
    assert!(
        a.get("answer").is_none(),
        "another run's state is never shown"
    );

    // A run the runner no longer knows; an operation with no link.
    let gone = operation_for_run("dr-test", 9).to_string();
    send(
        &e.app,
        post_ingest(&e.ingest_key, linked_start(&gone, "dr-test", 9, 3)),
    )
    .await;
    let (_, a) = authority(&e.app, &e.reader, &gone).await;
    assert_eq!(
        (a["state"].as_str(), a["reason"].as_str()),
        (Some("unavailable"), Some("not_found")),
        "{a}"
    );
    let (_, a) = authority(&e.app, &e.reader, &"ab".repeat(16)).await;
    assert_eq!(a["state"], "not_linked", "{a}");

    // A slow runner is cut at the deadline and said to be slow, not guessed at.
    fake.slow.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let (_, a) = authority(&e.app, &e.reader, &real).await;
    assert_eq!(a["reason"], "timeout", "{a}");
    assert!(started.elapsed() < std::time::Duration::from_secs(4));

    // A token for another project is denied (and audited, as every query denial is).
    let (status, _) = authority(&e.app, &e.other_reader, &real).await;
    assert_eq!(status, 403);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_runner_that_is_down_or_refuses_the_key_is_named_as_such(pool: PgPool) {
    let (url, _) = fake_runner().await;
    let e = env_with(pool, |s| {
        s.config.authority = Some(AuthoritySource {
            label: "dr-test".into(),
            url: url.clone(),
            read_key: "wrong-key-0123456789abcdef0123456789abcd".into(),
        });
    })
    .await;
    let real = operation_for_run("dr-test", 7).to_string();
    send(
        &e.app,
        post_ingest(&e.ingest_key, linked_start(&real, "dr-test", 7, 1)),
    )
    .await;
    let (_, a) = authority(&e.app, &e.reader, &real).await;
    assert_eq!(a["reason"], "unauthorized", "{a}");

    let e2 = {
        let mut cfg = e.state.config.clone();
        cfg.authority = Some(AuthoritySource {
            label: "dr-test".into(),
            url: "http://127.0.0.1:9".into(), // nothing listens on discard
            read_key: READ_KEY.into(),
        });
        let state = Arc::new(aiops_server::AppState::new(
            e.store.clone(),
            cfg,
            aiops_server::system_clock(),
            None,
        ));
        aiops_server::router(state)
    };
    let (_, a) = authority(&e2, &e.reader, &real).await;
    assert_eq!(a["reason"], "unreachable", "{a}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn without_a_configured_runner_nothing_is_asked(pool: PgPool) {
    let e = common::env(pool).await;
    let (status, a) = authority(&e.app, &e.reader, &"ab".repeat(16)).await;
    assert_eq!((status, a["state"].as_str()), (200, Some("not_configured")));
}
