//! The retention sweep as scheduled by the collector (D-016).

use std::time::{Duration, Instant};

use aiops_domain::Timestamp;
use aiops_store::{Retention, Store, StoreError, SweepResult};

/// How one scheduled run sweeps.
#[derive(Debug, Clone, Copy)]
pub struct SweepPlan {
    /// Rows per DELETE.
    pub batch: i64,
    /// DELETEs per table per store call.
    pub rounds_per_call: u32,
    /// Pause between store calls, so ingest keeps the database.
    pub pause: Duration,
    /// A run stops after this long even if work remains (the next run continues).
    pub max_duration: Duration,
}

impl Default for SweepPlan {
    fn default() -> Self {
        Self {
            batch: 5_000,
            rounds_per_call: 20,
            pause: Duration::from_millis(50),
            max_duration: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepTotals {
    pub events: u64,
    pub operations: u64,
    pub instances: u64,
    pub audit: u64,
    pub calls: u32,
    /// Nothing expired was left when the run stopped.
    pub complete: bool,
}

/// One scheduled run: bounded DELETE batches, paced, repeated while expired rows remain
/// (review finding 5), up to `plan.max_duration`.
pub async fn run(
    store: &Store,
    now: Timestamp,
    retention: Retention,
    plan: SweepPlan,
) -> Result<SweepTotals, StoreError> {
    let started = Instant::now();
    let mut totals = SweepTotals::default();
    loop {
        let r: SweepResult = store
            .sweep(now, retention, plan.batch, plan.rounds_per_call)
            .await?;
        totals.events += r.events;
        totals.operations += r.operations;
        totals.instances += r.instances;
        totals.audit += r.audit;
        totals.calls += 1;
        if !r.more {
            totals.complete = true;
            return Ok(totals);
        }
        if started.elapsed() >= plan.max_duration {
            return Ok(totals);
        }
        tokio::time::sleep(plan.pause).await;
    }
}
