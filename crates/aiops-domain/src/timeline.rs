//! One operation's timeline, assembled from stored events without inventing anything
//! (invariants 2 and 3, D-011, D-018, D-019).
//!
//! - Status is what the host *declared* in `operation.finished`; without one it is
//!   `no_finish_observed` — never "running", never "did not run".
//! - Loss is counted from sequence numbers (`op_seq` holes are exact) and from the SDK's
//!   own gap reports; loss after the last received event of an unfinished operation is
//!   *unknown*, not zero.
//! - Entries are ordered by event time, with receive time shown beside it; within one
//!   source instance `seq` is the emission order, and disagreements are flagged as clock
//!   problems rather than resolved silently.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use crate::event::{AttrValue, Event, EventBody, Outcome, StepKind};
use crate::ids::{InstanceId, OperationId, StepId};
use crate::limits::Limits;
use crate::policy::is_default_attribute;
use crate::time::Timestamp;
use crate::validate::PolicyMarks;
use crate::wire::WireStats;

/// An event as the store returns it: the validated event plus the two things only the
/// collector knows — when it arrived and under which key and instance.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub event: Event,
    pub received_at: Timestamp,
    pub instance_id: InstanceId,
    /// The ingest key's name: provenance, so a forged event shows where it came from.
    pub ingest_key: String,
    pub marks: PolicyMarks,
}

/// A source instance's counters, as the store keeps them.
#[derive(Debug, Clone)]
pub struct InstanceStats {
    pub instance_id: InstanceId,
    pub service: String,
    pub sdk: String,
    pub ingest_key: String,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
    pub max_seq: u64,
    pub events_received: u64,
    /// Events of this instance the collector refused (each by name in its answer).
    pub events_refused: u64,
    pub reported: WireStats,
}

