//! The reference host (D-012): one noncritical content workflow — request → generation
//! (fake model) → validation → fact-check (fake tool) → publish (fake publisher) — with
//! injectable faults, estimated costs, and a publisher that keeps its own authoritative
//! record (`GET /publications/{business_key}`). It writes ordinary JSON log lines, the
//! "existing setup" the diagnosis drill's baseline uses (`docs/DRILL.md`).
//!
//! Instrumentation is the Rust SDK; with no `AIOPS_INGEST_KEY` the host runs with a
//! disabled client, identically otherwise. The host never depends on the collector.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aiops_sdk::{Client, Config, FailureClass, Finish, StepKind, UncertainReason};
use anyhow::Context;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The fake model's list price, as an estimate basis (never a bill).
const COST_BASIS: &str = "fake-list-price-2026-11";
const INPUT_MICROS_PER_TOKEN: u64 = 1; // $1 per million input tokens
const OUTPUT_MICROS_PER_TOKEN: u64 = 4; // $4 per million output tokens
const MODEL_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Fault {
    ModelTransportOnce,
    ModelTimeoutAll,
    InvalidDraft,
    ToolTimeout,
    PublishLostResponse,
    PublishRejected,
}

#[derive(Debug, Deserialize)]
struct ArticleRequest {
    topic: String,
    #[serde(default)]
    fault: Option<Fault>,
}

/// `POST /articles/retry`'s body: resubmit the stored request under this business key.
#[derive(Debug, Deserialize)]
struct RetryRequest {
    business_key: String,
}

#[derive(Debug, Clone, Serialize)]
struct Publication {
    business_key: String,
    title: String,
    first_published_at_ms: u128,
    /// Distinct publish effects under this key — always 1, because `publish` is
    /// idempotent by business key (D-030): effects dedup at their own authority.
    times_published: u32,
    /// Publish calls under this key, including deduped ones: a resubmission stays
    /// visible as an attempt even though its effect dedups.
    times_attempted: u32,
}

/// The publisher's own record: the authority for "was this published" (invariant 1).
/// Idempotent by business key: a repeated publish under a key that already landed adds
/// an attempt, never a second effect.
#[derive(Default)]
struct Publisher {
    records: Mutex<HashMap<String, Publication>>,
}

impl Publisher {
    fn publish(&self, key: &str, title: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        if let Ok(mut r) = self.records.lock() {
            r.entry(key.to_owned())
                .and_modify(|p| p.times_attempted += 1)
                .or_insert_with(|| Publication {
                    business_key: key.to_owned(),
                    title: title.to_owned(),
                    first_published_at_ms: now,
                    times_attempted: 1,
                    times_published: 1,
                });
        }
    }

    fn get(&self, key: &str) -> Option<Publication> {
        self.records.lock().ok()?.get(key).cloned()
    }
}

/// JSON lines through a bounded channel to a writer thread: logging never blocks an
/// async worker; a full channel drops the line and counts it.
struct Log {
    tx: std::sync::mpsc::SyncSender<String>,
    dropped: AtomicU64,
}

impl Log {
    fn start(path: Option<String>) -> anyhow::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(20_000);
        let mut out: Box<dyn Write + Send> = match path {
            Some(p) => Box::new(std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&p)
                    .with_context(|| format!("opening log file {p}"))?,
            )),
            None => Box::new(std::io::BufWriter::new(std::io::stdout())),
        };
        std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || {
                while let Ok(line) = rx.recv() {
                    let _ = writeln!(out, "{line}");
                    // Flush when the channel is drained, not per line.
                    for next in rx.try_iter() {
                        let _ = writeln!(out, "{next}");
                    }
                    let _ = out.flush();
                }
            })
            .context("spawning the log writer")?;
        Ok(Self {
            tx,
            dropped: AtomicU64::new(0),
        })
    }

    fn line(&self, level: &str, request_id: &str, msg: &str, extra: serde_json::Value) {
        let ts = time_rfc3339();
        let mut v = json!({"ts": ts, "level": level, "request_id": request_id, "msg": msg});
        if let (Some(obj), serde_json::Value::Object(more)) = (v.as_object_mut(), extra) {
            obj.extend(more);
        }
        if self.tx.try_send(v.to_string()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn time_rfc3339() -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(0));
    aiops_sdk_time(micros)
}

