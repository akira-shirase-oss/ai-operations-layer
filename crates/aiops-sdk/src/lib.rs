//! The AI Operations Layer's Rust SDK (D-013, D-021).
//!
//! ```no_run
//! # async fn demo() -> Result<(), aiops_sdk::SdkError> {
//! use aiops_sdk::{Client, Config, Finish, StepKind};
//! let client = Client::start(&Config::new("http://127.0.0.1:57400", "aiops_ik_…", "my-service"))?;
//! let op = client.operation("publish_article").request_id("req-1").start();
//! let step = op.step(StepKind::Model, "generate").model("my-model").start();
//! step.finish(Finish::succeeded().usage(120, 480));
//! op.succeed();
//! client.shutdown(std::time::Duration::from_secs(2)).await;
//! # Ok(()) }
//! ```
//!
//! **The host always wins.** Emitting is an O(1) non-blocking enqueue into a bounded
//! queue; when it is full the event is dropped and counted (per operation, so the
//! collector can show the gap). A background task batches, sends with timeouts, retries
//! only retryable answers inside a bounded budget, then drops and counts. Nothing here
//! panics, and nothing waits on the collector except [`Client::shutdown`], which has a
//! deadline. Server-side only: the ingest key is a secret.

mod flush;
mod ids;
mod text;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use aiops_domain::context::TraceParent;
use aiops_domain::event::{
    AttrValue, CostEstimate, Event, EventBody, GapReason, OperationFinish, OperationMeta,
    StepFinish, StepStart, Usage,
};
pub use aiops_domain::{FailureClass, OperationId, Outcome, StepId, StepKind, UncertainReason};
use aiops_domain::{InstanceId, Limits, Timestamp};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ids::IdGen;

/// The SDK's name and version on the wire.
pub const SDK_NAME: &str = concat!("aiops-sdk-rust/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    #[error("Client::start must be called inside a Tokio runtime")]
    NoRuntime,
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    #[error("could not build the HTTP client")]
    Http(#[source] reqwest::Error),
    #[error("the operating system's randomness is unavailable")]
    Random,
}

/// SDK configuration. Every bound is explicit (D-021).
#[derive(Clone)]
pub struct Config {
    /// The collector's base URL, e.g. `http://127.0.0.1:57400`.
    pub endpoint: String,
    /// `aiops_ik_…`. Never logged (see the `Debug` impl).
    pub ingest_key: String,
    pub service: String,
    pub release: Option<String>,
    pub deployment: Option<String>,
    /// Events held while the collector is slow or down. Full → drop newest, counted.
    pub queue_capacity: usize,
    pub batch_max_events: usize,
    /// A batch's body at most (the collector refuses more than 1 MiB). Events are also cut
    /// to the collector's 8 KiB per-event cap before sending.
    pub batch_max_bytes: usize,
    pub flush_interval: Duration,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_send_attempts: u32,
    /// Total time one batch may spend sending, every attempt's timeout included, before it
    /// is dropped and counted.
    pub retry_budget: Duration,
    /// Failed batches in a row that open the circuit breaker: the next batch is then held
    /// and sent as a probe after the cool-down.
    pub breaker_threshold: u32,
    /// The first cool-down; it doubles with each failed probe, up to 5 s, so recovery
    /// after an outage takes at most that long.
    pub breaker_cooldown: Duration,
    /// Operations whose drops are tracked one by one; beyond it, drops are counted only
    /// in aggregate (`gap_overflow`).
    pub gap_capacity: usize,
    /// Send `error_message` text (redacted, ≤ 512 bytes). Off by default: error strings
    /// are the usual privacy leak.
    pub include_error_messages: bool,
    /// Attribute keys this host sends beyond the default allowlist (the project must opt
    /// in too, or the collector withholds them). Everything else is dropped here.
    pub opt_in_attributes: Vec<String>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("endpoint", &self.endpoint)
            .field("ingest_key", &"[redacted]")
            .field("service", &self.service)
            .field("queue_capacity", &self.queue_capacity)
            .field("batch_max_events", &self.batch_max_events)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn new(
        endpoint: impl Into<String>,
        ingest_key: impl Into<String>,
        service: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            ingest_key: ingest_key.into(),
            service: service.into(),
            release: None,
            deployment: None,
            queue_capacity: 8_192,
            batch_max_events: 500,
            batch_max_bytes: 900 * 1024,
            flush_interval: Duration::from_millis(200),
            connect_timeout: Duration::from_millis(500),
            request_timeout: Duration::from_secs(2),
            max_send_attempts: 3,
            retry_budget: Duration::from_secs(5),
            breaker_threshold: 3,
            breaker_cooldown: Duration::from_millis(500),
            gap_capacity: 4_096,
            include_error_messages: false,
            opt_in_attributes: Vec::new(),
        }
    }
}

