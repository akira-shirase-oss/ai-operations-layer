//! The proxy Durable Runner calls instead of its handlers, and the watcher that reads the
//! runner's own answer for a run's outcome (D-027).
//!
//! The proxy never changes what the runner decides. A call is forwarded unchanged; the
//! handler's answer goes back unchanged; a handler that cannot be reached is answered
//! 502 (the runner fails the attempt either way); and a call whose connection is lost
//! after it was sent is answered with a reply the runner cannot read, so the runner
//! reconciles exactly as it would have. The proxy waits for the handler past the runner's
//! deadline, so the runner's own timeout decides, and the proxy still learns what the
//! handler did.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use aiops_domain::correlate::{operation_for_run, step_for};
use aiops_sdk::{Client, FailureClass, Operation, StepKind};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::classify::{RunState, Seen, call_outcome, failure_of, probe_outcome, run_finish};
use crate::config::{Config, MAX_CALL_BYTES, MAX_REPLY_BYTES, Route};

/// The call headers Durable Runner sends (its D-050).
pub const RUN_ID_HEADER: &str = "x-durable-run-id";
pub const STEP_HEADER: &str = "x-durable-step";
pub const ATTEMPT_HEADER: &str = "x-durable-attempt";
const IDEM_HEADER: &str = "x-idempotency-key";

/// Which call the runner says this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallId {
    pub run: u64,
    pub step: u32,
    pub attempt: u32,
}

impl CallId {
    pub fn from_headers(h: &HeaderMap) -> Option<Self> {
        let num = |name: &str| h.get(name)?.to_str().ok()?.parse::<u64>().ok();
        let attempt = u32::try_from(num(ATTEMPT_HEADER)?).ok()?;
        Some(Self {
            run: num(RUN_ID_HEADER)?,
            step: u32::try_from(num(STEP_HEADER)?).ok()?,
            attempt: (1..=1_000).contains(&attempt).then_some(attempt)?,
        })
    }
}

/// One run this adapter instance reports.
struct Tracked {
    op: Operation,
    /// Calls of this run in flight here: the operation is never finished under one.
    in_flight: u32,
    /// Per step, the class of the last failed attempt seen (the finish's class).
    last_failure: HashMap<u32, FailureClass>,
    next_poll: Instant,
}

/// The adapter's own counters (`GET /stats`): nothing it could not observe is hidden.
#[derive(Default)]
pub struct Counters {
    pub calls: AtomicU64,
    pub probes: AtomicU64,
    /// Calls without the runner's call headers (an older runner): forwarded, not reported.
    pub calls_unidentified: AtomicU64,
    /// Runs not reported because `max_runs_tracked` were tracked already.
    pub runs_untracked_capacity: AtomicU64,
    /// Calls refused because `max_calls_in_flight` were in flight (answered 503).
    pub calls_shed: AtomicU64,
    pub runs_finished: AtomicU64,
    /// Runs the runner no longer knows (expired, or another journal): never finished here.
    pub runs_unknown_to_runner: AtomicU64,
    pub polls: AtomicU64,
    pub poll_errors: AtomicU64,
}

#[derive(Serialize)]
pub struct CountersView {
    pub calls: u64,
    pub probes: u64,
    pub calls_unidentified: u64,
    pub runs_untracked_capacity: u64,
    pub calls_shed: u64,
    pub runs_tracked: usize,
    pub runs_finished: u64,
    pub runs_unknown_to_runner: u64,
    pub polls: u64,
    pub poll_errors: u64,
}

pub struct Adapter {
    pub config: Config,
    pub sdk: Client,
    http: reqwest::Client,
    runs: Mutex<HashMap<u64, Tracked>>,
    calls: Arc<Semaphore>,
    /// Each call's forwarding and reporting runs here, not in the HTTP handler: the runner
    /// hangs up at its deadline, which drops the handler, and the call must still be
    /// reported and leave the run (else the run is never finished).
    pub tasks: TaskTracker,
    pub counters: Counters,
}

