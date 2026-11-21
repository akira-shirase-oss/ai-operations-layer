//! Typed-field redaction policy (`docs/REDACTION-POLICY.md`): a credential-named
//! attribute key is withheld whatever its value's shape and whatever the
//! caller opted in; an unknown key is withheld by default; a field the event
//! type does not carry is refused; keyed text inside free text still loses
//! the value; and a credential-shaped identifier is refused, never rewritten.

use crate::common::{cli, create_ingest_key, create_project, post_ingest, stack, table_count};
use crate::shared::{batch, journey};
use serde_json::{Value, json};
use sqlx::PgPool;

/// The stored event bodies as one JSON text — the ledger read-back.
async fn stored_bodies(pool: &PgPool) -> String {
    let rows: Vec<String> = sqlx::query_scalar("SELECT body::text FROM events")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.join("\n")
}

fn attributes(map: serde_json::Map<String, Value>) -> Option<serde_json::Map<String, Value>> {
    Some(map)
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_credential_key_is_withheld_even_when_opted_in(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    // The caller opted `password` in — the closed credential rule wins, not
    // the opt-in list.
    let (out, err) = cli(
        &s.url,
        &["admin", "set-opt-in", "--project", "pilot", "password"],
    );
    assert!(out.contains("password"), "{out} / {err}");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut events = journey(0x21, 0);
    // `hunter2` is not credential-shaped: only the key's name may decide.
    // `api_token` was never opted in: a credential key dies regardless.
    events[0].attributes = attributes(
        json!({"password": "hunter2", "api_token": "hunter2", "business_key": "bk-1"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["withheld_attributes"].as_u64().unwrap(),
        2,
        "both credential keys must be withheld: {body}"
    );
    let stored = stored_bodies(&pool).await;
    assert!(stored.contains("business_key"), "safe key kept: {stored}");
    assert!(
        !stored.contains("hunter2"),
        "credential value stored: {stored}"
    );
    assert!(
        !stored.contains("api_token"),
        "credential key stored: {stored}"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_unknown_attribute_key_is_withheld(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut events = journey(0x22, 0);
    events[0].attributes = attributes(
        json!({"zzz_unclassified": "visible-me", "tool.name": "search"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["withheld_attributes"].as_u64().unwrap(), 1, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        stored.contains("tool.name"),
        "allowlisted key kept: {stored}"
    );
    assert!(
        !stored.contains("visible-me"),
        "unknown-key value stored: {stored}"
    );
    assert!(
        !stored.contains("zzz_unclassified"),
        "unknown key stored: {stored}"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_field_the_event_type_does_not_carry_is_refused(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut events = journey(0x23, 0);
    // `operation.finished` carries no `name` — an unclassified field on the
    // type is `field_not_allowed`, never silently dropped.
    let finished = events
        .iter_mut()
        .find(|e| e.event_type == "operation.finished")
        .unwrap();
    finished.name = Some("sneaked-in".to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let refused = body["refused"].as_array().unwrap();
    assert!(
        refused
            .iter()
            .any(|r| r["code"] == "field_not_allowed" && r["field"] == "name"),
        "the extra field must be refused by name: {body}"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn keyed_text_inside_free_text_loses_the_value(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut events = journey(0x24, 0);
    // `error_message` is free text — `key=value` inside it has no schema key,
    // so the backstop must still strip the value.
    events[2].error_message = Some("upload failed: password=hunter2 retry later".to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        !stored.contains("hunter2"),
        "keyed value kept in free text: {stored}"
    );
    assert!(
        stored.contains("[redacted]"),
        "backstop did not mark the span: {stored}"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_credential_shaped_identifier_is_refused_not_rewritten(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    // `source.service` is an identifier field: a credential-keyed value is
    // refused whole, never stored raw and never `[redacted]`-rewritten.
    let (status, body, _) = post_ingest(
        &s.base,
        Some(&key),
        crate::shared::batch_from("11111111111111111111111111111111", &journey(0x25, 0))
            .replace("reference-host", "password:hunter2"),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(table_count(&pool, "events").await, 0, "{body}");
}