/// Everything the SDK counted, for the host to expose (D-021: every counter exposable).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub emitted: u64,
    pub accepted: u64,
    pub duplicates: u64,
    /// Refused one by one by the collector (named refusals).
    pub refused: u64,
    pub dropped_queue_full: u64,
    pub dropped_send_failed: u64,
    pub dropped_rejected: u64,
    pub dropped_shutdown: u64,
    pub gap_overflow: u64,
    pub gap_reports_sent: u64,
    pub batches_sent: u64,
    pub batches_failed: u64,
    /// Times the circuit breaker opened (sends paused for a cool-down, then probed).
    pub breaker_opened: u64,
    pub truncated_events: u64,
    pub attributes_dropped_locally: u64,
    /// Age of the oldest event in the most recent batch at send time (queue age).
    pub last_queue_age_us: u64,
    pub max_queue_age_us: u64,
}

impl Stats {
    pub fn dropped(&self) -> u64 {
        self.dropped_queue_full
            + self.dropped_send_failed
            + self.dropped_rejected
            + self.dropped_shutdown
    }
}

#[derive(Default)]
struct Counters {
    emitted: AtomicU64,
    accepted: AtomicU64,
    duplicates: AtomicU64,
    refused: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_send_failed: AtomicU64,
    dropped_rejected: AtomicU64,
    dropped_shutdown: AtomicU64,
    gap_overflow: AtomicU64,
    gap_reports_sent: AtomicU64,
    batches_sent: AtomicU64,
    batches_failed: AtomicU64,
    breaker_opened: AtomicU64,
    truncated_events: AtomicU64,
    attributes_dropped_locally: AtomicU64,
    last_queue_age_us: AtomicU64,
    max_queue_age_us: AtomicU64,
}

fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

impl Counters {
    fn snapshot(&self) -> Stats {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Stats {
            emitted: g(&self.emitted),
            accepted: g(&self.accepted),
            duplicates: g(&self.duplicates),
            refused: g(&self.refused),
            dropped_queue_full: g(&self.dropped_queue_full),
            dropped_send_failed: g(&self.dropped_send_failed),
            dropped_rejected: g(&self.dropped_rejected),
            dropped_shutdown: g(&self.dropped_shutdown),
            gap_overflow: g(&self.gap_overflow),
            gap_reports_sent: g(&self.gap_reports_sent),
            batches_sent: g(&self.batches_sent),
            batches_failed: g(&self.batches_failed),
            breaker_opened: g(&self.breaker_opened),
            truncated_events: g(&self.truncated_events),
            attributes_dropped_locally: g(&self.attributes_dropped_locally),
            last_queue_age_us: g(&self.last_queue_age_us),
            max_queue_age_us: g(&self.max_queue_age_us),
        }
    }
}

/// Drops not yet reported, per operation and reason (bounded by `gap_capacity`).
#[derive(Default)]
pub(crate) struct GapMap {
    entries: HashMap<(OperationId, GapReasonKey), GapEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct GapReasonKey(u8);

impl GapReasonKey {
    fn of(r: GapReason) -> Self {
        Self(match r {
            GapReason::QueueFull => 0,
            GapReason::SendFailed => 1,
            GapReason::Rejected => 2,
            GapReason::Shutdown => 3,
        })
    }

