//! The collector's in-process locks are measured (D-026): their wait and hold times are
//! exposed with the collector-wide counters, and an ignored measurement drives the real
//! router against real PostgreSQL at three concurrencies to record them.
//!
//! `cargo test -p aiops-server --release --test locks -- --ignored --nocapture` prints one
//! JSON line per concurrency (the result file is `perf/results/…-ingest-locks.md`).
//! "Not 200" counts every answer that was not a success (shed, rate-limited, …): a
//! measurement that lost requests says so.
#![expect(
    clippy::unwrap_used,
    reason = "integration tests: a failed setup step is a test failure"
)]
#![expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "measurement arithmetic on small, non-negative counts"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aiops_server::auth::{INGEST_KEY_PREFIX, generate, hash};
use aiops_server::{AppState, Config, router, system_clock};
use aiops_store::{Store, StoreConfig};
use common::{ScratchDb, env, get, now, post_ingest, send};
use serde_json::{Value, json};
use sqlx::PgPool;

/// One batch of `ops` operations of the k6 scenario's reference shape (ten events each:
/// start, four steps started and finished, finish), from source instance `instance`.
fn reference_batch(instance: u32, first_op: u32, ops: u32) -> String {
    let at = now();
    let mut events = Vec::with_capacity(ops as usize * 10);
    let mut seq = u64::from(first_op) * 10;
    for n in first_op..first_op + ops {
        let op = format!("{instance:08x}{n:024x}");
        let mut k = 0u32;
        let mut base = |kind: &str| {
            seq += 1;
            k += 1;
            json!({
                "event_id": format!("{instance:08x}{:024x}", u64::from(n) * 10 + u64::from(k)),
                "type": kind, "operation_id": op, "seq": seq, "op_seq": k,
                "event_time": at.to_rfc3339(),
            })
        };
        let mut start = base("operation.started");
        start["name"] = json!("publish_article");
        start["request_id"] = json!(format!("locks-{instance}-{n}"));
        start["release"] = json!("locks-r1");
        events.push(start);
        for (i, (kind, name)) in [
            ("model", "generate"),
            ("validation", "validate"),
            ("tool", "fact_check"),
            ("tool", "publish"),
        ]
        .into_iter()
        .enumerate()
        {
            let step = format!("{:016x}", u64::from(n) * 8 + i as u64 + 1);
            let mut s = base("step.started");
            s["step_id"] = json!(step);
            s["kind"] = json!(kind);
            s["name"] = json!(name);
            s["attempt"] = json!(1);
            s["external_effect"] = json!(name == "publish");
            events.push(s);
            let mut f = base("step.finished");
            f["step_id"] = json!(step);
            f["attempt"] = json!(1);
            f["outcome"] = json!("succeeded");
            if kind == "model" {
                f["usage"] = json!({"input_tokens": 132, "output_tokens": 480});
                f["cost_estimate"] =
                    json!({"micros": 2052, "currency": "USD", "basis": "fake-list-price-2026-11"});
            }
            events.push(f);
        }
        let mut fin = base("operation.finished");
        fin["outcome"] = json!("succeeded");
        fin["events_emitted"] = json!(10);
        events.push(fin);
    }
    json!({
        "schema_version": 1,
        "source": {"instance_id": format!("{instance:08x}{:024x}", 0xfeed_u64), "service": "locks", "sdk": "test"},
        "events": events,
    })
    .to_string()
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_collector_reports_its_locks_wait_and_hold(pool: PgPool) {
    let e = env(pool).await;
    let r = send(&e.app, post_ingest(&e.ingest_key, reference_batch(1, 0, 3))).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["accepted"], 30);
    let stats = send(&e.app, get("/v1/collector/stats", &e.admin)).await;
    assert_eq!(stats.status, 200);
    let locks = &stats.body["locks"];
    for lock in [
        "names_map",
        "names_shared",
        "names_exclusive",
        "ingest_key_cache",
        "ingest_rate_limit",
        "token_cache",
    ] {
        assert!(
            locks[lock]["acquisitions"].as_u64().unwrap() >= 1,
            "{lock}: {locks}"
        );
        for field in [
            "wait_ns_total",
            "wait_ns_max",
            "hold_ns_total",
            "hold_ns_max",
        ] {
            assert!(locks[lock][field].is_u64(), "{lock}.{field}: {locks}");
        }
    }
    // A project's first batch: the map is read (missing) and written (loaded), and its new
    // names are checked shared, then admitted exclusively.
    let count = |l: &Value, name: &str| l[name]["acquisitions"].as_u64().unwrap();
    assert_eq!(count(locks, "names_map"), 2, "{locks}");
    assert_eq!(count(locks, "names_shared"), 1, "{locks}");
    assert_eq!(count(locks, "names_exclusive"), 1, "{locks}");
    assert!(locks["names_exclusive"]["hold_ns_total"].as_u64().unwrap() > 0);

    // A batch whose names are all known never takes the registry exclusively (D-026).
    let r = send(&e.app, post_ingest(&e.ingest_key, reference_batch(1, 3, 3))).await;
    assert_eq!(r.body["accepted"], 30, "{:?}", r.body);
    let after = send(&e.app, get("/v1/collector/stats", &e.admin))
        .await
        .body;
    let locks = &after["locks"];
    assert_eq!(count(locks, "names_map"), 3, "{locks}");
    assert_eq!(count(locks, "names_shared"), 2, "{locks}");
    assert_eq!(count(locks, "names_exclusive"), 1, "{locks}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_new_name_over_budget_is_still_stored_as_overflow_on_the_exclusive_path(pool: PgPool) {
    // The shared fast path must not change admission: a known batch, then one that brings a
    // new release name to a project whose budget is spent, which is stored as `_overflow`.
    let e = common::env_with(pool, |s| {
        s.limits.max_names_per_project = 7;
        s.limits.max_names_per_key = 7;
    })
    .await;
    let r = send(&e.app, post_ingest(&e.ingest_key, reference_batch(2, 0, 1))).await;
    assert_eq!(r.body["accepted"], 10, "{:?}", r.body);
    assert_eq!(
        r.body["overflowed_names"], 0,
        "op, release, four steps and the cost basis fit: {:?}",
        r.body
    );
    let mut batch: Value = serde_json::from_str(&reference_batch(2, 1, 1)).unwrap();
    batch["events"][0]["release"] = json!("locks-r2");
    let r = send(&e.app, post_ingest(&e.ingest_key, batch.to_string())).await;
    assert_eq!(r.body["accepted"], 10, "{:?}", r.body);
    assert_eq!(r.body["overflowed_names"], 1, "{:?}", r.body);
    let list = send(
        &e.app,
        get(
            "/v1/projects/pilot/operations?request_id=locks-2-1",
            &e.reader,
        ),
    )
    .await;
    assert_eq!(
        list.body["operations"][0]["release"], "_overflow",
        "{:?}",
        list.body
    );
}

/// An ingest request with its `Content-Length`, as an HTTP sender declares it (without one
/// the collector charges the full 1 MiB cap against its in-flight body budget).
fn ingest_request(key: &str, body: String) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::post("/v1/ingest")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("content-length", body.len())
        .body(axum::body::Body::from(body))
        .unwrap()
}

