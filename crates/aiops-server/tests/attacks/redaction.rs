//! Typed-field redaction policy (`docs/REDACTION-POLICY.md`): a credential-named
//! attribute key is withheld whatever its value's shape and whatever the
//! caller opted in; an unknown key is withheld by default; a field the event
//! type does not carry is refused; keyed text inside free text still loses
//! the value; and a credential-shaped identifier is refused, never rewritten.

use crate::common::{
    cli_status, create_ingest_key, create_project, post_ingest, stack, table_count,
};
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
    // the opt-in list. (The CLI itself refuses credential-named keys now —
    // round 6, B1 — so this opt-in is written behind its back.)
    sqlx::query("UPDATE projects SET opt_in_attribute_keys = '{password}'")
        .execute(&pool)
        .await
        .unwrap();
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
    // The refusal must name the code and the field, not just the status
    // (review round 6, B3).
    assert_eq!(body["error"]["code"], "invalid_source", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("source.service"),
        "the refusal must name the field: {body}"
    );
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
    // Exactly 32 bytes including the `/` — the interior run is 31, so only
    // the `/`-opening fix catches it.
    events[2].error_message = Some("x /AbCdEf1234567890aBcDeF123456789 y".to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        !stored.contains("AbCdEf1234567890aBcDeF123456789"),
        "a leading-`/` token survived: {stored}"
    );
}

/// Review round 6, A1: a `%25`-nest deeper than `DECODE_PASSES` fails
/// closed on the WHOLE field — the span-level version stored the value
/// beside a hidden separator (`password[redacted]hunter2` still carries
/// the credential). Both the bare residue and the keyed shapes die whole.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_nested_escape_erases_the_whole_field(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    for (op, msg, must_die) in [
        (0x34u8, "x %2525252541 y", "x %2525252541 y"),
        (0x35, "key password%252525253Ahunter2 end", "hunter2"),
        (0x36, "key passw%25252525256Frd=hunter2 end", "hunter2"),
    ] {
        let mut events = journey(op, 0);
        events[2].error_message = Some(msg.to_owned());
        let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
        assert_eq!(status, 200, "{body}");
        let stored = stored_bodies(&pool).await;
        assert!(
            stored.contains("[redacted]"),
            "no fail-closed marker for {msg:?}: {stored}"
        );
        assert!(
            !stored.contains(must_die),
            "{must_die:?} survived the residual span — the field did not die whole: {stored}"
        );
        // The keyed shapes must not leave the credential's halves joined
        // around a marker either.
        assert!(
            !stored.contains("password"),
            "the hidden key prefix stored beside the marker: {stored}"
        );
    }
}

/// Review round 6, A4: a bare ≥32-hex value is a credential shape in every
/// identifier field EXCEPT the two declared to carry hex (`release`,
/// `request_id`). Twilio auth tokens and Datadog API keys wear exactly this
/// shape; the refusals must name the code and the field.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_bare_hex_credential_dies_outside_release_and_request_id(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let hex32 = "0123456789abcdef0123456789abcdef";

    // Per-event identifier fields: the refusal lands in `refused[]` with
    // the code and the field named.
    for (op, set, field) in [
        (0x37u8, "deployment", "deployment"),
        (0x38, "model", "model"),
        (0x39, "error_code", "error_code"),
    ] {
        let mut events = journey(op, 0);
        match set {
            "deployment" => events[0].deployment = Some(hex32.to_owned()),
            "model" => events[1].model = Some(hex32.to_owned()),
            _ => events[2].error_code = Some(hex32.to_owned()),
        }
        let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
        assert_eq!(status, 200, "{field}: {body}");
        assert!(
            body["refused"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| { r["code"] == "invalid_value" && r["field"] == field }),
            "{field} must refuse the bare hex run by name: {body}"
        );
    }
    // `cost_estimate.basis` — an identifier field under a nested object.
    let mut events = journey(0x3a, 0);
    events[4].cost_estimate = Some(aiops_domain::wire::WireCost {
        micros: 1,
        currency: "USD".into(),
        basis: hex32.to_owned(),
    });
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["refused"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| { r["code"] == "invalid_value" && r["field"] == "cost_estimate.basis" }),
        "cost_estimate.basis must refuse the bare hex run by name: {body}"
    );

    // `source.service` — a batch-level identifier: the whole batch is
    // refused and the envelope names the field.
    let (status, body, _) = post_ingest(
        &s.base,
        Some(&key),
        crate::shared::batch_from("11111111111111111111111111111111", &journey(0x3b, 0))
            .replace("reference-host", hex32),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_source", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("source.service"),
        "the envelope must name the field: {body}"
    );

    // …while `release` and `request_id` still store the same shape verbatim.
    let mut events = journey(0x3c, 0);
    events[0].release = Some(hex32.to_owned());
    events[0].request_id = Some(hex32.to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        stored.contains(hex32),
        "release/request_id must keep bare hex: {stored}"
    );
}