    fn reason(self) -> GapReason {
        match self.0 {
            0 => GapReason::QueueFull,
            1 => GapReason::SendFailed,
            2 => GapReason::Rejected,
            _ => GapReason::Shutdown,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GapEntry {
    dropped: u64,
    first: Timestamp,
    last: Timestamp,
}

pub(crate) struct Inner {
    tx: mpsc::Sender<Event>,
    instance_id: InstanceId,
    seq: AtomicU64,
    ids: IdGen,
    gaps: Mutex<GapMap>,
    counters: Counters,
    gap_capacity: usize,
    release: Option<String>,
    deployment: Option<String>,
    include_error_messages: bool,
    opt_in_attributes: Vec<String>,
    limits: Limits,
    cancel: CancellationToken,
    flusher: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Events in the batch being sent right now (counted as lost if shutdown aborts it).
    in_flight: AtomicU64,
}

pub(crate) fn now() -> Timestamp {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
    Timestamp::from_unix_micros(micros)
}

impl Inner {
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn event_id(&self, seq: u64) -> aiops_domain::EventId {
        ids::event_id(self.instance_id, seq)
    }

    fn record_gap(&self, operation: OperationId, reason: GapReason, at: Timestamp, n: u64) {
        let Ok(mut gaps) = self.gaps.lock() else {
            add(&self.counters.gap_overflow, n);
            return;
        };
        let key = (operation, GapReasonKey::of(reason));
        let full = gaps.entries.len() >= self.gap_capacity;
        match gaps.entries.get_mut(&key) {
            Some(e) => {
                e.dropped += n;
                e.first = e.first.min(at);
                e.last = e.last.max(at);
            }
            None if full => add(&self.counters.gap_overflow, n),
            None => {
                gaps.entries.insert(
                    key,
                    GapEntry {
                        dropped: n,
                        first: at,
                        last: at,
                    },
                );
            }
        }
    }

    /// The non-blocking emit path: build, number, try to enqueue, count.
    fn emit(
        &self,
        op: &OpCore,
        body: EventBody,
        attributes: Vec<(String, AttrValue)>,
        truncated: u32,
    ) {
        self.emit_with(op, |_| body, attributes, truncated);
    }

    /// `body` receives the event's own `op_seq` (the finish uses it as `events_emitted`).
    fn emit_with(
        &self,
        op: &OpCore,
        body: impl FnOnce(u32) -> EventBody,
        attributes: Vec<(String, AttrValue)>,
        truncated: u32,
    ) {
        let seq = self.next_seq();
        let op_seq = op.op_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let body = body(op_seq);
        let event = Event {
            event_id: self.event_id(seq),
            operation_id: op.id,
            seq,
            op_seq,
            event_time: now(),
            body,
            attributes,
            truncated,
        };
        add(&self.counters.emitted, 1);
        if truncated > 0 {
            add(&self.counters.truncated_events, 1);
        }
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(e)) => {
                add(&self.counters.dropped_queue_full, 1);
                self.record_gap(e.operation_id, GapReason::QueueFull, e.event_time, 1);
            }
            Err(mpsc::error::TrySendError::Closed(e)) => {
                add(&self.counters.dropped_shutdown, 1);
                self.record_gap(e.operation_id, GapReason::Shutdown, e.event_time, 1);
            }
        }
    }

    fn keep_attribute(&self, key: &str) -> bool {
        aiops_domain::policy::is_default_attribute(key)
            || self.opt_in_attributes.iter().any(|k| k == key)
    }
}

/// A handle to the SDK. Cheap to clone. [`Client::disabled`] is a no-op client with the
/// same API (for running a host with instrumentation off).
#[derive(Clone)]
pub struct Client {
    inner: Option<Arc<Inner>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("enabled", &self.inner.is_some())
            .finish()
    }
}

impl Client {
    /// Starts the SDK: validates the configuration and spawns its flush task on the
    /// current Tokio runtime. Never contacts the collector.
    pub fn start(config: &Config) -> Result<Self, SdkError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| SdkError::NoRuntime)?;
        if config.queue_capacity == 0
            || config.batch_max_events == 0
            || config.batch_max_events > 500
        {
            return Err(SdkError::Config(
                "queue_capacity ≥ 1 and 1 ≤ batch_max_events ≤ 500",
            ));
        }
        if !(16 * 1024..=1024 * 1024).contains(&config.batch_max_bytes) {
            return Err(SdkError::Config("16 KiB ≤ batch_max_bytes ≤ 1 MiB"));
        }
        if !config.ingest_key.starts_with("aiops_ik_") {
            return Err(SdkError::Config("ingest_key must be an aiops_ik_ key"));
        }
        let url = reqwest::Url::parse(&config.endpoint)
            .and_then(|u| u.join("/v1/ingest"))
            .map_err(|_| SdkError::Config("endpoint must be an http(s) URL"))?;
        let (service, _) = text::ident(&config.service, Limits::default().max_name_bytes);
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .pool_max_idle_per_host(2)
            .build()
            .map_err(SdkError::Http)?;
        let ids = IdGen::new().map_err(|()| SdkError::Random)?;
        let instance_id = ids.instance_id();
        let (tx, rx) = mpsc::channel(config.queue_capacity);
        let inner = Arc::new(Inner {
            tx,
            instance_id,
            seq: AtomicU64::new(0),
            ids,
            gaps: Mutex::new(GapMap::default()),
            counters: Counters::default(),
            gap_capacity: config.gap_capacity,
            release: config.release.as_deref().map(|r| text::ident(r, 128).0),
            deployment: config.deployment.as_deref().map(|d| text::ident(d, 128).0),
            include_error_messages: config.include_error_messages,
            opt_in_attributes: config.opt_in_attributes.clone(),
            limits: Limits::default(),
            cancel: CancellationToken::new(),
            flusher: Mutex::new(None),
            in_flight: AtomicU64::new(0),
        });
        let flusher = flush::Flusher::new(inner.clone(), rx, http, url, config, service);
        let task = handle.spawn(flusher.run());
        if let Ok(mut slot) = inner.flusher.lock() {
            *slot = Some(task);
        }
        Ok(Self { inner: Some(inner) })
    }

    /// A client that records nothing and sends nothing.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn operation(&self, name: &str) -> OperationBuilder {
        OperationBuilder {
            client: self.clone(),
            name: name.to_owned(),
            request_id: None,
            parent: None,
            attributes: Vec::new(),
        }
    }

    /// Join an operation that another SDK-instrumented service owns, from the W3C
    /// `traceparent` it sent. The handle records steps under that operation (as
    /// children of the caller's span) but never emits `operation.started` or
    /// `operation.finished`: only the owning service declares the outcome, so two
    /// services can never contradict each other's finish. `None` for an invalid header.
    pub fn continue_operation(&self, traceparent: &str) -> Option<Operation> {
        let tp = TraceParent::parse(traceparent)?;
        let Some(inner) = self.inner.clone() else {
            return Some(Operation { core: None });
        };
        Some(Operation {
            core: Some(Arc::new(OpCore {
                inner,
                id: tp.trace_id,
                root_span: tp.parent_id,
                op_seq: AtomicU32::new(0),
                // Already "finished" from this service's point of view: finish() is a no-op.
                finished: AtomicBool::new(true),
                continued: true,
            })),
        })
    }

    pub fn stats(&self) -> Stats {
        self.inner
            .as_ref()
            .map(|i| i.counters.snapshot())
            .unwrap_or_default()
    }

    /// Flushes what is queued, for at most `deadline`; anything left is dropped and
    /// counted (`dropped_shutdown`). Returns the final counters.
    pub async fn shutdown(&self, deadline: Duration) -> Stats {
        let Some(inner) = &self.inner else {
            return Stats::default();
        };
        inner.cancel.cancel();
        let task = inner.flusher.lock().ok().and_then(|mut t| t.take());
        if let Some(mut task) = task
            && tokio::time::timeout(deadline, &mut task).await.is_err()
        {
            task.abort();
            // Queued plus in flight: lost to the deadline (an in-flight batch may in
            // fact have been stored; it is counted as lost, never as delivered).
            let queued = u64::try_from(inner.tx.max_capacity() - inner.tx.capacity()).unwrap_or(0);
            let in_flight = inner.in_flight.load(Ordering::Relaxed);
            add(&inner.counters.dropped_shutdown, queued + in_flight);
        }
        inner.counters.snapshot()
    }
}

