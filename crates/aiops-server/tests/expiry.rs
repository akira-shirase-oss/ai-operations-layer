//! Review finding 10: events removed by retention are shown as expired, never as never
//! received or as proven loss. Needs `DATABASE_URL`.
#![expect(
    clippy::unwrap_used,
    reason = "database tests: a failed setup step is a test failure"
)]

mod common;

use aiops_domain::policy::Policy;
use aiops_domain::validate::validate_source;
use aiops_domain::wire::{WireEvent, WireSource, WireStats};
use aiops_domain::{Limits, Mode, Validator};
use aiops_store::{IngestBatch, PreparedEvent, Retention};
use common::{env, get, op_hex, send};
use sqlx::PgPool;

const DAY_US: i64 = 24 * 3_600 * 1_000_000;

/// Stores `events` as received `days_ago` days ago (straight into the store: the API would
/// refuse event times that old).
async fn ingest_aged(e: &common::Env, events: Vec<WireEvent>, days_ago: i64) {
    let now = common::now();
    let received = now.plus_micros(-days_ago * DAY_US);
    let limits = Limits::default();
    let policy = Policy::default();
    let v = Validator {
        limits: &limits,
        policy: &policy,
        mode: Mode::Ingest {
            received_at: received,
        },
    };
    let prepared = events
        .into_iter()
        .map(|mut w| {
            w.event_time = received.plus_micros(-1_000).to_rfc3339();
            let ok = v.validate(w).unwrap();
            PreparedEvent::new(ok.event, ok.marks).unwrap()
        })
        .collect();
    let source = validate_source(
        &WireSource {
            instance_id: common::INSTANCE.into(),
            service: "reference-host".into(),
            sdk: "test/1".into(),
        },
        &limits,
    )
    .unwrap();
    let key = e
        .store
        .find_ingest_key(&aiops_server::auth::hash(&e.ingest_key))
        .await
        .unwrap()
        .unwrap();
    e.store
        .ingest(IngestBatch {
            project_id: key.project_id,
            ingest_key_id: key.id,
            source: &source,
            stats: WireStats::default(),
            received_at: received,
            events: prepared,
            refused: 0,
            new_names: vec![],
            touched_names: vec![],
        })
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_fully_expired_operation_is_shown_as_expired_with_its_declared_outcome(pool: PgPool) {
    let e = env(pool).await;
    ingest_aged(&e, common::journey(1, 0), 15).await;
    e.store
        .sweep(common::now(), Retention::default(), 1_000, 10)
        .await
        .unwrap();
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(
        t.status, 200,
        "an expired operation is not 'never received': {}",
        t.body
    );
    assert_eq!(t.body["expired_by_retention"], true);
    assert_eq!(
        t.body["status"]["state"], "uncertain",
        "the declared outcome survives in the summary"
    );
    assert!(
        t.body["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["kind"] == "expired")
    );
    let list = send(
        &e.app,
        get("/v1/projects/pilot/operations?request_id=req-1", &e.reader),
    )
    .await;
    assert_eq!(list.body["operations"][0]["detail_expired"], true);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_partly_expired_operation_shows_expired_events_not_proven_loss(pool: PgPool) {
    let e = env(pool).await;
    let mut events = common::journey(1, 0);
    let recent = events.split_off(3);
    ingest_aged(&e, events, 15).await; // op_seq 1–3, past retention
    ingest_aged(&e, recent, 0).await; // op_seq 4–8, fresh
    e.store
        .sweep(common::now(), Retention::default(), 1_000, 10)
        .await
        .unwrap();
    let t = send(
        &e.app,
        get(
            &format!("/v1/projects/pilot/operations/{}", op_hex(1)),
            &e.reader,
        ),
    )
    .await;
    assert_eq!(t.status, 200);
    assert_eq!(t.body["expired_by_retention"], true);
    assert_eq!(
        t.body["coverage"]["missing_known"], 0,
        "not proven lost: they expired"
    );
    assert_eq!(t.body["coverage"]["expired"], 3);
    let kinds: Vec<&str> = t.body["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"expired") && !kinds.contains(&"op_seq_hole"),
        "{kinds:?}"
    );
}