fn add(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

impl Adapter {
    pub fn new(config: Config, sdk: Client) -> anyhow::Result<Arc<Self>> {
        // Like the runner's own handler client (its D-039): no redirects, no proxy
        // variables, a connect timeout.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let calls = Arc::new(Semaphore::new(config.file.max_calls_in_flight));
        Ok(Arc::new(Self {
            config,
            sdk,
            http,
            runs: Mutex::new(HashMap::new()),
            calls,
            tasks: TaskTracker::new(),
            counters: Counters::default(),
        }))
    }

    pub fn view(&self) -> CountersView {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let c = &self.counters;
        CountersView {
            calls: g(&c.calls),
            probes: g(&c.probes),
            calls_unidentified: g(&c.calls_unidentified),
            runs_untracked_capacity: g(&c.runs_untracked_capacity),
            calls_shed: g(&c.calls_shed),
            runs_tracked: self.runs.lock().map_or(0, |r| r.len()),
            runs_finished: g(&c.runs_finished),
            runs_unknown_to_runner: g(&c.runs_unknown_to_runner),
            polls: g(&c.polls),
            poll_errors: g(&c.poll_errors),
        }
    }

    /// The run's operation, started here on this instance's first sight of the run (an
    /// instance that sees a run part-way, after a restart, starts it too: the earlier
    /// instance's unfinished tail stays an unknown, D-027). `None` when the run is not
    /// reported (tracking is full). Marks a call in flight.
    fn enter(&self, call: CallId, idem_key: &str) -> Option<Operation> {
        let mut runs = self.runs.lock().ok()?;
        if let Some(t) = runs.get_mut(&call.run) {
            t.in_flight += 1;
            return Some(t.op.clone());
        }
        if runs.len() >= self.config.file.max_runs_tracked {
            add(&self.counters.runs_untracked_capacity);
            return None;
        }
        let op_key = idem_key.rsplit_once('/').map_or(idem_key, |(k, _)| k);
        let runner = &self.config.file.runner.label;
        let op = self
            .sdk
            .operation(&self.config.file.workflow)
            .id(operation_for_run(runner, call.run))
            .request_id(op_key)
            .attribute("business_key", op_key)
            .attribute("job.runner", runner.as_str())
            .attribute("job.id", i64::try_from(call.run).unwrap_or(i64::MAX))
            .start();
        runs.insert(
            call.run,
            Tracked {
                op: op.clone(),
                in_flight: 1,
                last_failure: HashMap::new(),
                next_poll: Instant::now(),
            },
        );
        Some(op)
    }

    fn leave(&self, call: CallId, failure: Option<FailureClass>) {
        if let Ok(mut runs) = self.runs.lock()
            && let Some(t) = runs.get_mut(&call.run)
        {
            t.in_flight = t.in_flight.saturating_sub(1);
            if let Some(class) = failure {
                t.last_failure.insert(call.step, class);
            }
            // Ask the runner soon: this call may have ended the run.
            t.next_poll = Instant::now() + Duration::from_millis(50);
        }
    }

    async fn forward(
        &self,
        route: &Route,
        method: reqwest::Method,
        query: Option<&str>,
        headers: &HeaderMap,
        body: Bytes,
    ) -> (Seen, Option<(StatusCode, HeaderValue, Bytes)>) {
        let mut url = route.upstream.clone();
        if let Some(q) = query {
            url.push('?');
            url.push_str(q);
        }
        let mut req = self
            .http
            .request(method, url)
            .timeout(self.config.upstream_timeout());
        for name in [
            IDEM_HEADER,
            RUN_ID_HEADER,
            STEP_HEADER,
            ATTEMPT_HEADER,
            header::CONTENT_TYPE.as_str(),
        ] {
            if let Some(v) = headers.get(name) {
                req = req.header(name, v.clone());
            }
        }
        if !body.is_empty() {
            req = req.body(body);
        }
        let res = match req.send().await {
            Ok(r) => r,
            // Timeout first, as the runner checks it: a connect that timed out is lost to
            // the runner (it reconciles), not a refused connection (review R1).
            Err(e) if e.is_timeout() => return (Seen::NoAnswer, None),
            Err(e) if e.is_connect() => return (Seen::Unreachable, None),
            Err(_) => return (Seen::ConnectionLost, None),
        };
        let status = res.status();
        let content_type = res
            .headers()
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or(HeaderValue::from_static("application/json"));
        match read_bounded(res, MAX_REPLY_BYTES).await {
            Ok(Some(bytes)) => {
                let usage = usage_of(&bytes);
                (
                    Seen::Answered {
                        status: status.as_u16(),
                        usage,
                    },
                    Some((status, content_type, bytes)),
                )
            }
            Ok(None) => (
                Seen::ReplyOverLimit {
                    status: status.as_u16(),
                },
                None,
            ),
            Err(e) if e.is_timeout() => (Seen::NoAnswer, None),
            Err(_) => (Seen::ConnectionLost, None),
        }
    }
}

/// Reads a reply up to `max` bytes; `None` past it (nothing beyond is buffered).
async fn read_bounded(
    mut res: reqwest::Response,
    max: usize,
) -> Result<Option<Bytes>, reqwest::Error> {
    if res.content_length().is_some_and(|n| n > max as u64) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        if body.len() + chunk.len() > max {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(Bytes::from(body)))
}

/// Token usage, if the reply reports it the common way (`{"usage": {"input_tokens",
/// "output_tokens"}}`). Nothing else of the reply is read: it is the handler's output.
fn usage_of(bytes: &[u8]) -> Option<(u64, u64)> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let u = v.get("usage")?;
    Some((
        u.get("input_tokens")?.as_u64()?,
        u.get("output_tokens")?.as_u64()?,
    ))
}

