//! The timeline never invents anything: declared status or `no_finish_observed`, exact
//! loss from sequence numbers, unknown loss as unknown, both clocks, skew flagged.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures: a fixture that fails to build is a test failure"
)]

use aiops_domain::event::{
    CostEstimate, Event, EventBody, FailureClass, GapReason, GapReport, OperationFinish,
    OperationMeta, Outcome, StepFinish, StepKind, StepStart, UncertainReason, Usage,
};
use aiops_domain::timeline::{StoredEvent, assemble};
use aiops_domain::{EventId, InstanceId, Limits, OperationId, PolicyMarks, StepId, Timestamp};

const T0: i64 = 1_763_287_200_000_000; // 2025-11-16T10:00:00Z in µs

fn op() -> OperationId {
    OperationId::parse("4bf92f3577b34da6a3ce929d0e0e4736").unwrap()
}

fn instance(n: u8) -> InstanceId {
    InstanceId::from_bytes([n; 16]).unwrap()
}

fn step(n: u8) -> StepId {
    StepId::from_bytes([n; 8]).unwrap()
}

struct B {
    seq: u64,
    op_seq: u32,
    instance: InstanceId,
}

impl B {
    fn new() -> Self {
        Self {
            seq: 0,
            op_seq: 0,
            instance: instance(1),
        }
    }

    /// An event at `t_ms` after T0, received `delay_ms` later.
    fn ev(&mut self, t_ms: i64, delay_ms: i64, body: EventBody) -> StoredEvent {
        self.seq += 1;
        let gap = matches!(body, EventBody::GapReported(_));
        if !gap {
            self.op_seq += 1;
        }
        let mut id = [0u8; 16];
        id[8..].copy_from_slice(&self.seq.to_be_bytes());
        id[0] = 0xee;
        StoredEvent {
            event: Event {
                event_id: EventId::from_bytes(id).unwrap(),
                operation_id: op(),
                seq: self.seq,
                op_seq: if gap { 0 } else { self.op_seq },
                event_time: Timestamp::from_unix_micros(T0 + t_ms * 1_000),
                body,
                attributes: Vec::new(),
                truncated: 0,
            },
            received_at: Timestamp::from_unix_micros(T0 + (t_ms + delay_ms) * 1_000),
            instance_id: self.instance,
            ingest_key: "reference-host".into(),
            marks: PolicyMarks::default(),
        }
    }
}

fn started() -> EventBody {
    EventBody::OperationStarted(OperationMeta {
        name: "publish_article".into(),
        request_id: Some("req-1".into()),
        release: Some("r42".into()),
        ..OperationMeta::default()
    })
}

fn step_start(id: u8, kind: StepKind, name: &str, attempt: u32, external: bool) -> EventBody {
    EventBody::StepStarted(StepStart {
        step_id: step(id),
        parent_step_id: None,
        kind,
        name: name.into(),
        attempt,
        model: (kind == StepKind::Model).then(|| "fake-model-1".into()),
        prompt_version: None,
        config_version: None,
        external_effect: external,
    })
}

fn step_finish(id: u8, attempt: u32, outcome: Outcome, cost: Option<u64>) -> EventBody {
    EventBody::StepFinished(StepFinish {
        step_id: step(id),
        attempt,
        kind: None,
        name: None,
        outcome,
        error_code: None,
        error_message: None,
        usage: cost.map(|_| Usage {
            input_tokens: 100,
            output_tokens: 400,
        }),
        cost_estimate: cost.map(|micros| CostEstimate {
            micros,
            currency: "USD".into(),
            basis: "list-price-2026-11".into(),
        }),
    })
}

fn finished(outcome: Outcome, emitted: u32) -> EventBody {
    EventBody::OperationFinished(OperationFinish {
        outcome,
        events_emitted: emitted,
        error_code: None,
        error_message: None,
    })
}

/// request → generation (retried once) → validation → publish → finish: 10 events.
fn full_journey(b: &mut B) -> Vec<StoredEvent> {
    vec![
        b.ev(0, 5, started()),
        b.ev(1, 5, step_start(1, StepKind::Model, "generate", 1, false)),
        b.ev(
            2,
            5,
            step_finish(1, 1, Outcome::Failed(FailureClass::Transport), None),
        ),
        b.ev(100, 5, step_start(1, StepKind::Model, "generate", 2, false)),
        b.ev(400, 5, step_finish(1, 2, Outcome::Succeeded, Some(2_500))),
        b.ev(
            401,
            5,
            step_start(2, StepKind::Validation, "validate", 1, false),
        ),
        b.ev(402, 5, step_finish(2, 1, Outcome::Succeeded, None)),
        b.ev(403, 5, step_start(3, StepKind::Tool, "publish", 1, true)),
        b.ev(500, 5, step_finish(3, 1, Outcome::Succeeded, None)),
        b.ev(501, 5, finished(Outcome::Succeeded, 10)),
    ]
}

