//! Wire → validated event, with a named refusal for every way an event can be wrong.
//! Parse, don't validate: nothing downstream re-checks what this module proved.

use std::fmt;

use crate::event::{
    AttrValue, CostEstimate, Event, EventBody, FailureClass, GapReason, GapReport, OperationFinish,
    OperationMeta, Outcome, StepFinish, StepKind, StepStart, UncertainReason, Usage,
};
use crate::ids::{EventId, OperationId, StepId, TraceId};
use crate::limits::Limits;
use crate::policy::{KeyTreatment, Policy, looks_like_secret, redact};
use crate::time::Timestamp;
use crate::wire::WireEvent;

/// Stable refusal codes. Clients branch on these; renaming one is a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RefusalCode {
    /// Not a JSON object of the schema's shape (wrong types, unknown fields).
    Malformed,
    EventTooLarge,
    UnknownEventType,
    InvalidId,
    MissingField,
    /// A field this event type does not carry.
    FieldNotAllowed,
    InvalidValue,
    TooLong,
    InvalidTime,
    /// Event time too far in the future or older than the raw retention window.
    EventTimeOutOfRange,
    InvalidSequence,
    /// Outcome fields inconsistent (`failed` without a class, `uncertain` without a reason…).
    InvalidOutcome,
    TooManyAttributes,
    InvalidAttribute,
    /// The project reached its cap of distinct names (D-014, bounded cardinality).
    CardinalityExceeded,
    /// The event ID was already stored with different content (first write wins).
    ConflictingDuplicate,
}

impl RefusalCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::EventTooLarge => "event_too_large",
            Self::UnknownEventType => "unknown_event_type",
            Self::InvalidId => "invalid_id",
            Self::MissingField => "missing_field",
            Self::FieldNotAllowed => "field_not_allowed",
            Self::InvalidValue => "invalid_value",
            Self::TooLong => "too_long",
            Self::InvalidTime => "invalid_time",
            Self::EventTimeOutOfRange => "event_time_out_of_range",
            Self::InvalidSequence => "invalid_sequence",
            Self::InvalidOutcome => "invalid_outcome",
            Self::TooManyAttributes => "too_many_attributes",
            Self::InvalidAttribute => "invalid_attribute",
            Self::CardinalityExceeded => "cardinality_exceeded",
            Self::ConflictingDuplicate => "conflicting_duplicate",
        }
    }

    /// Every code, for exhaustive tests and the contract document.
    pub const ALL: &'static [RefusalCode] = &[
        Self::Malformed,
        Self::EventTooLarge,
        Self::UnknownEventType,
        Self::InvalidId,
        Self::MissingField,
        Self::FieldNotAllowed,
        Self::InvalidValue,
        Self::TooLong,
        Self::InvalidTime,
        Self::EventTimeOutOfRange,
        Self::InvalidSequence,
        Self::InvalidOutcome,
        Self::TooManyAttributes,
        Self::InvalidAttribute,
        Self::CardinalityExceeded,
        Self::ConflictingDuplicate,
    ];
}

/// Why one event was refused. `field` names the schema field, never echoes client data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub code: RefusalCode,
    pub field: Option<&'static str>,
}

impl Refusal {
    pub const fn new(code: RefusalCode) -> Self {
        Self { code, field: None }
    }

    pub const fn on(code: RefusalCode, field: &'static str) -> Self {
        Self {
            code,
            field: Some(field),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.field {
            Some(field) => write!(f, "{} ({field})", self.code.as_str()),
            None => f.write_str(self.code.as_str()),
        }
    }
}

impl std::error::Error for Refusal {}

/// Where validation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// On ingest: time windows are checked against the receive time.
    Ingest { received_at: Timestamp },
    /// Re-reading what was stored: time windows were checked on the way in.
    Stored,
}

/// What the collector changed, labeled and counted (D-014).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PolicyMarks {
    pub withheld_attributes: u32,
    pub redactions: u32,
    /// Names over the project's or the key's budget, stored as `_overflow` (finding 11).
    pub overflowed_names: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub event: Event,
    pub marks: PolicyMarks,
}

