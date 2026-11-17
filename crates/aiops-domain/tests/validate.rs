//! Every way an event can be wrong is refused by name (A0 gate), and the policy changes
//! the collector is allowed to make are labeled and counted.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures: a fixture that fails to build is a test failure"
)]

use aiops_domain::policy::Policy;
use aiops_domain::wire::{WireCost, WireEvent, WireUsage};
use aiops_domain::{Limits, Mode, RefusalCode, Timestamp, Validator};

const NOW: &str = "2026-11-16T10:00:00Z";

fn now() -> Timestamp {
    Timestamp::parse_rfc3339(NOW).unwrap()
}

fn base(event_type: &str) -> WireEvent {
    WireEvent {
        event_id: "0123456789abcdef0123456789abcdef".into(),
        event_type: event_type.into(),
        operation_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
        seq: 1,
        op_seq: 1,
        event_time: "2026-11-16T09:59:59.5Z".into(),
        ..WireEvent::default()
    }
}

fn started() -> WireEvent {
    WireEvent {
        name: Some("publish_article".into()),
        ..base("operation.started")
    }
}

fn step_started() -> WireEvent {
    WireEvent {
        step_id: Some("00f067aa0ba902b7".into()),
        kind: Some("model".into()),
        name: Some("draft".into()),
        attempt: Some(1),
        model: Some("fake-model-1".into()),
        ..base("step.started")
    }
}

fn step_finished(outcome: &str) -> WireEvent {
    WireEvent {
        step_id: Some("00f067aa0ba902b7".into()),
        attempt: Some(1),
        outcome: Some(outcome.into()),
        ..base("step.finished")
    }
}

fn gap() -> WireEvent {
    WireEvent {
        op_seq: 0,
        dropped: Some(3),
        reason: Some("queue_full".into()),
        first_dropped_at: Some("2026-11-16T09:59:58Z".into()),
        last_dropped_at: Some("2026-11-16T09:59:59Z".into()),
        ..base("gap.reported")
    }
}

fn check(w: WireEvent) -> Result<aiops_domain::Validated, aiops_domain::Refusal> {
    let limits = Limits::default();
    let policy = Policy::default();
    Validator {
        limits: &limits,
        policy: &policy,
        mode: Mode::Ingest { received_at: now() },
    }
    .validate(w)
}

fn refused(w: WireEvent) -> (RefusalCode, Option<&'static str>) {
    let r = check(w).expect_err("expected a refusal");
    (r.code, r.field)
}

#[test]
fn accepts_each_event_type_in_its_valid_form() {
    for w in [
        started(),
        step_started(),
        step_finished("succeeded"),
        WireEvent {
            failure_class: Some("transport".into()),
            ..step_finished("failed")
        },
        WireEvent {
            uncertain_reason: Some("timeout_after_send".into()),
            ..step_finished("uncertain")
        },
        WireEvent {
            outcome: Some("succeeded".into()),
            events_emitted: Some(4),
            op_seq: 4,
            ..base("operation.finished")
        },
        gap(),
    ] {
        let t = w.event_type.clone();
        assert!(check(w).is_ok(), "{t} should be accepted");
    }
}

#[test]
fn round_trips_through_the_wire_form() {
    let w = WireEvent {
        usage: Some(WireUsage {
            input_tokens: 10,
            output_tokens: 20,
        }),
        cost_estimate: Some(WireCost {
            micros: 1_500,
            currency: "USD".into(),
            basis: "list-price-2026-11".into(),
        }),
        kind: Some("model".into()),
        name: Some("draft".into()),
        error_code: Some("ok".into()),
        ..step_finished("succeeded")
    };
    let v = check(w).unwrap();
    let again = check(v.event.to_wire()).unwrap();
    assert_eq!(v.event, again.event);
}

#[test]
fn refuses_malformed_unknown_fields_and_oversized_events_by_name() {
    let limits = Limits::default();
    let policy = Policy::default();
    let v = Validator {
        limits: &limits,
        policy: &policy,
        mode: Mode::Ingest { received_at: now() },
    };
    assert_eq!(
        v.validate_raw("[1,2]").unwrap_err().1.code,
        RefusalCode::Malformed
    );
    let mut json = serde_json::to_value(started()).unwrap();
    json["prompt"] = "write about cats".into();
    let raw = json.to_string();
    assert_eq!(
        v.validate_raw(&raw).unwrap_err().1.code,
        RefusalCode::Malformed
    );
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(9_000));
    assert_eq!(
        v.validate_raw(&big).unwrap_err().1.code,
        RefusalCode::EventTooLarge
    );
}