/// A body that fails as soon as it is read: the runner reads a 2xx reply, the read fails,
/// and it reconciles the call (its `Lost`), as it would have against the handler.
struct Unreadable;

impl http_body::Body for Unreadable {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        Poll::Ready(Some(Err(std::io::Error::other(
            "the handler's connection was lost after the call was sent",
        ))))
    }
}

fn answer(seen: &Seen, reply: Option<(StatusCode, HeaderValue, Bytes)>) -> Response {
    match (seen, reply) {
        (_, Some((status, content_type, bytes))) => {
            let mut r = ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
            *r.status_mut() = status;
            r
        }
        // The runner fails these attempts against the handler itself too.
        (Seen::Unreachable | Seen::ReplyOverLimit { .. }, None) => {
            StatusCode::BAD_GATEWAY.into_response()
        }
        // A timeout is lost to the runner too (it reconciles); a status here would make it
        // retry without reconciling (review R1).
        (Seen::NoAnswer | Seen::ConnectionLost | Seen::Answered { .. }, None) => {
            (StatusCode::OK, Body::new(Unreadable)).into_response()
        }
    }
}

pub fn router(adapter: Arc<Adapter>) -> Router {
    Router::new()
        .route("/h/{route}", get(probe).post(call))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/stats",
            get(|State(a): State<Arc<Adapter>>| async move { axum::Json(a.view()) }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_CALL_BYTES))
        .with_state(adapter)
}