impl InstanceStats {
    /// Events this instance numbered but the collector never stored. Exact up to
    /// `max_seq`; anything the instance emitted after it is unknown.
    pub fn missing_by_seq(&self) -> u64 {
        self.max_seq.saturating_sub(self.events_received)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Status {
    /// `succeeded`, `failed`, `uncertain`, `conflicting` (hosts declared different
    /// outcomes) or `no_finish_observed`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// True when the state is the host's own declaration.
    pub declared: bool,
}

impl Status {
    pub fn from_outcome(outcome: Outcome) -> Self {
        let (class, reason) = match outcome {
            Outcome::Succeeded => (None, None),
            Outcome::Failed(c) => (Some(c.as_str()), None),
            Outcome::Uncertain(r) => (None, Some(r.as_str())),
        };
        Self {
            state: outcome.as_str(),
            class,
            reason,
            declared: true,
        }
    }

    /// Hosts declared different outcomes for one operation: no single answer is shown.
    pub fn conflicting() -> Self {
        Self {
            state: "conflicting",
            class: None,
            reason: None,
            declared: true,
        }
    }

    pub fn no_finish_observed() -> Self {
        Self {
            state: "no_finish_observed",
            class: None,
            reason: None,
            declared: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent observations, each rendered on its own"
)]
pub struct Flags {
    pub failed_attempts: u32,
    pub uncertain_attempts: u32,
    pub retried_steps: u32,
    /// An attempt of a step that can affect the outside world ended `uncertain`, or never
    /// reported a finish: the effect may have happened.
    pub external_effect_unknown: bool,
    pub clock_skew_suspected: bool,
    pub late_events: u32,
    /// More than one `operation.finished` with different outcomes.
    pub conflicting_finish: bool,
    /// No `operation.started` was received.
    pub start_not_observed: bool,
    /// Events are proven missing (sequence holes) or were reported dropped by the SDK: they
    /// may include a whole step with an external effect, which no step view can show
    /// (review N6). Sets `external_effect_unknown` too.
    pub missing_events_may_hide_effects: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InstanceCoverage {
    pub instance_id: String,
    /// From this instance's `operation.finished`, when it arrived.
    pub emitted: Option<u32>,
    pub received: u32,
    /// Exactly these `op_seq` numbers are missing (first 100).
    pub missing_op_seq: Vec<u32>,
    pub missing_known: u32,
    /// Whether events after the last received one may be missing (no finish observed).
    pub tail_unknown: bool,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Coverage {
    /// Distinct host-emitted events stored for this operation.
    pub events_received: u32,
    pub sdk_gap_reports: u32,
    /// Events the SDK said it dropped for this operation.
    pub reported_dropped: u64,
    /// Events proven missing by `op_seq` holes.
    pub missing_known: u32,
    /// Some loss cannot be sized (an instance never reported a finish).
    pub missing_unknown: bool,
    /// Events deleted by retention (exact, from the surviving summary).
    pub expired: u32,
    pub per_instance: Vec<InstanceCoverage>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct CostView {
    /// Always "estimate": never a reconciled bill (invariant 7).
    pub label: &'static str,
    pub estimate_micros: u64,
    pub currency: Option<String>,
    pub mixed_currencies: bool,
    pub attempts_with_estimate: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AttrView {
    pub key: String,
    pub value: serde_json::Value,
    /// Allowed by the project's opt-in, not by the default allowlist.
    pub opt_in: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EntryView {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub instance_id: String,
    pub ingest_key: String,
    pub seq: u64,
    pub op_seq: u32,
    pub event_time: Timestamp,
    pub received_at: Timestamp,
    /// `received_at - event_time`; negative means the event claims to come from the future.
    pub receive_delay_us: i64,
    /// `late`, `clock_ahead`, `clock_went_backwards`, `starts_before_parent`.
    pub flags: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub attributes: Vec<AttrView>,
    pub withheld_attributes: u32,
    pub redactions: u32,
    /// Names over budget, stored as `_overflow`.
    pub overflowed_names: u32,
    pub truncated: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AttemptView {
    pub attempt: u32,
    /// `succeeded`, `failed`, `uncertain` or `no_finish_observed`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub start_observed: bool,
    pub finish_observed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_us: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_estimate_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StepView {
    pub step_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_step_id: Option<String>,
    pub kind: Option<&'static str>,
    pub name: Option<String>,
    pub external_effect: bool,
    pub first_event_time: Timestamp,
    pub attempts: Vec<AttemptView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GapView {
    /// `sdk_reported`, `op_seq_hole`, `unknown_tail`, `undeclared_count` (a joined service,
    /// which schema v1 gives no count) or `expired`.
    pub kind: &'static str,
    pub instance_id: String,
    /// `None` means the size is unknown.
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_seq_from: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_seq_to: Option<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceView {
    pub instance_id: String,
    pub service: String,
    pub sdk: String,
    pub ingest_key: String,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
    /// Over the instance's whole life, not just this operation.
    pub missing_by_seq: u64,
    /// Refused by the collector (part of `missing_by_seq` when their sequence numbers
    /// were valid): a reason, not an unexplained hole.
    pub events_refused: u64,
    pub reported_dropped_queue_full: u64,
    pub reported_dropped_send_failed: u64,
    pub reported_dropped_rejected: u64,
    pub reported_gap_overflow: u64,
}

/// What survives retention for an operation (its summary), and the retention cutoff: raw
/// events received before `cutoff` may have been deleted (review finding 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionContext {
    pub cutoff: Timestamp,
    /// When the operation's first event was received (from the summary).
    pub first_received_at: Timestamp,
    /// The declared finish the summary kept, and whether finishes conflicted.
    pub finish: Option<Outcome>,
    pub finish_conflict: bool,
    /// Host events ever stored for the operation (from the summary).
    pub events_received: u32,
}

/// Why the timeline says an external effect may have happened (one per attempt).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EffectCause {
    pub step_id: String,
    pub name: Option<String>,
    pub attempt: u32,
    /// `uncertain` or `no_finish_observed`.
    pub cause: &'static str,
    /// `yes` — a start declared an external effect; `unknown` — no start of the step was
    /// received, so its declaration is unknown and it is treated as possibly effectful.
    pub declared: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Timeline {
    pub operation_id: String,
    pub name: Option<String>,
    pub request_id: Option<String>,
    pub trace_id: Option<String>,
    pub release: Option<String>,
    pub deployment: Option<String>,
    pub status: Status,
    pub flags: Flags,
    pub coverage: Coverage,
    pub cost: CostView,
    pub steps: Vec<StepView>,
    pub entries: Vec<EntryView>,
    pub gaps: Vec<GapView>,
    pub sources: Vec<SourceView>,
    /// Attempts whose effect outside the host is unknown (the console's warning).
    pub effect_unknown_steps: Vec<EffectCause>,
    /// Some or all of this operation's raw events were deleted by retention; what is shown
    /// comes from what remains and from the summary.
    pub expired_by_retention: bool,
    /// Raw events received before this may have been deleted.
    pub retention_cutoff: Option<Timestamp>,
    /// The store returned only the first part of a very large operation.
    pub events_truncated: bool,
}

const MAX_LISTED_HOLES: usize = 100;

#[derive(Default)]
struct AttemptAcc<'a> {
    start: Option<&'a StoredEvent>,
    finish: Option<&'a StoredEvent>,
}

/// What one pass over the events collects.
struct Collected<'a> {
    meta: Option<&'a crate::event::OperationMeta>,
    finishes: Vec<(&'a StoredEvent, Outcome, u32)>,
    step_order: Vec<StepId>,
    steps: HashMap<StepId, BTreeMap<u32, AttemptAcc<'a>>>,
    step_start_time: HashMap<StepId, Timestamp>,
    /// Per instance: the host `op_seq` numbers seen and the declared emitted count.
    per_instance: BTreeMap<InstanceId, (BTreeSet<u32>, Option<u32>)>,
    /// Instances that sent an `operation.started`.
    starters: BTreeSet<InstanceId>,
    gaps: Vec<GapView>,
    coverage: Coverage,
}

fn collect(events: &[StoredEvent]) -> Collected<'_> {
    let mut c = Collected {
        meta: None,
        finishes: Vec::new(),
        step_order: Vec::new(),
        steps: HashMap::new(),
        step_start_time: HashMap::new(),
        per_instance: BTreeMap::new(),
        starters: BTreeSet::new(),
        gaps: Vec::new(),
        coverage: Coverage::default(),
    };
    for stored in events {
        let e = &stored.event;
        match &e.body {
            EventBody::OperationStarted(m) => {
                c.meta.get_or_insert(m);
                c.starters.insert(stored.instance_id);
            }
            EventBody::OperationFinished(f) => {
                c.finishes.push((stored, f.outcome, f.events_emitted));
            }
            EventBody::StepStarted(s) => {
                if !c.steps.contains_key(&s.step_id) {
                    c.step_order.push(s.step_id);
                }
                c.step_start_time
                    .entry(s.step_id)
                    .and_modify(|t| *t = (*t).min(e.event_time))
                    .or_insert(e.event_time);
                let slot = c
                    .steps
                    .entry(s.step_id)
                    .or_default()
                    .entry(s.attempt)
                    .or_default();
                slot.start.get_or_insert(stored);
            }
            EventBody::StepFinished(f) => {
                if !c.steps.contains_key(&f.step_id) {
                    c.step_order.push(f.step_id);
                }
                let slot = c
                    .steps
                    .entry(f.step_id)
                    .or_default()
                    .entry(f.attempt)
                    .or_default();
                slot.finish.get_or_insert(stored);
            }
            EventBody::GapReported(g) => {
                c.coverage.sdk_gap_reports += 1;
                c.coverage.reported_dropped = c.coverage.reported_dropped.saturating_add(g.dropped);
                c.gaps.push(GapView {
                    kind: "sdk_reported",
                    instance_id: stored.instance_id.to_string(),
                    count: Some(g.dropped),
                    reason: Some(g.reason.as_str()),
                    from: Some(g.first_dropped_at),
                    to: Some(g.last_dropped_at),
                    op_seq_from: None,
                    op_seq_to: None,
                });
            }
        }
        if e.op_seq > 0 {
            c.coverage.events_received += 1;
            c.per_instance
                .entry(stored.instance_id)
                .or_default()
                .0
                .insert(e.op_seq);
        }
    }
    for (stored, _, emitted) in &c.finishes {
        let entry = c.per_instance.entry(stored.instance_id).or_default();
        entry.1 = Some(entry.1.map_or(*emitted, |e| e.max(*emitted)));
    }
    c
}

/// Exact holes up to what is known, an unknown tail where no finish was observed.
fn measure_loss(c: &mut Collected<'_>) {
    for (instance, (seen, emitted)) in &c.per_instance {
        let highest_seen = seen.iter().next_back().copied().unwrap_or(0);
        let upper = emitted.unwrap_or(highest_seen).max(highest_seen);
        let mut missing = Vec::new();
        let mut missing_known = 0u32;
        let mut run: Option<(u32, u32)> = None;
        for n in 1..=upper {
            if seen.contains(&n) {
                if let Some((a, b)) = run.take() {
                    c.gaps.push(hole(*instance, a, b));
                }
            } else {
                missing_known += 1;
                if missing.len() < MAX_LISTED_HOLES {
                    missing.push(n);
                }
                run = Some(run.map_or((n, n), |(a, _)| (a, n)));
            }
        }
        if let Some((a, b)) = run {
            c.gaps.push(hole(*instance, a, b));
        }
        let tail_unknown = emitted.is_none();
        // A service joined with `continue_operation` neither starts nor finishes the
        // operation, so schema v1 has no count for it (review N7): say so, rather than
        // "no finish observed", once another source declared the finish.
        let joined = !c.starters.contains(instance) && !c.finishes.is_empty();
        if tail_unknown {
            c.gaps.push(GapView {
                kind: if joined {
                    "undeclared_count"
                } else {
                    "unknown_tail"
                },
                instance_id: instance.to_string(),
                count: None,
                reason: None,
                from: None,
                to: None,
                op_seq_from: Some(highest_seen + 1),
                op_seq_to: None,
            });
        }
        c.coverage.missing_known += missing_known;
        c.coverage.missing_unknown |= tail_unknown;
        c.coverage.per_instance.push(InstanceCoverage {
            instance_id: instance.to_string(),
            emitted: *emitted,
            received: u32::try_from(seen.len()).unwrap_or(u32::MAX),
            missing_op_seq: missing,
            missing_known,
            tail_unknown,
        });
    }
    // Only gap reports and no host events at all: the size of what is missing is unknown.
    if c.per_instance.is_empty() {
        c.coverage.missing_unknown = true;
    }
}

fn build_steps(
    c: &Collected<'_>,
    flags: &mut Flags,
    cost: &mut CostView,
    causes: &mut Vec<EffectCause>,
) -> Vec<StepView> {
    let mut step_views = Vec::with_capacity(c.step_order.len());
    for step_id in &c.step_order {
        let Some(attempts) = c.steps.get(step_id) else {
            continue;
        };
        let mut view = StepView {
            step_id: step_id.to_string(),
            parent_step_id: None,
            kind: None,
            name: None,
            external_effect: false,
            first_event_time: Timestamp::from_unix_micros(i64::MAX),
            attempts: Vec::with_capacity(attempts.len()),
        };
        if attempts.len() > 1 {
            flags.retried_steps += 1;
        }
        // First pass: every attempt, so the step's declaration is settled over all of its
        // starts before any flag is derived (the attempt order must not matter).
        for (n, acc) in attempts {
            let attempt = attempt_view(*n, acc, &mut view, cost);
            match attempt.state {
                "failed" => flags.failed_attempts += 1,
                "uncertain" => flags.uncertain_attempts += 1,
                _ => {}
            }
            view.attempts.push(attempt);
        }
        // A step whose starts were all lost has an unknown declaration: treat it as
        // possibly effectful, never as effect-free.
        let start_seen = attempts.values().any(|a| a.start.is_some());
        let declared = if view.external_effect {
            Some("yes")
        } else if start_seen {
            None // declared effect-free
        } else {
            Some("unknown")
        };
        if let Some(declared) = declared {
            for a in &view.attempts {
                if matches!(a.state, "uncertain" | "no_finish_observed") {
                    flags.external_effect_unknown = true;
                    causes.push(EffectCause {
                        step_id: view.step_id.clone(),
                        name: view.name.clone(),
                        attempt: a.attempt,
                        cause: a.state,
                        declared,
                    });
                }
            }
        }
        step_views.push(view);
    }
    step_views.sort_by_key(|s| s.first_event_time);
    step_views
}

fn build_entries(
    events: &[StoredEvent],
    step_start_time: &HashMap<StepId, Timestamp>,
    limits: &Limits,
    flags: &mut Flags,
) -> Vec<EntryView> {
    let parent_of: HashMap<StepId, StepId> = events
        .iter()
        .filter_map(|s| match &s.event.body {
            EventBody::StepStarted(st) => st.parent_step_id.map(|p| (st.step_id, p)),
            _ => None,
        })
        .collect();
    let mut last_time: HashMap<InstanceId, Timestamp> = HashMap::new();
    let mut entries = Vec::with_capacity(events.len());
    for stored in events {
        let e = &stored.event;
        let delay = stored.received_at.micros_since(e.event_time);
        let mut entry_flags = Vec::new();
        if delay > limits.late_after_micros {
            entry_flags.push("late");
            flags.late_events += 1;
        }
        if -delay > limits.skew_tolerance_micros {
            entry_flags.push("clock_ahead");
            flags.clock_skew_suspected = true;
        }
        // In emission order, a time earlier than the previous event's is the clock going
        // back, flagged on the event that carries the earlier time (review finding 14).
        let prev = last_time.insert(stored.instance_id, e.event_time);
        if prev.is_some_and(|p| p > e.event_time) {
            entry_flags.push("clock_went_backwards");
            flags.clock_skew_suspected = true;
        }
        if let EventBody::StepStarted(s) = &e.body
            && let Some(parent) = parent_of.get(&s.step_id)
            && step_start_time
                .get(parent)
                .is_some_and(|t| *t > e.event_time)
        {
            entry_flags.push("starts_before_parent");
            flags.clock_skew_suspected = true;
        }
        entries.push(entry_view(stored, delay, entry_flags));
    }
    entries
}

fn source_views(instances: &[InstanceStats], events: &[StoredEvent]) -> Vec<SourceView> {
    instances
        .iter()
        .filter(|i| events.iter().any(|e| e.instance_id == i.instance_id))
        .map(|i| SourceView {
            instance_id: i.instance_id.to_string(),
            service: i.service.clone(),
            sdk: i.sdk.clone(),
            ingest_key: i.ingest_key.clone(),
            first_seen: i.first_seen,
            last_seen: i.last_seen,
            missing_by_seq: i.missing_by_seq(),
            events_refused: i.events_refused,
            reported_dropped_queue_full: i.reported.dropped_queue_full,
            reported_dropped_send_failed: i.reported.dropped_send_failed,
            reported_dropped_rejected: i.reported.dropped_rejected,
            reported_gap_overflow: i.reported.gap_overflow,
        })
        .collect()
}

/// Assembles the timeline. `events` may contain anything the store returned for the
/// operation, in any order; duplicates were merged on the way in.
pub fn assemble(
    operation_id: OperationId,
    events: Vec<StoredEvent>,
    instances: &[InstanceStats],
    limits: &Limits,
    events_truncated: bool,
    retention: Option<&RetentionContext>,
) -> Timeline {
    let events = emission_order(events);
    let mut c = collect(&events);
    measure_loss(&mut c);

    let mut flags = Flags {
        start_not_observed: c.meta.is_none(),
        ..Flags::default()
    };
    // The declared status. Finishes that disagree are `conflicting`: never pick one,
    // whatever order they arrived or were stamped in.
    let status = match c.finishes.first() {
        Some((_, outcome, _)) => {
            flags.conflicting_finish = c.finishes.iter().any(|(_, o, _)| o != outcome);
            if flags.conflicting_finish {
                Status::conflicting()
            } else {
                Status::from_outcome(*outcome)
            }
        }
        None => Status::no_finish_observed(),
    };
    let expired = retention.filter(|r| r.first_received_at < r.cutoff);
    let status = match expired {
        // Finishes may straddle the cutoff: when the summary recorded a conflict, it stands
        // whichever finish survived (review N10).
        Some(r) if r.finish_conflict => {
            flags.conflicting_finish = true;
            Status::conflicting()
        }
        // The finish may itself have expired: the summary kept the declaration.
        Some(r) if status.state == "no_finish_observed" => match r.finish {
            Some(_) if r.finish_conflict => Status::conflicting(),
            Some(outcome) => Status::from_outcome(outcome),
            None => status,
        },
        _ => status,
    };
    if let Some(r) = expired {
        apply_expiry(&mut c, r);
    }
    let mut cost = CostView {
        label: "estimate",
        ..CostView::default()
    };
    let mut effect_unknown_steps = Vec::new();
    let steps = build_steps(&c, &mut flags, &mut cost, &mut effect_unknown_steps);
    if c.coverage.missing_known > 0 || c.coverage.reported_dropped > 0 {
        flags.missing_events_may_hide_effects = true;
        flags.external_effect_unknown = true;
    }
    let entries = build_entries(&events, &c.step_start_time, limits, &mut flags);
    let sources = source_views(instances, &events);
    let meta = c.meta;

    Timeline {
        operation_id: operation_id.to_string(),
        name: meta.map(|m| m.name.clone()),
        request_id: meta.and_then(|m| m.request_id.clone()),
        trace_id: meta.and_then(|m| m.trace_id.map(|t| t.to_string())),
        release: meta.and_then(|m| m.release.clone()),
        deployment: meta.and_then(|m| m.deployment.clone()),
        status,
        flags,
        coverage: c.coverage,
        cost,
        steps,
        entries,
        gaps: c.gaps,
        sources,
        effect_unknown_steps,
        expired_by_retention: expired.is_some(),
        retention_cutoff: retention.map(|r| r.cutoff),
        events_truncated,
    }
}

/// Raw events of this operation were deleted by retention: the holes they leave are
/// expired, not proven loss. The expired count is exact (the summary counted every stored
/// event); holes beyond it are still missing, though which numbers they are is unknown.
fn apply_expiry(c: &mut Collected<'_>, r: &RetentionContext) {
    let expired = r.events_received.saturating_sub(c.coverage.events_received);
    let holes: u64 = c
        .gaps
        .iter()
        .filter(|g| g.kind == "op_seq_hole")
        .filter_map(|g| g.count)
        .sum();
    c.gaps.retain(|g| g.kind != "op_seq_hole");
    let still_missing = holes.saturating_sub(u64::from(expired));
    if still_missing > 0 {
        c.gaps.push(GapView {
            kind: "op_seq_hole",
            instance_id: String::new(),
            count: Some(still_missing),
            reason: None,
            from: None,
            to: None,
            op_seq_from: None,
            op_seq_to: None,
        });
    }
    if expired > 0 {
        c.gaps.push(GapView {
            kind: "expired",
            instance_id: String::new(),
            count: Some(u64::from(expired)),
            reason: None,
            from: None,
            to: Some(r.cutoff),
            op_seq_from: None,
            op_seq_to: None,
        });
    }
    c.coverage.missing_known = u32::try_from(still_missing).unwrap_or(u32::MAX);
    c.coverage.expired = expired;
    for i in &mut c.coverage.per_instance {
        i.missing_known = 0;
        i.missing_op_seq.clear();
    }
}

/// Each source instance's events in `seq` order (its emission order, D-019), merged across
/// instances by event time; a tie keeps the instance order, so the result is deterministic.
fn emission_order(mut events: Vec<StoredEvent>) -> Vec<StoredEvent> {
    events.sort_by_key(|e| (e.instance_id, e.event.seq));
    let mut queues: Vec<std::collections::VecDeque<StoredEvent>> = Vec::new();
    for e in events {
        match queues.last_mut() {
            Some(q) if q.back().is_some_and(|b| b.instance_id == e.instance_id) => q.push_back(e),
            _ => queues.push(std::collections::VecDeque::from([e])),
        }
    }
    let mut out = Vec::with_capacity(queues.iter().map(std::collections::VecDeque::len).sum());
    loop {
        let next = queues
            .iter()
            .enumerate()
            .filter_map(|(i, q)| q.front().map(|e| (e.event.event_time, i)))
            .min();
        let Some((_, i)) = next else { break };
        if let Some(e) = queues[i].pop_front() {
            out.push(e);
        }
    }
    out
}

fn hole(instance: InstanceId, from: u32, to: u32) -> GapView {
    GapView {
        kind: "op_seq_hole",
        instance_id: instance.to_string(),
        count: Some(u64::from(to - from + 1)),
        reason: None,
        from: None,
        to: None,
        op_seq_from: Some(from),
        op_seq_to: Some(to),
    }
}

fn attempt_view(
    n: u32,
    acc: &AttemptAcc<'_>,
    step: &mut StepView,
    cost: &mut CostView,
) -> AttemptView {
    let mut view = AttemptView {
        attempt: n,
        state: "no_finish_observed",
        class: None,
        reason: None,
        start_observed: acc.start.is_some(),
        finish_observed: acc.finish.is_some(),
        started_at: None,
        finished_at: None,
        duration_us: None,
        model: None,
        prompt_version: None,
        config_version: None,
        error_code: None,
        error_message: None,
        input_tokens: None,
        output_tokens: None,
        cost_estimate_micros: None,
        cost_currency: None,
    };
    if let Some(stored) = acc.start
        && let EventBody::StepStarted(s) = &stored.event.body
    {
        view.started_at = Some(stored.event.event_time);
        view.model.clone_from(&s.model);
        view.prompt_version.clone_from(&s.prompt_version);
        view.config_version.clone_from(&s.config_version);
        step.parent_step_id = s.parent_step_id.map(|p| p.to_string());
        step.kind = Some(s.kind.as_str());
        step.name = Some(s.name.clone());
        step.external_effect |= s.external_effect;
        step.first_event_time = step.first_event_time.min(stored.event.event_time);
    }
    if let Some(stored) = acc.finish
        && let EventBody::StepFinished(f) = &stored.event.body
    {
        let status = Status::from_outcome(f.outcome);
        view.state = status.state;
        view.class = status.class;
        view.reason = status.reason;
        view.finished_at = Some(stored.event.event_time);
        view.duration_us = view
            .started_at
            .map(|s| stored.event.event_time.micros_since(s));
        view.error_code.clone_from(&f.error_code);
        view.error_message.clone_from(&f.error_message);
        if step.kind.is_none() {
            step.kind = f.kind.map(StepKind::as_str);
        }
        if step.name.is_none() {
            step.name.clone_from(&f.name);
        }
        step.first_event_time = step.first_event_time.min(stored.event.event_time);
        if let Some(u) = f.usage {
            view.input_tokens = Some(u.input_tokens);
            view.output_tokens = Some(u.output_tokens);
            cost.input_tokens = cost.input_tokens.saturating_add(u.input_tokens);
            cost.output_tokens = cost.output_tokens.saturating_add(u.output_tokens);
        }
        if let Some(c) = &f.cost_estimate {
            view.cost_estimate_micros = Some(c.micros);
            view.cost_currency = Some(c.currency.clone());
            cost.attempts_with_estimate += 1;
            cost.estimate_micros = cost.estimate_micros.saturating_add(c.micros);
            match &cost.currency {
                None => cost.currency = Some(c.currency.clone()),
                Some(cur) if *cur != c.currency => cost.mixed_currencies = true,
                Some(_) => {}
            }
        }
    }
    view
}

fn entry_view(stored: &StoredEvent, delay: i64, flags: Vec<&'static str>) -> EntryView {
    let e = &stored.event;
    let mut view = EntryView {
        event_id: e.event_id.to_string(),
        event_type: e.body.type_str(),
        instance_id: stored.instance_id.to_string(),
        ingest_key: stored.ingest_key.clone(),
        seq: e.seq,
        op_seq: e.op_seq,
        event_time: e.event_time,
        received_at: stored.received_at,
        receive_delay_us: delay,
        flags,
        step_id: e.step_id().map(|s| s.to_string()),
        attempt: None,
        kind: None,
        name: None,
        outcome: None,
        dropped: None,
        reason: None,
        attributes: e
            .attributes
            .iter()
            .map(|(k, v)| AttrView {
                key: k.clone(),
                value: match v {
                    AttrValue::Bool(b) => serde_json::Value::Bool(*b),
                    AttrValue::Int(i) => serde_json::Value::from(*i),
                    AttrValue::Float(f) => serde_json::Number::from_f64(*f)
                        .map_or(serde_json::Value::Null, serde_json::Value::Number),
                    AttrValue::Str(s) => serde_json::Value::String(s.clone()),
                },
                opt_in: !is_default_attribute(k),
            })
            .collect(),
        withheld_attributes: stored.marks.withheld_attributes,
        redactions: stored.marks.redactions,
        overflowed_names: stored.marks.overflowed_names,
        truncated: e.truncated,
    };
    match &e.body {
        EventBody::OperationStarted(m) => view.name = Some(m.name.clone()),
        EventBody::OperationFinished(f) => view.outcome = Some(Status::from_outcome(f.outcome)),
        EventBody::StepStarted(s) => {
            view.attempt = Some(s.attempt);
            view.kind = Some(s.kind.as_str());
            view.name = Some(s.name.clone());
        }
        EventBody::StepFinished(f) => {
            view.attempt = Some(f.attempt);
            view.kind = f.kind.map(StepKind::as_str);
            view.name.clone_from(&f.name);
            view.outcome = Some(Status::from_outcome(f.outcome));
        }
        EventBody::GapReported(g) => {
            view.dropped = Some(g.dropped);
            view.reason = Some(g.reason.as_str());
        }
    }
    view
}

/// For lists: the state of an operation from its summary counters alone.
pub fn list_state(finish: Option<Outcome>, conflict: bool) -> Status {
    if conflict {
        return Status::conflicting();
    }
    finish.map_or_else(Status::no_finish_observed, Status::from_outcome)
}