#[test]
fn refuses_bad_ids_types_and_sequences() {
    assert_eq!(
        refused(WireEvent {
            event_id: "nothex".into(),
            ..started()
        }),
        (RefusalCode::InvalidId, Some("event_id"))
    );
    assert_eq!(
        refused(WireEvent {
            operation_id: "00000000000000000000000000000000".into(),
            ..started()
        }),
        (RefusalCode::InvalidId, Some("operation_id"))
    );
    assert_eq!(
        refused(base("operation.restarted")),
        (RefusalCode::UnknownEventType, None)
    );
    assert_eq!(
        refused(WireEvent {
            seq: 0,
            ..started()
        }),
        (RefusalCode::InvalidSequence, Some("seq"))
    );
    assert_eq!(
        refused(WireEvent {
            op_seq: 0,
            ..started()
        }),
        (RefusalCode::InvalidSequence, Some("op_seq"))
    );
    assert_eq!(
        refused(WireEvent { op_seq: 2, ..gap() }),
        (RefusalCode::InvalidSequence, Some("op_seq"))
    );
    assert_eq!(
        refused(WireEvent {
            outcome: Some("succeeded".into()),
            events_emitted: Some(2),
            op_seq: 5,
            ..base("operation.finished")
        }),
        (RefusalCode::InvalidSequence, Some("events_emitted"))
    );
}

#[test]
fn refuses_missing_and_misplaced_fields() {
    assert_eq!(
        refused(base("operation.started")),
        (RefusalCode::MissingField, Some("name"))
    );
    assert_eq!(
        refused(WireEvent {
            outcome: Some("succeeded".into()),
            ..started()
        }),
        (RefusalCode::FieldNotAllowed, Some("outcome"))
    );
    assert_eq!(
        refused(WireEvent {
            step_id: None,
            ..step_started()
        }),
        (RefusalCode::MissingField, Some("step_id"))
    );
    assert_eq!(
        refused(WireEvent {
            attempt: Some(0),
            ..step_started()
        }),
        (RefusalCode::InvalidValue, Some("attempt"))
    );
    assert_eq!(
        refused(WireEvent {
            kind: Some("llm".into()),
            ..step_started()
        }),
        (RefusalCode::InvalidValue, Some("kind"))
    );
    assert_eq!(
        refused(WireEvent {
            dropped: Some(0),
            ..gap()
        }),
        (RefusalCode::InvalidValue, Some("dropped"))
    );
}

#[test]
fn outcome_fields_must_agree() {
    assert_eq!(
        refused(step_finished("failed")),
        (RefusalCode::InvalidOutcome, Some("outcome"))
    );
    assert_eq!(
        refused(WireEvent {
            failure_class: Some("timeout".into()),
            ..step_finished("failed")
        }),
        (RefusalCode::InvalidOutcome, Some("failure_class"))
    );
    assert_eq!(
        refused(WireEvent {
            failure_class: Some("transport".into()),
            ..step_finished("uncertain")
        }),
        (RefusalCode::InvalidOutcome, Some("outcome"))
    );
    assert_eq!(
        refused(step_finished("maybe")),
        (RefusalCode::InvalidValue, Some("outcome"))
    );
}

#[test]
fn names_are_bounded_identifiers() {
    assert_eq!(
        refused(WireEvent {
            name: Some("x".repeat(129)),
            ..started()
        }),
        (RefusalCode::TooLong, Some("name"))
    );
    assert_eq!(
        refused(WireEvent {
            name: Some("<script>alert(1)</script>".into()),
            ..started()
        }),
        (RefusalCode::InvalidValue, Some("name"))
    );
    assert_eq!(
        refused(WireEvent {
            release: Some("has space".into()),
            ..started()
        }),
        (RefusalCode::InvalidValue, Some("release"))
    );
}

#[test]
fn time_must_parse_and_sit_inside_the_window() {
    assert_eq!(
        refused(WireEvent {
            event_time: "yesterday".into(),
            ..started()
        }),
        (RefusalCode::InvalidTime, Some("event_time"))
    );
    assert_eq!(
        refused(WireEvent {
            event_time: "2026-11-16T12:00:01Z".into(),
            ..started()
        }),
        (RefusalCode::EventTimeOutOfRange, None)
    );
    assert_eq!(
        refused(WireEvent {
            event_time: "2026-10-01T00:00:00Z".into(),
            ..started()
        }),
        (RefusalCode::EventTimeOutOfRange, None)
    );
    // Skewed but inside the hour: accepted (the timeline flags it).
    assert!(
        check(WireEvent {
            event_time: "2026-11-16T10:00:30Z".into(),
            ..started()
        })
        .is_ok()
    );
    assert_eq!(
        refused(WireEvent {
            last_dropped_at: Some("2026-11-16T09:00:00Z".into()),
            ..gap()
        }),
        (RefusalCode::InvalidTime, Some("last_dropped_at"))
    );
}

