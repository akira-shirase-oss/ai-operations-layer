//! Controlled recovery (A3, D-030): plan, confirm, list.
//!
//! A person with the `operate` scope plans a recovery of a run the runner says failed or
//! was cancelled, naming the state they saw and the run's business key. The collector
//! reads the runner (the authority, never the trace), refuses unless both match, asks
//! every effect check (the publisher's own record, by business key) and stores the plan:
//! `planned` when every check says "not landed", `blocked` otherwise. A confirm by a
//! person re-reads the runner and re-asks the checks immediately before it submits the
//! same spec as a **new** run under `<key>#r<n>`. Every request is audited, allowed or
//! refused; a replayed plan or confirm returns the stored record and does nothing again.
//! Whether the recovery succeeded is the runner's answer, read when asked.

use std::fmt::Write as _;
use std::time::Duration;

use aiops_domain::control::{
    Effect, MAX_RECOVERIES, Refused, Status, derived_key, recoverable, root_key, verdict,
};
use aiops_domain::correlate::operation_for_run;
use aiops_domain::{OperationId, Timestamp};
use aiops_store::{AuditEntry, NewRecovery, Recovery};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{AppState, SharedState};
use crate::authority::{RunView, read_bounded, read_run, verify};
use crate::config::{AuthoritySource, ControlConfig};
use crate::error::ApiError;
use crate::query::{Actor, authenticate, authorize, authorize_scope};

pub const SCOPE_OPERATE: &str = "operate";
/// The largest spec a resubmit carries back (the runner's 64 KiB input, base64, plus steps).
const MAX_SPEC_BYTES: usize = 256 * 1024;
/// Submitted recoveries whose runs are read live per list (the newest).
const LIVE_READS: usize = 3;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    /// The client's key for this plan: a replay with the same key is the same plan.
    pub request_key: String,
    /// The runner state the person saw (for example `failed:step:3`).
    pub expected_state: String,
    /// The run's business key (its `op_key`) the person saw.
    pub business_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    /// The runner state the person saw when confirming.
    pub expected_state: String,
}

#[derive(Debug, Serialize)]
pub struct NewRunView {
    pub run_id: i64,
    /// The operation the adapter reports the new run under (derived, like every run's).
    pub operation_id: String,
    /// The runner's own answer for the new run, when it was read for this response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_at: Option<Timestamp>,
}

#[derive(Debug, Serialize)]
pub struct RecoveryView {
    pub id: String,
    pub status: String,
    pub planned_by: String,
    pub runner: String,
    pub run_id: i64,
    pub business_key: String,
    pub expected_state: String,
    pub observed_state: String,
    pub effect: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_detail: Option<String>,
    pub recovery_number: i32,
    pub derived_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_run: Option<NewRunView>,
    pub created_at: Timestamp,
}

fn view(r: Recovery) -> RecoveryView {
    let new_run = r.new_run_id.map(|id| NewRunView {
        run_id: id,
        operation_id: u64::try_from(id)
            .map(|n| operation_for_run(&r.runner, n).to_string())
            .unwrap_or_default(),
        runner_state: None,
        unavailable: None,
        read_at: None,
    });
    RecoveryView {
        id: r.id.to_string(),
        status: r.status,
        planned_by: r.planned_by,
        runner: r.runner,
        run_id: r.run_id,
        business_key: r.business_key,
        expected_state: r.expected_state,
        observed_state: r.observed_state,
        effect: r.effect,
        effect_detail: r.effect_detail,
        recovery_number: r.recovery_number,
        derived_key: r.derived_key,
        approved_by: r.approved_by,
        approved_at: r.approved_at,
        new_run,
        created_at: r.created_at,
    }
}

#[derive(Debug, Serialize)]
pub struct RecoveryList {
    /// Whether this collector can recover runs at all (control configured).
    pub configured: bool,
    pub recoveries: Vec<RecoveryView>,
}