fn aiops_sdk_time(micros: i64) -> String {
    // The same RFC 3339 rendering the layer uses (UTC, microseconds).
    let secs = micros.div_euclid(1_000_000);
    let sub = micros.rem_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{sub:06}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

struct Host {
    sdk: Client,
    publisher: Publisher,
    /// The request each business key was made under (key → topic): a real host keeps
    /// its work, so `POST /articles/retry` can resubmit it by key. Bounded.
    pending: Mutex<HashMap<String, String>>,
    log: Log,
    allow_faults: bool,
    model_latency: Duration,
    timeout: Duration,
    requests: AtomicU64,
}

type Shared = Arc<Host>;

#[derive(Serialize)]
struct ArticleResponse {
    request_id: String,
    operation_id: Option<String>,
    business_key: String,
    status: &'static str,
}

fn reply(
    status: StatusCode,
    request_id: String,
    op: &aiops_sdk::Operation,
    key: String,
    what: &'static str,
) -> Response {
    (
        status,
        Json(ArticleResponse {
            request_id,
            operation_id: op.id().map(|i| i.to_string()),
            business_key: key,
            status: what,
        }),
    )
        .into_response()
}

fn cost(input: u64, output: u64) -> u64 {
    input * INPUT_MICROS_PER_TOKEN + output * OUTPUT_MICROS_PER_TOKEN
}

async fn create_article(
    State(host): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<ArticleRequest>,
) -> Response {
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64)
        .map_or_else(
            || format!("req-{}", uuid::Uuid::new_v4().simple()),
            str::to_owned,
        );
    let fault = if host.allow_faults { req.fault } else { None };
    // The host keeps its work: the key can be resubmitted later (POST /articles/retry).
    if let Ok(mut p) = host.pending.lock()
        && p.len() < 10_000
    {
        p.insert(format!("article:{request_id}"), req.topic.clone());
    }
    run_article(&host, request_id, req.topic, fault).await
}

/// `POST /articles/retry {"business_key": "article:<id>"}` — resubmits the stored
/// request under the same business key (D-030). A resubmission never re-injects a
/// fault, and its publish dedups at the publisher, so the same effect cannot land
/// twice. 404 for a key the host does not hold: it cannot resubmit work it never saw.
async fn retry_article(State(host): State<Shared>, Json(req): Json<RetryRequest>) -> Response {
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "the host holds no request under this business key"})),
        )
            .into_response()
    };
    let Some(request_id) = req
        .business_key
        .strip_prefix("article:")
        .filter(|s| !s.is_empty() && s.len() <= 64)
    else {
        return not_found();
    };
    let topic = host
        .pending
        .lock()
        .ok()
        .and_then(|p| p.get(&req.business_key).cloned());
    match topic {
        Some(topic) => run_article(&host, request_id.to_owned(), topic, None).await,
        None => not_found(),
    }
}