#[test]
fn a_complete_operation_reassembles_with_retries_cost_and_no_loss() {
    let mut b = B::new();
    let t = assemble(op(), full_journey(&mut b), &[], &Limits::default(), false);
    assert_eq!(t.status.state, "succeeded");
    assert!(t.status.declared);
    assert_eq!(t.name.as_deref(), Some("publish_article"));
    assert_eq!(t.release.as_deref(), Some("r42"));
    assert_eq!(t.steps.len(), 3);
    assert_eq!(t.steps[0].name.as_deref(), Some("generate"));
    assert_eq!(t.steps[0].attempts.len(), 2);
    assert_eq!(t.steps[0].attempts[0].class, Some("transport"));
    assert_eq!(t.flags.retried_steps, 1);
    assert_eq!(t.flags.failed_attempts, 1);
    assert_eq!(t.coverage.events_received, 10);
    assert_eq!(t.coverage.missing_known, 0);
    assert!(!t.coverage.missing_unknown);
    assert_eq!(t.gaps, vec![]);
    assert_eq!(t.cost.label, "estimate");
    assert_eq!(t.cost.estimate_micros, 2_500);
    assert!(!t.flags.external_effect_unknown);
}

#[test]
fn input_order_does_not_change_the_timeline() {
    let mut b = B::new();
    let events = full_journey(&mut b);
    let sorted = assemble(op(), events.clone(), &[], &Limits::default(), false);
    let mut reversed = events;
    reversed.reverse();
    let shuffled = assemble(op(), reversed, &[], &Limits::default(), false);
    assert_eq!(sorted, shuffled);
}

#[test]
fn a_missing_finish_is_no_finish_observed_with_an_unknown_tail_never_did_not_run() {
    let mut b = B::new();
    let mut events = full_journey(&mut b);
    events.truncate(8); // lost: publish's finish and the operation's finish
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert_eq!(t.status.state, "no_finish_observed");
    assert!(!t.status.declared);
    let publish = &t.steps[2];
    assert_eq!(publish.attempts[0].state, "no_finish_observed");
    assert!(t.flags.external_effect_unknown, "publish may have happened");
    assert!(t.coverage.missing_unknown);
    assert!(
        t.gaps
            .iter()
            .any(|g| g.kind == "unknown_tail" && g.count.is_none())
    );
}

#[test]
fn op_seq_holes_are_counted_exactly_and_sdk_reports_shown_beside_them() {
    let mut b = B::new();
    let mut events = full_journey(&mut b);
    // Lose op_seq 4 and 5 (the second generation attempt) and 7.
    events.retain(|e| ![4, 5, 7].contains(&e.event.op_seq));
    events.push(b.ev(
        600,
        5,
        EventBody::GapReported(GapReport {
            dropped: 3,
            reason: GapReason::QueueFull,
            first_dropped_at: Timestamp::from_unix_micros(T0 + 100_000),
            last_dropped_at: Timestamp::from_unix_micros(T0 + 402_000),
        }),
    ));
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert_eq!(t.status.state, "succeeded");
    assert_eq!(t.coverage.missing_known, 3);
    assert_eq!(t.coverage.reported_dropped, 3);
    assert!(!t.coverage.missing_unknown);
    assert_eq!(t.coverage.per_instance[0].missing_op_seq, vec![4, 5, 7]);
    let holes: Vec<_> = t
        .gaps
        .iter()
        .filter(|g| g.kind == "op_seq_hole")
        .map(|g| (g.op_seq_from, g.op_seq_to, g.count))
        .collect();
    assert_eq!(
        holes,
        vec![(Some(4), Some(5), Some(2)), (Some(7), Some(7), Some(1))]
    );
    assert!(
        t.gaps
            .iter()
            .any(|g| g.kind == "sdk_reported" && g.reason == Some("queue_full"))
    );
}

