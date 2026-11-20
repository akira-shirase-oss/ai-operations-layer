//! Safe control (invariant 4, D-007): the control endpoints and the in-server executor
//! that drives their records.
//!
//! Telemetry is never read to authorize anything. The adapter asks the effect's own
//! authority; the decision and its audit row commit in one transaction; the external
//! call happens only after that commit and is idempotent under the business key; the
//! outcome is observed against the authority afterwards, never assumed (D-029..D-033).
//!
//! Bounds (D-033): a per-token control bucket, at most [`DRIVE_MAX`] external calls and
//! driver tasks at once, 2 s reads, 5 s submits, [`MAX_SUBMIT_TRIES`] submit tries per
//! record including re-drives, and a bounded post-submit observation — never assumed.
//! All of it runs on the store's control pool, so ingest and query floods cannot starve
//! a control action.

use std::time::Duration;

use aiops_domain::OperationId;
use aiops_domain::control::{ControlKind, ControlStatus, Resolution};
use aiops_store::{
    AuditEntry, ControlInsert, ControlPatch, ControlRecord, NewControlRecord, Project,
    ResumableControl,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::app::{AppState, Metrics, SharedState};
use crate::authority::read_bounded;
use crate::error::ApiError;
use crate::query::{Actor, SCOPE_READ, authenticate_with};

/// The scope control actions require. `admin` does not imply it: running an operation
/// and administering credentials are different duties.
pub const SCOPE_OPERATE: &str = "operate";

/// Per-token control budget (D-033): controls are rare, deliberate acts.
pub const CONTROL_RATE: u32 = 5;
pub const CONTROL_BURST: u32 = 10;
/// Driver tasks (and so external calls) at most at once — the D-033 semaphore.
pub const DRIVE_MAX: usize = 16;
/// A read of an effect's authority takes at most this (D-033).
const READ_DEADLINE: Duration = Duration::from_secs(2);
/// A submit to a host takes at most this (D-033).
const SUBMIT_DEADLINE: Duration = Duration::from_secs(5);
/// A submitted body's own bound, separate from the submit's: the status line
/// already carries the verdict, so a body still in flight must not turn a 2xx
/// Transient and re-drive a fresh `op_key` — that is a second job at the
/// runner (white-hat N5). The body is still drained (connection reuse, and a
/// host may return a reference in it).
const BODY_DRAIN: Duration = Duration::from_secs(2);
/// Submit tries one record may make, re-drives included; then `submit_failed`.
const MAX_SUBMIT_TRIES: i32 = 3;
/// Post-submit observation: bounded polling, then `observed_unknown` — honest.
const OBSERVE_TRIES: u32 = 4;
const OBSERVE_GAP: Duration = Duration::from_millis(500);
/// Records one executor pass drives at most.
const DRIVE_BATCH: i64 = 64;
/// `expected_state` on the wire at most (D-032).
const EXPECTED_STATE_MAX: usize = 4 * 1024;
const BUSINESS_KEY_MAX: usize = 256;
/// An authority answer's body at most — the host's status is the answer; the body is
/// its detail. The deadline wraps send AND read together, so a host that answers but
/// trickles its body cannot hold a control-gate slot open (white-hat S2-3).
const AUTHORITY_BODY_MAX: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Adapters: the hosts that own execution for a project (D-007).
// ---------------------------------------------------------------------------

/// A configured control adapter. v1 wires the reference host; the Durable Runner
/// adapter lands with its own decision's implementation (D-031).
#[derive(Clone)]
pub enum ControlAdapter {
    ReferenceHost(ReferenceHost),
    DurableRunner(DurableRunner),
}

/// The reference host (`fixtures/reference-host`): reconcile is
/// `GET /publications/{key}`, a retry resubmits the stored request under the same
/// business key via `POST /articles/retry` (D-030).
#[derive(Clone)]
pub struct ReferenceHost {
    base: String,
}

impl ReferenceHost {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
        }
    }
}

/// Durable Runner (D-031): a retry is a NEW job under a NEW `op_key` — an `op_key`
/// returns its first run, so it cannot be reused. The business key travels in the job
/// input; the control record's `submitted_ref` is the new `op_key`, the
/// `op_key` ↔ `business_key` mapping. Reconcile reads the EFFECT's authority
/// (`GET {effect}/publications/{key}` — the handler's own record), never only the
/// runner's run state.
#[derive(Clone)]
pub struct DurableRunner {
    /// The runner's API base (`POST {runner}/jobs` submits a run).
    runner_base: String,
    /// The article worker's base — the effect's authority lives there.
    effect_base: String,
    /// The runner's write-scope API key. Never logged, never in a snapshot.
    submit_key: String,
}

impl DurableRunner {
    pub fn new(runner_base: &str, effect_base: &str, submit_key: &str) -> Self {
        Self {
            runner_base: runner_base.trim_end_matches('/').to_owned(),
            effect_base: effect_base.trim_end_matches('/').to_owned(),
            submit_key: submit_key.to_owned(),
        }
    }
}