pub struct OperationBuilder {
    client: Client,
    name: String,
    request_id: Option<String>,
    parent: Option<TraceParent>,
    attributes: Vec<(String, AttrValue)>,
}

impl OperationBuilder {
    #[must_use]
    pub fn request_id(mut self, id: &str) -> Self {
        self.request_id = Some(id.to_owned());
        self
    }

    /// Record the OpenTelemetry trace this operation belongs to (from an incoming W3C
    /// `traceparent` written by a service that does not use this SDK). The operation is
    /// still this service's own: it gets its own ID, start and finish. To join an
    /// operation another SDK-instrumented service owns, use [`Client::continue_operation`].
    #[must_use]
    pub fn external_trace(mut self, header: &str) -> Self {
        self.parent = TraceParent::parse(header);
        self
    }

    #[must_use]
    pub fn attribute(mut self, key: &str, value: impl Into<AttrValueArg>) -> Self {
        self.attributes.push((key.to_owned(), value.into().0));
        self
    }

    /// Emits `operation.started` and returns the handle.
    pub fn start(self) -> Operation {
        let Some(inner) = self.client.inner.clone() else {
            return Operation { core: None };
        };
        let trace_id = self
            .parent
            .and_then(|tp| aiops_domain::TraceId::from_bytes(*tp.trace_id.as_bytes()));
        let core = Arc::new(OpCore {
            id: inner.ids.operation_id(),
            root_span: inner.ids.step_id(),
            inner,
            op_seq: AtomicU32::new(0),
            finished: AtomicBool::new(false),
            continued: false,
        });
        let limits = &core.inner.limits;
        let mut truncated = 0;
        let (name, t1) = text::ident(&self.name, limits.max_name_bytes);
        let request_id = self.request_id.as_deref().map(|r| {
            let (r, t) = text::ident(r, limits.max_request_id_bytes);
            truncated += u32::from(t);
            r
        });
        truncated += u32::from(t1);
        let attributes = core.attributes(self.attributes, &mut truncated);
        core.inner.emit(
            &core,
            EventBody::OperationStarted(OperationMeta {
                name,
                trace_id,
                request_id,
                release: core.inner.release.clone(),
                deployment: core.inner.deployment.clone(),
            }),
            attributes,
            truncated,
        );
        Operation { core: Some(core) }
    }
}