pub struct Validator<'a> {
    pub limits: &'a Limits,
    pub policy: &'a Policy,
    pub mode: Mode,
}

/// The event's ID when it can be read, for refusal reports.
pub fn peek_event_id(w: &WireEvent) -> Option<EventId> {
    EventId::parse(&w.event_id).ok()
}

impl Validator<'_> {
    /// Size check, parse, validate. Errors carry no client data.
    pub fn validate_raw(&self, raw: &str) -> Result<Validated, (Option<EventId>, Refusal)> {
        if raw.len() > self.limits.max_event_bytes {
            return Err((None, Refusal::new(RefusalCode::EventTooLarge)));
        }
        let wire: WireEvent =
            serde_json::from_str(raw).map_err(|_| (None, Refusal::new(RefusalCode::Malformed)))?;
        let id = peek_event_id(&wire);
        self.validate(wire).map_err(|r| (id, r))
    }

    pub fn validate(&self, w: WireEvent) -> Result<Validated, Refusal> {
        let mut marks = PolicyMarks::default();
        let event_id = EventId::parse(&w.event_id)
            .map_err(|_| Refusal::on(RefusalCode::InvalidId, "event_id"))?;
        let operation_id = OperationId::parse(&w.operation_id)
            .map_err(|_| Refusal::on(RefusalCode::InvalidId, "operation_id"))?;
        if w.seq == 0 {
            return Err(Refusal::on(RefusalCode::InvalidSequence, "seq"));
        }
        let event_time = time(&w.event_time, "event_time")?;
        self.check_window(event_time)?;
        let truncated = w.truncated.unwrap_or(0);

        let body = match w.event_type.as_str() {
            "operation.started" => self.operation_started(&w)?,
            "operation.finished" => self.operation_finished(&w, &mut marks)?,
            "step.started" => self.step_started(&w)?,
            "step.finished" => self.step_finished(&w, &mut marks)?,
            "gap.reported" => self.gap_reported(&w)?,
            _ => return Err(Refusal::new(RefusalCode::UnknownEventType)),
        };

        let attributes = self.attributes(w.attributes, &mut marks)?;
        Ok(Validated {
            event: Event {
                event_id,
                operation_id,
                seq: w.seq,
                op_seq: w.op_seq,
                event_time,
                body,
                attributes,
                truncated,
            },
            marks,
        })
    }

    fn operation_started(&self, w: &WireEvent) -> Result<EventBody, Refusal> {
        only(
            w,
            &["name", "trace_id", "request_id", "release", "deployment"],
        )?;
        host_seq(w)?;
        Ok(EventBody::OperationStarted(OperationMeta {
            name: self.name(w.name.as_deref(), "name")?,
            trace_id: w
                .trace_id
                .as_deref()
                .map(TraceId::parse)
                .transpose()
                .map_err(|_| Refusal::on(RefusalCode::InvalidId, "trace_id"))?,
            request_id: opt_ident(
                w.request_id.as_deref(),
                "request_id",
                self.limits.max_request_id_bytes,
            )?,
            release: self.opt_name(w.release.as_deref(), "release")?,
            deployment: self.opt_name(w.deployment.as_deref(), "deployment")?,
        }))
    }

    fn operation_finished(
        &self,
        w: &WireEvent,
        marks: &mut PolicyMarks,
    ) -> Result<EventBody, Refusal> {
        only(
            w,
            &[
                "outcome",
                "failure_class",
                "uncertain_reason",
                "events_emitted",
                "error_code",
                "error_message",
            ],
        )?;
        host_seq(w)?;
        let events_emitted = w
            .events_emitted
            .ok_or(Refusal::on(RefusalCode::MissingField, "events_emitted"))?;
        if events_emitted < w.op_seq {
            return Err(Refusal::on(RefusalCode::InvalidSequence, "events_emitted"));
        }
        Ok(EventBody::OperationFinished(OperationFinish {
            outcome: outcome(w)?,
            events_emitted,
            error_code: opt_ident(
                w.error_code.as_deref(),
                "error_code",
                self.limits.max_error_code_bytes,
            )?,
            error_message: self.message(w.error_message.as_deref(), marks)?,
        }))
    }

    fn step_started(&self, w: &WireEvent) -> Result<EventBody, Refusal> {
        only(
            w,
            &[
                "step_id",
                "parent_step_id",
                "kind",
                "name",
                "attempt",
                "model",
                "prompt_version",
                "config_version",
                "external_effect",
            ],
        )?;
        host_seq(w)?;
        Ok(EventBody::StepStarted(StepStart {
            step_id: step_id(w.step_id.as_deref(), "step_id")?
                .ok_or(Refusal::on(RefusalCode::MissingField, "step_id"))?,
            parent_step_id: step_id(w.parent_step_id.as_deref(), "parent_step_id")?,
            kind: kind(w.kind.as_deref())?.ok_or(Refusal::on(RefusalCode::MissingField, "kind"))?,
            name: self.name(w.name.as_deref(), "name")?,
            attempt: self.attempt(w.attempt)?,
            model: self.opt_name(w.model.as_deref(), "model")?,
            prompt_version: self.opt_name(w.prompt_version.as_deref(), "prompt_version")?,
            config_version: self.opt_name(w.config_version.as_deref(), "config_version")?,
            external_effect: w.external_effect.unwrap_or(false),
        }))
    }

    fn step_finished(&self, w: &WireEvent, marks: &mut PolicyMarks) -> Result<EventBody, Refusal> {
        only(
            w,
            &[
                "step_id",
                "attempt",
                "kind",
                "name",
                "outcome",
                "failure_class",
                "uncertain_reason",
                "error_code",
                "error_message",
                "usage",
                "cost_estimate",
            ],
        )?;
        host_seq(w)?;
        Ok(EventBody::StepFinished(StepFinish {
            step_id: step_id(w.step_id.as_deref(), "step_id")?
                .ok_or(Refusal::on(RefusalCode::MissingField, "step_id"))?,
            attempt: self.attempt(w.attempt)?,
            kind: kind(w.kind.as_deref())?,
            name: self.opt_name(w.name.as_deref(), "name")?,
            outcome: outcome(w)?,
            error_code: opt_ident(
                w.error_code.as_deref(),
                "error_code",
                self.limits.max_error_code_bytes,
            )?,
            error_message: self.message(w.error_message.as_deref(), marks)?,
            usage: self.usage(w)?,
            cost_estimate: self.cost(w)?,
        }))
    }

    fn gap_reported(&self, w: &WireEvent) -> Result<EventBody, Refusal> {
        only(
            w,
            &["dropped", "reason", "first_dropped_at", "last_dropped_at"],
        )?;
        if w.op_seq != 0 {
            return Err(Refusal::on(RefusalCode::InvalidSequence, "op_seq"));
        }
        if w.attributes.is_some() {
            return Err(Refusal::on(RefusalCode::FieldNotAllowed, "attributes"));
        }
        let dropped = w
            .dropped
            .ok_or(Refusal::on(RefusalCode::MissingField, "dropped"))?;
        if dropped == 0 || dropped > self.limits.max_dropped_per_report {
            return Err(Refusal::on(RefusalCode::InvalidValue, "dropped"));
        }
        let reason = w
            .reason
            .as_deref()
            .ok_or(Refusal::on(RefusalCode::MissingField, "reason"))?;
        let reason =
            GapReason::parse(reason).ok_or(Refusal::on(RefusalCode::InvalidValue, "reason"))?;
        let first = time(
            w.first_dropped_at
                .as_deref()
                .ok_or(Refusal::on(RefusalCode::MissingField, "first_dropped_at"))?,
            "first_dropped_at",
        )?;
        let last = time(
            w.last_dropped_at
                .as_deref()
                .ok_or(Refusal::on(RefusalCode::MissingField, "last_dropped_at"))?,
            "last_dropped_at",
        )?;
        if last < first {
            return Err(Refusal::on(RefusalCode::InvalidTime, "last_dropped_at"));
        }
        // The drop window is held to the event-time window too (review finding 18).
        self.check_window(first)
            .map_err(|_| Refusal::on(RefusalCode::EventTimeOutOfRange, "first_dropped_at"))?;
        self.check_window(last)
            .map_err(|_| Refusal::on(RefusalCode::EventTimeOutOfRange, "last_dropped_at"))?;
        Ok(EventBody::GapReported(GapReport {
            dropped,
            reason,
            first_dropped_at: first,
            last_dropped_at: last,
        }))
    }

    fn check_window(&self, t: Timestamp) -> Result<(), Refusal> {
        if let Mode::Ingest { received_at } = self.mode {
            let ahead = t.micros_since(received_at);
            let behind = received_at.micros_since(t);
            if ahead > self.limits.max_future_micros || behind > self.limits.max_past_micros {
                return Err(Refusal::new(RefusalCode::EventTimeOutOfRange));
            }
        }
        Ok(())
    }

    fn attempt(&self, attempt: Option<u32>) -> Result<u32, Refusal> {
        let attempt = attempt.ok_or(Refusal::on(RefusalCode::MissingField, "attempt"))?;
        if attempt == 0 || attempt > self.limits.max_attempt {
            return Err(Refusal::on(RefusalCode::InvalidValue, "attempt"));
        }
        Ok(attempt)
    }

    fn name(&self, text: Option<&str>, field: &'static str) -> Result<String, Refusal> {
        self.opt_name(text, field)?
            .ok_or(Refusal::on(RefusalCode::MissingField, field))
    }

    fn opt_name(&self, text: Option<&str>, field: &'static str) -> Result<Option<String>, Refusal> {
        opt_ident(text, field, self.limits.max_name_bytes)
    }

    fn message(
        &self,
        text: Option<&str>,
        marks: &mut PolicyMarks,
    ) -> Result<Option<String>, Refusal> {
        let Some(text) = text else { return Ok(None) };
        if text.len() > self.limits.max_error_message_bytes {
            return Err(Refusal::on(RefusalCode::TooLong, "error_message"));
        }
        storable(text, "error_message")?;
        Ok(Some(redacted(text, marks)))
    }

    fn usage(&self, w: &WireEvent) -> Result<Option<Usage>, Refusal> {
        let Some(u) = w.usage else { return Ok(None) };
        if u.input_tokens > self.limits.max_tokens || u.output_tokens > self.limits.max_tokens {
            return Err(Refusal::on(RefusalCode::InvalidValue, "usage"));
        }
        Ok(Some(Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        }))
    }

    fn cost(&self, w: &WireEvent) -> Result<Option<CostEstimate>, Refusal> {
        let Some(c) = &w.cost_estimate else {
            return Ok(None);
        };
        if c.micros > self.limits.max_cost_micros {
            return Err(Refusal::on(RefusalCode::InvalidValue, "cost_estimate"));
        }
        if c.currency.len() != 3 || !c.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(Refusal::on(RefusalCode::InvalidValue, "cost_estimate"));
        }
        let basis = opt_ident(Some(&c.basis), "cost_estimate.basis", 64)?.unwrap_or_default();
        Ok(Some(CostEstimate {
            micros: c.micros,
            currency: c.currency.clone(),
            basis,
        }))
    }

    fn attributes(
        &self,
        attrs: Option<serde_json::Map<String, serde_json::Value>>,
        marks: &mut PolicyMarks,
    ) -> Result<Vec<(String, AttrValue)>, Refusal> {
        let Some(attrs) = attrs else {
            return Ok(Vec::new());
        };
        if attrs.len() > self.limits.max_attributes {
            return Err(Refusal::new(RefusalCode::TooManyAttributes));
        }
        let mut out = Vec::with_capacity(attrs.len());
        for (key, value) in attrs {
            if key.is_empty()
                || key.len() > self.limits.max_attribute_key_bytes
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_')
            {
                return Err(Refusal::on(RefusalCode::InvalidAttribute, "attributes"));
            }
            if self.policy.treat(&key) == KeyTreatment::Withheld {
                marks.withheld_attributes += 1;
                continue;
            }
            let value = match value {
                serde_json::Value::Bool(b) => AttrValue::Bool(b),
                serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
                    (Some(i), _) => AttrValue::Int(i),
                    (None, Some(f)) if f.is_finite() => AttrValue::Float(f),
                    _ => return Err(Refusal::on(RefusalCode::InvalidAttribute, "attributes")),
                },
                serde_json::Value::String(s) => {
                    if s.len() > self.limits.max_attribute_string_bytes {
                        return Err(Refusal::on(RefusalCode::TooLong, "attributes"));
                    }
                    storable(&s, "attributes")?;
                    AttrValue::Str(redacted(&s, marks))
                }
                _ => return Err(Refusal::on(RefusalCode::InvalidAttribute, "attributes")),
            };
            out.push((key, value));
        }
        Ok(out)
    }
}