/// One audit row, allowed or refused; the request fails closed when it cannot be written.
async fn audit(
    app: &AppState,
    actor: &Actor,
    action: &str,
    slug: &str,
    target: &str,
    allowed: bool,
    reason: &str,
) -> Result<(), ApiError> {
    let entry = AuditEntry {
        actor: &actor.token.actor,
        token_id: Some(actor.token.id),
        action,
        project_slug: Some(slug),
        target: Some(target),
        allowed,
        reason,
    };
    match tokio::time::timeout(app.config.audit_deadline, app.store.audit(&entry)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(ApiError::AuditUnavailable),
    }
}

/// Audits a refusal and returns it.
async fn refuse(
    app: &AppState,
    actor: &Actor,
    action: &str,
    slug: &str,
    target: &str,
    err: ApiError,
) -> ApiError {
    let reason = err.code();
    match audit(app, actor, action, slug, target, false, reason).await {
        Ok(()) => err,
        Err(e) => e,
    }
}

fn conflict(r: Refused) -> ApiError {
    ApiError::Conflict {
        code: r.code(),
        message: r.message(),
    }
}

fn configured(app: &AppState) -> Option<(AuthoritySource, ControlConfig)> {
    Some((app.config.authority.clone()?, app.config.control.clone()?))
}

