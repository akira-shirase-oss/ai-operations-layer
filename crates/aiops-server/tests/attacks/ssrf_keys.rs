//! SSRF and path injection through the control surface: `business_key` and
//! `operation_id` are the only things between the caller's text and the
//! adapter's outbound URL — forged keys must die before any request leaves,
//! and a recording stub proves none did.

use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::extract::{Request, State};
use axum::routing::any;
use serde_json::json;
use sqlx::PgPool;

use crate::common::{client, create_ingest_key, create_project, create_token, post_json, stack};

/// A host that only counts what it was asked for.
async fn recording_stub() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let app = Router::new()
        .fallback(any(
            |State(s): State<Arc<Mutex<Vec<String>>>>, req: Request| async move {
                s.lock()
                    .unwrap()
                    .push(format!("{} {}", req.method(), req.uri()));
                axum::Json(serde_json::json!({"published": false}))
            },
        ))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), seen)
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_forged_business_key_never_reaches_the_authority(pool: PgPool) {
    let (stub, seen) = recording_stub().await;
    let s = stack(
        &pool,
        &[
            ("AIOPS_CONTROL_REFERENCE_HOST", &format!("pilot={stub}")),
            ("AIOPS_CONTROL_REQUESTS_PER_SEC", "200"),
            ("AIOPS_CONTROL_BURST", "200"),
        ],
    )
    .await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let (op_token, _) = create_token(&s.url, "bob", &["operate"], &["pilot"]);

    // Seed the operation the control calls hang off.
    let _ = client()
        .post(format!("{}/v1/ingest", s.base))
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .body(crate::common::batch(&crate::common::journey(1, 0)))
        .send()
        .await
        .unwrap();
    let path = format!(
        "/v1/projects/pilot/operations/{}/reconcile",
        crate::common::op_hex(1)
    );
    let expected = json!({"published": false});

    // Every injection shape: traversal, absolute path, encoded traversal,
    // URL-structural bytes, userinfo, whitespace, unicode.
    for key in [
        "article:req-7/../../admin",
        "article:req-7/../../../etc/passwd",
        "article:req-7%2F..%2Fadmin",
        "article:req-7%252fadmin",
        "article:/etc/passwd",
        "article:req-7?debug=true",
        "article:req-7#frag",
        "article:req-7@internal:8080",
        "article:req-7:8080",
        "article:req 7",
        "article:req-7\nGET /admin",
        "article:req-\u{202e}7",
        "article:",
        "article:.",
        "article:..",
        "artical:req-7",
        &format!("article:{}", "x".repeat(300)),
    ] {
        let (status, body) = post_json(
            &s.base,
            &path,
            Some(&op_token),
            &json!({"business_key": key, "expected_state": expected}),
        )
        .await;
        assert_eq!(status, 400, "{key}: {body}");
        assert_eq!(body["error"]["code"], "bad_request", "{key}: {body}");
    }
    let calls = seen.lock().unwrap().clone();
    assert_eq!(
        calls,
        Vec::<String>::new(),
        "no forged key may produce an outbound call: {calls:?}"
    );

    // The honest key still reaches the stub — the gate rejects, not the route.
    let (status, body) = post_json(
        &s.base,
        &path,
        Some(&op_token),
        &json!({"business_key": "article:req-7", "expected_state": expected}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains("/publications/"))
            .count(),
        1,
        "the honest key made exactly one authority read"
    );

    // And a forged operation_id in the path itself never becomes a target:
    for forged in [
        "not-hex%0Ainjected",
        "..%2Fadmin",
        &"f".repeat(64),
        "00000000-0000-0000-0000-000000000000",
    ] {
        let (status, _) = post_json(
            &s.base,
            &format!("/v1/projects/pilot/operations/{forged}/reconcile"),
            Some(&op_token),
            &json!({"business_key": "article:req-7", "expected_state": expected}),
        )
        .await;
        assert!(
            matches!(status, 400 | 404 | 405),
            "forged operation_id {forged}: {status}"
        );
    }
}