/// Free text must be storable: PostgreSQL's `jsonb` cannot hold U+0000, so an event carrying
/// it is refused alone, by name, rather than failing its whole batch at the store (review N1).
fn storable(text: &str, field: &'static str) -> Result<(), Refusal> {
    if text.contains('\0') {
        return Err(Refusal::on(RefusalCode::InvalidValue, field));
    }
    Ok(())
}

fn redacted(text: &str, marks: &mut PolicyMarks) -> String {
    match redact(text) {
        Some(clean) => {
            marks.redactions += 1;
            clean
        }
        None => text.to_owned(),
    }
}

pub(crate) fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/' | b'@')
}

fn step_id(text: Option<&str>, field: &'static str) -> Result<Option<StepId>, Refusal> {
    text.map(StepId::parse)
        .transpose()
        .map_err(|_| Refusal::on(RefusalCode::InvalidId, field))
}

fn kind(text: Option<&str>) -> Result<Option<StepKind>, Refusal> {
    text.map(|k| StepKind::parse(k).ok_or(Refusal::on(RefusalCode::InvalidValue, "kind")))
        .transpose()
}

fn outcome(w: &WireEvent) -> Result<Outcome, Refusal> {
    let text = w
        .outcome
        .as_deref()
        .ok_or(Refusal::on(RefusalCode::MissingField, "outcome"))?;
    match (
        text,
        w.failure_class.as_deref(),
        w.uncertain_reason.as_deref(),
    ) {
        ("succeeded", None, None) => Ok(Outcome::Succeeded),
        ("failed", Some(class), None) => FailureClass::parse(class)
            .map(Outcome::Failed)
            .ok_or(Refusal::on(RefusalCode::InvalidOutcome, "failure_class")),
        ("uncertain", None, Some(reason)) => UncertainReason::parse(reason)
            .map(Outcome::Uncertain)
            .ok_or(Refusal::on(RefusalCode::InvalidOutcome, "uncertain_reason")),
        ("succeeded" | "failed" | "uncertain", _, _) => {
            Err(Refusal::on(RefusalCode::InvalidOutcome, "outcome"))
        }
        _ => Err(Refusal::on(RefusalCode::InvalidValue, "outcome")),
    }
}