/// The pilot workflow's step names, in order — the resubmitted job's spec mirrors the
/// seeded spec (same names, same handler names).
const DR_STEPS: [&str; 4] = ["generate", "validate", "factcheck", "publish"];

/// What the effect's authority answered for one business key.
struct EffectRead {
    /// `Some` = the authority answered yes/no; `None` = it could not answer within the
    /// read bound — the outcome stays uncertain rather than being guessed.
    present: Option<bool>,
    /// The stored snapshot: `{source, status, answer, read_at}`.
    snapshot: Value,
}

/// A submitted retry's own identity at the host (a new operation id; for the Durable
/// Runner its new `op_key` — the `op_key` ↔ `business_key` mapping).
struct SubmitRef(String);

/// How a submit failed. `Transient` may be re-driven within the attempts budget;
/// `Rejected` is the host's own final answer and ends the record as `submit_failed`.
enum SubmitError {
    Transient(&'static str),
    Rejected(u16),
}

impl ControlAdapter {
    /// Adapter-side request validation (D-032): business-key format and the
    /// `expected_state` schema — unknown fields are refused.
    fn check_request(
        &self,
        business_key: &str,
        expected_state: &Value,
    ) -> Result<(), &'static str> {
        match self {
            Self::ReferenceHost(_) => ReferenceHost::check_request(business_key, expected_state),
            Self::DurableRunner(_) => DurableRunner::check_request(business_key, expected_state),
        }
    }

    /// Reads the effect's authority for `key`, bounded by [`READ_DEADLINE`]. Always
    /// answers — `present: None` is the honest "it could not say".
    async fn read_effect(&self, app: &AppState, key: &str) -> EffectRead {
        match self {
            Self::ReferenceHost(h) => h.read_effect(app, key).await,
            Self::DurableRunner(d) => d.read_effect(app, key).await,
        }
    }

    /// Whether the caller's `expected_state` assertion matches the authority's answer.
    fn assertion_matches(&self, expected_state: &Value, read: &EffectRead) -> bool {
        match self {
            Self::ReferenceHost(_) => ReferenceHost::assertion_matches(expected_state, read),
            Self::DurableRunner(_) => DurableRunner::assertion_matches(expected_state, read),
        }
    }

    /// Resubmits the work under `key` — idempotent at the host's own authority (D-030),
    /// bounded by [`SUBMIT_DEADLINE`].
    async fn submit(&self, app: &AppState, key: &str) -> Result<SubmitRef, SubmitError> {
        match self {
            Self::ReferenceHost(h) => h.submit(app, key).await,
            Self::DurableRunner(d) => d.submit(app, key).await,
        }
    }
}

/// The id half of a business key: ONE URL segment — `[A-Za-z0-9._-]` and never the
/// `.`/`..` segment itself — because `check_request` is the only thing between the
/// caller's text and `format!("{}/publications/{key}")`. A `/`, `?`, `#` or `..`
/// would leave the publications path or read a different literal key's answer
/// (white-hat S2-2).
fn business_key_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl ReferenceHost {
    fn check_request(key: &str, expected: &Value) -> Result<(), &'static str> {
        if key.len() > BUSINESS_KEY_MAX
            || !key.strip_prefix("article:").is_some_and(business_key_id)
        {
            return Err(
                "business_key must be \"article:<id>\" — <id> a single [A-Za-z0-9._-] segment (256 bytes at most)",
            );
        }
        match expected.as_object().map(|m| (m.len(), m.get("published"))) {
            Some((1, Some(Value::Bool(_)))) => Ok(()),
            _ => Err("expected_state must be exactly {\"published\": bool}"),
        }
    }

    /// `GET /publications/{key}`: 200 is "published", 404 is "not published", anything
    /// else is the authority not answering (`present: None`).
    async fn read_effect(&self, app: &AppState, key: &str) -> EffectRead {
        let url = format!("{}/publications/{key}", self.base);
        let result = async {
            let _permit = app.control_gate.acquire().await.ok()?;
            tokio::time::timeout(READ_DEADLINE, async {
                let res = app.authority_client.get(&url).send().await.ok()?;
                let status = res.status().as_u16();
                let body = read_bounded(res, AUTHORITY_BODY_MAX).await;
                Some((status, body))
            })
            .await
            .ok()?
        }
        .await;
        let (status, body) = match result {
            Some((s, Some(bytes))) => (
                s,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            ),
            Some((s, None)) => (s, Value::Null),
            None => (0, Value::Null),
        };
        let present = match status {
            200 => Some(true),
            404 => Some(false),
            _ => None,
        };
        EffectRead {
            present,
            snapshot: json!({
                "source": "publications",
                "status": status,
                "answer": body,
                "read_at": app.now().to_rfc3339(),
            }),
        }
    }

    fn assertion_matches(expected: &Value, read: &EffectRead) -> bool {
        expected
            .get("published")
            .and_then(Value::as_bool)
            .is_some_and(|claimed| read.present == Some(claimed))
    }

    /// `POST /articles/retry {"business_key": key}`: the host reruns the stored request;
    /// its publish dedups under the key, so a repeated submit cannot double the effect.
    async fn submit(&self, app: &AppState, key: &str) -> Result<SubmitRef, SubmitError> {
        let url = format!("{}/articles/retry", self.base);
        let result = async {
            let _permit = app
                .control_gate
                .acquire()
                .await
                .map_err(|_| SubmitError::Transient("the control gate closed"))?;
            let res = match tokio::time::timeout(
                SUBMIT_DEADLINE,
                app.authority_client
                    .post(&url)
                    .json(&json!({"business_key": key}))
                    .send(),
            )
            .await
            {
                Ok(Ok(res)) => res,
                Ok(Err(_)) => {
                    return Err(SubmitError::Transient(
                        "the submit could not reach the host",
                    ));
                }
                Err(_) => {
                    return Err(SubmitError::Transient("the submit deadline passed"));
                }
            };
            let status = res.status();
            let body = tokio::time::timeout(BODY_DRAIN, read_bounded(res, AUTHORITY_BODY_MAX))
                .await
                .ok()
                .flatten();
            let body = body
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .unwrap_or(Value::Null);
            Ok::<_, SubmitError>((status, body))
        }
        .await;
        match result {
            Ok((status, body)) if status.is_success() => {
                let reference = body
                    .get("operation_id")
                    .and_then(Value::as_str)
                    .or_else(|| body.get("request_id").and_then(Value::as_str))
                    .map_or_else(|| key.to_owned(), str::to_owned);
                Ok(SubmitRef(reference))
            }
            Ok((status, _)) if status.is_client_error() => {
                Err(SubmitError::Rejected(status.as_u16()))
            }
            Ok((status, _)) => Err(SubmitError::Transient(if status.is_server_error() {
                "the host answered 5xx"
            } else {
                "the host's answer was unexpected"
            })),
            Err(e) => Err(e),
        }
    }
}

