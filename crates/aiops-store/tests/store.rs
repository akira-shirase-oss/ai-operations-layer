//! The store against real PostgreSQL (per-test databases; needs `DATABASE_URL`, see
//! `scripts/stack.sh env`): idempotent ingest, exact summaries under re-delivery,
//! conflicting duplicates, source counters, lists, audit, retention, pool isolation.
#![expect(
    clippy::unwrap_used,
    reason = "database tests: a failed connection or setup step is a test failure"
)]

use std::time::Duration;

use aiops_domain::policy::Policy;
use aiops_domain::validate::{Source, validate_source};
use aiops_domain::wire::{WireEvent, WireSource, WireStats};
use aiops_domain::{InstanceId, Limits, Mode, OperationId, Timestamp, Validator};
use aiops_store::{
    AuditEntry, IngestBatch, ListFilter, PreparedEvent, Retention, Store, StoreConfig,
};
use sqlx::PgPool;
use uuid::Uuid;

const NOW: i64 = 1_779_000_000_000_000; // 2026-05-17, inside any test's window

fn now() -> Timestamp {
    Timestamp::from_unix_micros(NOW)
}

fn source(n: u8) -> Source {
    validate_source(
        &WireSource {
            instance_id: InstanceId::from_bytes([n; 16]).unwrap().to_string(),
            service: "reference-host".into(),
            sdk: "test/1".into(),
        },
        &Limits::default(),
    )
    .unwrap()
}

fn op(n: u8) -> OperationId {
    OperationId::from_bytes([n; 16]).unwrap()
}

/// Wire events for one operation: started, one step started+finished, finished.
fn journey(op_n: u8, first_seq: u64) -> Vec<WireEvent> {
    let t = |ms: i64| Timestamp::from_unix_micros(NOW - 60_000_000 + ms * 1_000).to_rfc3339();
    let id = |k: u64| format!("{:02x}{:030x}", op_n, first_seq + k);
    let base = |k: u64, ty: &str| WireEvent {
        event_id: id(k),
        event_type: ty.into(),
        operation_id: op(op_n).to_string(),
        seq: first_seq + k,
        op_seq: u32::try_from(k + 1).unwrap(),
        event_time: t(i64::try_from(k).unwrap() * 10),
        ..WireEvent::default()
    };
    vec![
        WireEvent {
            name: Some("publish_article".into()),
            request_id: Some(format!("req-{op_n}")),
            ..base(0, "operation.started")
        },
        WireEvent {
            step_id: Some("00f067aa0ba902b7".into()),
            kind: Some("model".into()),
            name: Some("generate".into()),
            attempt: Some(1),
            ..base(1, "step.started")
        },
        WireEvent {
            step_id: Some("00f067aa0ba902b7".into()),
            attempt: Some(1),
            outcome: Some("succeeded".into()),
            cost_estimate: Some(aiops_domain::wire::WireCost {
                micros: 1_000,
                currency: "USD".into(),
                basis: "test".into(),
            }),
            ..base(2, "step.finished")
        },
        WireEvent {
            outcome: Some("succeeded".into()),
            events_emitted: Some(4),
            ..base(3, "operation.finished")
        },
    ]
}

fn prepare(events: Vec<WireEvent>) -> Vec<PreparedEvent> {
    let limits = Limits::default();
    let policy = Policy::default();
    let v = Validator {
        limits: &limits,
        policy: &policy,
        mode: Mode::Ingest { received_at: now() },
    };
    events
        .into_iter()
        .map(|w| {
            let ok = v.validate(w).unwrap();
            PreparedEvent::new(ok.event, ok.marks).unwrap()
        })
        .collect()
}

struct Fixture {
    store: Store,
    project: Uuid,
    key: Uuid,
}