/// An attribute value (`bool`, integers, `f64`, strings).
pub struct AttrValueArg(AttrValue);

impl From<bool> for AttrValueArg {
    fn from(v: bool) -> Self {
        Self(AttrValue::Bool(v))
    }
}
impl From<i64> for AttrValueArg {
    fn from(v: i64) -> Self {
        Self(AttrValue::Int(v))
    }
}
impl From<u32> for AttrValueArg {
    fn from(v: u32) -> Self {
        Self(AttrValue::Int(i64::from(v)))
    }
}
impl From<f64> for AttrValueArg {
    fn from(v: f64) -> Self {
        Self(AttrValue::Float(v))
    }
}
impl From<&str> for AttrValueArg {
    fn from(v: &str) -> Self {
        Self(AttrValue::Str(storable(v)))
    }
}
impl From<String> for AttrValueArg {
    fn from(v: String) -> Self {
        Self(AttrValue::Str(if v.contains('\0') {
            storable(&v)
        } else {
            v
        }))
    }
}

/// The collector refuses free text holding U+0000 (its store cannot keep it), so the SDK
/// replaces it with U+FFFD before the event is queued (review N1).
fn storable(text: &str) -> String {
    text.replace('\0', "\u{fffd}")
}

struct OpCore {
    inner: Arc<Inner>,
    id: OperationId,
    root_span: StepId,
    op_seq: AtomicU32,
    finished: AtomicBool,
    /// Joined from another service: contributes steps, never the start or the finish
    /// (only the owning service declares the operation's outcome).
    continued: bool,
}

