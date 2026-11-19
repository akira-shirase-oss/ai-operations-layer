//! The article workflow's steps as Durable Runner handlers (A2, D-012): `generate` (a fake
//! model), `validate`, `factcheck` (a fake tool) and `publish` (a fake publisher that
//! keeps its own record, the authority for "was it published":
//! `GET /publications/{business_key}`).
//!
//! It knows nothing of the AI Operations Layer: the adapter observes it from outside.
//! Like Durable Runner's own pilot handler it writes no log lines, so the drill's
//! logs-only baseline rests on what Durable Runner ships (D-024).
//!
//! The runner POSTs `{"input": [bytes], "idem_key": "<op_key>/<step>"}`; the job input is
//! `{"topic": "…", "fault": "…"}`. Every handler is idempotent by `idem_key` (the runner is
//! at-least-once) and answers its reconcile probe, `GET /<step>?idem_key=…`, with 200 once
//! that call completed. Faults (one per job): `model_retry_ok`, `model_exhausted`,
//! `slow_generate`, `validation_failed`, `tool_timeout`, `publish_rejected`,
//! `publish_lost_response`.
//!
//! `WORKER_LISTEN` (default 127.0.0.1:53333), `WORKER_HANG_MS` (how long a hanging call
//! hangs; default 3000, above the runner's step timeout).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Calls remembered at most (oldest forgotten first): a bound, not a database.
const MAX_CALLS: usize = 100_000;

#[derive(Deserialize)]
struct Call {
    input: Vec<u8>,
    idem_key: String,
}

#[derive(Deserialize, Default)]
struct Job {
    #[serde(default)]
    fault: Option<String>,
    /// A retry's business key (D-031): the new job carries it in the input so its
    /// publish dedups under the original key — a new `op_key`, the same effect.
    #[serde(default)]
    business_key: Option<String>,
}

#[derive(Clone, Serialize)]
struct Publication {
    business_key: String,
    first_published_at_ms: u128,
    times_published: u32,
    /// Publish calls that reached this key, deduplicated retries included — attempts
    /// are not effects (`times_published` is the effect: one per key).
    times_attempted: u32,
}

#[derive(Default)]
struct Calls {
    /// Per `idem_key`: how many times it was called, and its answer once it completed.
    by_key: HashMap<String, (u32, Option<String>)>,
    order: std::collections::VecDeque<String>,
}

#[derive(Default)]
struct Worker {
    calls: Mutex<Calls>,
    publications: Mutex<HashMap<String, Publication>>,
    hang: Duration,
}

impl Worker {
    /// Counts the call; returns its number and the answer if the key already completed.
    fn arrive(&self, key: &str) -> (u32, Option<String>) {
        let Ok(mut c) = self.calls.lock() else {
            return (1, None);
        };
        if !c.by_key.contains_key(key) {
            if c.order.len() >= MAX_CALLS
                && let Some(old) = c.order.pop_front()
            {
                c.by_key.remove(&old);
            }
            c.order.push_back(key.to_owned());
        }
        let e = c.by_key.entry(key.to_owned()).or_default();
        e.0 += 1;
        (e.0, e.1.clone())
    }

    fn complete(&self, key: &str, answer: &str) {
        if let Ok(mut c) = self.calls.lock()
            && let Some(e) = c.by_key.get_mut(key)
        {
            e.1 = Some(answer.to_owned());
        }
    }

    fn completed(&self, key: &str) -> bool {
        self.calls
            .lock()
            .is_ok_and(|c| c.by_key.get(key).is_some_and(|e| e.1.is_some()))
    }

    /// The publish effect dedups at its own authority (D-030 applies to the runner's
    /// workflow too): attempts count every call, `times_published` is the effect —
    /// one per business key however often a resubmitted run reaches this step.
    fn publish(&self, business_key: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        if let Ok(mut p) = self.publications.lock() {
            p.entry(business_key.to_owned())
                .and_modify(|p| p.times_attempted += 1)
                .or_insert_with(|| Publication {
                    business_key: business_key.to_owned(),
                    first_published_at_ms: now,
                    times_published: 1,
                    times_attempted: 1,
                });
        }
    }
}

fn ok(body: &str) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_owned(),
    )
        .into_response()
}

async fn step(
    State(w): State<Arc<Worker>>,
    Path(name): Path<String>,
    Json(call): Json<Call>,
) -> Response {
    let job: Job = serde_json::from_slice(&call.input).unwrap_or_default();
    let fault = job.fault.as_deref().unwrap_or("");
    // The retry's key comes from the job input (D-031); a first run derives it from the
    // op_key half of the idem key, as it always has.
    let business_key = job.business_key.clone().unwrap_or_else(|| {
        call.idem_key
            .rsplit_once('/')
            .map_or(call.idem_key.clone(), |(k, _)| k.to_owned())
    });
    let (n, done) = w.arrive(&call.idem_key);
    if let Some(answer) = done {
        return ok(&answer); // at-least-once: a repeat gets the first answer
    }
    let hang = w.hang;
    let answer = match (name.as_str(), fault) {
        ("generate", "model_retry_ok") if n == 1 => {
            return (StatusCode::SERVICE_UNAVAILABLE, "{}").into_response();
        }
        ("generate", "model_exhausted") | ("factcheck", "tool_timeout") => {
            tokio::time::sleep(hang).await;
            return (StatusCode::GATEWAY_TIMEOUT, "{}").into_response();
        }
        ("generate", "slow_generate") if n == 1 => {
            tokio::time::sleep(Duration::from_millis(2_000)).await;
            json!({"draft_words": 640, "usage": {"input_tokens": 132, "output_tokens": 480}})
        }
        ("generate", _) => {
            json!({"draft_words": 640, "usage": {"input_tokens": 132, "output_tokens": 480}})
        }
        ("validate", "validation_failed") => {
            return (StatusCode::UNPROCESSABLE_ENTITY, r#"{"rule":"length"}"#).into_response();
        }
        ("validate" | "factcheck", _) => json!({"ok": true}),
        ("publish", "publish_rejected") => {
            return (StatusCode::CONFLICT, r#"{"refused":"duplicate_title"}"#).into_response();
        }
        ("publish", "publish_lost_response") => {
            w.publish(&business_key);
            let answer = json!({"published": true}).to_string();
            w.complete(&call.idem_key, &answer);
            tokio::time::sleep(hang).await; // the reply comes too late to be read
            return ok(&answer);
        }
        ("publish", _) => {
            w.publish(&business_key);
            json!({"published": true})
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    .to_string();
    w.complete(&call.idem_key, &answer);
    ok(&answer)
}

#[derive(Deserialize)]
struct Probe {
    idem_key: String,
}

async fn probe(State(w): State<Arc<Worker>>, Query(p): Query<Probe>) -> StatusCode {
    if w.completed(&p.idem_key) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn publication(State(w): State<Arc<Worker>>, Path(key): Path<String>) -> Response {
    match w
        .publications
        .lock()
        .ok()
        .and_then(|p| p.get(&key).cloned())
    {
        Some(p) => Json(p).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"business_key": key, "published": false})),
        )
            .into_response(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen = std::env::var("WORKER_LISTEN").unwrap_or_else(|_| "127.0.0.1:53333".into());
    let hang_ms = std::env::var("WORKER_HANG_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000);
    let worker = Arc::new(Worker {
        hang: Duration::from_millis(hang_ms),
        ..Worker::default()
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/publications/{*key}", get(publication))
        .route("/{step}", get(probe).post(step))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(worker);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
