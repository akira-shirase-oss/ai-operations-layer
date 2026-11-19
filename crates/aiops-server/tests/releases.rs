//! Operation filtering and release comparison (A2) read the same summaries: for every
//! status, a release's count in the comparison is the number of operations the filtered
//! list returns. Operations that never finished or lost events are counted as such, never
//! as success.
#![expect(
    clippy::unwrap_used,
    reason = "integration tests: a failed setup step is a test failure"
)]

mod common;

use common::{env, get, now, post_ingest, send};
use serde_json::{Value, json};
use sqlx::PgPool;

/// One operation of `release` with `name`, finished as `outcome` (`None`: no finish), from
/// source instance `instance`; `lose` drops its second event (a proven hole).
fn op(n: u32, name: &str, release: &str, outcome: Option<&str>, lose: bool) -> Vec<Value> {
    let at = now().to_rfc3339();
    let op = format!("{n:032x}");
    let mut events = vec![json!({
        "event_id": format!("{:032x}", 0x10_0000 + n * 4), "type": "operation.started", "operation_id": op,
        "seq": 1, "op_seq": 1, "event_time": at, "name": name, "release": release,
        "request_id": format!("rel-{n}")
    })];
    if !lose {
        events.push(json!({
            "event_id": format!("{:032x}", 0x10_0000 + n * 4 + 1), "type": "step.started", "operation_id": op,
            "seq": 2, "op_seq": 2, "event_time": at, "step_id": format!("{n:016x}"), "kind": "tool",
            "name": "publish", "attempt": 1
        }));
    }
    if let Some(outcome) = outcome {
        let mut fin = json!({
            "event_id": format!("{:032x}", 0x10_0000 + n * 4 + 2), "type": "operation.finished",
            "operation_id": op, "seq": 3, "op_seq": 3, "event_time": at, "outcome": outcome,
            "events_emitted": 3
        });
        match outcome {
            "failed" => fin["failure_class"] = json!("rejected"),
            "uncertain" => fin["uncertain_reason"] = json!("timeout_after_send"),
            _ => {}
        }
        events.push(fin);
    }
    events
}

fn batch(instance: u32, events: &[Value]) -> String {
    json!({
        "schema_version": 1,
        "source": {"instance_id": format!("{:032x}", 0xbeef_0000 + instance), "service": "rel", "sdk": "test"},
        "events": events
    })
    .to_string()
}

async fn list(e: &common::Env, query: &str) -> Vec<Value> {
    let r = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations?limit=200&{query}"),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{query}: {}", r.body);
    r.body["operations"].as_array().unwrap().clone()
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn filters_and_the_release_comparison_read_the_same_records(pool: PgPool) {
    let e = env(pool).await;
    // r1: two succeeded. r2: one succeeded with a lost event, one failed, one uncertain,
    // one never finished. Another name in r2, succeeded. Each operation its own instance.
    let ops = [
        (1, "publish_article", "r1", Some("succeeded"), false),
        (2, "publish_article", "r1", Some("succeeded"), false),
        (3, "publish_article", "r2", Some("succeeded"), true),
        (4, "publish_article", "r2", Some("failed"), false),
        (5, "publish_article", "r2", Some("uncertain"), false),
        (6, "publish_article", "r2", None, false),
        (7, "bulk_import", "r2", Some("succeeded"), false),
    ];
    for (n, name, release, outcome, lose) in ops {
        let r = send(
            &e.app,
            post_ingest(
                &e.ingest_key,
                batch(n, &op(n, name, release, outcome, lose)),
            ),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.body);
    }

    assert_eq!(list(&e, "release=r1").await.len(), 2);
    assert_eq!(list(&e, "release=r2").await.len(), 5);
    assert_eq!(list(&e, "release=r2&name=bulk_import").await.len(), 1);
    let failed = list(&e, "release=r2&status=failed").await;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["request_id"], "rel-4");
    let open = list(&e, "status=no_finish_observed").await;
    assert_eq!(
        open.len(),
        1,
        "an operation without a finish is its own status"
    );
    assert_eq!(open[0]["status"]["state"], "no_finish_observed");
    let later = now().plus_micros(60_000_000).to_rfc3339();
    assert!(
        list(&e, &format!("from={later}")).await.is_empty(),
        "the window bounds the list"
    );
    let bad = send(
        &e.app,
        get("/v1/projects/pilot/operations?status=running", &e.reader),
    )
    .await;
    assert_eq!(bad.status, 400, "there is no 'running': {}", bad.body);

    let c = send(
        &e.app,
        get(
            "/v1/projects/pilot/releases/compare?releases=r1,r2,r9",
            &e.reader,
        ),
    )
    .await;
    assert_eq!(c.status, 200, "{}", c.body);
    let releases = c.body["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 3, "an empty release is listed with zeros");
    assert_eq!(releases[2]["release"], "r9");
    assert_eq!(releases[2]["operations"], 0);
    for r in &releases[..2] {
        let name = r["release"].as_str().unwrap();
        let mut total = 0;
        for status in [
            "succeeded",
            "failed",
            "uncertain",
            "conflicting",
            "no_finish_observed",
        ] {
            let listed = list(&e, &format!("release={name}&status={status}"))
                .await
                .len();
            assert_eq!(
                r[status], listed,
                "{name} {status}: the comparison and the list agree"
            );
            total += listed;
        }
        assert_eq!(
            r["operations"], total,
            "{name}: every operation has one status"
        );
    }
    let r2 = &releases[1];
    assert_eq!(
        r2["missing_events"], 1,
        "the succeeded operation that lost an event: {r2}"
    );
    assert_eq!(r2["succeeded"], 2);

    let too_wide = send(
        &e.app,
        get(
            "/v1/projects/pilot/releases/compare?releases=r1&from=2026-01-01T00:00:00Z&to=2026-02-01T00:00:00Z",
            &e.reader,
        ),
    )
    .await;
    assert_eq!(too_wide.status, 400, "a window is at most 7 days");
    let denied = send(
        &e.app,
        get(
            "/v1/projects/pilot/releases/compare?releases=r1",
            &e.other_reader,
        ),
    )
    .await;
    assert_eq!(denied.status, 403);
}

