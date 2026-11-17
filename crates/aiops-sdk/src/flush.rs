//! The background task: batch, send with timeouts, retry only retryable answers inside a
//! bounded budget, then drop and count. Drops are turned into `gap.reported` events,
//! which keep their IDs until delivered (so a re-send never double-counts a gap).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use aiops_domain::event::{Event, EventBody, GapReason, GapReport};
use aiops_domain::wire::{IngestResponse, SCHEMA_VERSION, WireSource, WireStats};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use tokio::sync::mpsc;

use crate::{Config, Inner, add, now};

/// Gap reports held for delivery at most (each is one small event).
const PENDING_REPORTS_CAP: usize = 1_024;
const BACKOFF_BASE_MS: u64 = 100;
const BACKOFF_CAP_MS: u64 = 2_000;

pub(crate) struct Flusher {
    inner: Arc<Inner>,
    rx: mpsc::Receiver<Event>,
    http: reqwest::Client,
    url: reqwest::Url,
    auth: Option<HeaderValue>,
    source: WireSource,
    batch_max: usize,
    flush_interval: Duration,
    max_attempts: u32,
    retry_budget: Duration,
    batch_max_bytes: usize,
    request_timeout: Duration,
    pending_reports: Vec<Event>,
    /// Circuit breaker (review finding 21): after `breaker_threshold` failed batches in a
    /// row, the flusher holds its next batch until `open_until` and sends it as a probe, so
    /// a failing collector gets one attempt per cool-down instead of a stream of them;
    /// each failed probe doubles the cool-down, up to `BREAKER_MAX_COOLDOWN`. Meanwhile the
    /// bounded queue absorbs new events (and drops, counted, only if it fills).
    consecutive_failures: u32,
    open_until: Option<tokio::time::Instant>,
    cooldown: Duration,
    breaker_threshold: u32,
    breaker_cooldown: Duration,
}

const BREAKER_MAX_COOLDOWN: Duration = Duration::from_secs(5);

enum Sent {
    Accepted(Option<IngestResponse>),
    /// A definitive refusal of the whole batch (400, 401, 413), with the collector's error
    /// code when it gave one: never retried unchanged.
    Rejected(StatusCode, Option<String>),
    /// Retries exhausted or out of budget.
    Failed,
}

/// A refusal caused by the batch's size or by one event's content, which splitting the
/// batch can isolate: 413, or 400 `unstorable_text` (review N1). Auth, ownership, schema
/// and source refusals apply to every batch alike, so splitting them would only multiply
/// requests.
fn splittable(status: StatusCode, code: Option<&str>) -> bool {
    status == StatusCode::PAYLOAD_TOO_LARGE
        || (status == StatusCode::BAD_REQUEST && code == Some("unstorable_text"))
}

impl Flusher {
    pub(crate) fn new(
        inner: Arc<Inner>,
        rx: mpsc::Receiver<Event>,
        http: reqwest::Client,
        url: reqwest::Url,
        config: &Config,
        service: String,
    ) -> Self {
        let auth = HeaderValue::from_str(&format!("Bearer {}", config.ingest_key))
            .ok()
            .map(|mut v| {
                v.set_sensitive(true);
                v
            });
        let source = WireSource {
            instance_id: inner.instance_id.to_string(),
            service,
            sdk: crate::SDK_NAME.to_owned(),
        };
        Self {
            inner,
            rx,
            http,
            url,
            auth,
            source,
            batch_max: config.batch_max_events,
            flush_interval: config.flush_interval,
            max_attempts: config.max_send_attempts.max(1),
            retry_budget: config.retry_budget,
            batch_max_bytes: config.batch_max_bytes,
            request_timeout: config.request_timeout,
            pending_reports: Vec::new(),
            consecutive_failures: 0,
            open_until: None,
            cooldown: config.breaker_cooldown,
            breaker_threshold: config.breaker_threshold.max(1),
            breaker_cooldown: config.breaker_cooldown,
        }
    }

    fn gaps_waiting(&self) -> bool {
        !self.pending_reports.is_empty()
            || self.inner.gaps.lock().is_ok_and(|g| !g.entries.is_empty())
    }

