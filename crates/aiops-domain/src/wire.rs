//! Schema v1 on the wire (`docs/INGEST_CONTRACT.md`). These types are deliberately
//! loose — strings and options — so that one wrong field becomes a *named* refusal from
//! [`crate::validate`] instead of an opaque parse error, and strict where it matters:
//! unknown fields are refused (`deny_unknown_fields`), because a field this version does
//! not define is schema drift, which needs a version bump.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// The only schema version this build accepts.
pub const SCHEMA_VERSION: u16 = 1;

/// A batch as the collector parses it: the envelope is typed, the events stay raw so each
/// is validated (and refused) on its own.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBatch<'a> {
    pub schema_version: u16,
    pub source: WireSource,
    #[serde(default)]
    pub stats: WireStats,
    #[serde(borrow)]
    pub events: Vec<&'a RawValue>,
}

/// A batch as the SDK writes it.
#[derive(Debug, Serialize)]
pub struct OutgoingBatch<'a> {
    pub schema_version: u16,
    pub source: &'a WireSource,
    pub stats: WireStats,
    pub events: &'a [WireEvent],
}

/// Who sent the batch. The project is not here: it comes from the ingest key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WireSource {
    /// Random per SDK process (32 hex).
    pub instance_id: String,
    /// The service name, e.g. `reference-host`.
    pub service: String,
    /// The SDK and its version, e.g. `aiops-sdk-rust/0.1.0`.
    pub sdk: String,
}

/// The SDK's cumulative counters since its process started (D-021). They travel with every
/// batch, so the collector keeps the latest even when the batches that carried earlier
/// values were lost.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WireStats {
    #[serde(default)]
    pub dropped_queue_full: u64,
    #[serde(default)]
    pub dropped_send_failed: u64,
    #[serde(default)]
    pub dropped_rejected: u64,
    /// Drops the SDK could not attribute to an operation (its gap map was full).
    #[serde(default)]
    pub gap_overflow: u64,
    #[serde(default)]
    pub truncated_events: u64,
}

/// One event, flat. Which fields are required or allowed depends on `type`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WireEvent {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub operation_id: String,
    /// Per source instance, from 1, in emission order.
    pub seq: u64,
    /// Per operation within one source instance, from 1; `0` only on `gap.reported`.
    pub op_seq: u32,
    pub event_time: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_effect: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertain_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<WireUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_estimate: Option<WireCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_emitted: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_dropped_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dropped_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<serde_json::Map<String, serde_json::Value>>,
    /// How many string fields the SDK truncated to the caps before sending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WireUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// An estimate, never a bill (invariant 7): the field name says so on the wire too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WireCost {
    /// Millionths of one currency unit.
    pub micros: u64,
    /// ISO 4217, e.g. `USD`.
    pub currency: String,
    /// What the estimate is based on, e.g. `list-price-2026-11`.
    pub basis: String,
}

/// The collector's answer to an accepted batch (HTTP 200).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestResponse {
    /// Stored for the first time.
    pub accepted: u32,
    /// Already stored with identical content (re-delivery; merged idempotently).
    pub duplicates: u32,
    /// Refused one by one; the rest of the batch was still processed.
    pub refused: Vec<RefusedEvent>,
    /// Attributes withheld by the default-deny policy (D-006), summed over the batch.
    pub withheld_attributes: u32,
    /// Secret-shaped strings replaced with `[redacted]`, summed over the batch.
    pub redactions: u32,
    /// Names over the project's or the key's budget, stored as `_overflow`.
    #[serde(default)]
    pub overflowed_names: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefusedEvent {
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}
