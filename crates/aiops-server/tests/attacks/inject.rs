//! Injection attempts: forged slugs and ids carrying log-breaking bytes must be
//! stored only as `<invalid>` or refused outright, and the error envelope must
//! never echo attacker bytes.

use crate::common::{
    audit_rows, client, create_ingest_key, create_project, create_token, get, stack,
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn forged_slugs_and_ids_in_paths_are_never_stored_raw(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);

    // Slugs that would forge an audit line or smuggle a path if stored raw.
    for slug in [
        "evil%0A%80%AEslug",
        "not-a-slug%0Ainjected",
        "pilot%2F..%2Fadmin",
        "PILOT",
        "a b",
        "pilot_",
    ] {
        let (status, _) = get(
            &s.base,
            &format!("/v1/projects/{slug}/operations"),
            Some(&reader),
        )
        .await;
        assert!(
            matches!(status, 400 | 403 | 404),
            "forged slug {slug} answered {status}"
        );
    }
    // Whatever refused them, nothing hostile survived into the audit columns.
    for (_, _, slug, target) in audit_rows(&pool, 50).await {
        if let Some(v) = slug.as_deref().or(target.as_deref()) {
            assert!(
                v == "<invalid>"
                    || v == "pilot"
                    || v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "hostile value stored raw: {v:?}"
            );
        }
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn error_bodies_never_echo_attacker_bytes(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let marker = "9d5f2a8e-marker";

    // The marker lives only where reflection is never legitimate: the request
    // body and the URL path. (A request-id header echoing back is the
    // correlation feature — the sanitize test covers its hostile shapes.)
    let bodies: Vec<String> = vec![
        format!("{{\"schema_version\":1,\"events\":[{{\"event_id\":\"{marker}\"]}}}}"),
        marker.to_owned(),
    ];
    let mut responses = Vec::new();
    for b in bodies {
        let r = client()
            .post(format!("{}/v1/ingest", s.base))
            .bearer_auth(&key)
            .header("content-type", "application/json")
            .body(b)
            .send()
            .await
            .unwrap();
        responses.push(r.text().await.unwrap());
    }
    let r = client()
        .get(format!("{}/v1/projects/{marker}/operations", s.base))
        .send()
        .await
        .unwrap();
    responses.push(r.text().await.unwrap());
    let r = client()
        .get(format!("{}/v1/projects/pilot/operations/{marker}", s.base))
        .send()
        .await
        .unwrap();
    responses.push(r.text().await.unwrap());

    for (i, body) in responses.iter().enumerate() {
        assert!(
            !body.contains(marker),
            "error body {i} echoed attacker bytes: {body}"
        );
    }
    // The ledger re-read: nothing stored, nothing audited (every call above
    // was a client error that never reached authorize).
    assert_eq!(crate::common::table_count(&pool, "events").await, 0);
    assert_eq!(crate::common::audit_rows(&pool, 20).await, Vec::new());
}
