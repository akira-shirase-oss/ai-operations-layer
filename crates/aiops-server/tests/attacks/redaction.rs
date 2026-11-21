//! Typed-field redaction policy (`docs/REDACTION-POLICY.md`): a credential-named
//! attribute key is withheld whatever its value's shape and whatever the
//! caller opted in; an unknown key is withheld by default; a field the event
//! type does not carry is refused; keyed text inside free text still loses
//! the value; and a credential-shaped identifier is refused, never rewritten.

use crate::common::{cli, create_ingest_key, create_project, post_ingest, stack, table_count};
use crate::shared::{batch, journey};
use serde_json::json;
use sqlx::PgPool;

/// The stored event bodies as one JSON text — the ledger read-back.
async fn stored_bodies(pool: &PgPool) -> String {
    let rows: Vec<String> = sqlx::query_scalar("SELECT body::text FROM events")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.join("\n")
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
    events[0].attributes = json!({
        "password": "hunter2",
        "api_token": "hunter2",
        "business_key": "bk-1"
    })
    .as_object()
    .cloned();
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
    events[0].attributes = json!({"zzz_unclassified": "visible-me", "tool.name": "search"})
        .as_object()
        .cloned();
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

/// Review round 5, finding 1: the N-J scanner skip must not swallow the
/// position after a leading `_` — `_AKIA…_` is how a key rides Markdown
/// italics in model output. All three wrapped shapes die in free text.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_underscore_wrapped_token_dies_in_free_text(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    for (op, msg, must_die) in [
        (
            0x30u8,
            "key _AKIAIOSFODNN7EXAMPLE_ used",
            "AKIAIOSFODNN7EXAMPLE",
        ),
        (
            0x31,
            "x _glpat-AbCdEfGhIjKlMnOpQrSt_ y",
            "glpat-AbCdEfGhIjKlMnOpQrSt",
        ),
        (
            0x32,
            "x _0123456789abcdef0123456789abcdef_ y",
            "0123456789abcdef0123456789abcdef",
        ),
    ] {
        let mut events = journey(op, 0);
        events[2].error_message = Some(msg.to_owned());
        let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
        assert_eq!(status, 200, "{body}");
        let stored = stored_bodies(&pool).await;
        assert!(
            !stored.contains(must_die),
            "{must_die:?} survived inside underscores: {stored}"
        );
        assert!(
            stored.contains("[redacted]"),
            "no redaction marker for {msg:?}: {stored}"
        );
    }
}

/// Review round 5, finding 8: a standard-base64 token may LEAD with `/`
/// (1 in 64 unpadded 24-byte draws). `/` must open a run or the 31-char
/// tail survives.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_leading_slash_token_dies_in_free_text(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut events = journey(0x33, 0);
    events[2].error_message = Some("x /AbCdEf1234567890aBcDeF1234567890a y".to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        !stored.contains("AbCdEf1234567890aBcDeF1234567890a"),
        "a leading-`/` token survived: {stored}"
    );
}

/// Review round 5, finding 6: a `%25`-nest deeper than `DECODE_PASSES` fails
/// closed on its leftover span — the text around it is not the credential
/// and must survive.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_nested_escape_redacts_only_its_span(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut events = journey(0x34, 0);
    events[2].error_message = Some("x %2525252541 y".to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        stored.contains("x [redacted] y"),
        "the leftover span alone must be redacted, not the field: {stored}"
    );
}

/// Review round 5, finding 4: a commit SHA is a `release`, a hex id is a
/// `request_id` — the ≥32-hex credential rule fires only under a
/// credential KEY (the typed-field policy), never bare. Both must store
/// verbatim.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_commit_sha_and_a_hex_trace_id_are_identifiers(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut events = journey(0x35, 0);
    events[0].release = Some("76fc524aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned());
    events[0].request_id = Some("0123456789abcdef0123456789abcdef".to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        stored.contains("76fc524aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        "the SHA release was refused or redacted: {stored}"
    );
    assert!(
        stored.contains("0123456789abcdef0123456789abcdef"),
        "the hex request_id was refused or redacted: {stored}"
    );
}

/// Review round 5, finding 5: the `:`-tail keep is a port/state rule, not
/// a PIN rule — `password:1234-5678`, `password:4821`, `token:12345` all
/// refuse; `pin`, `otp`, `passcode`, `code`, `mfa` are credential keys;
/// `auth:`/`authorization:` carry no exemption.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn pin_tails_and_new_credential_keys_still_refuse(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    for (service, why) in [
        ("password:1234-5678", "a two-piece PIN tail is not a port"),
        ("password:4821", "a four-digit PIN is not a port"),
        ("token:12345", "a five-digit PIN is not a port"),
        ("pin:1234", "pin is a credential key"),
        ("otp:123456", "otp is a credential key"),
        ("passcode:998877", "passcode is a credential key"),
        ("code:ab12cd", "code is a credential key"),
        ("mfa:totp-x", "mfa is a credential key"),
        ("auth:hunter2", "auth is not exempt"),
    ] {
        let (status, body, _) = post_ingest(
            &s.base,
            Some(&key),
            crate::shared::batch_from("11111111111111111111111111111111", &journey(0x36, 0))
                .replace("reference-host", service),
        )
        .await;
        assert_eq!(status, 400, "{why} admitted: {body}");
    }
    assert_eq!(table_count(&pool, "events").await, 0);
    // …while a real port tail and a state word still keep.
    for service in ["session-api:8080", "token:expired", "auth:login"] {
        let (status, body, _) = post_ingest(
            &s.base,
            Some(&key),
            crate::shared::batch_from("11111111111111111111111111111111", &journey(0x37, 0))
                .replace("reference-host", service),
        )
        .await;
        assert_eq!(status, 200, "{service} refused: {body}");
    }
}