#[test]
fn an_operation_known_only_from_gap_reports_is_shown_as_unknown_not_absent() {
    let mut b = B::new();
    let gap = b.ev(
        900,
        5,
        EventBody::GapReported(GapReport {
            dropped: 7,
            reason: GapReason::SendFailed,
            first_dropped_at: Timestamp::from_unix_micros(T0),
            last_dropped_at: Timestamp::from_unix_micros(T0 + 800_000),
        }),
    );
    let t = assemble(op(), vec![gap], &[], &Limits::default(), false);
    assert_eq!(t.status.state, "no_finish_observed");
    assert!(t.flags.start_not_observed);
    assert_eq!(t.coverage.reported_dropped, 7);
    assert!(t.coverage.missing_unknown);
    assert_eq!(t.steps, vec![]);
}

#[test]
fn an_uncertain_publish_stays_uncertain() {
    let mut b = B::new();
    let events = vec![
        b.ev(0, 5, started()),
        b.ev(1, 5, step_start(3, StepKind::Tool, "publish", 1, true)),
        b.ev(
            5_001,
            5,
            step_finish(
                3,
                1,
                Outcome::Uncertain(UncertainReason::TimeoutAfterSend),
                None,
            ),
        ),
        b.ev(
            5_002,
            5,
            finished(Outcome::Uncertain(UncertainReason::TimeoutAfterSend), 4),
        ),
    ];
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert_eq!(t.status.state, "uncertain");
    assert_eq!(t.status.reason, Some("timeout_after_send"));
    assert_eq!(t.steps[0].attempts[0].state, "uncertain");
    assert_eq!(t.flags.uncertain_attempts, 1);
    assert!(t.flags.external_effect_unknown);
}

#[test]
fn both_clocks_are_kept_and_late_or_skewed_events_are_flagged() {
    let mut b = B::new();
    let mut events = full_journey(&mut b);
    // Arrived 30 s late (a retried batch).
    events[4].received_at = events[4].event.event_time.plus_micros(30_000_000);
    // A host clock 5 s ahead of the collector.
    events[8].received_at = events[8].event.event_time.plus_micros(-5_000_000);
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert_eq!(t.flags.late_events, 1);
    assert!(t.flags.clock_skew_suspected);
    let late = t
        .entries
        .iter()
        .find(|e| e.flags.contains(&"late"))
        .unwrap();
    assert_eq!(late.receive_delay_us, 30_000_000);
    let ahead = t
        .entries
        .iter()
        .find(|e| e.flags.contains(&"clock_ahead"))
        .unwrap();
    assert_eq!(ahead.receive_delay_us, -5_000_000);
}

#[test]
fn a_host_clock_that_jumps_back_is_flagged_not_reordered_silently() {
    let mut b = B::new();
    let mut events = full_journey(&mut b);
    // The 7th emitted event carries an event time before the 6th.
    events[6].event.event_time = events[5].event.event_time.plus_micros(-50_000);
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert!(t.flags.clock_skew_suspected);
    assert!(
        t.entries
            .iter()
            .any(|e| e.flags.contains(&"clock_went_backwards"))
    );
}

#[test]
fn a_finish_whose_start_was_lost_is_legible_and_marked() {
    let mut b = B::new();
    let events = vec![
        b.ev(0, 5, started()),
        b.ev(
            10,
            5,
            EventBody::StepFinished(StepFinish {
                step_id: step(9),
                attempt: 1,
                kind: Some(StepKind::Tool),
                name: Some("fact_check".into()),
                outcome: Outcome::Failed(FailureClass::Rejected),
                error_code: Some("http_422".into()),
                error_message: None,
                usage: None,
                cost_estimate: None,
            }),
        ),
    ];
    let t = assemble(op(), events, &[], &Limits::default(), false);
    let a = &t.steps[0].attempts[0];
    assert!(!a.start_observed);
    assert!(a.finish_observed);
    assert_eq!(t.steps[0].name.as_deref(), Some("fact_check"));
    assert_eq!(a.error_code.as_deref(), Some("http_422"));
}

#[test]
fn conflicting_finish_events_are_flagged() {
    let mut b = B::new();
    let events = vec![
        b.ev(0, 5, started()),
        b.ev(10, 5, finished(Outcome::Succeeded, 3)),
        b.ev(11, 5, finished(Outcome::Failed(FailureClass::Internal), 3)),
    ];
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert!(t.flags.conflicting_finish);
    assert_eq!(
        t.status.state, "conflicting",
        "never pick one of two declared outcomes"
    );
}