fn key_ok(k: &str) -> bool {
    (8..=64).contains(&k.len())
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Percent-encodes a business key for a URL path segment.
fn encode(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Asks every effect check about `key`: 200 = landed, 404 = not landed, else unknown.
async fn check_effects(app: &AppState, control: &ControlConfig, key: &str) -> Vec<Effect> {
    let mut out = Vec::with_capacity(control.effect_checks.len());
    for template in &control.effect_checks {
        let url = template.replace("{key}", &encode(key));
        let res = app
            .authority_client
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        out.push(match res.map(|r| r.status().as_u16()) {
            Ok(200) => Effect::Landed,
            Ok(404) => Effect::NotLanded,
            Ok(s) => Effect::Unknown(format!("an effect check answered {s}")),
            Err(e) if e.is_timeout() => Effect::Unknown("an effect check timed out".into()),
            Err(_) => Effect::Unknown("an effect check could not be reached".into()),
        });
    }
    out
}

fn effect_detail(e: &Effect, key: &str) -> String {
    match e {
        Effect::NotLanded => format!("no effect check has a record of {key}"),
        Effect::Landed => format!("an effect check has a record of {key}: it already happened"),
        Effect::Unknown(why) => format!("{why}: whether {key}'s effect happened is unknown"),
    }
}

/// The runner's answer for the operation's run, verified as the operation's own.
async fn authority_of(
    app: &AppState,
    source: &AuthoritySource,
    project: Uuid,
    op: OperationId,
) -> Result<(u64, RunView), ApiError> {
    let links = app.store.operation_links(project, op).await?;
    let run = verify(source, op, &links).map_err(|_| ApiError::Conflict {
        code: "not_a_runner_operation",
        message: "this operation is not linked to a run of the configured runner",
    })?;
    match read_run(app, source, run).await {
        (Ok(answer), _) => Ok((run, answer)),
        (Err(_), _) => Err(ApiError::Unavailable(
            "the runner could not be asked; nothing was planned or submitted",
        )),
    }
}

/// `POST /v1/projects/{project}/operations/{operation_id}/recoveries`
pub async fn plan(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id)): Path<(String, String)>,
    Json(req): Json<PlanRequest>,
) -> Result<(StatusCode, Json<RecoveryView>), ApiError> {
    const ACTION: &str = "recovery.plan";
    let actor = authenticate(&app, &headers).await?;
    let target = (operation_id.len() <= 32).then_some(operation_id.as_str());
    let project = authorize_scope(&app, &actor, SCOPE_OPERATE, ACTION, &slug, target).await?;
    let op = OperationId::parse(&operation_id).map_err(|_| ApiError::BadRequest("operation_id"))?;
    let target = op.to_string();
    let fail = |e| refuse(&app, &actor, ACTION, &slug, &target, e);
    if !key_ok(&req.request_key) {
        return Err(fail(ApiError::BadRequest("request_key (8-64 of [A-Za-z0-9_-])")).await);
    }
    if req.expected_state.len() > 64 || req.business_key.is_empty() || req.business_key.len() > 256
    {
        return Err(fail(ApiError::BadRequest("expected_state or business_key")).await);
    }
    // A replay is the plan already made: nothing is asked or written again.
    if let Some(r) = app
        .store
        .recovery_by_request(project.id, &req.request_key)
        .await?
    {
        if r.operation_id != op {
            return Err(fail(ApiError::Conflict {
                code: "request_key_reused",
                message: "this request key was used for another operation",
            })
            .await);
        }
        audit(&app, &actor, ACTION, &slug, &target, true, "replayed").await?;
        return Ok((StatusCode::OK, Json(view(r))));
    }
    let Some((source, control)) = configured(&app) else {
        return Err(fail(ApiError::Conflict {
            code: "control_not_configured",
            message: "this collector is not configured to recover runs",
        })
        .await);
    };
    let (run, answer) = match authority_of(&app, &source, project.id, op).await {
        Ok(x) => x,
        Err(e) => return Err(fail(e).await),
    };
    if answer.state != req.expected_state {
        return Err(fail(conflict(Refused::StateChanged)).await);
    }
    if answer.key != req.business_key {
        return Err(fail(conflict(Refused::KeyMismatch)).await);
    }
    if let Err(r) = recoverable(&answer.state) {
        return Err(fail(conflict(r)).await);
    }
    let root = root_key(&answer.key).to_owned();
    let done = app
        .store
        .recovery_count(project.id, &source.label, &root)
        .await?;
    let n = u32::try_from(done + 1)
        .unwrap_or(u32::MAX)
        .min(MAX_RECOVERIES + 1);
    let derived = match derived_key(&root, n) {
        Ok(k) => k,
        Err(r) => return Err(fail(conflict(r)).await),
    };
    let effect = verdict(&check_effects(&app, &control, &root).await);
    let status = Status::of_plan(&effect);
    let detail = effect_detail(&effect, &root);
    let new = NewRecovery {
        project_id: project.id,
        operation_id: op,
        request_key: &req.request_key,
        planned_by: &actor.token.actor,
        runner: &source.label,
        run_id: i64::try_from(run).unwrap_or(i64::MAX),
        business_key: &root,
        expected_state: &req.expected_state,
        observed_state: &answer.state,
        effect: effect.code(),
        effect_detail: Some(&detail),
        recovery_number: i32::try_from(n).unwrap_or(i32::MAX),
        derived_key: &derived,
        status: status.as_str(),
    };
    let Some(stored) = app.store.insert_recovery(&new).await? else {
        // Raced: the same request key (a concurrent replay) or the same derived key.
        if let Some(r) = app
            .store
            .recovery_by_request(project.id, &req.request_key)
            .await?
        {
            audit(&app, &actor, ACTION, &slug, &target, true, "replayed").await?;
            return Ok((StatusCode::OK, Json(view(r))));
        }
        return Err(fail(ApiError::Conflict {
            code: "planned_concurrently",
            message: "another recovery of this business key was planned at the same time: reload",
        })
        .await);
    };
    let reason = format!("{} {} effect {}", status.as_str(), derived, effect.code());
    audit(&app, &actor, ACTION, &slug, &target, true, &reason).await?;
    Ok((StatusCode::CREATED, Json(view(stored))))
}

#[derive(Debug, Deserialize)]
struct SubmitAnswer {
    run_id: u64,
}

