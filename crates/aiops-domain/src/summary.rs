//! Per-operation deltas from one batch's *newly stored* events, merged into the store's
//! operation summaries. Duplicates never reach this (they were not inserted), so the
//! counters stay exact under re-delivery.

use std::collections::BTreeMap;

use crate::event::{Event, EventBody, Outcome};
use crate::ids::OperationId;
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDelta {
    pub operation_id: OperationId,
    pub name: Option<String>,
    pub request_id: Option<String>,
    pub release: Option<String>,
    pub min_event_time: Timestamp,
    pub max_event_time: Timestamp,
    /// Host-emitted events (`op_seq` ≥ 1).
    pub host_events: u32,
    pub gap_reports: u32,
    pub reported_dropped: u64,
    pub finish: Option<Outcome>,
    pub events_emitted: Option<u32>,
    pub failed_attempts: u32,
    pub uncertain_attempts: u32,
    pub cost_estimate_micros: u64,
    pub cost_currency: Option<String>,
}

/// The currency recorded when a summary saw more than one.
pub const MIXED_CURRENCY: &str = "MIXED";

/// Groups events by operation, sorted by operation ID (the store upserts in this order
/// so concurrent batches lock summary rows in one global order and cannot deadlock).
pub fn summarize<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<OperationDelta> {
    let mut by_op: BTreeMap<OperationId, OperationDelta> = BTreeMap::new();
    for e in events {
        let d = by_op
            .entry(e.operation_id)
            .or_insert_with(|| OperationDelta {
                operation_id: e.operation_id,
                name: None,
                request_id: None,
                release: None,
                min_event_time: e.event_time,
                max_event_time: e.event_time,
                host_events: 0,
                gap_reports: 0,
                reported_dropped: 0,
                finish: None,
                events_emitted: None,
                failed_attempts: 0,
                uncertain_attempts: 0,
                cost_estimate_micros: 0,
                cost_currency: None,
            });
        d.min_event_time = d.min_event_time.min(e.event_time);
        d.max_event_time = d.max_event_time.max(e.event_time);
        if e.op_seq > 0 {
            d.host_events += 1;
        }
        match &e.body {
            EventBody::OperationStarted(m) => {
                d.name.get_or_insert_with(|| m.name.clone());
                if d.request_id.is_none() {
                    d.request_id.clone_from(&m.request_id);
                }
                if d.release.is_none() {
                    d.release.clone_from(&m.release);
                }
            }
            EventBody::OperationFinished(f) => {
                d.finish.get_or_insert(f.outcome);
                d.events_emitted = Some(
                    d.events_emitted
                        .map_or(f.events_emitted, |n| n.max(f.events_emitted)),
                );
            }
            EventBody::StepStarted(_) => {}
            EventBody::StepFinished(f) => {
                match f.outcome {
                    Outcome::Failed(_) => d.failed_attempts += 1,
                    Outcome::Uncertain(_) => d.uncertain_attempts += 1,
                    Outcome::Succeeded => {}
                }
                if let Some(c) = &f.cost_estimate {
                    d.cost_estimate_micros = d.cost_estimate_micros.saturating_add(c.micros);
                    match &d.cost_currency {
                        None => d.cost_currency = Some(c.currency.clone()),
                        Some(cur) if *cur != c.currency => {
                            d.cost_currency = Some(MIXED_CURRENCY.to_owned());
                        }
                        Some(_) => {}
                    }
                }
            }
            EventBody::GapReported(g) => {
                d.gap_reports += 1;
                d.reported_dropped = d.reported_dropped.saturating_add(g.dropped);
            }
        }
    }
    by_op.into_values().collect()
}