/// The pilot workflow, shared by `POST /articles` and `POST /articles/retry`: request →
/// generation → validation → fact-check → publish, every step telemetry-described, the
/// publish's effect owned by the publisher.
#[expect(
    clippy::too_many_lines,
    reason = "the whole pilot workflow, step by step, in one readable place"
)]
async fn run_article(
    host: &Shared,
    request_id: String,
    topic: String,
    fault: Option<Fault>,
) -> Response {
    let n = host.requests.fetch_add(1, Ordering::Relaxed) + 1;
    let key = format!("article:{request_id}");
    let req = ArticleRequest { topic, fault };
    let log = &host.log;
    let op = host
        .sdk
        .operation("publish_article")
        .request_id(&request_id)
        .attribute("business_key", key.as_str())
        .attribute("http.route", "/articles")
        .start();
    log.line(
        "info",
        &request_id,
        "article requested",
        json!({"topic_len": req.topic.len(), "n": n}),
    );

    // 1. Generation: a fake model, up to three attempts.
    let mut builder = op
        .step(StepKind::Model, "generate")
        .model("fake-writer-1")
        .prompt_version("article-v3");
    let mut draft = None;
    for attempt in 1..=MODEL_ATTEMPTS {
        let step = builder.start();
        tokio::time::sleep(host.model_latency).await;
        let (input, output) = (120 + req.topic.len() as u64, 480);
        match fault {
            Some(Fault::ModelTransportOnce) if attempt == 1 => {
                let done = step.finish(Finish::failed(FailureClass::Transport, "connect_refused"));
                log.line(
                    "warn",
                    &request_id,
                    "model call failed: connection refused",
                    json!({"attempt": attempt}),
                );
                builder = done.retry();
            }
            Some(Fault::ModelTimeoutAll) => {
                tokio::time::sleep(host.timeout).await;
                // Sent, no answer: the model may have run (and cost money).
                let done = step.finish(
                    Finish::uncertain(UncertainReason::TimeoutAfterSend, "model_timeout")
                        .cost_estimate(cost(input, 0), "USD", COST_BASIS),
                );
                log.line(
                    "warn",
                    &request_id,
                    "model call timed out",
                    json!({"attempt": attempt, "after_ms": host.timeout.as_millis()}),
                );
                builder = done.retry();
            }
            _ => {
                step.finish(Finish::succeeded().usage(input, output).cost_estimate(
                    cost(input, output),
                    "USD",
                    COST_BASIS,
                ));
                draft = Some(format!("A short article about {}", req.topic));
                break;
            }
        }
    }
    let Some(draft) = draft else {
        op.finish(Finish::failed(
            FailureClass::Internal,
            "model_attempts_exhausted",
        ));
        log.line(
            "error",
            &request_id,
            "generation failed",
            json!({"attempts": MODEL_ATTEMPTS}),
        );
        return reply(StatusCode::BAD_GATEWAY, request_id, &op, key, "failed");
    };

    // 2. Validation.
    let v = op.step(StepKind::Validation, "validate").start();
    if fault == Some(Fault::InvalidDraft) || draft.len() < 10 {
        v.finish(Finish::failed(FailureClass::Validation, "draft_too_short"));
        op.finish(Finish::failed(FailureClass::Validation, "draft_rejected"));
        log.line(
            "warn",
            &request_id,
            "validation failed",
            json!({"rule": "min_length"}),
        );
        return reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            request_id,
            &op,
            key,
            "failed",
        );
    }
    v.succeed();

    // 3. Fact-check: a fake tool with no external effect.
    let t = op
        .step(StepKind::Tool, "fact_check")
        .attribute("tool.name", "fake-facts")
        .start();
    tokio::time::sleep(host.model_latency / 4).await;
    if fault == Some(Fault::ToolTimeout) {
        tokio::time::sleep(host.timeout).await;
        t.finish(Finish::uncertain(
            UncertainReason::TimeoutAfterSend,
            "fact_check_timeout",
        ));
        op.finish(Finish::failed(FailureClass::Internal, "fact_check_timeout"));
        log.line(
            "error",
            &request_id,
            "fact check timed out",
            json!({"after_ms": host.timeout.as_millis()}),
        );
        return reply(StatusCode::GATEWAY_TIMEOUT, request_id, &op, key, "failed");
    }
    t.succeed();

    // 4. Publishing: an external effect, recorded by the publisher (the authority).
    let p = op
        .step(StepKind::Tool, "publish")
        .external_effect(true)
        .attribute("business_key", key.as_str())
        .start();
    tokio::time::sleep(host.model_latency / 4).await;
    match fault {
        Some(Fault::PublishRejected) => {
            p.finish(Finish::failed(FailureClass::Rejected, "publisher_422"));
            op.finish(Finish::failed(FailureClass::Rejected, "publish_rejected"));
            log.line(
                "error",
                &request_id,
                "publish failed: publisher returned 422",
                json!({}),
            );
            reply(StatusCode::BAD_GATEWAY, request_id, &op, key, "failed")
        }
        Some(Fault::PublishLostResponse) => {
            host.publisher.publish(&key, &draft); // it landed…
            tokio::time::sleep(host.timeout).await; // …but the answer never came back
            p.finish(Finish::uncertain(
                UncertainReason::TimeoutAfterSend,
                "publish_timeout",
            ));
            op.finish(Finish::uncertain(
                UncertainReason::TimeoutAfterSend,
                "publish_timeout",
            ));
            log.line(
                "error",
                &request_id,
                "publish request timed out",
                json!({"after_ms": host.timeout.as_millis()}),
            );
            reply(StatusCode::GATEWAY_TIMEOUT, request_id, &op, key, "unknown")
        }
        _ => {
            host.publisher.publish(&key, &draft);
            p.succeed();
            op.succeed();
            log.line("info", &request_id, "article published", json!({}));
            reply(StatusCode::CREATED, request_id, &op, key, "published")
        }
    }
}