impl DurableRunner {
    /// Business keys carry the runner's `op_key` convention (`article/<id>`); the
    /// effect assertion is the same publication shape as the reference host.
    fn check_request(key: &str, expected: &Value) -> Result<(), &'static str> {
        if key.len() > BUSINESS_KEY_MAX
            || !key.strip_prefix("article/").is_some_and(business_key_id)
        {
            return Err(
                "business_key must be \"article/<id>\" — <id> a single [A-Za-z0-9._-] segment (256 bytes at most)",
            );
        }
        match expected.as_object().map(|m| (m.len(), m.get("published"))) {
            Some((1, Some(Value::Bool(_)))) => Ok(()),
            _ => Err("expected_state must be exactly {\"published\": bool}"),
        }
    }

    /// `GET {effect}/publications/{key}` — the handler's own record of the effect,
    /// never the runner's run state (D-031). 200 is published, 404 is absent,
    /// anything else is the authority not answering.
    async fn read_effect(&self, app: &AppState, key: &str) -> EffectRead {
        let url = format!("{}/publications/{key}", self.effect_base);
        let result = async {
            let _permit = app.control_gate.acquire().await.ok()?;
            tokio::time::timeout(READ_DEADLINE, async {
                let res = app.authority_client.get(&url).send().await.ok()?;
                let status = res.status().as_u16();
                let body = read_bounded(res, AUTHORITY_BODY_MAX).await;
                Some((status, body))
            })
            .await
            .ok()?
        }
        .await;
        let (status, body) = match result {
            Some((s, Some(bytes))) => (
                s,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            ),
            Some((s, None)) => (s, Value::Null),
            None => (0, Value::Null),
        };
        let present = match status {
            200 => Some(true),
            404 => Some(false),
            _ => None,
        };
        EffectRead {
            present,
            snapshot: json!({
                "source": "publications",
                "status": status,
                "answer": body,
                "read_at": app.now().to_rfc3339(),
            }),
        }
    }

    fn assertion_matches(expected: &Value, read: &EffectRead) -> bool {
        expected
            .get("published")
            .and_then(Value::as_bool)
            .is_some_and(|claimed| read.present == Some(claimed))
    }

    /// `POST {runner}/jobs` with a NEW `op_key` and the business key in the input
    /// (D-031). The new `op_key` is the submitted reference — the record's
    /// `business_key` ↔ `submitted_ref` pair is the key mapping the decision keeps.
    async fn submit(&self, app: &AppState, key: &str) -> Result<SubmitRef, SubmitError> {
        let new_op_key = format!("{key}-r{}", &Uuid::new_v4().simple().to_string()[..8]);
        let steps: Vec<Value> = DR_STEPS
            .iter()
            .map(|h| json!({"name": h, "handler": h}))
            .collect();
        let input_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            serde_json::to_vec(&json!({"business_key": key})).unwrap_or_default(),
        );
        let job = json!({
            "op_key": new_op_key,
            "spec": {"version": 1, "steps": steps, "input_b64": input_b64},
        });
        let url = format!("{}/jobs", self.runner_base);
        let result = async {
            let _permit = app
                .control_gate
                .acquire()
                .await
                .map_err(|_| SubmitError::Transient("the control gate closed"))?;
            let res = match tokio::time::timeout(
                SUBMIT_DEADLINE,
                app.authority_client
                    .post(&url)
                    .bearer_auth(&self.submit_key)
                    .json(&job)
                    .send(),
            )
            .await
            {
                Ok(Ok(res)) => res,
                Ok(Err(_)) => {
                    return Err(SubmitError::Transient(
                        "the submit could not reach the runner",
                    ));
                }
                Err(_) => {
                    return Err(SubmitError::Transient("the submit deadline passed"));
                }
            };
            let status = res.status();
            let body = tokio::time::timeout(BODY_DRAIN, read_bounded(res, AUTHORITY_BODY_MAX))
                .await
                .ok()
                .flatten();
            let body = body
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .unwrap_or(Value::Null);
            Ok::<_, SubmitError>((status, body))
        }
        .await;
        match result {
            Ok((status, _)) if status.is_success() => Ok(SubmitRef(new_op_key)),
            Ok((status, _)) if status.is_client_error() => {
                Err(SubmitError::Rejected(status.as_u16()))
            }
            Ok((status, _)) => Err(SubmitError::Transient(if status.is_server_error() {
                "the runner answered 5xx"
            } else {
                "the runner's answer was unexpected"
            })),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// The wire view of a record.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    business_key: String,
    expected_state: Value,
}