impl OpCore {
    fn attributes(
        &self,
        attrs: Vec<(String, AttrValue)>,
        truncated: &mut u32,
    ) -> Vec<(String, AttrValue)> {
        let max = self.inner.limits.max_attribute_string_bytes;
        let mut out = Vec::with_capacity(attrs.len());
        for (k, v) in attrs.into_iter().take(self.inner.limits.max_attributes) {
            if !self.inner.keep_attribute(&k) {
                add(&self.inner.counters.attributes_dropped_locally, 1);
                continue;
            }
            let v = match v {
                AttrValue::Str(s) => {
                    let (s, t) = text::clip(&s, max);
                    *truncated += u32::from(t);
                    AttrValue::Str(aiops_domain::policy::redact(&s).unwrap_or(s))
                }
                AttrValue::Float(f) if !f.is_finite() => continue,
                other => other,
            };
            out.push((k, v));
        }
        out
    }
}

/// One operation. Cheap to clone; finishing is idempotent (only the first call emits).
#[derive(Clone)]
pub struct Operation {
    core: Option<Arc<OpCore>>,
}

impl Operation {
    /// The operation ID (`None` on a disabled client).
    pub fn id(&self) -> Option<OperationId> {
        self.core.as_ref().map(|c| c.id)
    }

    /// A W3C `traceparent` for outgoing calls, so a downstream service continues this
    /// operation.
    pub fn traceparent(&self) -> Option<String> {
        self.core.as_ref().map(|c| {
            TraceParent {
                trace_id: c.id,
                parent_id: c.root_span,
                sampled: true,
            }
            .header()
        })
    }

    pub fn step(&self, kind: StepKind, name: &str) -> StepBuilder {
        // In a continued operation, steps hang under the caller's span.
        let parent = self
            .core
            .as_ref()
            .filter(|c| c.continued)
            .map(|c| c.root_span);
        StepBuilder {
            op: self.clone(),
            kind,
            name: name.to_owned(),
            step_id: None,
            attempt: 1,
            parent,
            model: None,
            prompt_version: None,
            config_version: None,
            external_effect: false,
            attributes: Vec::new(),
        }
    }

    pub fn succeed(&self) {
        self.finish(Finish::succeeded());
    }

    /// Emits `operation.finished` with the declared outcome and the number of events
    /// this SDK emitted for the operation (so the collector can count what is missing).
    /// A no-op on a continued operation ([`Client::continue_operation`]).
    pub fn finish(&self, finish: Finish) {
        let Some(core) = &self.core else { return };
        if core.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut truncated = 0;
        let parts = finish.into_parts(&core.inner, &mut truncated);
        // This event's own op_seq is the count of events emitted for the operation.
        core.inner.emit_with(
            core,
            |op_seq| {
                EventBody::OperationFinished(OperationFinish {
                    outcome: parts.outcome,
                    events_emitted: op_seq,
                    error_code: parts.error_code,
                    error_message: parts.error_message,
                })
            },
            Vec::new(),
            truncated,
        );
    }
}

