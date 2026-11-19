//! The collector in-process against real PostgreSQL (needs `DATABASE_URL`): bounded,
//! authenticated ingest with named refusals; idempotent and out-of-order delivery; a
//! flooding host limited without affecting others; the timeline's honest states; and
//! denials that are audited.
#![expect(
    clippy::unwrap_used,
    reason = "integration tests: a failed setup step is a test failure"
)]

mod common;

use std::time::Duration;

use aiops_domain::wire::WireEvent;
use axum::body::Body;
use axum::http::Request;
use common::{
    audit_rows, batch, batch_from, env, env_with, get, journey, op_hex, post_ingest, send,
};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn ingest_needs_a_valid_ingest_key(pool: PgPool) {
    let e = env(pool).await;
    let body = batch(&journey(1, 0));
    for key in [
        "",
        "aiops_ik_short",
        e.reader.as_str(),
        "aiops_ik_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        let r = send(&e.app, post_ingest(key, body.clone())).await;
        assert_eq!(r.status, 401, "key {key:?}");
        assert_eq!(r.body["error"]["code"], "unauthenticated");
        assert!(
            r.body["error"]["request_id"].is_string(),
            "the envelope carries the request id"
        );
    }
    let r = send(&e.app, post_ingest(&e.ingest_key, body)).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_revoked_key_stops_working(pool: PgPool) {
    let e = env(pool).await;
    e.store
        .revoke_ingest_key("pilot", "reference-host")
        .await
        .unwrap();
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    assert_eq!(r.status, 401);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn each_bad_event_is_refused_by_name_and_the_rest_is_stored(pool: PgPool) {
    let e = env(pool).await;
    let mut events = journey(1, 0);
    events[1].kind = Some("llm".into()); // invalid_value (kind)
    events[2].failure_class = None; // invalid_outcome
    let mut extra = events[0].clone();
    extra.event_id = "nothex".into(); // invalid_id
    events.push(extra);
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["accepted"], 6);
    let codes: Vec<(u64, &str)> = r.body["refused"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["index"].as_u64().unwrap(), x["code"].as_str().unwrap()))
        .collect();
    assert_eq!(
        codes,
        vec![
            (1, "invalid_value"),
            (2, "invalid_outcome"),
            (8, "invalid_id")
        ]
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn envelopes_and_caps_are_enforced_before_storing(pool: PgPool) {
    let e = env(pool).await;
    let r = send(&e.app, post_ingest(&e.ingest_key, "{not json")).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (400, Some("malformed_batch"))
    );

    let v2 = batch(&journey(1, 0)).replace("\"schema_version\":1", "\"schema_version\":2");
    let r = send(&e.app, post_ingest(&e.ingest_key, v2)).await;
    assert_eq!(r.body["error"]["code"], "unsupported_schema_version");

    let bad_source = batch_from("not-an-instance", &journey(1, 0));
    let r = send(&e.app, post_ingest(&e.ingest_key, bad_source)).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (400, Some("invalid_source"))
    );

    let many: Vec<WireEvent> = (0..501u64)
        .map(|i| {
            let mut w = journey(1, i * 10)[0].clone();
            w.event_id = format!("{i:032x}");
            w
        })
        .collect();
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&many))).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (413, Some("too_many_events"))
    );

    let huge = format!("{{\"pad\":\"{}\"}}", "x".repeat(1_100_000));
    let r = send(&e.app, post_ingest(&e.ingest_key, huge)).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (413, Some("payload_too_large"))
    );
    let list = send(&e.app, get("/v1/projects/pilot/operations", &e.reader)).await;
    assert_eq!(
        list.body["operations"],
        json!([]),
        "nothing refused at the envelope was stored"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn duplicate_and_out_of_order_delivery_merge_without_false_chronology(pool: PgPool) {
    let e = env(pool).await;
    let mut events = journey(1, 0);
    let late_half = events.split_off(4);
    // The second half arrives first, then the first half, then everything again.
    let r1 = send(&e.app, post_ingest(&e.ingest_key, batch(&late_half))).await;
    let r2 = send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    let mut all = events.clone();
    all.extend(late_half);
    let r3 = send(&e.app, post_ingest(&e.ingest_key, batch(&all))).await;
    assert_eq!(
        (r1.body["accepted"].as_u64(), r2.body["accepted"].as_u64()),
        (Some(4), Some(4))
    );
    assert_eq!(
        (r3.body["accepted"].as_u64(), r3.body["duplicates"].as_u64()),
        (Some(0), Some(8))
    );

    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(t.status, 200, "{}", t.body);
    let seqs: Vec<u64> = t.body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        seqs,
        (1..=8).collect::<Vec<_>>(),
        "event-time order, not arrival order"
    );
    let first = &t.body["entries"][0];
    assert!(
        first["event_time"].is_string() && first["received_at"].is_string(),
        "both clocks"
    );
    assert_eq!(t.body["coverage"]["events_received"], 8);
    assert_eq!(t.body["coverage"]["missing_known"], 0);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_uncertain_publish_is_shown_as_uncertain_with_retries_and_estimated_cost(pool: PgPool) {
    let e = env(pool).await;
    send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    let b = &t.body;
    assert_eq!(b["status"]["state"], "uncertain");
    assert_eq!(b["status"]["reason"], "timeout_after_send");
    assert_eq!(b["flags"]["external_effect_unknown"], true);
    assert_eq!(b["flags"]["retried_steps"], 1);
    assert_eq!(b["steps"][0]["attempts"][0]["class"], "transport");
    assert_eq!(b["steps"][1]["attempts"][0]["state"], "uncertain");
    assert_eq!(b["cost"]["label"], "estimate");
    assert_eq!(b["cost"]["estimate_micros"], 2_500);
    assert_eq!(
        b["entries"][0]["ingest_key"], "reference-host",
        "provenance per event"
    );

    let list = send(
        &e.app,
        get("/v1/projects/pilot/operations?request_id=req-1", &e.reader),
    )
    .await;
    assert_eq!(list.body["operations"][0]["status"]["state"], "uncertain");
    assert_eq!(list.body["operations"][0]["events_missing"], 0);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn missing_events_are_counted_not_hidden(pool: PgPool) {
    let e = env(pool).await;
    let mut events = journey(1, 0);
    events.remove(3); // lose op_seq 4
    events.truncate(6); // and both finishes
    send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(t.body["status"]["state"], "no_finish_observed");
    assert_eq!(t.body["coverage"]["missing_known"], 1);
    assert_eq!(t.body["coverage"]["missing_unknown"], true);
    let kinds: Vec<&str> = t.body["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["op_seq_hole", "unknown_tail"]);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_flooding_host_is_limited_and_another_host_is_not(pool: PgPool) {
    // One event a second: the second batch cannot be refilled in time, however slowly the
    // first is stored on a loaded machine.
    let e = env_with(pool, |s| {
        s.ingest_buckets = aiops_server::auth::Buckets::new(1, 10);
    })
    .await;
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    assert_eq!(r.status, 200);
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&journey(2, 100)))).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.body["error"]["code"], "rate_limited");
    assert!(r.headers.get("retry-after").is_some());
    // A different host (key) has its own bucket.
    let other = batch_from("22222222222222222222222222222222", &journey(3, 0));
    let r = send(&e.app, post_ingest(&e.other_ingest_key, other)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let stats = send(&e.app, get("/v1/collector/stats", &e.admin)).await;
    assert_eq!(stats.body["rate_limited"], 1);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_host_inventing_names_hits_the_cardinality_cap(pool: PgPool) {
    let e = env_with(pool, |s| s.limits.max_names_per_project = 7).await;
    // journey() introduces op, release, two steps, model, error code and cost basis: 7 names.
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    assert_eq!(r.body["accepted"], 8);
    let mut fresh = journey(2, 100);
    fresh[1].name = Some("step-0001".into());
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&fresh))).await;
    // Past the cap the new name is stored as _overflow, labeled; the event is kept.
    assert_eq!(r.body["refused"], json!([]));
    assert_eq!(r.body["overflowed_names"], 1);
    assert_eq!(r.body["accepted"], 8, "every event still flows");
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(2)),
            &e.reader,
        ),
    )
    .await;
    let started = t.body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["type"] == "step.started")
        .unwrap();
    assert_eq!(started["name"], "_overflow");
    assert_eq!(
        started["overflowed_names"], 1,
        "the rewrite is labeled per event"
    );
    // Another project's cap is its own: its first operation is not affected.
    let other = batch_from("22222222222222222222222222222222", &journey(3, 0));
    let r = send(&e.app, post_ingest(&e.other_ingest_key, other)).await;
    assert_eq!(r.body["accepted"], 8);
    assert_eq!(r.body["overflowed_names"], 0);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_collector_sheds_when_in_flight_is_full(pool: PgPool) {
    let e = env_with(pool, |s| {
        s.ingest_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    })
    .await;
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (503, Some("overloaded"))
    );
    assert_eq!(r.headers.get("retry-after").unwrap(), "1");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn queries_without_scope_or_project_are_denied_and_audited(pool: PgPool) {
    let e = env(pool).await;
    send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    let path = format!("/v1/projects/pilot/operations/{}", op_hex(1));

    let r = send(&e.app, Request::get(&path).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, 401);
    let r = send(&e.app, get(&path, &e.ingest_key)).await;
    assert_eq!(r.status, 401, "an ingest key cannot read");

    let r = send(&e.app, get(&path, &e.no_read)).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (403, Some("forbidden"))
    );
    let r = send(&e.app, get(&path, &e.other_reader)).await;
    assert_eq!(
        r.status, 403,
        "a token for another project cannot read this one"
    );
    let r = send(&e.app, get("/v1/projects/nope/operations", &e.other_reader)).await;
    assert_eq!(
        r.status, 403,
        "an unknown project answers like a forbidden one"
    );

    let audit = audit_rows(&e.store, 10).await;
    let seen: Vec<(&str, &str, &str)> = audit
        .iter()
        .map(|a| (a.actor.as_str(), a.decision.as_str(), a.reason.as_str()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("eve", "denied", "project_not_in_scope"),
            ("eve", "denied", "project_not_in_scope"),
            ("bob", "denied", "scope_missing"),
        ]
    );
    assert_eq!(audit[1].target.as_deref(), Some(op_hex(1).as_str()));
    let ok = send(&e.app, get(&path, &e.reader)).await;
    assert_eq!(ok.status, 200);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_unknown_slug_flood_is_bounded_by_the_query_limit(pool: PgPool) {
    // Every denial writes an audit row synchronously, so a token holder spraying random
    // unknown slugs would otherwise turn denials into an audit-write flood. The bound is
    // the token's own query bucket: a rate-limited request never reaches authorize(), so
    // it writes no row. Audited rows must equal the denials the limiter admitted.
    let e = env_with(pool, |s| {
        s.query_buckets = aiops_server::auth::Buckets::new(1, 5);
    })
    .await;
    let (mut denied, mut limited) = (0usize, 0usize);
    for i in 0..40 {
        let r = send(
            &e.app,
            get(&format!("/v1/projects/nope-{i}/operations"), &e.reader),
        )
        .await;
        match r.status {
            403 => denied += 1,
            429 => limited += 1,
            s => panic!("an unknown slug answered {s}: {}", r.body),
        }
    }
    let audit = audit_rows(&e.store, 100).await;
    assert_eq!(denied + limited, 40);
    assert!(
        audit.len() == denied,
        "every denial audited exactly once; a 429 never audits"
    );
    assert!(
        denied <= 10,
        "burst 5 plus a slow refill admits a few, not a flood: {denied}"
    );
    assert!(limited >= 30, "the limiter refused most of the flood");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_expired_token_is_unauthenticated(pool: PgPool) {
    let e = env(pool).await;
    let t = aiops_server::auth::generate(aiops_server::auth::TOKEN_PREFIX).unwrap();
    let past = common::now().plus_micros(-1);
    e.store
        .create_token(
            "old",
            &["read".into()],
            &["pilot".into()],
            &aiops_server::auth::hash(&t),
            past,
        )
        .await
        .unwrap();
    let r = send(&e.app, get("/v1/projects", &t)).await;
    assert_eq!(r.status, 401);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn attributes_outside_the_allowlist_are_withheld_and_counted(pool: PgPool) {
    let e = env(pool).await;
    let mut events = journey(1, 0);
    let mut attrs = serde_json::Map::new();
    attrs.insert("http.route".into(), "/articles".into());
    attrs.insert("prompt".into(), "write about cats".into());
    events[0].attributes = Some(attrs);
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    assert_eq!(r.body["withheld_attributes"], 1);
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    let first = &t.body["entries"][0];
    assert_eq!(first["withheld_attributes"], 1);
    assert_eq!(first["attributes"][0]["key"], "http.route");
    assert!(!t.body.to_string().contains("cats"));
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn lists_are_project_scoped_and_paged(pool: PgPool) {
    let e = env(pool).await;
    for n in 1..=3u8 {
        send(
            &e.app,
            post_ingest(&e.ingest_key, batch(&journey(n, u64::from(n) * 100))),
        )
        .await;
    }
    let page = send(
        &e.app,
        get("/v1/projects/pilot/operations?limit=2", &e.reader),
    )
    .await;
    assert_eq!(page.body["operations"].as_array().unwrap().len(), 2);
    let next = &page.body["next"];
    let path = format!(
        "/v1/projects/pilot/operations?limit=2&before_time={}&before_id={}",
        next[0].as_str().unwrap().replace('+', "%2B"),
        next[1].as_str().unwrap()
    );
    let page2 = send(&e.app, get(&path, &e.reader)).await;
    assert_eq!(
        page2.body["operations"].as_array().unwrap().len(),
        1,
        "{}",
        page2.body
    );
    let other = send(
        &e.app,
        get("/v1/projects/other/operations", &e.other_reader),
    )
    .await;
    assert_eq!(other.body["operations"], json!([]));
    let projects = send(&e.app, get("/v1/projects", &e.reader)).await;
    assert_eq!(projects.body["projects"], json!([{"slug": "pilot"}]));
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_console_is_served_under_a_hash_based_csp_with_an_spa_fallback(pool: PgPool) {
    let dir = std::env::temp_dir().join(format!("aiops-console-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("index.html"),
        "<html><body><script>start()</script><script src=\"/console/_app/x.js\"></script></body></html>",
    )
    .unwrap();
    let csp = aiops_server::console::content_security_policy(&dir).unwrap();
    let csp_text = csp.to_str().unwrap().to_owned();
    let dir_for_state = dir.clone();
    let e = env_with(pool, move |s| {
        s.config.console_dir = Some(dir_for_state);
        s.console_csp = Some(csp);
    })
    .await;
    // sha256("start()") in base64, computed outside the server's code.
    assert!(
        csp_text.contains("'sha256-DIm7WJS6ZKDYe5qFLPy+h4JFI9Bol5QmYC57mt3Fb00='"),
        "{csp_text}"
    );
    assert!(csp_text.contains("default-src 'none'") && csp_text.contains("frame-ancestors 'none'"));
    assert!(!csp_text.contains("script-src 'self' 'unsafe-inline'"));
    for path in ["/console/", "/console/p/pilot/op/0123"] {
        let res = tower::ServiceExt::oneshot(
            e.app.clone(),
            Request::get(path).body(Body::empty()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), 200, "{path}");
        assert_eq!(
            res.headers()["content-security-policy"],
            csp_text.as_str(),
            "{path}"
        );
        assert_eq!(res.headers()["x-frame-options"], "DENY");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn conflicting_finishes_show_as_conflicting_in_the_list_whether_they_arrive_together_or_apart(
    pool: PgPool,
) {
    let e = env(pool).await;
    // Apart: the honest finish first, a contradicting one (another key) later.
    let events = journey(1, 0);
    send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    let mut contradiction = events[7].clone();
    contradiction.event_id = format!("{:032x}", 0xc0ff_u128);
    contradiction.outcome = Some("succeeded".into());
    contradiction.uncertain_reason = None;
    contradiction.seq = 1;
    contradiction.op_seq = 1;
    contradiction.events_emitted = Some(1);
    let other = batch_from("22222222222222222222222222222222", &[contradiction.clone()]);
    let r = send(&e.app, post_ingest(&e.other_ingest_key_same_project, other)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["accepted"], 1, "{}", r.body);
    // Together: both finishes in one batch.
    let mut together = journey(2, 100);
    let mut second = together[7].clone();
    second.event_id = format!("{:032x}", 0xc1ff_u128);
    second.outcome = Some("succeeded".into());
    second.uncertain_reason = None;
    together.push(second);
    send(&e.app, post_ingest(&e.ingest_key, batch(&together))).await;

    for (op, rid) in [(1u8, "req-1"), (2u8, "req-2")] {
        let list = send(
            &e.app,
            get(
                &format!("/v1/projects/pilot/operations?request_id={rid}"),
                &e.reader,
            ),
        )
        .await;
        assert_eq!(
            list.body["operations"][0]["status"]["state"], "conflicting",
            "list, op {op}"
        );
        let t = send(
            &e.app,
            get(
                &format!("/v1/projects/pilot/operations/{}", op_hex(op)),
                &e.reader,
            ),
        )
        .await;
        assert_eq!(
            t.body["status"]["state"], "conflicting",
            "timeline, op {op}"
        );
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn e1_an_in_batch_twin_cannot_hide_a_missing_event(pool: PgPool) {
    let e = env(pool).await;
    // op_seq 1, 2, 3 and the finish (op_seq 5, events_emitted 5); op_seq 4 is never sent;
    // event 3 appears twice with different outcomes.
    let all = journey(1, 0);
    let mut finish = all[7].clone();
    finish.seq = 5;
    finish.op_seq = 5;
    finish.events_emitted = Some(5);
    let mut twin = all[2].clone();
    twin.failure_class = Some("rejected".into());
    let events = vec![all[0].clone(), all[1].clone(), all[2].clone(), finish, twin];
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    assert_eq!(r.body["accepted"], 4, "{}", r.body);
    assert_eq!(r.body["duplicates"], 0, "{}", r.body);
    assert_eq!(r.body["refused"][0]["index"], 4, "{}", r.body);
    assert_eq!(r.body["refused"][0]["code"], "conflicting_duplicate");
    let list = send(
        &e.app,
        get("/v1/projects/pilot/operations?request_id=req-1", &e.reader),
    )
    .await;
    let row = &list.body["operations"][0];
    assert_eq!(row["events_received"], 4);
    assert_eq!(
        row["events_missing"], 1,
        "the list must agree with the timeline"
    );
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(t.body["coverage"]["missing_known"], 1);
    assert_eq!(
        t.body["sources"][0]["missing_by_seq"], 1,
        "seq 4 never arrived"
    );
}

// Review finding 4: unauthenticated traffic must not shed a legitimate host.
// Multi-threaded, so 64 flooding tasks cannot starve the legitimate request of CPU (a
// single-threaded test runtime measures the harness, not the collector).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_random_key_flood_cannot_shed_a_legitimate_host() {
    let db = common::ScratchDb::new().await;
    let pool = db.pool.clone();
    // The collector as deployed: separate ingest, query, control and auth pools (a single
    // shared test pool would let the flood's lookups compete with ingest for connections).
    let base = (*pool.connect_options()).clone();
    let separate = aiops_store::Store::connect_with(&aiops_store::StoreConfig::new(""), &base)
        .await
        .unwrap();
    let e = env_with(pool, move |s| {
        s.ingest_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
        s.store = separate;
    })
    .await;
    // Warm the legitimate key (it is cached from then on).
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    assert_eq!(r.status, 200);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut flooders = Vec::new();
    for _ in 0..64 {
        let app = e.app.clone();
        let stop = stop.clone();
        flooders.push(tokio::spawn(async move {
            let body = batch(&journey(9, 0));
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let key =
                    aiops_server::auth::generate(aiops_server::auth::INGEST_KEY_PREFIX).unwrap();
                let r = send(&app, post_ingest(&key, body.clone())).await;
                assert!(matches!(r.status, 401 | 503), "flood got {}", r.status);
                // An immediately-ready refusal never yields; on the test's single-threaded
                // runtime the flood would starve everything else.
                tokio::task::yield_now().await;
            }
        }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut shed: Vec<String> = Vec::new();
    for n in 2..=61u8 {
        let r = send(
            &e.app,
            post_ingest(&e.ingest_key, batch(&journey(n, u64::from(n) * 100))),
        )
        .await;
        if r.status != 200 {
            shed.push(format!("{} {}", r.status, r.body["error"]["code"]));
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for f in flooders {
        f.await.unwrap();
    }
    db.drop().await;
    assert_eq!(
        shed,
        Vec::<String>::new(),
        "the legitimate host lost batches to unauthenticated traffic"
    );
}

// Review finding 7: another key of the project cannot write under a host's instance.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn e5_another_key_cannot_fill_a_hosts_gaps_or_inflate_its_counters(pool: PgPool) {
    let e = env(pool).await;
    let mut events = journey(1, 0);
    let lost = events.remove(6); // the uncertain publish finish never arrives
    send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    // A second key of the same project posts the missing op_seq, claiming success, under
    // the first host's instance, with inflated drop counters.
    let mut forged = lost.clone();
    forged.outcome = Some("succeeded".into());
    forged.uncertain_reason = None;
    let body = serde_json::json!({
        "schema_version": 1,
        "source": {"instance_id": common::INSTANCE, "service": "reference-host", "sdk": "test/1"},
        "stats": {"dropped_queue_full": 999_999},
        "events": [forged],
    })
    .to_string();
    let r = send(&e.app, post_ingest(&e.other_ingest_key_same_project, body)).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (409, Some("instance_owned_by_another_key")),
        "{}",
        r.body
    );

    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(
        t.body["coverage"]["missing_known"], 1,
        "the hole stays a hole"
    );
    assert_eq!(
        t.body["flags"]["external_effect_unknown"], true,
        "the publish stays unknown"
    );
    assert_eq!(
        t.body["sources"][0]["reported_dropped_queue_full"], 0,
        "counters untouched"
    );
    assert_eq!(t.body["sources"][0]["ingest_key"], "reference-host");
}

// Review finding 11: one key cannot exhaust the project's name budget, and a name over
// budget never costs the event.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_noisy_key_cannot_exhaust_the_projects_names_and_over_budget_names_keep_their_event(
    pool: PgPool,
) {
    let e = env_with(pool, |s| {
        s.limits.max_names_per_project = 50;
        s.limits.max_names_per_key = 20;
    })
    .await;
    // The noisy host invents 60 step names.
    let noisy: Vec<WireEvent> = (0..60u64)
        .map(|i| {
            let mut w = journey(9, i * 10)[1].clone();
            w.event_id = format!("{:032x}", 0x00ab_c000 + i);
            w.operation_id = format!("{:032x}", 0x00de_f000 + i);
            w.seq = i + 1;
            w.op_seq = 1;
            w.name = Some(format!("junk-{i}"));
            w.model = None;
            w
        })
        .collect();
    let r = send(
        &e.app,
        post_ingest(
            &e.other_ingest_key_same_project,
            batch_from("22222222222222222222222222222222", &noisy),
        ),
    )
    .await;
    assert_eq!(
        r.body["accepted"], 60,
        "no event is refused for its name: {}",
        r.body
    );
    assert_eq!(r.body["refused"], serde_json::json!([]));
    assert_eq!(
        r.body["overflowed_names"], 40,
        "past the key's budget, names are stored as _overflow"
    );

    // The honest host's new names still fit the project.
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    assert_eq!(r.body["accepted"], 8, "{}", r.body);
    assert_eq!(r.body["overflowed_names"], 0, "{}", r.body);
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(t.body["name"], "publish_article");
    assert_eq!(t.body["steps"][0]["name"], "generate");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_source_view_shows_how_many_events_the_collector_refused(pool: PgPool) {
    let e = env(pool).await;
    let mut events = journey(1, 0);
    events[1].kind = Some("llm".into()); // refused: invalid_value
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    assert_eq!(r.body["refused"].as_array().unwrap().len(), 1);
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(
        t.body["sources"][0]["events_refused"], 1,
        "{}",
        t.body["sources"]
    );
}

// Review finding 17: collector-wide counters need the admin scope; others are denied and audited.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn collector_wide_counters_need_the_admin_scope(pool: PgPool) {
    let e = env(pool).await;
    for token in [&e.reader, &e.no_read, &e.other_reader] {
        let r = send(&e.app, get("/v1/collector/stats", token)).await;
        assert_eq!(
            (r.status, r.body["error"]["code"].as_str()),
            (403, Some("forbidden"))
        );
    }
    let r = send(&e.app, get("/v1/collector/stats", &e.admin)).await;
    assert_eq!(r.status, 200);
    let audit = audit_rows(&e.store, 10).await;
    assert_eq!(
        audit
            .iter()
            .filter(|a| a.action == "stats.read" && a.decision == "denied")
            .count(),
        3
    );
}

// Review finding 15: an operation with events from several sources has no single emitted
// count, so the list must not claim "nothing missing" from one source's count.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_list_does_not_claim_completeness_for_an_operation_with_several_sources(pool: PgPool) {
    let e = env(pool).await;
    send(&e.app, post_ingest(&e.ingest_key, batch(&journey(1, 0)))).await;
    // A downstream service (its own instance, the same key) adds two steps to the operation.
    let mut downstream = journey(1, 500);
    let mut a = downstream.remove(1);
    let mut b = downstream.remove(1);
    for (w, n) in [(&mut a, 1u32), (&mut b, 2u32)] {
        w.event_id = format!("dd{n:030x}");
        w.seq = u64::from(n);
        w.op_seq = n;
        w.step_id = Some("3333333333333333".into());
    }
    send(
        &e.app,
        post_ingest(
            &e.ingest_key,
            batch_from("33333333333333333333333333333333", &[a, b]),
        ),
    )
    .await;
    let list = send(
        &e.app,
        get("/v1/projects/pilot/operations?request_id=req-1", &e.reader),
    )
    .await;
    let row = &list.body["operations"][0];
    assert_eq!(row["events_received"], 10);
    assert_eq!(row["multi_source"], true, "{row}");
    assert_eq!(
        row["events_missing"],
        serde_json::Value::Null,
        "one source's count cannot speak for two: {row}"
    );
}

// Review N1: one event carrying U+0000 used to fail the whole batch at the jsonb cast (500,
// every operation lost). It is refused alone, by name; the rest of the batch is stored.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn one_event_with_a_nul_is_refused_alone_and_the_batch_is_stored(pool: PgPool) {
    let e = env(pool).await;
    let mut events = journey(1, 0);
    let mut poisoned = journey(2, 100);
    poisoned[2].error_message = Some("upstream said \u{0}".into());
    events.extend(poisoned);
    events.extend(journey(3, 200));
    let n = events.len();
    let r = send(&e.app, post_ingest(&e.ingest_key, batch(&events))).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["accepted"], n - 1, "{}", r.body);
    let refused = r.body["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{}", r.body);
    assert_eq!(refused[0]["code"], "invalid_value");
    assert_eq!(refused[0]["field"], "error_message");
    for req in ["req-1", "req-2", "req-3"] {
        let list = send(
            &e.app,
            get(
                &format!("/v1/projects/pilot/operations?request_id={req}"),
                &e.reader,
            ),
        )
        .await;
        assert_eq!(
            list.body["operations"].as_array().map(Vec::len),
            Some(1),
            "{req}: {}",
            list.body
        );
    }
}

/// A request body that sends one 1 KiB chunk and then never finishes: a client holding the
/// rest of a large body.
struct Stalled(Option<axum::body::Bytes>);

impl axum::body::HttpBody for Stalled {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match self.0.take() {
            Some(b) => std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(b)))),
            None => std::task::Poll::Pending,
        }
    }
}

fn stalled_ingest(key: &str) -> Request<Body> {
    Request::post("/v1/ingest")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("content-length", "999999")
        .body(Body::new(Stalled(Some(vec![b' '; 1024].into()))))
        .unwrap()
}

// Review N2: an unknown key is refused from its headers; its body is never read, so
// unauthenticated clients cannot make the collector buffer bodies.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_unknown_keys_body_is_never_read(pool: PgPool) {
    let e = env(pool).await;
    let unknown = format!("aiops_ik_{}", "Q".repeat(43));
    let r = tokio::time::timeout(
        Duration::from_secs(3),
        send(&e.app, stalled_ingest(&unknown)),
    )
    .await
    .expect("answered from the headers, without waiting for the body");
    assert_eq!(r.status, 401, "{}", r.body);
}

// Review N2: authenticated bodies are read under a global byte budget; past it a request is
// shed (503) before its body is read.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn bodies_beyond_the_in_flight_byte_budget_are_shed_before_reading(pool: PgPool) {
    let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(2 * 1024));
    let held = budget.clone();
    let e = env_with(pool, move |s| s.ingest_body_budget = held).await;
    let holders: Vec<_> = (0..2)
        .map(|_| {
            let app = e.app.clone();
            let key = e.ingest_key.clone();
            tokio::spawn(async move { send(&app, stalled_ingest(&key)).await })
        })
        .collect();
    // Wait until both held bodies have taken their share (977 KiB each for a 999,999-byte
    // Content-Length; a loaded machine may be slow to start them). 94 KiB stay free, less
    // than the third body needs.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while budget.available_permits() > 2 * 1024 - 2 * 977 {
        assert!(
            std::time::Instant::now() < deadline,
            "the held bodies never took the budget"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = tokio::time::timeout(
        Duration::from_secs(3),
        send(&e.app, stalled_ingest(&e.ingest_key)),
    )
    .await
    .expect("shed at once, not queued behind the held bodies");
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (503, Some("overloaded")),
        "{}",
        r.body
    );
    for h in holders {
        h.abort();
    }
}