async fn fixture(pool: PgPool) -> Fixture {
    let store = Store::from_pool(pool);
    let project = store.create_project("pilot").await.unwrap();
    let key = store
        .create_ingest_key("pilot", "reference-host", &[7; 32])
        .await
        .unwrap();
    Fixture {
        store,
        project,
        key,
    }
}

fn batch<'a>(
    f: &Fixture,
    src: &'a Source,
    events: Vec<PreparedEvent>,
    stats: WireStats,
) -> IngestBatch<'a> {
    IngestBatch {
        project_id: f.project,
        ingest_key_id: f.key,
        source: src,
        stats,
        received_at: now(),
        events,
        refused: 0,
        new_names: vec!["publish_article".into(), "generate".into()],
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn redelivery_merges_and_summaries_stay_exact(pool: PgPool) {
    let f = fixture(pool).await;
    let src = source(1);
    let first = f
        .store
        .ingest(batch(
            &f,
            &src,
            prepare(journey(1, 1)),
            WireStats::default(),
        ))
        .await
        .unwrap();
    assert_eq!((first.accepted, first.duplicates), (4, 0));
    let again = f
        .store
        .ingest(batch(
            &f,
            &src,
            prepare(journey(1, 1)),
            WireStats::default(),
        ))
        .await
        .unwrap();
    assert_eq!((again.accepted, again.duplicates), (0, 4));
    assert_eq!(again.conflicts, vec![]);

    let ops = f
        .store
        .list_operations(
            f.project,
            &ListFilter {
                limit: 10,
                ..ListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ops.len(), 1);
    let row = &ops[0];
    assert_eq!(row.host_events, 4, "re-delivery must not inflate counters");
    assert_eq!(row.finish_outcome.as_deref(), Some("succeeded"));
    assert_eq!(row.events_emitted, Some(4));
    assert_eq!(row.cost_estimate_micros, 1_000);
    assert_eq!(row.name.as_deref(), Some("publish_article"));

    let inst = f
        .store
        .instances(f.project, &[src.instance_id])
        .await
        .unwrap();
    assert_eq!(inst[0].events_received, 4);
    assert_eq!(inst[0].max_seq, 4);
    assert_eq!(inst[0].missing_by_seq(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_conflicting_duplicate_is_reported_and_the_first_write_wins(pool: PgPool) {
    let f = fixture(pool).await;
    let src = source(1);
    f.store
        .ingest(batch(
            &f,
            &src,
            prepare(journey(1, 1)),
            WireStats::default(),
        ))
        .await
        .unwrap();
    let mut forged = journey(1, 1);
    forged[3].outcome = Some("failed".into());
    forged[3].failure_class = Some("internal".into());
    let r = f
        .store
        .ingest(batch(&f, &src, prepare(forged), WireStats::default()))
        .await
        .unwrap();
    assert_eq!(r.accepted, 0);
    assert_eq!(r.duplicates, 3);
    assert_eq!(r.conflicts.len(), 1);
    let ops = f
        .store
        .list_operations(
            f.project,
            &ListFilter {
                limit: 10,
                ..ListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ops[0].finish_outcome.as_deref(), Some("succeeded"));
}

#[sqlx::test(migrations = "./migrations")]
async fn the_same_event_id_from_another_instance_is_a_conflict(pool: PgPool) {
    let f = fixture(pool).await;
    let (a, b) = (source(1), source(2));
    f.store
        .ingest(batch(&f, &a, prepare(journey(1, 1)), WireStats::default()))
        .await
        .unwrap();
    let r = f
        .store
        .ingest(batch(&f, &b, prepare(journey(1, 1)), WireStats::default()))
        .await
        .unwrap();
    assert_eq!(r.conflicts.len(), 4);
}

#[sqlx::test(migrations = "./migrations")]
async fn out_of_order_batches_reassemble_and_counters_take_the_latest(pool: PgPool) {
    let f = fixture(pool).await;
    let src = source(1);
    let mut events = journey(1, 1);
    let tail = events.split_off(2);
    // The second half arrives first, carrying newer cumulative SDK counters.
    let newer = WireStats {
        dropped_queue_full: 5,
        ..WireStats::default()
    };
    let older = WireStats {
        dropped_queue_full: 2,
        ..WireStats::default()
    };
    f.store
        .ingest(batch(&f, &src, prepare(tail), newer))
        .await
        .unwrap();
    f.store
        .ingest(batch(&f, &src, prepare(events), older))
        .await
        .unwrap();
    let (stored, truncated) = f
        .store
        .operation_events(f.project, op(1), 100)
        .await
        .unwrap();
    assert!(!truncated);
    let seqs: Vec<u64> = stored.iter().map(|s| s.event.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4], "ordered by event time, not arrival");
    let inst = f
        .store
        .instances(f.project, &[src.instance_id])
        .await
        .unwrap();
    assert_eq!(
        inst[0].reported.dropped_queue_full, 5,
        "cumulative counters never go backwards"
    );
    let ops = f
        .store
        .list_operations(
            f.project,
            &ListFilter {
                limit: 10,
                ..ListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ops[0].name.as_deref(), Some("publish_article"));
    assert_eq!(ops[0].finish_outcome.as_deref(), Some("succeeded"));
}

#[sqlx::test(migrations = "./migrations")]
async fn operation_events_are_bounded_and_say_so(pool: PgPool) {
    let f = fixture(pool).await;
    let src = source(1);
    f.store
        .ingest(batch(
            &f,
            &src,
            prepare(journey(1, 1)),
            WireStats::default(),
        ))
        .await
        .unwrap();
    let (stored, truncated) = f.store.operation_events(f.project, op(1), 3).await.unwrap();
    assert_eq!(stored.len(), 3);
    assert!(truncated);
    assert_eq!(
        stored[0].ingest_key, "reference-host",
        "provenance travels with each event"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn lists_page_by_keyset_and_filter_by_request(pool: PgPool) {
    let f = fixture(pool).await;
    let src = source(1);
    for n in 1..=5u8 {
        let events = journey(n, u64::from(n) * 10);
        f.store
            .ingest(batch(&f, &src, prepare(events), WireStats::default()))
            .await
            .unwrap();
    }
    let page1 = f
        .store
        .list_operations(
            f.project,
            &ListFilter {
                limit: 3,
                ..ListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page1.len(), 3);
    let last = page1.last().unwrap();
    let page2 = f
        .store
        .list_operations(
            f.project,
            &ListFilter {
                limit: 3,
                before: Some((last.first_event_time, last.operation_id)),
                ..ListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page2.len(), 2);
    let mut all: Vec<_> = page1.iter().chain(&page2).map(|r| r.operation_id).collect();
    all.dedup();
    assert_eq!(all.len(), 5);
    let one = f
        .store
        .list_operations(
            f.project,
            &ListFilter {
                limit: 10,
                request_id: Some("req-3".into()),
                ..ListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].operation_id, op(3));
}

#[sqlx::test(migrations = "./migrations")]
async fn keys_tokens_and_audit_round_trip(pool: PgPool) {
    let f = fixture(pool).await;
    let key = f.store.find_ingest_key(&[7; 32]).await.unwrap().unwrap();
    assert_eq!((key.project_slug.as_str(), key.revoked), ("pilot", false));
    f.store
        .revoke_ingest_key("pilot", "reference-host")
        .await
        .unwrap();
    assert!(
        f.store
            .find_ingest_key(&[7; 32])
            .await
            .unwrap()
            .unwrap()
            .revoked
    );
    assert!(f.store.find_ingest_key(&[8; 32]).await.unwrap().is_none());

    let tid = f
        .store
        .create_token(
            "ana",
            &["read".into()],
            &["pilot".into()],
            &[9; 32],
            now().plus_micros(3_600_000_000),
        )
        .await
        .unwrap();
    let t = f.store.find_token(&[9; 32]).await.unwrap().unwrap();
    assert_eq!(
        (t.id, t.actor.as_str(), t.project_ids.clone()),
        (tid, "ana", vec![f.project])
    );
    assert!(
        f.store
            .create_token("x", &[], &["nope".into()], &[1; 32], now())
            .await
            .is_err()
    );

    f.store
        .audit(&AuditEntry {
            actor: "ana",
            token_id: Some(tid),
            action: "timeline.read",
            project_slug: Some("other"),
            target: None,
            allowed: false,
            reason: "project_not_in_scope",
        })
        .await
        .unwrap();
    let rows = f.store.recent_audit(10).await.unwrap();
    assert_eq!(rows[0].decision, "denied");
    assert_eq!(rows[0].reason, "project_not_in_scope");
}

#[sqlx::test(migrations = "./migrations")]
async fn retention_sweeps_only_expired_rows_in_bounded_batches(pool: PgPool) {
    let f = fixture(pool).await;
    let src = source(1);
    for n in 1..=3u8 {
        f.store
            .ingest(batch(
                &f,
                &src,
                prepare(journey(n, u64::from(n) * 10)),
                WireStats::default(),
            ))
            .await
            .unwrap();
    }
    // Nothing is old enough yet.
    let r = f
        .store
        .sweep(now(), Retention::default(), 5, 10)
        .await
        .unwrap();
    assert_eq!((r.events, r.operations), (0, 0));
    // Fifteen days later: raw events expire, summaries (90 days) stay.
    let later = now().plus_micros(15 * 24 * 3_600 * 1_000_000);
    let r = f
        .store
        .sweep(later, Retention::default(), 5, 1)
        .await
        .unwrap();
    assert_eq!(r.events, 5, "one bounded batch per call");
    assert!(r.more);
    let r = f
        .store
        .sweep(later, Retention::default(), 5, 10)
        .await
        .unwrap();
    assert_eq!(r.events, 7);
    assert_eq!(r.operations, 0);
    let ops = f
        .store
        .list_operations(
            f.project,
            &ListFilter {
                limit: 10,
                ..ListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ops.len(), 3, "summaries outlive raw events (D-016)");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_saturated_query_pool_does_not_block_ingest_or_audit(pool: PgPool) {
    let base = (*pool.connect_options()).clone();
    let mut cfg = StoreConfig::new("");
    cfg.query_connections = 1;
    let store = Store::connect_with(&cfg, &base).await.unwrap();
    store.create_project("pilot").await.unwrap();
    let key = store
        .create_ingest_key("pilot", "h", &[7; 32])
        .await
        .unwrap();
    let project = store.project_by_slug("pilot").await.unwrap().unwrap().id;

    // A slow query: another session holds a lock the query path needs, so the query
    // pool's only connection waits (up to its statement timeout).
    let mut locker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE api_tokens IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *locker)
        .await
        .unwrap();
    let blocked = store.clone();
    let slow = tokio::spawn(async move { blocked.find_token(&[1; 32]).await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let src = source(1);
    let f = Fixture {
        store: store.clone(),
        project,
        key,
    };
    let started = std::time::Instant::now();
    let r = store
        .ingest(batch(
            &f,
            &src,
            prepare(journey(1, 1)),
            WireStats::default(),
        ))
        .await
        .unwrap();
    assert_eq!(r.accepted, 4);
    store
        .audit(&AuditEntry {
            actor: "t",
            token_id: None,
            action: "a",
            project_slug: None,
            target: None,
            allowed: true,
            reason: "r",
        })
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "ingest and audit waited {elapsed:?} on the query path"
    );
    // The query pool really was saturated: a second query cannot get a connection.
    assert!(store.ping().await.unwrap_err().is_retryable());
    locker.rollback().await.unwrap();
    assert!(slow.await.unwrap().unwrap().is_none());
}