/// Reads the run's spec with the API key, then submits it under `key`. Returns the new
/// run, or `None` with why when nothing is known to have been submitted.
async fn resubmit(
    app: &AppState,
    source: &AuthoritySource,
    control: &ControlConfig,
    run: u64,
    key: &str,
) -> Result<u64, &'static str> {
    let res = app
        .authority_client
        .get(format!("{}/jobs/{run}/spec", source.url))
        .bearer_auth(&control.api_key)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|_| "the runner's spec could not be read")?;
    if res.status() != StatusCode::OK {
        return Err("the runner did not return the run's spec");
    }
    let body = tokio::time::timeout(Duration::from_secs(2), read_bounded(res, MAX_SPEC_BYTES))
        .await
        .ok()
        .flatten()
        .ok_or("the runner's spec could not be read")?;
    let spec: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "the runner's spec could not be read")?;
    let spec = spec
        .get("spec")
        .cloned()
        .ok_or("the runner's spec could not be read")?;
    // The spec's input is the host's content: it passes through and is never stored or
    // logged here.
    let res = app
        .authority_client
        .post(format!("{}/jobs", source.url))
        .bearer_auth(&control.api_key)
        .header("content-type", "application/json")
        .body(serde_json::json!({"op_key": key, "spec": spec}).to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|_| "the submit's answer was lost; confirming again resubmits under the same key, which returns the first run")?;
    if res.status() != StatusCode::OK {
        return Err("the runner refused the submit");
    }
    let body = tokio::time::timeout(Duration::from_secs(2), read_bounded(res, 64 * 1024))
        .await
        .ok()
        .flatten()
        .ok_or("the submit's answer could not be read")?;
    serde_json::from_slice::<SubmitAnswer>(&body)
        .map(|a| a.run_id)
        .map_err(|_| "the submit's answer could not be read")
}

/// `POST /v1/projects/{project}/recoveries/{id}/confirm`
#[expect(
    clippy::too_many_lines,
    reason = "one linear, audited sequence: load, claim, re-read the runner, re-check, submit"
)]
pub async fn confirm(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, id)): Path<(String, String)>,
    Json(req): Json<ConfirmRequest>,
) -> Result<Json<RecoveryView>, ApiError> {
    const ACTION: &str = "recovery.confirm";
    let actor = authenticate(&app, &headers).await?;
    let target = (id.len() <= 36).then_some(id.as_str());
    let project = authorize_scope(&app, &actor, SCOPE_OPERATE, ACTION, &slug, target).await?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::BadRequest("recovery id"))?;
    let target = id.to_string();
    let fail = |e| refuse(&app, &actor, ACTION, &slug, &target, e);
    let Some(rec) = app.store.recovery(project.id, id).await? else {
        return Err(fail(ApiError::NotFound).await);
    };
    match Status::parse(&rec.status) {
        // A replayed confirm: the run was submitted once; nothing happens again.
        Some(Status::Submitted) => {
            audit(&app, &actor, ACTION, &slug, &target, true, "replayed").await?;
            return Ok(Json(view(rec)));
        }
        Some(Status::Blocked) => {
            return Err(fail(ApiError::Conflict {
                code: "blocked",
                message: "the effect checks did not permit a resubmit; nothing will be submitted",
            })
            .await);
        }
        Some(Status::Submitting) => {
            return Err(fail(ApiError::Conflict {
                code: "being_submitted",
                message: "another confirm is submitting this recovery: reload",
            })
            .await);
        }
        Some(Status::Planned) => {}
        None => {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "unknown recovery status"
            )));
        }
    }
    if req.expected_state != rec.observed_state {
        return Err(fail(conflict(Refused::StateChanged)).await);
    }
    let Some((source, control)) = configured(&app).filter(|(s, _)| s.label == rec.runner) else {
        return Err(fail(ApiError::Conflict {
            code: "control_not_configured",
            message: "this collector is not configured to recover this runner's runs",
        })
        .await);
    };
    // One confirm takes the plan; the person who confirmed is the approval.
    let Some(rec) = app
        .store
        .claim_recovery(project.id, id, &actor.token.actor)
        .await?
    else {
        return Err(fail(ApiError::Conflict {
            code: "being_submitted",
            message: "another confirm took this recovery first: reload",
        })
        .await);
    };
    let release = |status: &'static str, effect: Option<(String, String)>| {
        let app = app.clone();
        async move {
            let effect = effect.as_ref().map(|(c, d)| (c.as_str(), Some(d.as_str())));
            app.store
                .settle_recovery(project.id, id, status, None, effect)
                .await
        }
    };
    let run = u64::try_from(rec.run_id).unwrap_or_default();
    // Reconcile immediately before the rerun: the runner's state, then every effect.
    let state_now = match authority_of(&app, &source, project.id, rec.operation_id).await {
        Ok((r, answer)) if r == run => answer.state,
        Ok(_) | Err(_) => {
            release("planned", None).await?;
            return Err(fail(ApiError::Unavailable(
                "the runner could not be asked; nothing was submitted",
            ))
            .await);
        }
    };
    if state_now != rec.observed_state {
        release("planned", None).await?;
        return Err(fail(conflict(Refused::StateChanged)).await);
    }
    let effect = verdict(&check_effects(&app, &control, &rec.business_key).await);
    if effect != Effect::NotLanded {
        let detail = effect_detail(&effect, &rec.business_key);
        let blocked = release("blocked", Some((effect.code().to_owned(), detail))).await?;
        audit(
            &app,
            &actor,
            ACTION,
            &slug,
            &target,
            false,
            "blocked_at_confirm",
        )
        .await?;
        return match blocked {
            Some(_) => Err(ApiError::Conflict {
                code: "blocked",
                message: "an effect check now has a record of the effect, or cannot answer: nothing was submitted",
            }),
            None => Err(ApiError::Internal(anyhow::anyhow!(
                "recovery left submitting"
            ))),
        };
    }
    match resubmit(&app, &source, &control, run, &rec.derived_key).await {
        Ok(new_run) => {
            let stored = app
                .store
                .settle_recovery(
                    project.id,
                    id,
                    Status::Submitted.as_str(),
                    Some(i64::try_from(new_run).unwrap_or(i64::MAX)),
                    None,
                )
                .await?
                .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("recovery left submitting")))?;
            let reason = format!("submitted {} as run {new_run}", rec.derived_key);
            audit(&app, &actor, ACTION, &slug, &target, true, &reason).await?;
            Ok(Json(view(stored)))
        }
        Err(why) => {
            // Nothing is known to be submitted. A later confirm resubmits under the same
            // derived key, which the runner answers with the first run if one exists.
            release("planned", None).await?;
            Err(fail(ApiError::Unavailable(why)).await)
        }
    }
}

