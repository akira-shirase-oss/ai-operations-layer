//! Rate-limit evasion: burst past the bound on each surface, malformed bodies
//! that used to parse a MiB for free, and an unknown-slug flood that must stop
//! at its own bucket — audited exactly as many times as it was admitted.

use crate::common::{
    audit_rows, batch, create_ingest_key, create_project, create_token, get, journey, post_ingest,
    stack,
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn bursts_spend_their_own_budget_on_every_surface(pool: PgPool) {
    let s = stack(
        &pool,
        &[
            ("AIOPS_INGEST_EVENTS_PER_SEC", "1"),
            ("AIOPS_INGEST_BURST_EVENTS", "8"),
            ("AIOPS_QUERY_REQUESTS_PER_SEC", "1"),
            ("AIOPS_QUERY_BURST", "2"),
        ],
    )
    .await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);

    // Ingest: burst 8 is exactly one journey's events — the second batch
    // inside the window needs 8 more than the 1/s refill earned.
    assert_eq!(
        post_ingest(&s.base, Some(&key), batch(&journey(1, 0)))
            .await
            .0,
        200
    );
    let (status, body, headers) = post_ingest(&s.base, Some(&key), batch(&journey(2, 100))).await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["error"]["code"], "rate_limited");
    assert!(headers.get("retry-after").is_some());

    // Query: burst 2, then refusals — same shape.
    let path = "/v1/projects/pilot/operations";
    let mut limited = false;
    for _ in 0..4 {
        let (status, body) = get(&s.base, path, Some(&reader)).await;
        if status == 429 {
            limited = true;
            assert_eq!(body["error"]["code"], "rate_limited");
        }
    }
    assert!(limited, "the query bucket must refuse past its burst");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_malformed_flood_still_spends_the_ingest_budget_over_http(pool: PgPool) {
    // The charge lands before the body is read: four malformed bodies at
    // burst 3 means the fourth is refused at the gate, not at the parser.
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
    let mut statuses = Vec::new();
    for _ in 0..4 {
        statuses.push(post_ingest(&s.base, Some(&key), "{not json").await.0);
    }
    assert_eq!(statuses, [400, 400, 400, 429], "{statuses:?}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_unknown_slug_flood_stops_at_its_own_bucket(pool: PgPool) {
    let s = stack(
        &pool,
        &[
            ("AIOPS_QUERY_REQUESTS_PER_SEC", "1"),
            ("AIOPS_QUERY_BURST", "5"),
        ],
    )
    .await;
    create_project(&s.url, "pilot");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);

    let (mut denied, mut limited) = (0u32, 0u32);
    for i in 0..40 {
        let (status, _) = get(
            &s.base,
            &format!("/v1/projects/nope-{i}/operations"),
            Some(&reader),
        )
        .await;
        match status {
            403 => denied += 1,
            429 => limited += 1,
            x => panic!("an unknown slug answered {x}"),
        }
    }
    // Burst 5 on a wall clock can refill a little — but never 35 tokens' worth.
    assert_eq!(denied + limited, 40);
    assert!(
        denied <= 10 && limited >= 30,
        "{denied} denied, {limited} limited"
    );
    let rows = audit_rows(&pool, 100).await;
    let audited = rows.iter().filter(|(_, d, _, _)| d == "denied").count();
    assert_eq!(audited, denied as usize, "a 429 never audits: {rows:?}");
}
