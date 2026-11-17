//! The query path: project-scoped, token-authenticated, rate-limited per token, bounded
//! page and timeline sizes. A valid token asking outside its scope is denied with 403 and
//! audited synchronously; if the audit row cannot be written the request fails closed.

use aiops_domain::timeline::{RetentionContext, Status, Timeline, assemble, list_state};
use aiops_domain::{FailureClass, OperationId, Outcome, Timestamp, UncertainReason};
use aiops_store::{AuditEntry, ListFilter, OperationRow, Project, Token};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::app::{AppState, Metrics, MetricsView, SharedState};
use crate::auth::{TOKEN_PREFIX, bearer, hash};
use crate::error::ApiError;

pub const SCOPE_READ: &str = "read";
/// Collector-wide operations (counters across all projects).
pub const SCOPE_ADMIN: &str = "admin";

/// An authenticated actor.
pub struct Actor {
    pub token: Token,
}

pub async fn authenticate(app: &AppState, headers: &HeaderMap) -> Result<Actor, ApiError> {
    let Some(secret) = bearer(headers, TOKEN_PREFIX) else {
        Metrics::add(&app.metrics.unauthenticated, 1);
        return Err(ApiError::Unauthenticated);
    };
    let digest = hash(secret);
    let found = app
        .resolve(&app.tokens, digest, || app.store.find_token(&digest))
        .await?;
    let now = app.now();
    match found {
        Some(token) if !token.revoked && token.expires_at > now => {
            if let Err(retry_after_secs) = app.query_buckets.take(token.id, 1, now.unix_micros()) {
                Metrics::add(&app.metrics.rate_limited, 1);
                return Err(ApiError::RateLimited { retry_after_secs });
            }
            Ok(Actor { token })
        }
        _ => {
            Metrics::add(&app.metrics.unauthenticated, 1);
            Err(ApiError::Unauthenticated)
        }
    }
}

async fn deny(
    app: &AppState,
    actor: &Actor,
    action: &str,
    project: Option<&str>,
    target: Option<&str>,
    reason: &'static str,
) -> ApiError {
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
        Ok(Ok(())) => ApiError::Forbidden(reason),
        _ => ApiError::AuditUnavailable,
    }
}

/// Resolves `slug` to a project the actor may read, or denies and audits.
async fn authorize(
    app: &AppState,
    actor: &Actor,
    action: &str,
    slug: &str,
    target: Option<&str>,
) -> Result<Project, ApiError> {
    if !actor.token.scopes.iter().any(|s| s == SCOPE_READ) {
        return Err(deny(app, actor, action, Some(slug), target, "scope_missing").await);
    }
    let project = if let Some(p) = app.project_cache(slug) {
        Some(p)
    } else {
        let found = app.store.project_by_slug(slug).await?;
        if let Some(p) = &found {
            app.remember_project(p);
        }
        found
    };
    match project {
        Some(p) if actor.token.project_ids.contains(&p.id) => Ok(p),
        // An unknown project and a project outside the token are the same answer, so the
        // response does not reveal which projects exist.
        _ => Err(deny(
            app,
            actor,
            action,
            Some(slug),
            target,
            "project_not_in_scope",
        )
        .await),
    }
}

#[derive(Serialize)]
pub struct ProjectView {
    pub slug: String,
}

#[derive(Serialize)]
pub struct ProjectsView {
    pub actor: String,
    pub scopes: Vec<String>,
    pub projects: Vec<ProjectView>,
}

pub async fn projects(
    State(app): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<ProjectsView>, ApiError> {
    let actor = authenticate(&app, &headers).await?;
    let projects = app.store.projects_by_ids(&actor.token.project_ids).await?;
    Ok(Json(ProjectsView {
        actor: actor.token.actor.clone(),
        scopes: actor.token.scopes.clone(),
        projects: projects
            .into_iter()
            .map(|p| ProjectView { slug: p.slug })
            .collect(),
    }))
}

#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub before_time: Option<String>,
    pub before_id: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Serialize)]
pub struct OperationSummary {
    pub operation_id: String,
    pub name: Option<String>,
    pub request_id: Option<String>,
    pub release: Option<String>,
    pub status: Status,
    pub first_event_time: Timestamp,
    pub last_event_time: Timestamp,
    pub last_received_at: Timestamp,
    pub events_received: i32,
    pub events_emitted: Option<i32>,
    /// `events_emitted - events_received` when the finish arrived; otherwise unknown.
    pub events_missing: Option<i32>,
    pub reported_dropped: i64,
    pub failed_attempts: i32,
    pub uncertain_attempts: i32,
    pub cost_estimate_micros: i64,
    pub cost_currency: Option<String>,
    /// Some raw events were received before the retention cutoff and may be deleted; the
    /// summary still stands.
    pub detail_expired: bool,
    /// Events came from several source instances: completeness is per source (see the
    /// timeline), so `events_missing` is not given.
    pub multi_source: bool,
}

#[derive(Serialize)]
pub struct OperationList {
    pub operations: Vec<OperationSummary>,
    /// Pass back as `before_time`/`before_id` for the next page.
    pub next: Option<(Timestamp, String)>,
}