#[derive(Serialize)]
pub struct ControlView {
    control_id: String,
    action: &'static str,
    status: &'static str,
    business_key: String,
    actor: String,
    approved_by: Option<String>,
    approved_at: Option<String>,
    resolution: Option<&'static str>,
    authority_read: Option<Value>,
    submitted_ref: Option<String>,
    denied_reason: Option<String>,
    attempts: i32,
    created_at: String,
    updated_at: String,
    /// `true` on a repeated call that returned the existing record unchanged.
    replayed: bool,
}

fn view(r: &ControlRecord, replayed: bool) -> ControlView {
    ControlView {
        control_id: r.id.to_string(),
        action: ControlKind::parse(&r.action).map_or("unknown", |k| k.as_str()),
        status: ControlStatus::parse(&r.status).map_or("unknown", |s| s.as_str()),
        business_key: r.business_key.clone(),
        actor: r.actor.clone(),
        approved_by: r.approved_by.clone(),
        approved_at: r.approved_at.map(aiops_domain::Timestamp::to_rfc3339),
        resolution: r
            .resolution
            .as_deref()
            .and_then(Resolution::parse)
            .map(Resolution::as_str),
        authority_read: r.authority_read.clone(),
        submitted_ref: r.submitted_ref.clone(),
        denied_reason: r.deny_reason.clone(),
        attempts: r.attempts,
        created_at: r.created_at.to_rfc3339(),
        updated_at: r.updated_at.to_rfc3339(),
        replayed,
    }
}

// ---------------------------------------------------------------------------
// Auth: the control path's own authenticate + authorize, on the control pool so a
// saturated query pool cannot starve a control action (D-022, D-033).
// ---------------------------------------------------------------------------

/// The audit row for a denial; fails closed when the write cannot complete.
async fn audit_denial(
    app: &AppState,
    actor: &Actor,
    action: &str,
    project: Option<&str>,
    target: Option<&str>,
    reason: &'static str,
) -> Result<(), ApiError> {
    Metrics::add(&app.metrics.denied, 1);
    let entry = AuditEntry {
        actor: &actor.token.actor,
        token_id: Some(actor.token.id),
        action,
        project_slug: project,
        target,
        allowed: false,
        reason,
    };
    match tokio::time::timeout(app.config.audit_deadline, app.store.audit(&entry)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(ApiError::AuditUnavailable),
    }
}

/// Deny with a synchronous audit row — shared by the control and credential
/// surfaces (`pub(crate)` for `credentials.rs`).
pub(crate) async fn deny(
    app: &AppState,
    actor: &Actor,
    action: &str,
    project: Option<&str>,
    target: Option<&str>,
    reason: &'static str,
) -> ApiError {
    match audit_denial(app, actor, action, project, target, reason).await {
        Ok(()) => ApiError::Forbidden(reason),
        Err(e) => e,
    }
}

/// `authenticate` under the control bucket (not the query bucket), then `authorize`'s
/// project check on the control pool.
async fn authorize_control(
    app: &AppState,
    headers: &HeaderMap,
    action: &str,
    slug: &str,
    target: Option<&str>,
    scope: &'static str,
) -> Result<(Actor, Project), ApiError> {
    let actor = authenticate_with(app, headers, &app.control_buckets, false).await?;
    if !actor.token.scopes.iter().any(|s| s == scope) {
        return Err(deny(app, &actor, action, Some(slug), target, "scope_missing").await);
    }
    let project = if let Some(p) = app.project_cache(slug) {
        Some(p)
    } else {
        let found = app.store.control_project_by_slug(slug).await?;
        if let Some(p) = &found {
            app.remember_project(p);
        }
        found
    };
    match project {
        Some(p) if actor.token.project_ids.contains(&p.id) => Ok((actor, p)),
        _ => Err(deny(
            app,
            &actor,
            action,
            Some(slug),
            target,
            "project_not_in_scope",
        )
        .await),
    }
}

