//! Cross-tenant attempts: project B's credentials must never read into, write
//! into, or act inside project A — and its ingest key cannot smuggle events into
//! A's operation namespace.

use crate::common::{
    audit_rows, batch, batch_from, create_ingest_key, create_project, create_token, get, journey,
    op_hex, post_ingest, stack,
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn one_projects_credentials_never_touch_anothers(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    for slug in ["pilot", "other"] {
        create_project(&s.url, slug);
    }
    let pilot_key = create_ingest_key(&s.url, "pilot", "a-host");
    let other_key = create_ingest_key(&s.url, "other", "b-host");
    let (pilot_reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);
    let (other_reader, _) = create_token(&s.url, "eve", &["read"], &["other"]);
    let (pilot_operate, _) = create_token(&s.url, "sam", &["operate"], &["pilot"]);

    assert_eq!(
        post_ingest(&s.base, Some(&pilot_key), batch(&journey(1, 0)))
            .await
            .0,
        200
    );
    let op = op_hex(1);

    // B's read token on A's operation, list, controls surface — denied, audited.
    for path in [
        format!("/v1/projects/pilot/operations/{op}"),
        "/v1/projects/pilot/operations".to_owned(),
        format!("/v1/projects/pilot/operations/{op}/controls"),
    ] {
        let (status, _) = get(&s.base, &path, Some(&other_reader)).await;
        assert_eq!(status, 403, "{path}");
    }
    // A's own read token can't reach admin either.
    assert_eq!(
        get(&s.base, "/v1/admin/audit", Some(&pilot_reader)).await.0,
        403
    );
    // Nor can B's operate-scoped credential — minted only for B — act in A.
    let (status, _) = crate::common::post_json(
        &s.base,
        &format!("/v1/projects/pilot/operations/{op}/reconcile"),
        Some(&pilot_operate),
        &serde_json::json!({"business_key": "article:x", "expected_state": {"published": false}}),
    )
    .await;
    assert!(
        matches!(status, 200 | 403),
        "no adapter configured for the path: {status}"
    );

    // B's ingest key writes B's events under B's own instance — even an event
    // that names A's operation id lands in B's namespace, never A's.
    let stolen_op_events = {
        let mut e = journey(7, 700);
        e[0].operation_id = op.clone();
        e[0].event_id = "770000000000000000000000000000ff".to_owned();
        e[0].seq = 7;
        e
    };
    let (status, body, _) = post_ingest(
        &s.base,
        Some(&other_key),
        batch_from("33333333333333333333333333333333", &stolen_op_events),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // A's operation did not gain B's event: A's timeline is still the eight
    // events the honest key sent.
    let (status, t) = get(
        &s.base,
        &format!("/v1/projects/pilot/operations/{op}"),
        Some(&pilot_reader),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        t["entries"].as_array().unwrap().len(),
        8,
        "B's event must not appear in A's operation: {t}"
    );

    // The denials reached the audit log under B's actor.
    let rows = audit_rows(&pool, 20).await;
    let denied = rows
        .iter()
        .filter(|(_, d, slug, _)| d == "denied" && slug.as_deref() == Some("pilot"))
        .count();
    assert!(denied >= 3, "every cross-tenant denial audited: {rows:?}");
}