    pub(crate) async fn run(mut self) {
        let mut buf: Vec<Event> = Vec::with_capacity(self.batch_max);
        loop {
            if buf.is_empty() {
                let waiting = self.gaps_waiting();
                tokio::select! {
                    biased;
                    () = self.inner.cancel.cancelled() => {}
                    _ = self.rx.recv_many(&mut buf, self.batch_max) => {}
                    () = tokio::time::sleep(self.flush_interval), if waiting => {}
                }
            }
            if self.inner.cancel.is_cancelled() {
                self.drain(buf).await;
                return;
            }
            // Fill the batch for at most one flush interval.
            let deadline = tokio::time::Instant::now() + self.flush_interval;
            while !buf.is_empty() && buf.len() < self.batch_max {
                let room = self.batch_max - buf.len();
                match tokio::time::timeout_at(deadline, self.rx.recv_many(&mut buf, room)).await {
                    Ok(n) if n > 0 => {}
                    _ => break,
                }
            }
            self.flush(&mut buf).await;
        }
    }

    /// Shutdown: send what is queued now, batch by batch, then stop. The caller's deadline
    /// bounds this (it aborts the task and counts what was left).
    async fn drain(&mut self, mut buf: Vec<Event>) {
        loop {
            while buf.len() < self.batch_max {
                match self.rx.try_recv() {
                    Ok(e) => buf.push(e),
                    Err(_) => break,
                }
            }
            if buf.is_empty() && !self.gaps_waiting() {
                return;
            }
            let before = buf.len();
            self.flush(&mut buf).await;
            if before == 0 && self.gaps_waiting() {
                return; // only undeliverable reports left
            }
        }
    }

    /// Moves recorded drops into `gap.reported` events (bounded).
    fn collect_gap_reports(&mut self) {
        let room = PENDING_REPORTS_CAP.saturating_sub(self.pending_reports.len());
        if room == 0 {
            return;
        }
        let taken: Vec<_> = match self.inner.gaps.lock() {
            Ok(mut gaps) => {
                let keys: Vec<_> = gaps.entries.keys().take(room).copied().collect();
                keys.into_iter()
                    .filter_map(|k| gaps.entries.remove(&k).map(|e| (k, e)))
                    .collect()
            }
            Err(_) => return,
        };
        let at = now();
        for ((operation_id, reason), entry) in taken {
            let seq = self.inner.next_seq();
            self.pending_reports.push(Event {
                event_id: self.inner.event_id(seq),
                operation_id,
                seq,
                op_seq: 0,
                event_time: at,
                body: EventBody::GapReported(GapReport {
                    dropped: entry.dropped,
                    reason: reason.reason(),
                    first_dropped_at: entry.first,
                    last_dropped_at: entry.last,
                }),
                attributes: Vec::new(),
                truncated: 0,
            });
        }
    }

    fn stats(&self) -> WireStats {
        let c = &self.inner.counters;
        WireStats {
            dropped_queue_full: c.dropped_queue_full.load(Ordering::Relaxed),
            dropped_send_failed: c.dropped_send_failed.load(Ordering::Relaxed),
            dropped_rejected: c.dropped_rejected.load(Ordering::Relaxed),
            gap_overflow: c.gap_overflow.load(Ordering::Relaxed),
            truncated_events: c.truncated_events.load(Ordering::Relaxed),
        }
    }

    /// One event's JSON, under the per-event cap: an event over it loses its attributes
    /// and is marked `truncated` (so the collector never refuses it for size).
    fn encode(&self, e: &Event) -> Option<Vec<u8>> {
        let json = serde_json::to_vec(&e.to_wire()).ok()?;
        if json.len() <= self.inner.limits.max_event_bytes {
            return Some(json);
        }
        let mut slim = e.clone();
        slim.attributes.clear();
        if slim.truncated == 0 {
            add(&self.inner.counters.truncated_events, 1);
        }
        slim.truncated += 1;
        let json = serde_json::to_vec(&slim.to_wire()).ok()?;
        (json.len() <= self.inner.limits.max_event_bytes).then_some(json)
    }