#[test]
fn e2_a_downstream_success_never_hides_an_upstream_uncertain_outcome_in_either_order() {
    for downstream_first in [true, false] {
        let mut up = B::new();
        let mut down = B::new();
        down.instance = instance(2);
        let (t_up, t_down) = if downstream_first { (50, 10) } else { (10, 50) };
        let events = vec![
            up.ev(0, 5, started()),
            up.ev(t_up, 5, finished(LOST, 2)),
            down.ev(1, 5, started()),
            down.ev(t_down, 5, finished(Outcome::Succeeded, 2)),
        ];
        let t = assemble(op(), events, &[], &Limits::default(), false);
        assert_eq!(
            t.status.state, "conflicting",
            "downstream first: {downstream_first}"
        );
    }
}

// --- Review finding 1: the external-effect warning must not depend on which events
// arrived or in which order the attempts are read.

fn publish_finish(attempt: u32, outcome: Outcome) -> EventBody {
    EventBody::StepFinished(StepFinish {
        step_id: step(3),
        attempt,
        kind: Some(StepKind::Tool),
        name: Some("publish".into()),
        outcome,
        error_code: None,
        error_message: None,
        usage: None,
        cost_estimate: None,
    })
}

const LOST: Outcome = Outcome::Uncertain(UncertainReason::TimeoutAfterSend);

#[test]
fn e3_a_lost_publish_start_still_warns_that_the_effect_may_have_happened() {
    let mut b = B::new();
    let events = vec![
        b.ev(0, 5, started()),
        // step.started for the publish was lost
        b.ev(10, 5, publish_finish(1, LOST)),
        b.ev(11, 5, finished(Outcome::Failed(FailureClass::Internal), 3)),
    ];
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert_eq!(t.status.state, "failed");
    assert!(
        t.flags.external_effect_unknown,
        "the step's start (and its declaration) was lost; an uncertain attempt may have had an effect"
    );
    assert_eq!(t.effect_unknown_steps.len(), 1);
    assert_eq!(t.effect_unknown_steps[0].cause, "uncertain");
    assert_eq!(t.effect_unknown_steps[0].declared, "unknown");
}

#[test]
fn e4_an_uncertain_first_attempt_with_a_lost_start_warns_even_when_a_retry_succeeded() {
    let mut b = B::new();
    let events = vec![
        b.ev(0, 5, started()),
        b.ev(10, 5, publish_finish(1, LOST)), // attempt 1's start lost
        b.ev(20, 5, step_start(3, StepKind::Tool, "publish", 2, true)),
        b.ev(30, 5, publish_finish(2, Outcome::Succeeded)),
        b.ev(31, 5, finished(Outcome::Succeeded, 5)),
    ];
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert_eq!(t.status.state, "succeeded");
    assert!(t.steps[0].external_effect);
    assert!(
        t.flags.external_effect_unknown,
        "attempt 1 may have published too: a possible duplicate"
    );
}

#[test]
fn e4b_the_same_stream_with_attempt_one_started_gives_the_same_answer() {
    let mut b = B::new();
    let events = vec![
        b.ev(0, 5, started()),
        b.ev(5, 5, step_start(3, StepKind::Tool, "publish", 1, true)),
        b.ev(10, 5, publish_finish(1, LOST)),
        b.ev(20, 5, step_start(3, StepKind::Tool, "publish", 2, true)),
        b.ev(30, 5, publish_finish(2, Outcome::Succeeded)),
        b.ev(31, 5, finished(Outcome::Succeeded, 6)),
    ];
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert!(t.flags.external_effect_unknown);
}

#[test]
fn an_uncertain_attempt_of_a_step_declared_effect_free_does_not_raise_the_effect_warning() {
    let mut b = B::new();
    let events = vec![
        b.ev(0, 5, started()),
        b.ev(5, 5, step_start(4, StepKind::Tool, "fact_check", 1, false)),
        b.ev(
            10,
            5,
            EventBody::StepFinished(StepFinish {
                step_id: step(4),
                attempt: 1,
                kind: Some(StepKind::Tool),
                name: Some("fact_check".into()),
                outcome: LOST,
                error_code: None,
                error_message: None,
                usage: None,
                cost_estimate: None,
            }),
        ),
        b.ev(11, 5, finished(Outcome::Failed(FailureClass::Internal), 4)),
    ];
    let t = assemble(op(), events, &[], &Limits::default(), false);
    assert_eq!(t.flags.uncertain_attempts, 1);
    assert!(!t.flags.external_effect_unknown, "declared effect-free");
    assert_eq!(t.effect_unknown_steps, vec![]);
}
