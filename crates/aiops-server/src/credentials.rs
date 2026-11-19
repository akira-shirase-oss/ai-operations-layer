//! The credential surface (D-034): the console shows every credential — projects,
//! ingest keys, API tokens — with metadata only, and revokes keys and tokens under
//! the `admin` scope, audited. Minting stays CLI-only: a secret shown once in a
//! terminal is a smaller surface than a browser.
//!
//! Two rules hold the boundary:
//!
//! - **No hash ever leaves.** The store queries never select `key_hash`/`token_hash`;
//!   the wire carries id, name, actor, scopes, projects and timestamps only.
//! - **Revocation is a control action.** It runs on the control pool under the
//!   control bucket — an incident action must not starve behind a query flood —
//!   and every grant and denial lands in the audit log.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::app::{AppState, SharedState};
use crate::control;
use crate::error::ApiError;
use crate::query::{SCOPE_ADMIN, authenticate_with};
use aiops_store::{AuditEntry, StoreError};

/// Authenticate under the control bucket, then require `admin` — denied and audited.
/// No project check: credentials span projects by nature.
pub(crate) async fn authorize_admin(
    app: &AppState,
    headers: &HeaderMap,
    action: &str,
) -> Result<crate::query::Actor, ApiError> {
    let actor = authenticate_with(app, headers, &app.control_buckets).await?;
    if !actor.token.scopes.iter().any(|s| s == SCOPE_ADMIN) {
        return Err(control::deny(app, &actor, action, None, None, "scope_missing").await);
    }
    Ok(actor)
}

/// `GET /v1/admin/credentials` — the whole surface in one read: projects, ingest
/// keys, tokens. Metadata only; an allowed read is not audited (reads are traffic,
/// not decisions), a denial always is.
pub async fn list(
    State(app): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize_admin(&app, &headers, "credentials.list").await?;
    let projects = app.store.list_project_infos().await?;
    let ingest_keys = app.store.list_ingest_key_infos().await?;
    let tokens = app.store.list_token_infos().await?;
    Ok(Json(json!({
        "projects": projects.iter().map(|p| json!({
            "id": p.id.to_string(),
            "slug": p.slug,
            "opt_in_attribute_keys": p.opt_in_keys,
            "created_at": p.created_at.to_rfc3339(),
        })).collect::<Vec<_>>(),
        "ingest_keys": ingest_keys.iter().map(|k| json!({
            "id": k.id.to_string(),
            "name": k.name,
            "project": k.project_slug,
            "created_at": k.created_at.to_rfc3339(),
            "revoked_at": k.revoked_at.map(aiops_domain::Timestamp::to_rfc3339),
        })).collect::<Vec<_>>(),
        "tokens": tokens.iter().map(|t| json!({
            "id": t.id.to_string(),
            "actor": t.actor,
            "scopes": t.scopes,
            "projects": t.project_slugs,
            "created_at": t.created_at.to_rfc3339(),
            "expires_at": t.expires_at.to_rfc3339(),
            "revoked_at": t.revoked_at.map(aiops_domain::Timestamp::to_rfc3339),
        })).collect::<Vec<_>>(),
    })))
}

/// `POST /v1/admin/credentials/tokens/{id}/revoke` — audited under `admin.revoke_token`.
pub async fn revoke_token(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let actor = authorize_admin(&app, &headers, "admin.revoke_token").await?;
    revoke(&app, &actor, id, "admin.revoke_token", Kind::Token).await
}

/// `POST /v1/admin/credentials/ingest-keys/{id}/revoke` — audited under `admin.revoke_ingest_key`.
pub async fn revoke_ingest_key(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let actor = authorize_admin(&app, &headers, "admin.revoke_ingest_key").await?;
    revoke(&app, &actor, id, "admin.revoke_ingest_key", Kind::IngestKey).await
}

enum Kind {
    Token,
    IngestKey,
}

/// The shared revoke: parse the id, apply it on the control pool, audit the grant
/// (allowed) or the denial (unknown credential) — synchronously, before the answer.
async fn revoke(
    app: &AppState,
    actor: &crate::query::Actor,
    id: String,
    action: &'static str,
    kind: Kind,
) -> Result<Json<Value>, ApiError> {
    let Ok(credential) = Uuid::parse_str(&id) else {
        return Err(ApiError::BadRequest("credential id must be a uuid"));
    };
    let result = match kind {
        Kind::Token => app.store.revoke_token(credential).await,
        Kind::IngestKey => app.store.revoke_ingest_key_id(credential).await,
    };
    let digest = match result {
        Ok(digest) => Some(digest),
        Err(StoreError::NotFound(_)) => None,
        Err(e) => return Err(e.into()),
    };
    // Evict the credential's cached answer NOW: a revoked credential dies on its
    // next presentation, not when the resolver's TTL runs out (D-034 — revocation
    // is an incident action). The digest stays inside the process; it is never on
    // the wire.
    if let Some(digest) = &digest {
        match kind {
            Kind::Token => app.tokens.remove(digest),
            Kind::IngestKey => app.ingest_keys.remove(digest),
        }
    }
    let (allowed, reason) = if digest.is_some() {
        (true, "revoked")
    } else {
        (false, "unknown_or_revoked")
    };
    let entry = AuditEntry {
        actor: &actor.token.actor,
        token_id: Some(actor.token.id),
        action,
        project_slug: None,
        target: Some(&id),
        allowed,
        reason,
    };
    match tokio::time::timeout(app.config.audit_deadline, app.store.audit(&entry)).await {
        Ok(Ok(())) => {}
        _ => return Err(ApiError::AuditUnavailable),
    }
    if allowed {
        Ok(Json(json!({"id": id, "revoked": true})))
    } else {
        Err(ApiError::NotFound)
    }
}
