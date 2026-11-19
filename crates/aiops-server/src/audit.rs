//! The audit log's read surface: `GET /v1/admin/audit`, `admin`-scoped and
//! keyset-paged newest-first with bounded filters. Two rules hold the boundary:
//!
//! - **The read is never audited.** A read that wrote a row into the log it reads
//!   would be a write wearing a read's clothes — and would grow the log on every
//!   page turn. A denial is a decision and is audited, as everywhere.
//! - **Metadata only.** The reply carries the row's fields — who, what, when, the
//!   decision and its reason — and nothing else: no secret, digest or request body
//!   exists in `audit_log` and none is selected.
//!
//! It runs on the control pool under the control bucket: an incident read must not
//! starve behind a query flood.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::app::SharedState;
use crate::credentials::authorize_admin;
use crate::error::ApiError;
use crate::query::{ident_param, time_param};
use aiops_domain::Timestamp;
use aiops_store::AuditFilter;

#[derive(Deserialize)]
pub struct AuditParams {
    limit: Option<i64>,
    before_time: Option<String>,
    before_id: Option<i64>,
    action: Option<String>,
    actor: Option<String>,
    /// The acting token's id.
    token: Option<String>,
    /// RFC 3339: `at` in `[from, to)`.
    from: Option<String>,
    to: Option<String>,
}

/// `GET /v1/admin/audit` — one keyset page, newest first. `next` is the
/// `[before_time, before_id]` pair to pass back; absent on the last page.
pub async fn list(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Query(params): Query<AuditParams>,
) -> Result<Json<Value>, ApiError> {
    authorize_admin(&app, &headers, "audit.list").await?;
    let limit = params.limit.unwrap_or(50).clamp(1, 100);
    let before = match (params.before_time.as_deref(), params.before_id) {
        (None, None) => None,
        (Some(t), Some(id)) => Some((
            Timestamp::parse_rfc3339(t).map_err(|_| ApiError::BadRequest("before_time"))?,
            id,
        )),
        _ => {
            return Err(ApiError::BadRequest(
                "before_time and before_id go together",
            ));
        }
    };
    let action = ident_param(params.action, "action")?;
    let actor = ident_param(params.actor, "actor")?;
    let token_id = match params.token.as_deref() {
        None | Some("") => None,
        Some(t) => Some(Uuid::parse_str(t).map_err(|_| ApiError::BadRequest("token"))?),
    };
    let from = time_param(params.from.as_deref(), "from")?;
    let to = time_param(params.to.as_deref(), "to")?;
    let rows = app
        .store
        .audit_page(&AuditFilter {
            before,
            action: action.as_deref(),
            actor: actor.as_deref(),
            token_id,
            from,
            to,
            limit,
        })
        .await?;
    let next = (rows.len() == usize::try_from(limit).unwrap_or(0))
        .then(|| rows.last().map(|r| (r.at.to_rfc3339(), r.id.to_string())))
        .flatten();
    Ok(Json(json!({
        "audit": rows.iter().map(|r| json!({
            "id": r.id,
            "at": r.at.to_rfc3339(),
            "actor": r.actor,
            "token_id": r.token_id.map(|t| t.to_string()),
            "action": r.action,
            "project_slug": r.project_slug,
            "target": r.target,
            "decision": r.decision,
            "reason": r.reason,
        })).collect::<Vec<_>>(),
        "next": next,
    })))
}