/// Review round 6, A3: `code` is a credential only after a qualifier —
/// `http.status_code` is a default attribute, `status_code:404` an
/// identifier, `status code: 503` ordinary text. Qualified `code` keys
/// (auth, verification, recovery, otp, mfa, sms, reset) stay credential.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_default_attribute_surface_survives_code(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    // The default attribute stores — `code` inside its name is not a
    // credential part.
    let mut events = journey(0x3d, 0);
    events[0].attributes = json!({"http.status_code": "503", "error.type": "http"})
        .as_object()
        .cloned();
    events[2].error_message = Some("upstream answered status code: 503".to_owned());
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["withheld_attributes"].as_u64().unwrap(),
        0,
        "a default attribute must not be withheld: {body}"
    );
    let stored = stored_bodies(&pool).await;
    assert!(
        stored.contains("503"),
        "http.status_code dropped its value: {stored}"
    );
    assert!(
        stored.contains("status code: 503"),
        "ordinary text redacted on a bare `code`: {stored}"
    );

    // `status_code:404` is an identifier — a port tail under a
    // non-credential key.
    let (status, body, _) = post_ingest(
        &s.base,
        Some(&key),
        crate::shared::batch_from("11111111111111111111111111111111", &journey(0x3e, 0))
            .replace("reference-host", "status_code:404"),
    )
    .await;
    assert_eq!(status, 200, "status_code:404 refused: {body}");

    // …and the qualified `code` keys stay credential: withheld as an
    // attribute key, refused as an identifier.
    let mut events = journey(0x3f, 0);
    events[0].attributes = json!({"auth_code": "123456", "verification.code": "998877"})
        .as_object()
        .cloned();
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["withheld_attributes"].as_u64().unwrap(),
        2,
        "qualified code keys must be withheld: {body}"
    );
    for service in ["auth_code:123456", "otp-code:998877", "reset.code:4421"] {
        let (status, body, _) = post_ingest(
            &s.base,
            Some(&key),
            crate::shared::batch_from("11111111111111111111111111111111", &journey(0x40, 0))
                .replace("reference-host", service),
        )
        .await;
        assert_eq!(status, 400, "{service} admitted: {body}");
        assert_eq!(body["error"]["code"], "invalid_source", "{body}");
    }
}

