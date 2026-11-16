//! The background task: batch, send with timeouts, retry only retryable answers inside a
//! bounded budget, then drop and count. Drops are turned into `gap.reported` events,
//! which keep their IDs until delivered (so a re-send never double-counts a gap).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use aiops_domain::event::{Event, EventBody, GapReason, GapReport};
use aiops_domain::wire::{IngestResponse, OutgoingBatch, SCHEMA_VERSION, WireSource, WireStats};
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
    pending_reports: Vec<Event>,
}

enum Sent {
    Accepted(Option<IngestResponse>),
    /// A definitive refusal of the whole batch (400, 401, 413): never retried unchanged.
    Rejected(StatusCode),
    /// Retries exhausted or out of budget.
    Failed,
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
            pending_reports: Vec::new(),
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
        let wire: Vec<_> = self.pending_reports[..reports]
            .iter()
            .chain(buf.iter())
            .map(Event::to_wire)
            .collect();
        let Ok(body) = serde_json::to_vec(&OutgoingBatch {
            schema_version: SCHEMA_VERSION,
            source: &self.source,
            stats: self.stats(),
            events: &wire,
        }) else {
            self.dropped(buf, GapReason::Rejected);
            return;
        };
        let c = &self.inner.counters;
        self.inner
            .in_flight
            .store(buf.len() as u64, Ordering::Relaxed);
        let outcome = self.send(body).await;
        self.inner.in_flight.store(0, Ordering::Relaxed);
        match outcome {
            Sent::Accepted(answer) => {
                add(&c.batches_sent, 1);
                add(&c.gap_reports_sent, reports as u64);
                if let Some(a) = answer {
                    add(&c.accepted, u64::from(a.accepted));
                    add(&c.duplicates, u64::from(a.duplicates));
                    add(&c.refused, a.refused.len() as u64);
                }
                self.pending_reports.drain(..reports);
                buf.clear();
            }
            Sent::Rejected(status) => {
                add(&c.batches_failed, 1);
                tracing::warn!(
                    status = status.as_u16(),
                    "aiops collector refused a batch; dropping it"
                );
                self.dropped(buf, GapReason::Rejected);
            }
            Sent::Failed => {
                add(&c.batches_failed, 1);
                self.dropped(buf, GapReason::SendFailed);
            }
        }
    }

    /// Counts a lost batch and records its drops per operation (reports stay pending).
    fn dropped(&self, buf: &mut Vec<Event>, reason: GapReason) {
        let c = &self.inner.counters;
        let counter = match reason {
            GapReason::Rejected => &c.dropped_rejected,
            GapReason::Shutdown => &c.dropped_shutdown,
            _ => &c.dropped_send_failed,
        };
        add(counter, buf.len() as u64);
        for e in buf.drain(..) {
            self.inner
                .record_gap(e.operation_id, reason, e.event_time, 1);
        }
    }

    async fn send(&self, body: Vec<u8>) -> Sent {
        let Some(auth) = &self.auth else {
            return Sent::Rejected(StatusCode::UNAUTHORIZED);
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
                Ok(r) => return Sent::Rejected(r.status()),
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
            if started.elapsed() + wait > self.retry_budget {
                return Sent::Failed;
            }
            tokio::time::sleep(wait).await;
        }
    }
}