/// Query-path chaos (A2): the query pool's only connection is held by a query stuck on a
/// lock. Ingest and an audited denial still answer at once (their own pools); the query
/// path itself answers 503 within its acquire timeout instead of hanging.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_stuck_query_path_never_delays_ingest_or_audit_at_the_api(pool: PgPool) {
    use aiops_server::auth::{INGEST_KEY_PREFIX, TOKEN_PREFIX, generate, hash};
    use aiops_store::{Store, StoreConfig};
    use std::time::{Duration, Instant};

    let base = (*pool.connect_options()).clone();
    let mut cfg = StoreConfig::new("");
    cfg.query_connections = 1;
    cfg.acquire_timeout = Duration::from_secs(1);
    let store = Store::connect_with(&cfg, &base).await.unwrap();
    store.create_project("pilot").await.unwrap();
    store.create_project("other").await.unwrap();
    let key = generate(INGEST_KEY_PREFIX).unwrap();
    store
        .create_ingest_key("pilot", "h", &hash(&key))
        .await
        .unwrap();
    let later = now().plus_micros(3_600_000_000);
    let token = |project: &'static str| {
        let store = store.clone();
        async move {
            let t = generate(TOKEN_PREFIX).unwrap();
            store
                .create_token(
                    "t",
                    &["read".to_owned()],
                    &[project.to_owned()],
                    &hash(&t),
                    later,
                )
                .await
                .unwrap();
            t
        }
    };
    let (reader, outsider) = (token("pilot").await, token("other").await);
    let state = std::sync::Arc::new(
        aiops_server::AppState::new(
            store.clone(),
            aiops_server::Config::local(""),
            aiops_server::system_clock(),
            None,
        )
        .expect("invariant: the authority client builds with a static config"),
    );
    let app = aiops_server::router(state);
    // Warm the credential and project caches, as a running collector's are.
    let warm = |n: u32| {
        batch(
            100 + n,
            &op(100 + n, "publish_article", "r1", Some("succeeded"), false),
        )
    };
    assert_eq!(send(&app, post_ingest(&key, warm(1))).await.status, 200);
    assert_eq!(
        send(&app, get("/v1/projects/pilot/operations", &reader))
            .await
            .status,
        200
    );
    assert_eq!(
        send(&app, get("/v1/projects/pilot/operations", &outsider))
            .await
            .status,
        403
    );

    let mut locker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE projects IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *locker)
        .await
        .unwrap();
    let stuck = {
        let (app, reader) = (app.clone(), reader.clone());
        tokio::spawn(async move { send(&app, get("/v1/projects", &reader)).await.status })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    assert_eq!(
        send(&app, post_ingest(&key, warm(2))).await.status,
        200,
        "ingest"
    );
    assert_eq!(
        send(&app, get("/v1/projects/pilot/operations", &outsider))
            .await
            .status,
        403,
        "a denial, audited on its own pool"
    );
    let fast = started.elapsed();
    assert!(
        fast < Duration::from_secs(1),
        "ingest and audit waited {fast:?}"
    );

    let started = Instant::now();
    let compare = send(
        &app,
        get("/v1/projects/pilot/releases/compare?releases=r1", &reader),
    )
    .await;
    assert_eq!(
        compare.status, 503,
        "the query path is full: {}",
        compare.body
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded by the acquire timeout"
    );

    locker.rollback().await.unwrap();
    assert_eq!(
        stuck.await.unwrap(),
        200,
        "the stuck query finishes once the lock goes"
    );
}
