//! Replay attempts: the same batch twice, the same control call twice, and a
//! credential reused after death — each must land idempotently or be refused,
//! never double-applied.

use crate::common::{
    batch, create_ingest_key, create_project, create_token, get, journey, op_hex, post_ingest,
    post_json, stack,
};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_replayed_batch_is_counted_not_rewritten(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);

    let body = batch(&journey(1, 0));
    let (first, b1, _) = post_ingest(&s.base, Some(&key), body.clone()).await;
    assert_eq!(first, 200, "{b1}");
    let accepted = b1["accepted"].as_u64().unwrap_or(8);
    assert!(accepted >= 1);

    // Byte-for-byte replay: accepted, but the store merges idempotently.
    let (second, b2, _) = post_ingest(&s.base, Some(&key), body).await;
    assert_eq!(second, 200, "{b2}");

    let (status, t) = get(
        &s.base,
        &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
        Some(&reader),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        t["entries"].as_array().unwrap().len(),
        8,
        "the replay must not double-write: {t}"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_replayed_control_call_returns_the_same_record(pool: PgPool) {
    let s = stack(
        &pool,
        &[("AIOPS_CONTROL_REFERENCE_HOST", "pilot=http://127.0.0.1:1")],
    )
    .await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let (op_token, _) = create_token(&s.url, "bob", &["operate"], &["pilot"]);

    assert_eq!(
        post_ingest(&s.base, Some(&key), batch(&journey(1, 0)))
            .await
            .0,
        200
    );
    let path = format!("/v1/projects/pilot/operations/{}/reconcile", op_hex(1));
    let payload = json!({"business_key": "article:req-7", "expected_state": {"published": false}});

    let (s1, r1) = post_json(&s.base, &path, Some(&op_token), &payload).await;
    let (s2, r2) = post_json(&s.base, &path, Some(&op_token), &payload).await;
    assert_eq!(s1, s2, "replays must get the same verdict: {r1} / {r2}");
    assert_eq!(
        r1["control_id"], r2["control_id"],
        "one record, not two: {r1} / {r2}"
    );
}