async fn publication(State(host): State<Shared>, Path(key): Path<String>) -> Response {
    match host.publisher.get(&key) {
        Some(p) => Json(p).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"business_key": key, "published": false})),
        )
            .into_response(),
    }
}

async fn sdk_stats(State(host): State<Shared>) -> Json<serde_json::Value> {
    let s = host.sdk.stats();
    Json(json!({
        "enabled": host.sdk.is_enabled(),
        "emitted": s.emitted,
        "accepted": s.accepted,
        "duplicates": s.duplicates,
        "refused": s.refused,
        "dropped": s.dropped(),
        "dropped_queue_full": s.dropped_queue_full,
        "dropped_send_failed": s.dropped_send_failed,
        "dropped_rejected": s.dropped_rejected,
        "dropped_shutdown": s.dropped_shutdown,
        "gap_overflow": s.gap_overflow,
        "gap_reports_sent": s.gap_reports_sent,
        "batches_sent": s.batches_sent,
        "batches_failed": s.batches_failed,
        "max_queue_age_us": s.max_queue_age_us,
        "breaker_opened": s.breaker_opened,
        "log_lines_dropped": host.log.dropped.load(Ordering::Relaxed),
        "requests": host.requests.load(Ordering::Relaxed),
    }))
}

fn env_ms(name: &str, default: u64) -> anyhow::Result<Duration> {
    match std::env::var(name) {
        Ok(v) => Ok(Duration::from_millis(
            v.parse()
                .with_context(|| format!("{name} must be milliseconds"))?,
        )),
        Err(_) => Ok(Duration::from_millis(default)),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen: std::net::SocketAddr = std::env::var("REFHOST_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:57401".into())
        .parse()
        .context("REFHOST_LISTEN must be host:port")?;
    let sdk = match (
        std::env::var("AIOPS_ENDPOINT"),
        std::env::var("AIOPS_INGEST_KEY"),
    ) {
        (Ok(endpoint), Ok(key)) => {
            let mut cfg = Config::new(endpoint, key, "reference-host");
            cfg.release = std::env::var("REFHOST_RELEASE").ok();
            if let Ok(q) = std::env::var("REFHOST_SDK_QUEUE") {
                cfg.queue_capacity = q.parse().context("REFHOST_SDK_QUEUE")?;
            }
            if std::env::var("REFHOST_SDK_REQUEST_TIMEOUT_MS").is_ok() {
                cfg.request_timeout = env_ms("REFHOST_SDK_REQUEST_TIMEOUT_MS", 2_000)?;
            }
            if std::env::var("REFHOST_SDK_RETRY_BUDGET_MS").is_ok() {
                cfg.retry_budget = env_ms("REFHOST_SDK_RETRY_BUDGET_MS", 5_000)?;
            }
            Client::start(&cfg).context("starting the aiops SDK")?
        }
        _ => Client::disabled(),
    };
    let host = Arc::new(Host {
        sdk: sdk.clone(),
        publisher: Publisher::default(),
        pending: Mutex::new(HashMap::new()),
        log: Log::start(std::env::var("REFHOST_LOG_FILE").ok())?,
        allow_faults: std::env::var("REFHOST_ALLOW_FAULTS").is_ok_and(|v| v == "1"),
        model_latency: env_ms("REFHOST_MODEL_LATENCY_MS", 20)?,
        timeout: env_ms("REFHOST_TIMEOUT_MS", 100)?,
        requests: AtomicU64::new(0),
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/articles", post(create_article))
        .route("/articles/retry", post(retry_article))
        .route("/publications/{key}", get(publication))
        .route("/aiops/stats", get(sdk_stats))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(10),
        ))
        .with_state(host);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    let stats = sdk.shutdown(Duration::from_secs(2)).await;
    eprintln!(
        "{}",
        json!({"msg": "shutdown", "sdk_emitted": stats.emitted, "sdk_dropped": stats.dropped()})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_rfc3339_utc() {
        assert_eq!(aiops_sdk_time(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            aiops_sdk_time(1_763_287_200_123_456),
            "2025-11-16T10:00:00.123456Z"
        );
    }
}