/// Wall time per request and the locks' counters for `tasks` concurrent senders, each
/// sending `batches` batches of 500 events through the in-process router.
async fn measure(store: &Store, key: &str, tasks: u32, batches: u32, offset: u32) -> Value {
    let state = Arc::new(AppState::new(
        store.clone(),
        Config::local(""),
        system_clock(),
        None,
    ));
    let app = router(state.clone());
    // Warm the credential cache and the name registry, outside the measurement.
    let warm = send(&app, ingest_request(key, reference_batch(offset, 0, 50))).await;
    assert_eq!(warm.status, 200, "{:?}", warm.body);
    let before = state.metrics_view();
    let started = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for t in 0..tasks {
        let (app, key) = (app.clone(), key.to_owned());
        set.spawn(async move {
            let mut lat = Vec::new();
            for b in 0..batches {
                let body = reference_batch(offset + 1 + t, 1 + b * 50, 50);
                let at = Instant::now();
                let r = send(&app, ingest_request(&key, body)).await;
                lat.push((
                    at.elapsed(),
                    r.status,
                    r.body["accepted"].as_u64().unwrap_or(0),
                ));
            }
            lat
        });
    }
    let mut lat = Vec::new();
    while let Some(r) = set.join_next().await {
        lat.extend(r.unwrap());
    }
    let wall = started.elapsed();
    let accepted: u64 = lat.iter().map(|(_, _, a)| a).sum();
    let not_ok = lat.iter().filter(|(_, s, _)| *s != 200).count();
    let mut ms: Vec<f64> = lat.iter().map(|(d, _, _)| d.as_secs_f64() * 1e3).collect();
    ms.sort_by(f64::total_cmp);
    let pct = |p: f64| ms[((ms.len() as f64 - 1.0) * p).round() as usize];
    let after = state.metrics_view();
    let delta = |name: &str| {
        let a = serde_json::to_value(&after.locks).unwrap()[name].clone();
        let b = serde_json::to_value(&before.locks).unwrap()[name].clone();
        let n = a["acquisitions"].as_u64().unwrap() - b["acquisitions"].as_u64().unwrap();
        let wait = a["wait_ns_total"].as_u64().unwrap() - b["wait_ns_total"].as_u64().unwrap();
        let hold = a["hold_ns_total"].as_u64().unwrap() - b["hold_ns_total"].as_u64().unwrap();
        json!({
            "acquisitions": n,
            "wait_us_mean": wait as f64 / n.max(1) as f64 / 1e3,
            "hold_us_mean": hold as f64 / n.max(1) as f64 / 1e3,
            "wait_ms_total": wait as f64 / 1e6,
            "hold_ms_total": hold as f64 / 1e6,
            // Maxima are since the state was built (the warm-up batch included).
            "wait_us_max": a["wait_ns_max"].as_u64().unwrap() as f64 / 1e3,
            "hold_us_max": a["hold_ns_max"].as_u64().unwrap() as f64 / 1e3,
        })
    };
    json!({
        "concurrency": tasks,
        "requests": lat.len(),
        "not_200": not_ok,
        "events_accepted": accepted,
        "wall_s": wall.as_secs_f64(),
        "request_ms": {"p50": pct(0.5), "p95": pct(0.95), "p99": pct(0.99), "max": pct(1.0)},
        "request_ms_total": ms.iter().sum::<f64>(),
        "locks": {
            "names_map": delta("names_map"),
            "names_shared": delta("names_shared"),
            "names_exclusive": delta("names_exclusive"),
            "ingest_key_cache": delta("ingest_key_cache"),
            "ingest_rate_limit": delta("ingest_rate_limit"),
        },
    })
}