fn row_outcome(row: &OperationRow) -> Option<Outcome> {
    let detail = row.finish_detail.as_deref().unwrap_or("");
    match row.finish_outcome.as_deref()? {
        "succeeded" => Some(Outcome::Succeeded),
        "failed" => FailureClass::parse(detail).map(Outcome::Failed),
        "uncertain" => UncertainReason::parse(detail).map(Outcome::Uncertain),
        _ => None,
    }
}

pub async fn list_operations(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<OperationList>, ApiError> {
    let actor = authenticate(&app, &headers).await?;
    let project = authorize(&app, &actor, "operations.list", &slug, None).await?;
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let before = match (params.before_time.as_deref(), params.before_id.as_deref()) {
        (None, None) => None,
        (Some(t), Some(id)) => Some((
            Timestamp::parse_rfc3339(t).map_err(|_| ApiError::BadRequest("before_time"))?,
            OperationId::parse(id).map_err(|_| ApiError::BadRequest("before_id"))?,
        )),
        _ => {
            return Err(ApiError::BadRequest(
                "before_time and before_id go together",
            ));
        }
    };
    if params.request_id.as_ref().is_some_and(|r| r.len() > 128) {
        return Err(ApiError::BadRequest("request_id"));
    }
    let rows = app
        .store
        .list_operations(
            project.id,
            &ListFilter {
                limit,
                before,
                request_id: params.request_id,
                operation_id: None,
            },
        )
        .await?;
    let cutoff = app.store.retention_watermark().await?;
    let next = (rows.len() == usize::try_from(limit).unwrap_or(0))
        .then(|| {
            rows.last()
                .map(|r| (r.first_event_time, r.operation_id.to_string()))
        })
        .flatten();
    let operations = rows
        .iter()
        .map(|r| OperationSummary {
            operation_id: r.operation_id.to_string(),
            name: r.name.clone(),
            request_id: r.request_id.clone(),
            release: r.release.clone(),
            status: list_state(row_outcome(r), r.finish_conflict),
            first_event_time: r.first_event_time,
            last_event_time: r.last_event_time,
            last_received_at: r.last_received_at,
            events_received: r.host_events,
            events_emitted: r.events_emitted,
            events_missing: r
                .events_emitted
                .filter(|_| !r.multi_source)
                .map(|e| (e - r.host_events).max(0)),
            reported_dropped: r.reported_dropped,
            failed_attempts: r.failed_attempts,
            uncertain_attempts: r.uncertain_attempts,
            cost_estimate_micros: r.cost_estimate_micros,
            cost_currency: r.cost_currency.clone(),
            detail_expired: cutoff.is_some_and(|c| r.first_received_at < c),
            multi_source: r.multi_source,
        })
        .collect();
    Ok(Json(OperationList { operations, next }))
}

pub async fn timeline(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id)): Path<(String, String)>,
) -> Result<Json<Timeline>, ApiError> {
    let actor = authenticate(&app, &headers).await?;
    let target = (operation_id.len() <= 32).then_some(operation_id.as_str());
    let project = authorize(&app, &actor, "timeline.read", &slug, target).await?;
    let op = OperationId::parse(&operation_id).map_err(|_| ApiError::BadRequest("operation_id"))?;
    let (events, truncated) = app
        .store
        .operation_events(project.id, op, app.config.timeline_max_events)
        .await?;
    // The summary outlives the raw events (D-016): with the retention cutoff it tells an
    // expired operation from one that never sent anything (review finding 10).
    let summary = app.store.operation_summary(project.id, op).await?;
    let cutoff = app.store.retention_watermark().await?;
    let retention = match (&summary, cutoff) {
        (Some(row), Some(cutoff)) => Some(RetentionContext {
            cutoff,
            first_received_at: row.first_received_at,
            finish: row_outcome(row),
            finish_conflict: row.finish_conflict,
            events_received: u32::try_from(row.host_events).unwrap_or(0),
        }),
        _ => None,
    };
    let expired = retention
        .as_ref()
        .is_some_and(|r| r.first_received_at < r.cutoff);
    if events.is_empty() && !expired {
        return Err(ApiError::NotFound);
    }
    let mut instances: Vec<_> = events.iter().map(|e| e.instance_id).collect();
    instances.sort_unstable();
    instances.dedup();
    let stats = app.store.instances(project.id, &instances).await?;
    let mut timeline = assemble(
        op,
        events,
        &stats,
        &app.limits,
        truncated,
        retention.as_ref(),
    );
    if let Some(row) = summary.as_ref().filter(|_| timeline.name.is_none()) {
        // The start may have expired; the summary kept the name and request.
        timeline.name.clone_from(&row.name);
        timeline.request_id.clone_from(&row.request_id);
        timeline.release.clone_from(&row.release);
    }
    Ok(Json(timeline))
}

/// Collector-wide counters span every project, so they need the `admin` scope (review
/// finding 17); any other token is denied and audited.
pub async fn stats(
    State(app): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<MetricsView>, ApiError> {
    let actor = authenticate(&app, &headers).await?;
    if !actor.token.scopes.iter().any(|s| s == SCOPE_ADMIN) {
        return Err(deny(&app, &actor, "stats.read", None, None, "scope_missing").await);
    }
    Ok(Json(app.metrics_view()))
}