// ---------------------------------------------------------------------------
// Endpoints.
// ---------------------------------------------------------------------------

/// The shared front of `reconcile`/`retry`: authenticate + authorize under `operate`,
/// validate the request, check the operation exists, insert-or-replay the record.
/// `Ok(None)` is the replay/conflict path where the response is already decided.
/// Both variants boxed: `ControlRecord` is 360 bytes and `Fresh` carries even more —
/// the enum stays pointer-sized either way (`large_enum_variant`).
enum ControlFront {
    Fresh(Box<ControlFresh>),
    Replay(Box<ControlRecord>),
}

/// A first-call record plus everything its drive needs.
struct ControlFresh {
    actor: Actor,
    project: Project,
    adapter: ControlAdapter,
    record: ControlRecord,
}

async fn begin(
    app: &AppState,
    headers: &HeaderMap,
    slug: &str,
    operation_id: &str,
    kind: ControlKind,
    action: &'static str,
    body: ControlRequest,
) -> Result<ControlFront, ApiError> {
    let (actor, project) = authorize_control(
        app,
        headers,
        action,
        slug,
        Some(operation_id),
        SCOPE_OPERATE,
    )
    .await?;
    if body.business_key.is_empty() || body.business_key.len() > BUSINESS_KEY_MAX {
        return Err(ApiError::BadRequest("business_key must be 1-256 bytes"));
    }
    if serde_json::to_string(&body.expected_state).map_or(true, |s| s.len() > EXPECTED_STATE_MAX) {
        return Err(ApiError::BadRequest("expected_state exceeds 4 KiB"));
    }
    let adapter = match app.adapters.get(slug) {
        Some(a) => a.clone(),
        None => {
            return Err(deny(
                app,
                &actor,
                action,
                Some(slug),
                Some(operation_id),
                "no_adapter",
            )
            .await);
        }
    };
    if let Err(reason) = adapter.check_request(&body.business_key, &body.expected_state) {
        return Err(ApiError::BadRequest(reason));
    }
    let op = OperationId::parse(operation_id)
        .map_err(|_| ApiError::BadRequest("operation_id must be the 32-hex operation id"))?;
    let op_uuid = Uuid::from_bytes(*op.as_bytes());
    if !app
        .store
        .control_operation_exists(project.id, op_uuid)
        .await?
    {
        // A nonexistent operation and one outside the token's view get the same answer.
        return Err(deny(
            app,
            &actor,
            action,
            Some(slug),
            Some(operation_id),
            "unknown_operation",
        )
        .await);
    }
    let initial = match kind {
        ControlKind::Reconcile => "reconciling",
        ControlKind::Retry => "requested",
    };
    let insert = app
        .store
        .control_insert(&NewControlRecord {
            project_id: project.id,
            operation_id: op_uuid,
            action: kind.as_str(),
            business_key: &body.business_key,
            expected_state: &body.expected_state,
            actor: &actor.token.actor,
            token_id: actor.token.id,
            initial_status: initial,
        })
        .await?;
    match insert {
        ControlInsert::Created(record) => Ok(ControlFront::Fresh(Box::new(ControlFresh {
            actor,
            project,
            adapter,
            record,
        }))),
        ControlInsert::Existing(record) => {
            // The replay identity matched; the body must match too — a different
            // expected_state on the same key is an explicit conflict, audited.
            if record.expected_state == body.expected_state {
                Ok(ControlFront::Replay(Box::new(record)))
            } else {
                audit_denial(
                    app,
                    &actor,
                    action,
                    Some(slug),
                    Some(operation_id),
                    "conflicting_request",
                )
                .await?;
                Err(ApiError::ControlConflict)
            }
        }
    }
}

/// `POST /v1/projects/{project}/operations/{operation_id}/reconcile` — read the effect's
/// authority and store its answer; changes nothing outside (safe, single-actor, audited).
pub async fn reconcile(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id)): Path<(String, String)>,
    Json(body): Json<ControlRequest>,
) -> Result<Json<ControlView>, ApiError> {
    let front = begin(
        &app,
        &headers,
        &slug,
        &operation_id,
        ControlKind::Reconcile,
        "control.reconcile",
        body,
    )
    .await?;
    match front {
        ControlFront::Replay(record) => Ok(Json(view(&record, true))),
        ControlFront::Fresh(fresh) => {
            let ControlFresh {
                actor,
                project,
                adapter,
                record,
            } = *fresh;
            let read = adapter.read_effect(&app, &record.business_key).await;
            finish_reconcile(
                &app,
                &record,
                &project.slug,
                &actor.token.actor,
                actor.token.id,
                &read,
            )
            .await?;
            let stored = app
                .store
                .control_get(record.id)
                .await?
                .ok_or(ApiError::NotFound)?;
            Ok(Json(view(&stored, false)))
        }
    }
}