    /// The batch envelope around already-encoded events.
    fn envelope(&self, items: &[Encoded]) -> Option<Vec<u8>> {
        #[derive(serde::Serialize)]
        struct Head<'a> {
            schema_version: u16,
            source: &'a WireSource,
            stats: WireStats,
        }
        let mut body = serde_json::to_vec(&Head {
            schema_version: SCHEMA_VERSION,
            source: &self.source,
            stats: self.stats(),
        })
        .ok()?;
        body.pop(); // the closing brace
        body.extend_from_slice(b",\"events\":[");
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(&item.json);
        }
        body.extend_from_slice(b"]}");
        Some(body)
    }

    async fn flush(&mut self, buf: &mut Vec<Event>) {
        self.collect_gap_reports();
        let reports = self
            .pending_reports
            .len()
            .min(self.batch_max.saturating_sub(buf.len()));
        if buf.is_empty() && reports == 0 {
            return;
        }
        if let Some(oldest) = buf.iter().map(|e| e.event_time).min() {
            let age = u64::try_from(now().micros_since(oldest)).unwrap_or(0);
            let c = &self.inner.counters;
            c.last_queue_age_us.store(age, Ordering::Relaxed);
            c.max_queue_age_us.fetch_max(age, Ordering::Relaxed);
        }
        // Encode every event once; an event that cannot fit even without attributes is
        // lost as refused (it cannot happen within the schema's field caps).
        let mut items = Vec::with_capacity(reports + buf.len());
        for e in &self.pending_reports[..reports] {
            if let Some(json) = self.encode(e) {
                items.push(Encoded {
                    json,
                    event: None,
                    report: Some(e.event_id),
                });
            }
        }
        let mut unencodable = Vec::new();
        for e in buf.drain(..) {
            match self.encode(&e) {
                Some(json) => items.push(Encoded {
                    json,
                    event: Some(e),
                    report: None,
                }),
                None => unencodable.push(e),
            }
        }
        self.dropped(unencodable, GapReason::Rejected);

        // Pack by bytes as well as by count (review finding 6), then send each batch; a
        // 413 splits the batch and resends the halves instead of dropping it.
        let mut stack: Vec<Vec<Encoded>> = pack(items, self.batch_max, self.batch_max_bytes);
        stack.reverse();
        let mut delivered_reports = std::collections::HashSet::new();
        let c = &self.inner.counters;
        while let Some(chunk) = stack.pop() {
            if let Some(until) = self.open_until {
                // Hold this batch and send it as the probe when the cool-down ends (a
                // shutdown cuts the wait short; its deadline then bounds the rest).
                tokio::select! {
                    () = tokio::time::sleep_until(until) => {}
                    () = self.inner.cancel.cancelled() => {}
                }
            }
            let Some(body) = self.envelope(&chunk) else {
                self.lost(chunk, GapReason::Rejected);
                continue;
            };
            let events_in_chunk = chunk.iter().filter(|i| i.event.is_some()).count() as u64;
            self.inner
                .in_flight
                .store(events_in_chunk, Ordering::Relaxed);
            let outcome = self.send(body).await;
            self.inner.in_flight.store(0, Ordering::Relaxed);
            match outcome {
                Sent::Accepted(answer) => {
                    self.accepted(&chunk, answer.as_ref(), &mut delivered_reports);
                    self.consecutive_failures = 0;
                    self.open_until = None;
                    self.cooldown = self.breaker_cooldown;
                }
                // Too large, or one event's text the store cannot keep (review N1): split and
                // resend the halves until the cause is isolated, never drop the batch.
                Sent::Rejected(status, code)
                    if chunk.len() > 1 && splittable(status, code.as_deref()) =>
                {
                    let mut first = chunk;
                    let second = first.split_off(first.len() / 2);
                    stack.push(second);
                    stack.push(first);
                }
                Sent::Rejected(status, _) => {
                    add(&c.batches_failed, 1);
                    tracing::warn!(
                        status = status.as_u16(),
                        "aiops collector refused a batch; dropping it"
                    );
                    self.lost(chunk, GapReason::Rejected);
                }
                Sent::Failed => {
                    add(&c.batches_failed, 1);
                    self.lost(chunk, GapReason::SendFailed);
                    self.consecutive_failures += 1;
                    if self.consecutive_failures >= self.breaker_threshold {
                        if self.open_until.is_none() {
                            add(&c.breaker_opened, 1);
                        }
                        self.open_until = Some(tokio::time::Instant::now() + self.cooldown);
                        self.cooldown = (self.cooldown * 2).min(BREAKER_MAX_COOLDOWN);
                    }
                }
            }
        }
        add(&c.gap_reports_sent, delivered_reports.len() as u64);
        self.pending_reports
            .retain(|r| !delivered_reports.contains(&r.event_id));
    }

    /// Accounts for an accepted batch: counters, refused events as rejected gaps (review
    /// finding 13; a conflicting duplicate is not lost, so it is not reported), and the gap
    /// reports it delivered.
    fn accepted(
        &self,
        chunk: &[Encoded],
        answer: Option<&IngestResponse>,
        delivered: &mut std::collections::HashSet<aiops_domain::EventId>,
    ) {
        let c = &self.inner.counters;
        add(&c.batches_sent, 1);
        if let Some(a) = answer {
            add(&c.accepted, u64::from(a.accepted));
            add(&c.duplicates, u64::from(a.duplicates));
            add(&c.refused, a.refused.len() as u64);
            for r in a
                .refused
                .iter()
                .filter(|r| r.code != "conflicting_duplicate")
            {
                let item = usize::try_from(r.index).ok().and_then(|i| chunk.get(i));
                if let Some(e) = item.and_then(|i| i.event.as_ref()) {
                    self.inner
                        .record_gap(e.operation_id, GapReason::Rejected, e.event_time, 1);
                }
            }
        }
        delivered.extend(chunk.iter().filter_map(|i| i.report));
    }

    /// The events of a lost batch (its gap reports stay pending and are retried).
    fn lost(&self, chunk: Vec<Encoded>, reason: GapReason) {
        self.dropped(chunk.into_iter().filter_map(|i| i.event).collect(), reason);
    }

    /// Counts lost events and records their drops per operation.
    fn dropped(&self, events: Vec<Event>, reason: GapReason) {
        if events.is_empty() {
            return;
        }
        let c = &self.inner.counters;
        let counter = match reason {
            GapReason::Rejected => &c.dropped_rejected,
            GapReason::Shutdown => &c.dropped_shutdown,
            _ => &c.dropped_send_failed,
        };
        add(counter, events.len() as u64);
        for e in events {
            self.inner
                .record_gap(e.operation_id, reason, e.event_time, 1);
        }
    }

    async fn send(&self, body: Vec<u8>) -> Sent {
        let Some(auth) = &self.auth else {
            return Sent::Rejected(StatusCode::UNAUTHORIZED, None);
        };
        let started = Instant::now();
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let result = self
                .http
                .post(self.url.clone())
                .header(AUTHORIZATION, auth.clone())
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await;
            let retry_after = match result {
                Ok(r) if r.status().is_success() => {
                    let answer = r
                        .bytes()
                        .await
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok());
                    return Sent::Accepted(answer);
                }
                Ok(r)
                    if r.status() == StatusCode::TOO_MANY_REQUESTS
                        || r.status().is_server_error() =>
                {
                    r.headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs)
                }
                Ok(r) => {
                    let status = r.status();
                    let code = r
                        .bytes()
                        .await
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                        .and_then(|v| v["error"]["code"].as_str().map(str::to_owned));
                    return Sent::Rejected(status, code);
                }
                // Connection refused, DNS, timeouts: retryable. A timeout after sending may
                // mean the batch was stored; re-sending is idempotent (same event IDs).
                Err(_) => None,
            };
            if attempt >= self.max_attempts {
                return Sent::Failed;
            }
            let backoff_cap = BACKOFF_BASE_MS
                .saturating_mul(1 << (attempt - 1).min(10))
                .min(BACKOFF_CAP_MS);
            let wait = retry_after
                .unwrap_or_else(|| Duration::from_millis(self.inner.ids.jitter_ms(backoff_cap)));
            // The budget covers the next attempt's own timeout too (review finding 21).
            if started.elapsed() + wait + self.request_timeout > self.retry_budget {
                return Sent::Failed;
            }
            tokio::time::sleep(wait).await;
        }
    }
}

/// One encoded event: a host event (lost with its batch) or a pending gap report (kept and
/// retried until a batch carrying it is accepted).
struct Encoded {
    json: Vec<u8>,
    event: Option<Event>,
    report: Option<aiops_domain::EventId>,
}

/// Envelope bytes besides the events (source, stats, keys): a generous allowance.
const ENVELOPE_ALLOWANCE: usize = 2 * 1024;

/// Splits encoded events into batches of at most `max_events` and `max_bytes`.
fn pack(items: Vec<Encoded>, max_events: usize, max_bytes: usize) -> Vec<Vec<Encoded>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut bytes = ENVELOPE_ALLOWANCE;
    for item in items {
        let size = item.json.len() + 1;
        if !current.is_empty() && (current.len() >= max_events || bytes + size > max_bytes) {
            out.push(std::mem::take(&mut current));
            bytes = ENVELOPE_ALLOWANCE;
        }
        bytes += size;
        current.push(item);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
