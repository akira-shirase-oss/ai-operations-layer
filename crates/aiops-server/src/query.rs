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

use uuid::Uuid;

use crate::app::{AppState, Metrics, MetricsView, SharedState};
use crate::auth::{Buckets, TOKEN_PREFIX, bearer, hash};
use crate::error::ApiError;

pub const SCOPE_READ: &str = "read";
/// Collector-wide operations (counters across all projects).
pub const SCOPE_ADMIN: &str = "admin";

/// An authenticated actor.
pub struct Actor {
    pub token: Token,
}

pub async fn authenticate(app: &AppState, headers: &HeaderMap) -> Result<Actor, ApiError> {
    authenticate_with(app, headers, &app.query_buckets, true).await
}

/// `authenticate` under a caller-chosen bucket: the query path and the control path each
/// rate-limit their own, so one flood cannot starve the other. `allow_stale` lets the
/// read path keep serving a recently-valid record when the lookup budget is spent —
/// control/admin callers pass `false` and fail closed instead.
pub async fn authenticate_with(
    app: &AppState,
    headers: &HeaderMap,
    buckets: &Buckets<Uuid>,
    allow_stale: bool,
) -> Result<Actor, ApiError> {
    let Some(secret) = bearer(headers, TOKEN_PREFIX) else {
        Metrics::add(&app.metrics.unauthenticated, 1);
        return Err(ApiError::Unauthenticated);
    };
    let digest = hash(secret);
    let found = app
        .resolve(&app.tokens, digest, allow_stale, || {
            app.store.find_token(&digest)
        })
        .await?;
    let now = app.now();
    match found {
        Some(token) if !token.revoked && token.expires_at > now => {
            if let Err(retry_after_secs) = buckets.take(token.id, 1, now.unix_micros()) {
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
pub(crate) async fn authorize(
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
    /// The list's status words (`succeeded`, `failed`, `uncertain`, `conflicting`,
    /// `no_finish_observed`).
    pub status: Option<String>,
    pub name: Option<String>,
    pub release: Option<String>,
    /// RFC 3339: operations whose first event time is in `[from, to)`.
    pub from: Option<String>,
    pub to: Option<String>,
}

const STATUSES: [&str; 5] = [
    "succeeded",
    "failed",
    "uncertain",
    "conflicting",
    "no_finish_observed",
];

fn status_param(s: Option<&str>) -> Result<Option<&'static str>, ApiError> {
    match s {
        None | Some("") => Ok(None),
        Some(v) => STATUSES
            .iter()
            .find(|k| **k == v)
            .copied()
            .map(Some)
            .ok_or(ApiError::BadRequest("status")),
    }
}

pub(crate) fn ident_param(
    v: Option<String>,
    field: &'static str,
) -> Result<Option<String>, ApiError> {
    match v {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) if v.len() <= 128 => Ok(Some(v)),
        Some(_) => Err(ApiError::BadRequest(field)),
    }
}

pub(crate) fn time_param(
    v: Option<&str>,
    field: &'static str,
) -> Result<Option<Timestamp>, ApiError> {
    v.filter(|t| !t.is_empty())
        .map(|t| Timestamp::parse_rfc3339(t).map_err(|_| ApiError::BadRequest(field)))
        .transpose()
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
    let request_id = ident_param(params.request_id, "request_id")?;
    let status = status_param(params.status.as_deref())?;
    let name = ident_param(params.name, "name")?;
    let release = ident_param(params.release, "release")?;
    let from = time_param(params.from.as_deref(), "from")?;
    let to = time_param(params.to.as_deref(), "to")?;
    let rows = app
        .store
        .list_operations(
            project.id,
            &ListFilter {
                limit,
                before,
                request_id,
                operation_id: None,
                status,
                name,
                release,
                from,
                to,
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
            status: list_state(row_outcome(r), r.finish_code.as_deref(), r.finish_conflict),
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
            finish_code: row.finish_code.clone(),
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

#[derive(Deserialize)]
pub struct CompareParams {
    /// Comma-separated, at most four.
    pub releases: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Serialize)]
pub struct ReleaseView {
    pub release: String,
    pub operations: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub uncertain: i64,
    pub conflicting: i64,
    pub no_finish_observed: i64,
    pub missing_events: i64,
    pub reported_dropped: i64,
    pub failed_attempts: i64,
    pub uncertain_attempts: i64,
    /// Always an estimate, never a bill (invariant 7); `None` when currencies are mixed.
    pub cost_estimate_micros: Option<i64>,
    pub mixed_currencies: bool,
}

#[derive(Serialize)]
pub struct Comparison {
    pub from: Timestamp,
    pub to: Timestamp,
    /// Each requested release, in the order asked; one with no operations in the window is
    /// listed with zeros, never dropped.
    pub releases: Vec<ReleaseView>,
}

/// Longest window a comparison may span (it reads the summaries in it).
const MAX_COMPARE_WINDOW_MICROS: i64 = 7 * 24 * 3_600 * 1_000_000;

/// Releases side by side over one window, from the same summaries and statuses the list
/// filters by (A2): a release whose operations lost events or never finished shows that
/// in its own columns, never as success.
pub async fn compare_releases(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(params): Query<CompareParams>,
) -> Result<Json<Comparison>, ApiError> {
    let actor = authenticate(&app, &headers).await?;
    let project = authorize(&app, &actor, "releases.compare", &slug, None).await?;
    let mut releases: Vec<String> = Vec::new();
    for r in params
        .releases
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        if r.len() > 128 {
            return Err(ApiError::BadRequest("releases"));
        }
        if !releases.iter().any(|x| x == r) {
            releases.push(r.to_owned());
        }
    }
    if releases.is_empty() || releases.len() > 4 {
        return Err(ApiError::BadRequest("releases (one to four)"));
    }
    let to = time_param(params.to.as_deref(), "to")?.unwrap_or_else(|| app.now());
    let from = time_param(params.from.as_deref(), "from")?
        .unwrap_or_else(|| to.plus_micros(-24 * 3_600 * 1_000_000));
    let span = to.micros_since(from);
    if span <= 0 || span > MAX_COMPARE_WINDOW_MICROS {
        return Err(ApiError::BadRequest("from/to (a window of at most 7 days)"));
    }
    let counts = app
        .store
        .compare_releases(project.id, &releases, from, to)
        .await?;
    let releases = releases
        .into_iter()
        .map(|name| {
            let c = counts.iter().find(|c| c.release == name);
            let g = |f: fn(&aiops_store::ReleaseCounts) -> i64| c.map_or(0, f);
            let mixed = c.is_some_and(|c| c.currencies > 1);
            ReleaseView {
                operations: g(|c| c.operations),
                succeeded: g(|c| c.succeeded),
                failed: g(|c| c.failed),
                uncertain: g(|c| c.uncertain),
                conflicting: g(|c| c.conflicting),
                no_finish_observed: g(|c| c.no_finish_observed),
                missing_events: g(|c| c.missing_events),
                reported_dropped: g(|c| c.reported_dropped),
                failed_attempts: g(|c| c.failed_attempts),
                uncertain_attempts: g(|c| c.uncertain_attempts),
                cost_estimate_micros: (!mixed).then(|| g(|c| c.cost_estimate_micros)),
                mixed_currencies: mixed,
                release: name,
            }
        })
        .collect();
    Ok(Json(Comparison { from, to, releases }))
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