/// `POST /v1/projects/{project}/operations/{operation_id}/retry` — the whole operation
/// resubmitted under its business key after the authority's own checks (D-030, D-032;
/// `step_id` is reserved — no step-scoped retry in v1).
pub async fn retry(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id)): Path<(String, String)>,
    Json(body): Json<ControlRequest>,
) -> Result<Json<ControlView>, ApiError> {
    let front = begin(
        &app,
        &headers,
        &slug,
        &operation_id,
        ControlKind::Retry,
        "control.retry",
        body,
    )
    .await?;
    match front {
        ControlFront::Replay(record) => Ok(Json(view(&record, true))),
        ControlFront::Fresh(fresh) => {
            let ControlFresh {
                actor,
                project,
                adapter,
                record,
            } = *fresh;
            decide(
                &app,
                &record,
                &project.slug,
                &actor.token.actor,
                actor.token.id,
                &adapter,
            )
            .await?;
            app.control_wake.notify_one();
            let stored = app
                .store
                .control_get(record.id)
                .await?
                .ok_or(ApiError::NotFound)?;
            Ok(Json(view(&stored, false)))
        }
    }
}

#[derive(Deserialize)]
pub struct ControlsParams {
    limit: Option<i64>,
    before_time: Option<String>,
    before_id: Option<String>,
}

/// `GET …/controls` — one operation's control records (a `read` scope call), one
/// keyset page newest first. `next` is the `[before_time, before_id]` pair to pass
/// back; absent on the last page (the audit page's shape).
pub async fn controls(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id)): Path<(String, String)>,
    Query(params): Query<ControlsParams>,
) -> Result<Json<Value>, ApiError> {
    let (_actor, project) = authorize_control(
        &app,
        &headers,
        "controls.list",
        &slug,
        Some(&operation_id),
        SCOPE_READ,
    )
    .await?;
    let op = OperationId::parse(&operation_id)
        .map_err(|_| ApiError::BadRequest("operation_id must be the 32-hex operation id"))?;
    let limit = params.limit.unwrap_or(50).clamp(1, 100);
    let before = match (params.before_time.as_deref(), params.before_id.as_deref()) {
        (None, None) => None,
        (Some(t), Some(id)) => Some((
            aiops_domain::Timestamp::parse_rfc3339(t)
                .map_err(|_| ApiError::BadRequest("before_time"))?,
            Uuid::parse_str(id).map_err(|_| ApiError::BadRequest("before_id"))?,
        )),
        _ => {
            return Err(ApiError::BadRequest(
                "before_time and before_id go together",
            ));
        }
    };
    let records = app
        .store
        .control_list(project.id, Uuid::from_bytes(*op.as_bytes()), before, limit)
        .await?;
    let next = (records.len() == usize::try_from(limit).unwrap_or(0))
        .then(|| {
            records
                .last()
                .map(|r| (r.created_at.to_rfc3339(), r.id.to_string()))
        })
        .flatten();
    Ok(Json(json!({
        "controls": records.iter().map(|r| view(r, false)).collect::<Vec<_>>(),
        "next": next
    })))
}

/// `GET …/controls/{control_id}` — one record (a `read` scope call).
pub async fn control(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id, control_id)): Path<(String, String, String)>,
) -> Result<Json<ControlView>, ApiError> {
    let (_actor, project) = authorize_control(
        &app,
        &headers,
        "controls.get",
        &slug,
        Some(&operation_id),
        SCOPE_READ,
    )
    .await?;
    let op = OperationId::parse(&operation_id)
        .map_err(|_| ApiError::BadRequest("operation_id must be the 32-hex operation id"))?;
    let id = Uuid::parse_str(&control_id)
        .map_err(|_| ApiError::BadRequest("control_id must be a uuid"))?;
    match app.store.control_get(id).await? {
        Some(r)
            if r.project_id == project.id && r.operation_id == Uuid::from_bytes(*op.as_bytes()) =>
        {
            Ok(Json(view(&r, false)))
        }
        _ => Err(ApiError::NotFound),
    }
}

// ---------------------------------------------------------------------------
// The drives — shared by the endpoints and the executor.
// ---------------------------------------------------------------------------

/// A reconcile's finish: resolution + the authority's snapshot + the audit row, in one
/// transaction (the `reconciling → reconciled` transition). `actor`/`token_id` come from
/// the request at the endpoint and from the stored record in the executor — the audit
/// names whoever asked, not whoever happens to be driving.
async fn finish_reconcile(
    app: &AppState,
    record: &ControlRecord,
    slug: &str,
    actor_name: &str,
    token_id: Uuid,
    read: &EffectRead,
) -> Result<(), ApiError> {
    let resolution = match read.present {
        Some(true) => Resolution::EffectPresent,
        Some(false) => Resolution::EffectAbsent,
        None => Resolution::StillUncertain,
    };
    let id = record.id.to_string();
    let audit = AuditEntry {
        actor: actor_name,
        token_id: Some(token_id),
        action: "control.reconcile",
        project_slug: Some(slug),
        target: Some(&id),
        allowed: true,
        reason: resolution.as_str(),
    };
    let patch = ControlPatch {
        status: Some("reconciled"),
        resolution: Some(resolution.as_str()),
        authority_read: Some(&read.snapshot),
        ..ControlPatch::default()
    };
    // `false` means another driver moved the record first — the stored row stands as it
    // is; nothing here retries a transition it did not win.
    app.store
        .control_decide(record.id, Some("reconciling"), &patch, &audit)
        .await?;
    Ok(())
}