#[must_use = "a step builder does nothing until .start()"]
pub struct StepBuilder {
    op: Operation,
    kind: StepKind,
    name: String,
    step_id: Option<StepId>,
    attempt: u32,
    parent: Option<StepId>,
    model: Option<String>,
    prompt_version: Option<String>,
    config_version: Option<String>,
    external_effect: bool,
    attributes: Vec<(String, AttrValue)>,
}

impl StepBuilder {
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_owned());
        self
    }

    pub fn prompt_version(mut self, v: &str) -> Self {
        self.prompt_version = Some(v.to_owned());
        self
    }

    pub fn config_version(mut self, v: &str) -> Self {
        self.config_version = Some(v.to_owned());
        self
    }

    /// The step can cause an effect outside the host (a publish, a payment). An unknown
    /// outcome of such a step is shown as "the effect may have happened".
    pub fn external_effect(mut self, yes: bool) -> Self {
        self.external_effect = yes;
        self
    }

    pub fn parent(mut self, parent: &Step) -> Self {
        self.parent = parent.id;
        self
    }

    pub fn attribute(mut self, key: &str, value: impl Into<AttrValueArg>) -> Self {
        self.attributes.push((key.to_owned(), value.into().0));
        self
    }

    /// Emits `step.started`.
    pub fn start(self) -> Step {
        let Some(core) = self.op.core.clone() else {
            return Step::disabled();
        };
        let step_id = self.step_id.unwrap_or_else(|| core.inner.ids.step_id());
        let limits = &core.inner.limits;
        let mut truncated = 0;
        let mut ident = |s: &str| {
            let (s, t) = text::ident(s, limits.max_name_bytes);
            truncated += u32::from(t);
            s
        };
        let name = ident(&self.name);
        let model = self.model.as_deref().map(&mut ident);
        let prompt_version = self.prompt_version.as_deref().map(&mut ident);
        let config_version = self.config_version.as_deref().map(&mut ident);
        let attributes = core.attributes(self.attributes, &mut truncated);
        core.inner.emit(
            &core,
            EventBody::StepStarted(StepStart {
                step_id,
                parent_step_id: self.parent,
                kind: self.kind,
                name: name.clone(),
                attempt: self.attempt,
                model: model.clone(),
                prompt_version: prompt_version.clone(),
                config_version: config_version.clone(),
                external_effect: self.external_effect,
            }),
            attributes,
            truncated,
        );
        Step {
            op: self.op,
            id: Some(step_id),
            attempt: self.attempt,
            kind: self.kind,
            name,
            parent: self.parent,
            model,
            prompt_version,
            config_version,
            external_effect: self.external_effect,
        }
    }
}

/// A started step attempt. Finish it with [`Step::finish`]; dropping it unfinished emits
/// nothing, and the console shows "no finish observed" — honest, not "failed".
#[must_use = "an unfinished step is shown as no_finish_observed"]
pub struct Step {
    op: Operation,
    id: Option<StepId>,
    attempt: u32,
    kind: StepKind,
    name: String,
    parent: Option<StepId>,
    model: Option<String>,
    prompt_version: Option<String>,
    config_version: Option<String>,
    external_effect: bool,
}

impl Step {
    fn disabled() -> Self {
        Self {
            op: Operation { core: None },
            id: None,
            attempt: 1,
            kind: StepKind::Tool,
            name: String::new(),
            parent: None,
            model: None,
            prompt_version: None,
            config_version: None,
            external_effect: false,
        }
    }