/// Every optional wire field `only()` can refuse — the coverage surface
/// `FIELD_ROLES` is enumerated against (REDACTION-POLICY: a field with no
/// declared role is `field_not_allowed` here and a red coverage test).
pub const OPTIONAL_WIRE_FIELDS: &[&str] = &[
    "name",
    "trace_id",
    "request_id",
    "release",
    "deployment",
    "step_id",
    "parent_step_id",
    "kind",
    "attempt",
    "model",
    "prompt_version",
    "config_version",
    "external_effect",
    "outcome",
    "failure_class",
    "uncertain_reason",
    "error_code",
    "error_message",
    "usage",
    "cost_estimate",
    "events_emitted",
    "dropped",
    "reason",
    "first_dropped_at",
    "last_dropped_at",
];

fn field_present(w: &WireEvent, name: &str) -> bool {
    match name {
        "name" => w.name.is_some(),
        "trace_id" => w.trace_id.is_some(),
        "request_id" => w.request_id.is_some(),
        "release" => w.release.is_some(),
        "deployment" => w.deployment.is_some(),
        "step_id" => w.step_id.is_some(),
        "parent_step_id" => w.parent_step_id.is_some(),
        "kind" => w.kind.is_some(),
        "attempt" => w.attempt.is_some(),
        "model" => w.model.is_some(),
        "prompt_version" => w.prompt_version.is_some(),
        "config_version" => w.config_version.is_some(),
        "external_effect" => w.external_effect.is_some(),
        "outcome" => w.outcome.is_some(),
        "failure_class" => w.failure_class.is_some(),
        "uncertain_reason" => w.uncertain_reason.is_some(),
        "error_code" => w.error_code.is_some(),
        "error_message" => w.error_message.is_some(),
        "usage" => w.usage.is_some(),
        "cost_estimate" => w.cost_estimate.is_some(),
        "events_emitted" => w.events_emitted.is_some(),
        "dropped" => w.dropped.is_some(),
        "reason" => w.reason.is_some(),
        "first_dropped_at" => w.first_dropped_at.is_some(),
        "last_dropped_at" => w.last_dropped_at.is_some(),
        // Every name in `OPTIONAL_WIRE_FIELDS` must have an arm — the
        // exhaustive coverage test destructures WireEvent field-by-field
        // (review round 6, B3); a name with no arm is a bug, not data.
        _ => unreachable!("OPTIONAL_WIRE_FIELDS names a field with no presence arm"),
    }
}