/// A retry's decision (the `requested → authorized|denied` transition): the authority's
/// answer, the caller's assertion and the effect check, committed with its audit row.
async fn decide(
    app: &AppState,
    record: &ControlRecord,
    slug: &str,
    actor_name: &str,
    token_id: Uuid,
    adapter: &ControlAdapter,
) -> Result<(), ApiError> {
    let read = adapter.read_effect(app, &record.business_key).await;
    let (allowed, reason) = match read.present {
        None => (false, "authority_unavailable"),
        Some(present) => {
            if present {
                // The effect already happened: the correct recovery is none —
                // this answer precedes the caller's assertion (an effect that
                // happened denies the retry whatever was expected).
                (false, "effect_confirmed")
            } else if !adapter.assertion_matches(&record.expected_state, &read) {
                (false, "expected_state_mismatch")
            } else {
                (true, "authorized")
            }
        }
    };
    let id = record.id.to_string();
    let audit = AuditEntry {
        actor: actor_name,
        token_id: Some(token_id),
        action: "control.retry",
        project_slug: Some(slug),
        target: Some(&id),
        allowed,
        reason,
    };
    let patch = ControlPatch {
        status: Some(if allowed { "authorized" } else { "denied" }),
        deny_reason: (!allowed).then_some(reason),
        approved_by: allowed.then_some(actor_name),
        approved_at: allowed.then(|| app.now()),
        authority_read: Some(&read.snapshot),
        ..ControlPatch::default()
    };
    app.store
        .control_decide(record.id, Some("requested"), &patch, &audit)
        .await?;
    Ok(())
}

/// The `authorized|submitted → submitted → confirmed|observed_unknown` drive: submit
/// under the business key (idempotent at the host, so a re-driven submit cannot double
/// the effect), then read the effect back within the observation bound.
async fn drive_submit(app: &AppState, work: &ResumableControl, adapter: &ControlAdapter) {
    let record = &work.record;
    let expect = if record.status == "submitted" {
        "submitted"
    } else {
        "authorized"
    };
    if record.attempts >= MAX_SUBMIT_TRIES {
        let patch = ControlPatch {
            status: Some("submit_failed"),
            deny_reason: Some("attempts_exhausted"),
            ..ControlPatch::default()
        };
        let _ = app
            .store
            .control_update(record.id, Some(expect), &patch)
            .await;
        return;
    }
    match adapter.submit(app, &record.business_key).await {
        Ok(SubmitRef(reference)) => {
            let snapshot = json!({
                "source": "submit",
                "answer": {"reference": reference},
                "read_at": app.now().to_rfc3339(),
            });
            let patch = ControlPatch {
                status: Some("submitted"),
                submitted_ref: Some(&reference),
                authority_read: Some(&snapshot),
                add_attempts: Some(1),
                ..ControlPatch::default()
            };
            if let Ok(true) = app
                .store
                .control_update(record.id, Some(expect), &patch)
                .await
            {
                observe(app, record.id, adapter, &record.business_key).await;
            }
            // `false` or an error: someone else moved the record — leave it; re-reads
            // are another pass.
        }
        Err(SubmitError::Rejected(status)) => {
            let snapshot = json!({
                "source": "submit",
                "status": status,
                "answer": "the host refused the resubmission",
                "read_at": app.now().to_rfc3339(),
            });
            let patch = ControlPatch {
                status: Some("submit_failed"),
                deny_reason: Some("submit_rejected"),
                authority_read: Some(&snapshot),
                add_attempts: Some(1),
                ..ControlPatch::default()
            };
            let _ = app
                .store
                .control_update(record.id, Some(expect), &patch)
                .await;
        }
        Err(SubmitError::Transient(kind)) => {
            tracing::warn!(kind, control_id = %record.id, "control submit failed transiently");
            let exhausted = record.attempts + 1 >= MAX_SUBMIT_TRIES;
            let snapshot = json!({
                "source": "submit",
                "answer": kind,
                "read_at": app.now().to_rfc3339(),
            });
            let patch = ControlPatch {
                status: exhausted.then_some("submit_failed"),
                deny_reason: exhausted.then_some("attempts_exhausted"),
                authority_read: Some(&snapshot),
                add_attempts: Some(1),
                ..ControlPatch::default()
            };
            let _ = app
                .store
                .control_update(record.id, Some(expect), &patch)
                .await;
        }
    }
}