#[test]
fn attributes_are_default_deny_counted_and_redacted() {
    let mut attrs = serde_json::Map::new();
    attrs.insert("http.route".into(), "/articles".into());
    attrs.insert("prompt".into(), "write about cats".into());
    attrs.insert("llm.output".into(), "Cats are…".into());
    attrs.insert(
        "retry.reason".into(),
        "Authorization: Bearer abcdefgh12345678".into(),
    );
    let v = check(WireEvent {
        attributes: Some(attrs),
        ..started()
    })
    .unwrap();
    assert_eq!(v.marks.withheld_attributes, 2);
    assert_eq!(v.marks.redactions, 1);
    let keys: Vec<_> = v.event.attributes.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["http.route", "retry.reason"]);
    let wire = serde_json::to_string(&v.event.to_wire()).unwrap();
    assert!(
        !wire.contains("cats"),
        "withheld content must not survive: {wire}"
    );
    assert!(
        !wire.contains("abcdefgh12345678"),
        "secret must be redacted: {wire}"
    );
}

#[test]
fn error_messages_are_redacted_and_bounded() {
    let v = check(WireEvent {
        failure_class: Some("rejected".into()),
        error_message: Some("provider said: invalid api_key=sk-abcdefghijklmnop".into()),
        ..step_finished("failed")
    })
    .unwrap();
    assert_eq!(v.marks.redactions, 1);
    let wire = serde_json::to_string(&v.event.to_wire()).unwrap();
    assert!(!wire.contains("sk-abcdefghijklmnop"));
    assert_eq!(
        refused(WireEvent {
            failure_class: Some("rejected".into()),
            error_message: Some("e".repeat(513)),
            ..step_finished("failed")
        }),
        (RefusalCode::TooLong, Some("error_message"))
    );
}

#[test]
fn attribute_shape_is_bounded() {
    let mut many = serde_json::Map::new();
    for i in 0..33 {
        many.insert(format!("k{i}"), 1.into());
    }
    assert_eq!(
        refused(WireEvent {
            attributes: Some(many),
            ..started()
        }),
        (RefusalCode::TooManyAttributes, None)
    );
    let mut nested = serde_json::Map::new();
    nested.insert("http.route".into(), serde_json::json!({"a": 1}));
    assert_eq!(
        refused(WireEvent {
            attributes: Some(nested),
            ..started()
        }),
        (RefusalCode::InvalidAttribute, Some("attributes"))
    );
    let mut upper = serde_json::Map::new();
    upper.insert("Http.Route".into(), "/".into());
    assert_eq!(
        refused(WireEvent {
            attributes: Some(upper),
            ..started()
        }),
        (RefusalCode::InvalidAttribute, Some("attributes"))
    );
}

#[test]
fn cost_is_a_labeled_estimate_with_a_real_currency() {
    assert_eq!(
        refused(WireEvent {
            cost_estimate: Some(WireCost {
                micros: 10,
                currency: "usd".into(),
                basis: "list".into(),
            }),
            ..step_finished("succeeded")
        }),
        (RefusalCode::InvalidValue, Some("cost_estimate"))
    );
}

#[test]
fn every_refusal_code_has_a_distinct_stable_name() {
    let mut names: Vec<_> = RefusalCode::ALL.iter().map(|c| c.as_str()).collect();
    let n = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), n);
    assert!(
        names
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    );
}

// Review finding 18: a gap report's window is held to the event-time window too.
#[test]
fn a_gap_reports_drop_window_must_be_inside_the_event_time_window() {
    assert_eq!(
        refused(WireEvent {
            first_dropped_at: Some("1970-01-01T00:00:00Z".into()),
            ..gap()
        }),
        (RefusalCode::EventTimeOutOfRange, Some("first_dropped_at"))
    );
    assert_eq!(
        refused(WireEvent {
            last_dropped_at: Some("2099-01-01T00:00:00Z".into()),
            ..gap()
        }),
        (RefusalCode::EventTimeOutOfRange, Some("last_dropped_at"))
    );
}
