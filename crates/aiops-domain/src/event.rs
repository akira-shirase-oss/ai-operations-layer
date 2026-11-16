//! The validated event model. Constructed only by [`crate::validate`] (from the wire) or
//! by an SDK that builds it directly; converted back with [`Event::to_wire`].

use crate::ids::{EventId, OperationId, StepId, TraceId};
use crate::time::Timestamp;
use crate::wire::{WireCost, WireEvent, WireUsage};

/// What kind of work a step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepKind {
    Model,
    Tool,
    Job,
    Validation,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Tool => "tool",
            Self::Job => "job",
            Self::Validation => "validation",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "model" => Self::Model,
            "tool" => Self::Tool,
            "job" => Self::Job,
            "validation" => Self::Validation,
            _ => return None,
        })
    }
}

/// A definitive failure, declared by the host (D-020).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureClass {
    /// The request provably never reached the remote (refused connection, DNS, TLS before send).
    Transport,
    /// The remote answered with a definitive error; no effect happened.
    Rejected,
    /// The step's output failed a check.
    Validation,
    /// A bug or local fault in the host.
    Internal,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Rejected => "rejected",
            Self::Validation => "validation",
            Self::Internal => "internal",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "transport" => Self::Transport,
            "rejected" => Self::Rejected,
            "validation" => Self::Validation,
            "internal" => Self::Internal,
            _ => return None,
        })
    }
}

/// Why an outcome is unknown: the request left the host, so the effect may have happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UncertainReason {
    TimeoutAfterSend,
    ConnectionLostAfterSend,
    ResponseUnreadable,
}

impl UncertainReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimeoutAfterSend => "timeout_after_send",
            Self::ConnectionLostAfterSend => "connection_lost_after_send",
            Self::ResponseUnreadable => "response_unreadable",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "timeout_after_send" => Self::TimeoutAfterSend,
            "connection_lost_after_send" => Self::ConnectionLostAfterSend,
            "response_unreadable" => Self::ResponseUnreadable,
            _ => return None,
        })
    }
}

/// The declared outcome of a step attempt or an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    Succeeded,
    Failed(FailureClass),
    Uncertain(UncertainReason),
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Uncertain(_) => "uncertain",
        }
    }
}

/// Why the SDK lost events (reported as data, D-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GapReason {
    /// The bounded queue was full when the host emitted.
    QueueFull,
    /// A batch failed after its bounded retries (collector down, timeouts).
    SendFailed,
    /// The collector refused the whole batch (bad key, bad envelope).
    Rejected,
    /// The SDK shut down with events still queued past its deadline.
    Shutdown,
}