/// Refuses any optional field the event type does not carry.
fn only(w: &WireEvent, allowed: &[&str]) -> Result<(), Refusal> {
    match OPTIONAL_WIRE_FIELDS
        .iter()
        .find(|f| field_present(w, f) && !allowed.contains(f))
    {
        Some(field) => Err(Refusal::on(RefusalCode::FieldNotAllowed, field)),
        None => Ok(()),
    }
}

fn host_seq(w: &WireEvent) -> Result<(), Refusal> {
    if w.op_seq == 0 {
        Err(Refusal::on(RefusalCode::InvalidSequence, "op_seq"))
    } else {
        Ok(())
    }
}

fn time(text: &str, field: &'static str) -> Result<Timestamp, Refusal> {
    Timestamp::parse_rfc3339(text).map_err(|_| Refusal::on(RefusalCode::InvalidTime, field))
}

/// Identifiers (names, versions, codes, request IDs): `[A-Za-z0-9._:/@-]`, bounded.
/// No spaces or markup, so they are safe to index and to render. The charset admits
/// `://` and `@`, so a high-confidence credential shape in one refuses the event by
/// name (white-hat S2-1): storing `[redacted]` would corrupt the identifier the
/// caller needs back. Only high-confidence shapes refuse — the free-text
/// heuristics would take `mistralai/Mistral-7B-Instruct-v0.3` for a token and
/// `auth:login` for a key/value pair (review N3).
fn opt_ident(
    text: Option<&str>,
    field: &'static str,
    max: usize,
) -> Result<Option<String>, Refusal> {
    let Some(text) = text else { return Ok(None) };
    if text.is_empty() {
        return Err(Refusal::on(RefusalCode::InvalidValue, field));
    }
    if text.len() > max {
        return Err(Refusal::on(RefusalCode::TooLong, field));
    }
    if !text.bytes().all(is_ident_byte) {
        return Err(Refusal::on(RefusalCode::InvalidValue, field));
    }
    if looks_like_secret(text, field) {
        return Err(Refusal::on(RefusalCode::InvalidValue, field));
    }
    Ok(Some(text.to_owned()))
}

/// A batch's validated source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub instance_id: crate::ids::InstanceId,
    pub service: String,
    pub sdk: String,
}

/// Validates the batch envelope's source; a bad source refuses the whole batch.
pub fn validate_source(
    source: &crate::wire::WireSource,
    limits: &Limits,
) -> Result<Source, Refusal> {
    let instance_id = crate::ids::InstanceId::parse(&source.instance_id)
        .map_err(|_| Refusal::on(RefusalCode::InvalidId, "source.instance_id"))?;
    let service = opt_ident(
        Some(&source.service),
        "source.service",
        limits.max_name_bytes,
    )?
    .unwrap_or_default();
    let sdk =
        opt_ident(Some(&source.sdk), "source.sdk", limits.max_name_bytes)?.unwrap_or_default();
    Ok(Source {
        instance_id,
        service,
        sdk,
    })
}
