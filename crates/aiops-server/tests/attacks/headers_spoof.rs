//! Header spoofing: forwarded-for games can't move the per-token bucket,
//! conflicting Authorization lines don't smuggle a second credential, and a
//! hostile X-Request-Id never reaches the wire or the log.

use crate::common::{
    batch, client, create_ingest_key, create_project, create_token, journey, stack,
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn forwarded_headers_do_not_move_the_per_token_bucket(pool: PgPool) {
    // A one-event-per-second bucket: if the limiter keyed on a spoofable IP
    // header, rotating it would admit a flood.
    let s = stack(
        &pool,
        &[
            ("AIOPS_INGEST_EVENTS_PER_SEC", "1"),
            ("AIOPS_INGEST_BURST_EVENTS", "3"),
        ],
    )
    .await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");

    let mut limited = 0u32;
    for i in 0..6u8 {
        let r = client()
            .post(format!("{}/v1/ingest", s.base))
            .bearer_auth(&key)
            .header("content-type", "application/json")
            .header("x-forwarded-for", format!("203.0.113.{i}"))
            .header("x-real-ip", format!("198.51.100.{i}"))
            .header("forwarded", format!("for=198.51.100.{i}"))
            .body(batch(&journey(i + 1, u64::from(i) * 100)))
            .send()
            .await
            .unwrap();
        if r.status().as_u16() == 429 {
            limited += 1;
        }
    }
    assert!(
        limited >= 3,
        "rotating spoofed IPs must not reset the per-key bucket: {limited}/6 limited"
    );
    // The ledger re-read: burst 3 < one batch's 8 events, so every refused
    // batch must have stored exactly nothing.
    assert_eq!(
        crate::common::table_count(&pool, "events").await,
        0,
        "refused batches must not have written events"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn conflicting_authorization_lines_have_one_verdict(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);
    let path = "/v1/projects/pilot/operations";

    // A second Authorization header never weakens or strengthens the first:
    // valid-then-garbage must behave exactly like valid alone.
    let r = client()
        .get(format!("{}{path}", s.base))
        .header("authorization", format!("Bearer {reader}"))
        .header("authorization", "Bearer garbage")
        .send()
        .await
        .unwrap();
    let first_wins = r.status().as_u16();
    let solo = client()
        .get(format!("{}{path}", s.base))
        .bearer_auth(&reader)
        .send()
        .await
        .unwrap();
    assert_eq!(first_wins, solo.status().as_u16(), "one credential decides");

    // And a garbage line first is not rescued by a valid line second — the
    // first header decides, and the first here is garbage.
    let r = client()
        .get(format!("{}{path}", s.base))
        .header("authorization", "Bearer garbage")
        .header("authorization", format!("Bearer {reader}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        401,
        "a later good credential never rescues an earlier bad one"
    );
    // And none of those 401s reached authorize — the ledger stays empty.
    assert_eq!(
        crate::common::audit_rows(&pool, 20).await,
        Vec::new(),
        "an unauthenticated request never audits"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_hostile_request_id_never_reaches_the_wire(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);
    let path = "/v1/projects/pilot/operations";

    // Only header-legal hostiles reach the server — a client cannot express
    // control bytes in a header value, so the attack surface is length and
    // charset, not CR/LF smuggling.
    for bad in [
        "x".repeat(200),
        "a\tb".to_owned(),
        "a b c".to_owned(),
        "../../etc/passwd".to_owned(),
        "id; rm -rf /".to_owned(),
        "<script>alert(1)</script>".to_owned(),
    ] {
        let r = client()
            .get(format!("{}{path}", s.base))
            .bearer_auth(&reader)
            .header("x-request-id", &bad)
            .send()
            .await
            .unwrap();
        // The request still answers, but the hostile id was stripped before
        // anything could echo it — the response header is absent or the
        // sanitized id, never the bytes sent.
        let echoed = r
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_ne!(echoed, bad, "the hostile id must not echo");
        assert!(
            echoed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'),
            "any echoed id is charset-clean: {echoed:?}"
        );
    }
    // Ledger re-read: every request above was a 200 — nothing the hostile
    // headers tried produced an audited denial either.
    assert_eq!(
        crate::common::audit_rows(&pool, 50).await,
        Vec::new(),
        "hostile headers produced no audited denial"
    );
}
