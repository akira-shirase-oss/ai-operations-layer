//! The audit log's read surface, in-process: `GET /v1/admin/audit` is `admin`-scoped,
//! keyset-paged newest-first and filtered — and the read itself is never written back
//! into the log it reads (a denial always is). Metadata only: the wire carries the
//! row's fields and nothing else.
#![expect(
    clippy::unwrap_used,
    reason = "in-process tests: a failed call is a test failure"
)]

mod common;

use aiops_store::AuditEntry;
use axum::body::Body;
use axum::http::Request;
use common::{audit_rows, env, get as get_req, now, send};
use sqlx::PgPool;
use uuid::Uuid;

/// Write one audit row, all fields populated.
async fn seed(e: &common::Env, actor: &'static str, action: &'static str, token_id: Option<Uuid>) {
    e.store
        .audit(&AuditEntry {
            actor,
            token_id,
            action,
            project_slug: Some("pilot"),
            target: Some("t"),
            allowed: true,
            reason: "r",
        })
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn audit_reads_need_admin_scope_and_denials_are_audited(pool: PgPool) {
    let e = env(pool).await;
    let before = audit_rows(&e.store, 50).await.len();

    let r = send(&e.app, get_req("/v1/admin/audit", &e.reader)).await;
    assert_eq!(r.status, 403);
    let r = send(&e.app, get_req("/v1/admin/audit", &e.no_read)).await;
    assert_eq!(r.status, 403);
    let r = send(
        &e.app,
        Request::get("/v1/admin/audit").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(r.status, 401);

    let audits = audit_rows(&e.store, 50).await;
    let denials = audits[before..]
        .iter()
        .filter(|a| a.action == "audit.list" && a.decision == "denied")
        .count();
    assert_eq!(denials, 2, "two scope denials audited: {audits:?}");

    // An allowed read is traffic, not a decision: it must not grow the log it reads.
    let r = send(&e.app, get_req("/v1/admin/audit", &e.admin)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let audits = audit_rows(&e.store, 50).await;
    let reads = audits[before..]
        .iter()
        .filter(|a| a.action == "audit.list")
        .count();
    assert_eq!(reads, 2, "only the two denials, never the read: {audits:?}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_page_is_capped_at_one_hundred(pool: PgPool) {
    let e = env(pool).await;
    for _ in 0..110 {
        seed(&e, "ops", "x.y", None).await;
    }
    let r = send(&e.app, get_req("/v1/admin/audit?limit=500", &e.admin)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let rows = r.body["audit"].as_array().unwrap();
    assert_eq!(rows.len(), 100, "capped at 100");
    assert!(
        r.body["next"].is_array(),
        "a next page exists: {:?}",
        r.body
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_cursor_is_stable_under_concurrent_inserts(pool: PgPool) {
    let e = env(pool).await;
    seed(&e, "a-first", "x.y", None).await;
    seed(&e, "a-second", "x.y", None).await;
    seed(&e, "a-third", "x.y", None).await;

    let r = send(&e.app, get_req("/v1/admin/audit?limit=2", &e.admin)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let page1 = r.body["audit"].as_array().unwrap();
    assert_eq!(page1[0]["actor"], "a-third", "{page1:?}");
    assert_eq!(page1[1]["actor"], "a-second", "{page1:?}");
    let next = r.body["next"].as_array().unwrap();

    // A row that arrives between pages is NEWER than the cursor: it must not shift
    // the next page — no duplicate, no skip.
    seed(&e, "a-fourth", "x.y", None).await;
    let path = format!(
        "/v1/admin/audit?limit=2&before_time={}&before_id={}",
        urlencoded(next[0].as_str().unwrap()),
        next[1].as_str().unwrap()
    );
    let r = send(&e.app, get_req(&path, &e.admin)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let page2 = r.body["audit"].as_array().unwrap();
    let actors: Vec<&str> = page2.iter().filter_map(|r| r["actor"].as_str()).collect();
    assert_eq!(
        actors,
        ["a-first"],
        "the insert must not shift the page: {actors:?}"
    );
}

fn urlencoded(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn filters_select_action_actor_token_and_the_window(pool: PgPool) {
    let e = env(pool).await;
    let tid = Uuid::new_v4();
    seed(&e, "ops", "credentials.list", None).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let t0 = now();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    seed(&e, "ops", "admin.revoke_token", Some(tid)).await;
    seed(&e, "ana", "operations.list", None).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let t1 = now();

    let r = send(
        &e.app,
        get_req("/v1/admin/audit?action=admin.revoke_token", &e.admin),
    )
    .await;
    let rows = r.body["audit"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["action"], "admin.revoke_token");

    let r = send(&e.app, get_req("/v1/admin/audit?actor=ana", &e.admin)).await;
    let rows = r.body["audit"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["actor"], "ana");

    let r = send(
        &e.app,
        get_req(&format!("/v1/admin/audit?token={tid}"), &e.admin),
    )
    .await;
    let rows = r.body["audit"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["token_id"], tid.to_string());

    // The window: [t0, t1) holds only the two later writes.
    let r = send(
        &e.app,
        get_req(
            &format!(
                "/v1/admin/audit?from={}&to={}",
                urlencoded(&t0.to_rfc3339()),
                urlencoded(&t1.to_rfc3339())
            ),
            &e.admin,
        ),
    )
    .await;
    let rows = r.body["audit"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_wire_carries_metadata_only(pool: PgPool) {
    let e = env(pool).await;
    seed(&e, "ops", "x.y", None).await;
    let r = send(&e.app, get_req("/v1/admin/audit", &e.admin)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let top: Vec<&str> = r
        .body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(top, ["audit", "next"], "{top:?}");
    let expected: std::collections::BTreeSet<&str> = [
        "id",
        "at",
        "actor",
        "token_id",
        "action",
        "project_slug",
        "target",
        "decision",
        "reason",
    ]
    .into_iter()
    .collect();
    for row in r.body["audit"].as_array().unwrap() {
        let keys: std::collections::BTreeSet<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, expected, "{row:?}");
    }
    let text = r.body.to_string();
    for forbidden in ["hash", "secret", "digest", "password"] {
        assert!(
            !text.contains(forbidden),
            "{forbidden} material in response"
        );
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_forged_slug_or_target_is_never_stored_verbatim(pool: PgPool) {
    let e = env(pool).await;
    let before = audit_rows(&e.store, 50).await.len();

    // The path's slug and operation id are attacker text: a denial today writes
    // them raw — newlines and bidi into a row an admin later reads.
    let r = send(
        &e.app,
        get_req(
            "/v1/projects/evil%0A%E2%80%AEslug/operations/not-hex%0Ainjected",
            &e.reader,
        ),
    )
    .await;
    assert_eq!(r.status, 403);

    let rows = audit_rows(&e.store, 50).await;
    let forged = rows[before..]
        .iter()
        .find(|a| a.action == "timeline.read" && a.decision == "denied")
        .unwrap_or_else(|| panic!("the denial must be audited: {rows:?}"));
    assert_eq!(forged.project_slug.as_deref(), Some("<invalid>"));
    assert_eq!(forged.target.as_deref(), Some("<invalid>"));
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_half_cursor_and_bad_filters_are_400(pool: PgPool) {
    let e = env(pool).await;
    for path in [
        "/v1/admin/audit?before_time=2026-11-19T00:00:00Z",
        "/v1/admin/audit?before_id=12",
        "/v1/admin/audit?before_time=x&before_id=1",
        "/v1/admin/audit?token=not-a-uuid",
        "/v1/admin/audit?from=not-a-time",
    ] {
        let r = send(&e.app, get_req(path, &e.admin)).await;
        assert_eq!(r.status, 400, "{path}: {:?}", r.body);
    }
}