    pub fn id(&self) -> Option<StepId> {
        self.id
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Emits `step.finished` with the declared outcome. Returns a token that can start
    /// the next attempt of the same step.
    pub fn finish(self, finish: Finish) -> Finished {
        if let (Some(core), Some(step_id)) = (&self.op.core, self.id) {
            let mut truncated = 0;
            let parts = finish.into_parts(&core.inner, &mut truncated);
            core.inner.emit(
                core,
                EventBody::StepFinished(StepFinish {
                    step_id,
                    attempt: self.attempt,
                    kind: Some(self.kind),
                    name: Some(self.name.clone()),
                    outcome: parts.outcome,
                    error_code: parts.error_code,
                    error_message: parts.error_message,
                    usage: parts.usage,
                    cost_estimate: parts.cost,
                }),
                Vec::new(),
                truncated,
            );
        }
        Finished { step: self }
    }

    pub fn succeed(self) -> Finished {
        self.finish(Finish::succeeded())
    }
}

/// A finished attempt; [`Finished::retry`] starts the next attempt of the same step.
pub struct Finished {
    step: Step,
}

impl Finished {
    pub fn retry(self) -> StepBuilder {
        let s = self.step;
        StepBuilder {
            op: s.op,
            kind: s.kind,
            name: s.name,
            step_id: s.id,
            attempt: s.attempt.saturating_add(1),
            parent: s.parent,
            model: s.model,
            prompt_version: s.prompt_version,
            config_version: s.config_version,
            external_effect: s.external_effect,
            attributes: Vec::new(),
        }
    }
}

/// A declared outcome (D-020) plus what it cost.
#[derive(Debug, Clone)]
pub struct Finish {
    outcome: Outcome,
    error_code: Option<String>,
    error_message: Option<String>,
    usage: Option<Usage>,
    cost: Option<CostEstimate>,
}

impl Finish {
    pub fn succeeded() -> Self {
        Self {
            outcome: Outcome::Succeeded,
            error_code: None,
            error_message: None,
            usage: None,
            cost: None,
        }
    }

    /// A definitive failure. Use [`FailureClass::Transport`] only when the request
    /// provably never reached the remote; a timeout after sending is
    /// [`Finish::uncertain`].
    pub fn failed(class: FailureClass, code: &str) -> Self {
        Self {
            outcome: Outcome::Failed(class),
            error_code: Some(code.to_owned()),
            ..Self::succeeded()
        }
    }

    /// The request left the host and its effect is unknown.
    pub fn uncertain(reason: UncertainReason, code: &str) -> Self {
        Self {
            outcome: Outcome::Uncertain(reason),
            error_code: Some(code.to_owned()),
            ..Self::succeeded()
        }
    }

    #[must_use]
    pub fn usage(mut self, input_tokens: u64, output_tokens: u64) -> Self {
        self.usage = Some(Usage {
            input_tokens,
            output_tokens,
        });
        self
    }

    /// An estimate (never a bill): millionths of `currency`, and what it is based on.
    #[must_use]
    pub fn cost_estimate(mut self, micros: u64, currency: &str, basis: &str) -> Self {
        self.cost = Some(CostEstimate {
            micros,
            currency: currency.to_owned(),
            basis: text::ident(basis, 64).0,
        });
        self
    }

    /// Error text; sent only when the client was configured with
    /// `include_error_messages`, redacted and clipped to 512 bytes.
    #[must_use]
    pub fn message(mut self, text: &str) -> Self {
        self.error_message = Some(storable(text));
        self
    }

    fn into_parts(self, inner: &Inner, truncated: &mut u32) -> FinishParts {
        let code = self.error_code.as_deref().map(|c| {
            let (c, t) = text::ident(c, inner.limits.max_error_code_bytes);
            *truncated += u32::from(t);
            c
        });
        let message = self
            .error_message
            .as_deref()
            .filter(|_| inner.include_error_messages)
            .map(|m| {
                let (m, t) = text::clip(m, inner.limits.max_error_message_bytes);
                *truncated += u32::from(t);
                aiops_domain::policy::redact(&m).unwrap_or(m)
            });
        FinishParts {
            outcome: self.outcome,
            error_code: code,
            error_message: message,
            usage: self.usage,
            cost: self.cost,
        }
    }
}

struct FinishParts {
    outcome: Outcome,
    error_code: Option<String>,
    error_message: Option<String>,
    usage: Option<Usage>,
    cost: Option<CostEstimate>,
}