/// `GET /v1/projects/{project}/operations/{operation_id}/recoveries`: the plans, and for
/// the newest submitted ones the runner's own answer for the new run, read now.
pub async fn list(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id)): Path<(String, String)>,
) -> Result<Json<RecoveryList>, ApiError> {
    let actor = authenticate(&app, &headers).await?;
    let target = (operation_id.len() <= 32).then_some(operation_id.as_str());
    let project = authorize(&app, &actor, "recovery.list", &slug, target).await?;
    let op = OperationId::parse(&operation_id).map_err(|_| ApiError::BadRequest("operation_id"))?;
    let records = app.store.recoveries(project.id, op).await?;
    let mut views: Vec<RecoveryView> = records.into_iter().map(view).collect();
    if let Some(source) = app.config.authority.clone() {
        let mut live = 0;
        for v in views.iter_mut().rev() {
            if live == LIVE_READS {
                break;
            }
            let Some(new_run) = v.new_run.as_mut() else {
                continue;
            };
            if v.runner != source.label {
                continue;
            }
            live += 1;
            let run = u64::try_from(new_run.run_id).unwrap_or_default();
            let (answer, read_at) = read_run(&app, &source, run).await;
            new_run.read_at = Some(read_at);
            match answer {
                Ok(a) => new_run.runner_state = Some(a.state),
                Err(why) => new_run.unavailable = Some(why),
            }
        }
    }
    Ok(Json(RecoveryList {
        configured: configured(&app).is_some(),
        recoveries: views,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_paths_are_bounded_and_encoded() {
        assert!(key_ok("0f2c1d9e-aaaa"));
        assert!(!key_ok("short"));
        assert!(!key_ok("has space in it"));
        assert_eq!(encode("article/x#r1"), "article%2Fx%23r1");
        assert_eq!(root_key("article/x#r1"), "article/x");
    }
}