impl GapReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::SendFailed => "send_failed",
            Self::Rejected => "rejected",
            Self::Shutdown => "shutdown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "queue_full" => Self::QueueFull,
            "send_failed" => Self::SendFailed,
            "rejected" => Self::Rejected,
            "shutdown" => Self::Shutdown,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A labeled estimate (invariant 7). Never summed with reconciled billing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostEstimate {
    pub micros: u64,
    pub currency: String,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// The operation's metadata, from `operation.started`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OperationMeta {
    pub name: String,
    pub trace_id: Option<TraceId>,
    pub request_id: Option<String>,
    pub release: Option<String>,
    pub deployment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepStart {
    pub step_id: StepId,
    pub parent_step_id: Option<StepId>,
    pub kind: StepKind,
    pub name: String,
    pub attempt: u32,
    pub model: Option<String>,
    pub prompt_version: Option<String>,
    pub config_version: Option<String>,
    /// The step can cause an effect outside the host (a publish, a payment).
    pub external_effect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFinish {
    pub step_id: StepId,
    pub attempt: u32,
    /// Repeated from the start so a finish whose start was lost is still legible.
    pub kind: Option<StepKind>,
    pub name: Option<String>,
    pub outcome: Outcome,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub usage: Option<Usage>,
    pub cost_estimate: Option<CostEstimate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationFinish {
    pub outcome: Outcome,
    /// Host-emitted events for this operation from this instance, this one included.
    pub events_emitted: u32,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapReport {
    pub dropped: u64,
    pub reason: GapReason,
    pub first_dropped_at: Timestamp,
    pub last_dropped_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBody {
    OperationStarted(OperationMeta),
    OperationFinished(OperationFinish),
    StepStarted(StepStart),
    StepFinished(StepFinish),
    GapReported(GapReport),
}

impl EventBody {
    pub fn type_str(&self) -> &'static str {
        match self {
            Self::OperationStarted(_) => "operation.started",
            Self::OperationFinished(_) => "operation.finished",
            Self::StepStarted(_) => "step.started",
            Self::StepFinished(_) => "step.finished",
            Self::GapReported(_) => "gap.reported",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub event_id: EventId,
    pub operation_id: OperationId,
    pub seq: u64,
    pub op_seq: u32,
    pub event_time: Timestamp,
    pub body: EventBody,
    pub attributes: Vec<(String, AttrValue)>,
    pub truncated: u32,
}

impl Event {
    pub fn step_id(&self) -> Option<StepId> {
        match &self.body {
            EventBody::StepStarted(s) => Some(s.step_id),
            EventBody::StepFinished(f) => Some(f.step_id),
            _ => None,
        }
    }

    /// The wire form (also the stored form).
    pub fn to_wire(&self) -> WireEvent {
        let mut w = WireEvent {
            event_id: self.event_id.to_string(),
            event_type: self.body.type_str().to_owned(),
            operation_id: self.operation_id.to_string(),
            seq: self.seq,
            op_seq: self.op_seq,
            event_time: self.event_time.to_rfc3339(),
            truncated: (self.truncated > 0).then_some(self.truncated),
            ..WireEvent::default()
        };
        if !self.attributes.is_empty() {
            let mut map = serde_json::Map::new();
            for (k, v) in &self.attributes {
                let value = match v {
                    AttrValue::Bool(b) => serde_json::Value::Bool(*b),
                    AttrValue::Int(i) => serde_json::Value::from(*i),
                    AttrValue::Float(f) => serde_json::Number::from_f64(*f)
                        .map_or(serde_json::Value::Null, serde_json::Value::Number),
                    AttrValue::Str(s) => serde_json::Value::String(s.clone()),
                };
                map.insert(k.clone(), value);
            }
            w.attributes = Some(map);
        }
        match &self.body {
            EventBody::OperationStarted(m) => {
                w.name = Some(m.name.clone());
                w.trace_id = m.trace_id.map(|t| t.to_string());
                w.request_id.clone_from(&m.request_id);
                w.release.clone_from(&m.release);
                w.deployment.clone_from(&m.deployment);
            }
            EventBody::OperationFinished(f) => {
                set_outcome(&mut w, f.outcome);
                w.events_emitted = Some(f.events_emitted);
                w.error_code.clone_from(&f.error_code);
                w.error_message.clone_from(&f.error_message);
            }
            EventBody::StepStarted(s) => {
                w.step_id = Some(s.step_id.to_string());
                w.parent_step_id = s.parent_step_id.map(|p| p.to_string());
                w.kind = Some(s.kind.as_str().to_owned());
                w.name = Some(s.name.clone());
                w.attempt = Some(s.attempt);
                w.model.clone_from(&s.model);
                w.prompt_version.clone_from(&s.prompt_version);
                w.config_version.clone_from(&s.config_version);
                w.external_effect = Some(s.external_effect);
            }
            EventBody::StepFinished(f) => {
                w.step_id = Some(f.step_id.to_string());
                w.attempt = Some(f.attempt);
                w.kind = f.kind.map(|k| k.as_str().to_owned());
                w.name.clone_from(&f.name);
                set_outcome(&mut w, f.outcome);
                w.error_code.clone_from(&f.error_code);
                w.error_message.clone_from(&f.error_message);
                w.usage = f.usage.map(|u| WireUsage {
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                });
                w.cost_estimate = f.cost_estimate.as_ref().map(|c| WireCost {
                    micros: c.micros,
                    currency: c.currency.clone(),
                    basis: c.basis.clone(),
                });
            }
            EventBody::GapReported(g) => {
                w.dropped = Some(g.dropped);
                w.reason = Some(g.reason.as_str().to_owned());
                w.first_dropped_at = Some(g.first_dropped_at.to_rfc3339());
                w.last_dropped_at = Some(g.last_dropped_at.to_rfc3339());
            }
        }
        w
    }
}

fn set_outcome(w: &mut WireEvent, outcome: Outcome) {
    w.outcome = Some(outcome.as_str().to_owned());
    match outcome {
        Outcome::Succeeded => {}
        Outcome::Failed(class) => w.failure_class = Some(class.as_str().to_owned()),
        Outcome::Uncertain(reason) => w.uncertain_reason = Some(reason.as_str().to_owned()),
    }
}