/// Runs on a multi-thread runtime, as the collector does (`#[tokio::main]`): `#[sqlx::test]`
/// blocks on a current-thread runtime, where tasks never overlap and no lock can be
/// contended. It makes its own database, with the collector's six-pool store, and drops it.
#[ignore = "a measurement, not a check: run with --release --ignored --nocapture"]
#[test]
fn measure_ingest_locks() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let db = ScratchDb::new().await;
        let store = Store::connect(&StoreConfig::new(db.url.clone()))
            .await
            .unwrap();
        store.create_project("pilot").await.unwrap();
        let key = generate(INGEST_KEY_PREFIX).unwrap();
        store
            .create_ingest_key("pilot", "locks", &hash(&key))
            .await
            .unwrap();
        // 1 × 16, 16 × 4 and 64 × 2 batches of 500 events: 104,000 events in all, and 64 is
        // the collector's in-flight ingest cap.
        for (i, (tasks, batches)) in [(1, 16), (16, 4), (64, 2)].into_iter().enumerate() {
            let offset = 1_000 * (u32::try_from(i).unwrap() + 1);
            let r = measure(&store, &key, tasks, batches, offset).await;
            println!("LOCKS {r}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        drop(store);
        db.drop().await;
    });
}

/// Review G1: a name admitted in memory whose batch then fails to store must not be
/// treated as known by the next batch (which would only touch a row that was never
/// written, leaving the stored registry, and the budget a reload computes, behind).
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_name_whose_batch_failed_to_store_is_stored_by_the_next_batch(pool: PgPool) {
    let e = common::env(pool.clone()).await;
    sqlx::query(
        "CREATE FUNCTION refuse_g1() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.name = 'release:g1-r1' THEN RAISE EXCEPTION 'g1'; END IF; RETURN NEW; END $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_g1 BEFORE INSERT ON known_names FOR EACH ROW EXECUTE FUNCTION refuse_g1()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut batch: Value = serde_json::from_str(&reference_batch(4, 0, 1)).unwrap();
    batch["events"][0]["release"] = json!("g1-r1");
    let r = send(&e.app, post_ingest(&e.ingest_key, batch.to_string())).await;
    assert!(r.status >= 500, "{:?} {:?}", r.status, r.body);
    sqlx::query("DROP TRIGGER refuse_g1 ON known_names")
        .execute(&pool)
        .await
        .unwrap();
    let mut batch: Value = serde_json::from_str(&reference_batch(4, 1, 1)).unwrap();
    batch["events"][0]["release"] = json!("g1-r1");
    let r = send(&e.app, post_ingest(&e.ingest_key, batch.to_string())).await;
    assert_eq!(r.body["accepted"], 10, "{:?}", r.body);
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM known_names WHERE name = 'release:g1-r1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored, 1,
        "the name the failed batch admitted is stored now"
    );
}