async fn call(
    State(a): State<Arc<Adapter>>,
    Path(route): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let inner = a.clone();
    a.tasks
        .spawn(async move { handle_call(inner, route, headers, body).await })
        .await
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn handle_call(a: Arc<Adapter>, route: String, headers: HeaderMap, body: Bytes) -> Response {
    let Some(route) = a.config.route(&route).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(_permit) = a.calls.clone().try_acquire_owned() else {
        // The handler never sees this call: the runner fails the attempt and retries.
        add(&a.counters.calls_shed);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    add(&a.counters.calls);
    let id = CallId::from_headers(&headers);
    let idem = headers
        .get(IDEM_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if id.is_none() {
        add(&a.counters.calls_unidentified);
    }
    let op = id.and_then(|c| a.enter(c, &idem));
    let step = match (&op, id) {
        (Some(op), Some(c)) => {
            let op_id = op.id();
            let mut b = op
                .step(route.kind.step_kind(), &route.route)
                .attempt(c.attempt)
                .external_effect(route.external_effect);
            if let Some(op_id) = op_id {
                b = b.id(step_for(op_id, c.step, false));
            }
            if let Some(m) = &route.model {
                b = b.model(m);
            }
            Some(b.start())
        }
        _ => None,
    };
    let started = Instant::now();
    let (seen, reply) = a
        .forward(&route, reqwest::Method::POST, None, &headers, body)
        .await;
    let elapsed = started.elapsed();
    let deadline = a.config.step_timeout();
    let finish = call_outcome(&seen, route.kind, elapsed, deadline, route.price.as_ref());
    if let Some(s) = step {
        s.finish(finish);
    }
    let failure = failure_of(&seen, route.kind, elapsed, deadline);
    if let Some(c) = id.filter(|_| op.is_some()) {
        a.leave(c, failure);
    }
    answer(&seen, reply)
}

async fn probe(
    State(a): State<Arc<Adapter>>,
    Path(route): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let inner = a.clone();
    a.tasks
        .spawn(async move { handle_probe(inner, route, query, headers).await })
        .await
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn handle_probe(
    a: Arc<Adapter>,
    route: String,
    query: Option<String>,
    headers: HeaderMap,
) -> Response {
    let Some(route) = a.config.route(&route).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(_permit) = a.calls.clone().try_acquire_owned() else {
        add(&a.counters.calls_shed);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    add(&a.counters.probes);
    let id = CallId::from_headers(&headers);
    let idem = query
        .as_deref()
        .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("idem_key=")))
        .map(decode_component)
        .unwrap_or_default();
    let op = id.and_then(|c| a.enter(c, &idem));
    let step = match (&op, id) {
        (Some(op), Some(c)) => op.id().map(|op_id| {
            op.step(StepKind::Job, &format!("{}.reconcile", route.route))
                .id(step_for(op_id, c.step, true))
                .parent_id(step_for(op_id, c.step, false))
                .attempt(c.attempt)
                .start()
        }),
        _ => None,
    };
    let (seen, reply) = a
        .forward(
            &route,
            reqwest::Method::GET,
            query.as_deref(),
            &headers,
            Bytes::new(),
        )
        .await;
    if let Some(s) = step {
        s.finish(probe_outcome(&seen));
    }
    if let Some(c) = id.filter(|_| op.is_some()) {
        a.leave(c, None);
    }
    match reply {
        Some((status, _, _)) => status.into_response(),
        None => StatusCode::BAD_GATEWAY.into_response(),
    }
}

/// `%XX` and `+` decoding for one query value (the runner percent-encodes the key).
fn decode_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What `GET /jobs/{id}` answered.
#[derive(Debug, serde::Deserialize)]
struct RunAnswer {
    state: String,
}

/// Asks the runner, for each tracked run whose turn it is, what state the run is in, and
/// finishes the operations of runs that are terminal — with the runner's answer as the
/// declared outcome. At most `per_tick` runs are asked per tick, one at a time, each under
/// a 2 s deadline, so the watcher can never load the runner.
pub async fn watch(a: Arc<Adapter>, cancel: CancellationToken) {
    let every = Duration::from_millis(a.config.file.poll_interval_ms);
    let per_tick = 16;
    let mut tick = tokio::time::interval(every);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let now = Instant::now();
        let due: Vec<u64> = a.runs.lock().map_or_else(
            |_| Vec::new(),
            |runs| {
                runs.iter()
                    .filter(|(_, t)| t.in_flight == 0 && t.next_poll <= now)
                    .map(|(id, _)| *id)
                    .take(per_tick)
                    .collect()
            },
        );
        for run in due {
            if cancel.is_cancelled() {
                return;
            }
            poll_one(&a, run).await;
        }
    }
}

async fn poll_one(a: &Adapter, run: u64) {
    add(&a.counters.polls);
    let url = format!(
        "{}/jobs/{run}",
        a.config.file.runner.url.trim_end_matches('/')
    );
    let res = a
        .http
        .get(url)
        .bearer_auth(&a.config.read_key)
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    let later = |a: &Adapter, after: Duration| {
        if let Ok(mut runs) = a.runs.lock()
            && let Some(t) = runs.get_mut(&run)
        {
            t.next_poll = Instant::now() + after;
        }
    };
    let Ok(res) = res else {
        add(&a.counters.poll_errors);
        return later(a, Duration::from_secs(2));
    };
    if res.status() == StatusCode::NOT_FOUND {
        // The runner does not know the run (expired, or another journal): the operation is
        // left unfinished — its outcome is unknown, never guessed.
        add(&a.counters.runs_unknown_to_runner);
        if let Ok(mut runs) = a.runs.lock() {
            runs.remove(&run);
        }
        return;
    }
    let answer = match res.error_for_status() {
        Ok(r) => match read_bounded(r, 64 * 1024).await {
            Ok(Some(b)) => serde_json::from_slice::<RunAnswer>(&b).ok(),
            _ => None,
        },
        Err(_) => None,
    };
    let Some(answer) = answer else {
        add(&a.counters.poll_errors);
        return later(a, Duration::from_secs(2));
    };
    let Some(state) = RunState::parse(&answer.state) else {
        add(&a.counters.poll_errors);
        return later(a, Duration::from_secs(2));
    };
    let finished = {
        let Ok(mut runs) = a.runs.lock() else { return };
        let Some(t) = runs.get(&run) else { return };
        if t.in_flight > 0 {
            return; // a call started meanwhile: ask again after it
        }
        let last = match state {
            RunState::Failed { step } => t.last_failure.get(&step).copied(),
            _ => None,
        };
        match run_finish(&state, &answer.state, last) {
            Some(f) => runs.remove(&run).map(|t| (t.op, f)),
            None => None,
        }
    };
    match finished {
        Some((op, f)) => {
            op.finish(f);
            add(&a.counters.runs_finished);
        }
        None => later(a, Duration::from_secs(1)),
    }
}

/// Serves the adapter until `cancel`, then lets calls in flight finish (bounded).
pub async fn serve(
    a: Arc<Adapter>,
    listener: tokio::net::TcpListener,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let app = router(a);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_headers_parse_only_when_complete_and_in_range() {
        let mut h = HeaderMap::new();
        assert_eq!(CallId::from_headers(&h), None);
        h.insert(RUN_ID_HEADER, HeaderValue::from_static("42"));
        h.insert(STEP_HEADER, HeaderValue::from_static("3"));
        h.insert(ATTEMPT_HEADER, HeaderValue::from_static("2"));
        assert_eq!(
            CallId::from_headers(&h),
            Some(CallId {
                run: 42,
                step: 3,
                attempt: 2
            })
        );
        h.insert(ATTEMPT_HEADER, HeaderValue::from_static("0"));
        assert_eq!(CallId::from_headers(&h), None, "attempts start at 1");
        h.insert(ATTEMPT_HEADER, HeaderValue::from_static("x"));
        assert_eq!(CallId::from_headers(&h), None);
    }

    #[test]
    fn query_values_are_percent_decoded() {
        assert_eq!(decode_component("article%2Fr-1%2F3"), "article/r-1/3");
        assert_eq!(decode_component("a+b%2"), "a b%2");
        assert_eq!(decode_component("%zz"), "%zz");
    }

    #[test]
    fn usage_is_read_only_in_its_common_shape() {
        assert_eq!(
            usage_of(br#"{"draft":"x","usage":{"input_tokens":3,"output_tokens":5}}"#),
            Some((3, 5))
        );
        assert_eq!(usage_of(br#"{"usage":{"input_tokens":"3"}}"#), None);
        assert_eq!(usage_of(b"not json"), None);
    }
}