/// After a submit: bounded polling of the effect authority, then `confirmed` or the
/// honest `observed_unknown`.
async fn observe(app: &AppState, id: Uuid, adapter: &ControlAdapter, key: &str) {
    let mut last = None;
    for _ in 0..OBSERVE_TRIES {
        let read = adapter.read_effect(app, key).await;
        if read.present == Some(true) {
            let patch = ControlPatch {
                status: Some("confirmed"),
                authority_read: Some(&read.snapshot),
                ..ControlPatch::default()
            };
            let _ = app
                .store
                .control_update(id, Some("submitted"), &patch)
                .await;
            return;
        }
        last = Some(read.snapshot);
        tokio::time::sleep(OBSERVE_GAP).await;
    }
    let patch = ControlPatch {
        status: Some("observed_unknown"),
        authority_read: last.as_ref(),
        ..ControlPatch::default()
    };
    let _ = app
        .store
        .control_update(id, Some("submitted"), &patch)
        .await;
}

/// One resumable record through the state machine. Everything a crash stranded is
/// re-driven exactly once through the same path it would have taken. The audit rows
/// name the record's own actor and token — never whoever happens to be driving.
async fn drive_one(app: &AppState, work: &ResumableControl) {
    let record = &work.record;
    let kind = ControlKind::parse(&record.action);
    let status = ControlStatus::parse(&record.status);
    let adapter = app.adapters.get(&work.project_slug).cloned();
    match (kind, status, adapter) {
        // A reconcile stranded before its read: drive it to `reconciled`.
        (
            Some(ControlKind::Reconcile),
            Some(ControlStatus::Requested | ControlStatus::Reconciling),
            Some(a),
        ) => {
            if record.status == "requested" {
                // `requested → reconciling` first, then the read's finish below.
                let patch = ControlPatch {
                    status: Some("reconciling"),
                    ..ControlPatch::default()
                };
                let _ = app
                    .store
                    .control_update(record.id, Some("requested"), &patch)
                    .await;
            }
            let read = a.read_effect(app, &record.business_key).await;
            let _ = finish_reconcile(
                app,
                record,
                &work.project_slug,
                &record.actor,
                record.token_id,
                &read,
            )
            .await;
        }
        // A retry stranded before its decision: decide, then submit in the same pass.
        (Some(ControlKind::Retry), Some(ControlStatus::Requested), Some(a)) => {
            if decide(
                app,
                record,
                &work.project_slug,
                &record.actor,
                record.token_id,
                &a,
            )
            .await
            .is_ok()
            {
                match app.store.control_get(record.id).await {
                    Ok(Some(fresh)) if fresh.status == ControlStatus::Authorized.as_str() => {
                        drive_submit(
                            app,
                            &ResumableControl {
                                record: fresh,
                                project_slug: work.project_slug.clone(),
                            },
                            &a,
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        }
        (
            Some(ControlKind::Retry),
            Some(ControlStatus::Authorized | ControlStatus::Submitted),
            Some(a),
        ) => {
            drive_submit(app, work, &a).await;
        }
        // A record whose adapter is no longer configured cannot act: deny it rather than
        // leave it resumable forever (fail closed).
        (Some(_), Some(s), None) if s.is_resumable() => {
            let id = record.id.to_string();
            let audit = AuditEntry {
                actor: &record.actor,
                token_id: Some(record.token_id),
                action: "control.deny",
                project_slug: Some(&work.project_slug),
                target: Some(&id),
                allowed: false,
                reason: "no_adapter",
            };
            let patch = ControlPatch {
                status: Some("denied"),
                deny_reason: Some("no_adapter"),
                ..ControlPatch::default()
            };
            let _ = app
                .store
                .control_decide(record.id, None, &patch, &audit)
                .await;
        }
        _ => {
            tracing::warn!(
                control_id = %record.id,
                status = %record.status,
                "a control record is in a state the executor does not drive; left alone"
            );
        }
    }
}

/// One pass over every resumable record, [`DRIVE_MAX`] driver tasks at most. The
/// executor calls it on wake and tick; tests call it directly for determinism.
pub async fn drive_resumable(app: &SharedState) -> usize {
    let records = match app.store.control_resumable(DRIVE_BATCH).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "the resumable scan failed; the next pass retries");
            return 0;
        }
    };
    let n = records.len();
    let mut set = JoinSet::new();
    for work in records {
        while set.len() >= DRIVE_MAX {
            let _ = set.join_next().await;
        }
        let app = app.clone();
        set.spawn(async move { drive_one(&app, &work).await });
    }
    while set.join_next().await.is_some() {}
    n
}

/// The control executor (D-033): in-server, on the tracked tasks, driven by
/// `control_wake` and a slow tick; the first pass is the boot re-drive of anything a
/// crash stranded mid-work.
pub fn spawn_executor(app: SharedState, tracker: &TaskTracker, cancel: CancellationToken) {
    tracker.spawn(async move {
        drive_resumable(&app).await;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = app.control_wake.notified() => {}
                _ = tick.tick() => {}
            }
            drive_resumable(&app).await;
        }
    });
}
