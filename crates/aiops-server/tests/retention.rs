//! Review finding 5: the scheduled sweep must keep up with what expires. Needs `DATABASE_URL`.

use std::time::Duration;

use aiops_domain::policy::Policy;
use aiops_domain::validate::validate_source;
use aiops_domain::wire::{WireEvent, WireSource, WireStats};
use aiops_domain::{Limits, Mode, Timestamp, Validator};
use aiops_server::retention::{SweepPlan, run};
use aiops_store::{IngestBatch, PreparedEvent, Retention, Store};
use sqlx::PgPool;

const DAY_US: i64 = 24 * 3_600 * 1_000_000;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn one_scheduled_run_removes_everything_that_expired_not_one_budget(pool: PgPool) {
    let store = Store::from_pool(pool);
    store.create_project("pilot").await.unwrap();
    let key = store
        .create_ingest_key("pilot", "h", &[7; 32])
        .await
        .unwrap();
    let project = store.project_by_slug("pilot").await.unwrap().unwrap().id;
    let source = validate_source(
        &WireSource {
            instance_id: "11111111111111111111111111111111".into(),
            service: "h".into(),
            sdk: "t".into(),
        },
        &Limits::default(),
    )
    .unwrap();
    // 1,000 events received 15 days ago (past the 14-day raw retention).
    let received = Timestamp::from_unix_micros(1_779_000_000_000_000);
    let limits = Limits::default();
    let policy = Policy::default();
    let v = Validator {
        limits: &limits,
        policy: &policy,
        mode: Mode::Ingest {
            received_at: received,
        },
    };
    for b in 0..10u64 {
        let events = (0..100u64)
            .map(|i| {
                let n = b * 100 + i + 1;
                let w = WireEvent {
                    event_id: format!("{n:032x}"),
                    event_type: "operation.started".into(),
                    operation_id: format!("{n:032x}"),
                    seq: n,
                    op_seq: 1,
                    event_time: received.to_rfc3339(),
                    name: Some("x".into()),
                    ..WireEvent::default()
                };
                let ok = v.validate(w).unwrap();
                PreparedEvent::new(ok.event, ok.marks).unwrap()
            })
            .collect();
        store
            .ingest(IngestBatch {
                project_id: project,
                ingest_key_id: key,
                source: &source,
                stats: WireStats::default(),
                received_at: received,
                events,
                refused: 0,
                new_names: vec![],
                touched_names: vec![],
            })
            .await
            .unwrap();
    }
    let now = received.plus_micros(15 * DAY_US);
    // A per-call budget far smaller than what expired: one run must still finish.
    let plan = SweepPlan {
        batch: 100,
        rounds_per_call: 2,
        pause: Duration::ZERO,
        max_duration: Duration::from_secs(60),
    };
    let totals = run(&store, now, Retention::default(), plan).await.unwrap();
    assert_eq!(totals.events, 1_000, "{totals:?}");
    assert!(totals.complete, "{totals:?}");
}