/// Review round 6, B1: `set-opt-in` refuses credential-named keys instead
/// of reporting success on an opt-in that can never apply; `pass`/`cred*`
/// parts and any `*key` part (except the safe list) name credentials in
/// attributes, identifiers and keyed free text.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn credential_named_keys_die_everywhere_opt_in_cannot_reach(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");

    // The CLI refuses credential-named opt-in keys outright.
    for keys in ["password", "stripe_key", "db_pass", "creds", "hmac_key"] {
        let (ok, out, err) =
            cli_status(&s.url, &["admin", "set-opt-in", "--project", "pilot", keys]);
        assert!(!ok, "set-opt-in {keys} succeeded: {out}");
        assert!(
            err.contains(keys) || out.contains(keys),
            "the refusal must name the key: {err}"
        );
    }
    // Even an opt-in row written behind the CLI's back withholds them —
    // the closed rule beats the stored list.
    sqlx::query("UPDATE projects SET opt_in_attribute_keys = '{stripe_key,db_pass,hmac_key,business_key,cache_key}'")
        .execute(&pool)
        .await
        .unwrap();

    let key = create_ingest_key(&s.url, "pilot", "h");
    let mut events = journey(0x41, 0);
    events[0].attributes = json!({
        "stripe_key": "sk_live_123",
        "db_pass": "hunter2",
        "hmac_key": "aabbcc",
        "business_key": "bk-9",
        "cache_key": "user:42",
    })
    .as_object()
    .cloned();
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["withheld_attributes"].as_u64().unwrap(),
        3,
        "the opted-in credential keys must still be withheld: {body}"
    );
    let stored = stored_bodies(&pool).await;
    for kept in ["bk-9", "user:42"] {
        assert!(stored.contains(kept), "safe-list value dropped: {stored}");
    }
    for died in ["sk_live_123", "hunter2", "aabbcc"] {
        assert!(!stored.contains(died), "credential value stored: {stored}");
    }

    // Keyed free text: `stripe_key:` and `db_pass:` name credentials —
    // their values die; `business_key:`/`cache_key:` keep.
    let mut events = journey(0x42, 0);
    events[2].error_message = Some(
        "cfg stripe_key: sk_live_123 db_pass: hunter2 business_key: bk-9 cache_key: user:42"
            .to_owned(),
    );
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
    assert_eq!(status, 200, "{body}");
    let stored = stored_bodies(&pool).await;
    assert!(
        !stored.contains("sk_live_123") && !stored.contains("hunter2"),
        "credential value kept in free text: {stored}"
    );
    assert!(
        stored.contains("bk-9") && stored.contains("user:42"),
        "safe-list value redacted: {stored}"
    );

    // Identifiers: a credential-named key refuses (status + code + field);
    // a safe-list name keeps — `cache_key:user:42` is `host:port` text, not
    // a credential pair. Each post draws a fresh seq range so a stored
    // batch can't alias a later one as a duplicate (review round 6, B3).
    for (n, (service, want)) in [
        ("stripe_key:abc", 400u16),
        ("openai_key:abc", 400),
        ("accesskey:abc", 400),
        ("pgpass:x", 400),
        ("business_key:bk-9", 200),
        ("cache_key:user:42", 200),
    ]
    .iter()
    .enumerate()
    {
        let (status, body, _) = post_ingest(
            &s.base,
            Some(&key),
            crate::shared::batch_from(
                "11111111111111111111111111111111",
                &journey(0x43, n as u64 * 100),
            )
            .replace("reference-host", service),
        )
        .await;
        assert_eq!(status, *want, "{service}: {body}");
        if *want == 400 {
            assert_eq!(body["error"]["code"], "invalid_source", "{service}: {body}");
        }
    }
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
/// refuse; `pin`, `otp`, `passcode`, `mfa` are credential keys, `code`
/// only after a qualifier (round 6, A3); `auth:`/`authorization:` carry
/// no exemption.
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
        ("auth_code:ab12cd", "qualified code is a credential key"),
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
        assert_eq!(
            body["error"]["code"], "invalid_source",
            "{why} — the envelope must carry the code: {body}"
        );
    }
    assert_eq!(table_count(&pool, "events").await, 0);
    // …while a real port tail, a state word, and a bare `code` key keep.
    for service in [
        "session-api:8080",
        "token:expired",
        "auth:login",
        "code:ab12cd",
        "status_code:404",
    ] {
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
