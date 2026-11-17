//! Hard caps (D-014). The collector enforces them with named refusals; the SDK truncates
//! strings to them before sending and marks the event, so nothing is shortened silently.

use crate::time::MICROS_PER_SECOND;

/// Every bound the ingest path enforces. `Limits::default()` is the schema-v1 contract;
/// a deployment may lower values, never raise them past what `docs/INGEST_CONTRACT.md` states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Request body (one batch).
    pub max_body_bytes: usize,
    /// Events in one batch.
    pub max_batch_events: usize,
    /// One event's serialized JSON.
    pub max_event_bytes: usize,
    pub max_attributes: usize,
    pub max_attribute_key_bytes: usize,
    pub max_attribute_string_bytes: usize,
    /// Operation, step, model, release, deployment, prompt/config version names.
    pub max_name_bytes: usize,
    pub max_request_id_bytes: usize,
    pub max_error_code_bytes: usize,
    pub max_error_message_bytes: usize,
    pub max_attempt: u32,
    /// How far ahead of receive time an event time may be before it is refused.
    pub max_future_micros: i64,
    /// How far behind receive time an event time may be (the raw retention window, D-016).
    pub max_past_micros: i64,
    /// Event time ahead of receive time by more than this is flagged as clock skew.
    pub skew_tolerance_micros: i64,
    /// Receive time behind event time by more than this is flagged as late.
    pub late_after_micros: i64,
    /// Distinct names (operation/step/model/release/…) one project may introduce.
    pub max_names_per_project: usize,
    /// Of those, how many one ingest key may introduce (review finding 11).
    pub max_names_per_key: usize,
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    pub max_dropped_per_report: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            max_batch_events: 500,
            max_event_bytes: 8 * 1024,
            max_attributes: 32,
            max_attribute_key_bytes: 64,
            max_attribute_string_bytes: 256,
            max_name_bytes: 128,
            max_request_id_bytes: 128,
            max_error_code_bytes: 64,
            max_error_message_bytes: 512,
            max_attempt: 1_000,
            max_future_micros: 3_600 * MICROS_PER_SECOND,
            max_past_micros: 14 * 24 * 3_600 * MICROS_PER_SECOND,
            skew_tolerance_micros: 2 * MICROS_PER_SECOND,
            late_after_micros: 10 * MICROS_PER_SECOND,
            max_names_per_project: 2_000,
            max_names_per_key: 500,
            max_tokens: 1_000_000_000,
            max_cost_micros: 1_000_000_000_000,
            max_dropped_per_report: 1_000_000_000,
        }
    }
}
