//! `POST /v1/ingest` (`docs/INGEST_CONTRACT.md`): authenticate (headers only) → shed →
//! read the body under a byte budget → bound → rate-limit
//! → validate each event (named refusals) → admit names (bounded cardinality) → one
//! store transaction under a deadline. Nothing here touches control records or hosts.

use aiops_domain::event::EventBody;
use aiops_domain::names::{Admission, NameRegistry, OVERFLOW_NAME};
use aiops_domain::policy::Policy;
use aiops_domain::validate::{Refusal, RefusalCode, validate_source};
use aiops_domain::wire::{IngestResponse, RawBatch, RefusedEvent, SCHEMA_VERSION};
use aiops_domain::{Event, Mode, Validator};
use aiops_store::{IngestBatch, IngestKey, PreparedEvent};
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

use crate::app::{AppState, Metrics, SharedState};
use crate::auth::{INGEST_KEY_PREFIX, bearer, hash};
use crate::error::ApiError;

pub async fn ingest(
    State(app): State<SharedState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<IngestResponse>, ApiError> {
    // Authenticate from the headers before taking an in-flight slot (review finding 4) and
    // before reading the body (review N2): an unknown key costs at most one budgeted lookup,
    // never holds ingest capacity, and its body is never read.
    let key = authenticate(&app, &headers).await?;
    let Ok(_permit) = app.ingest_gate.clone().try_acquire_owned() else {
        Metrics::add(&app.metrics.shed, 1);
        return Err(ApiError::Overloaded);
    };
    let (body, _bytes_permit) = read_body(&app, &headers, body).await?;
    let received_at = app.now();

    let raw: RawBatch<'_> = serde_json::from_slice(&body).map_err(|_| ApiError::MalformedBatch)?;
    if raw.schema_version != SCHEMA_VERSION {
        return Err(ApiError::UnsupportedSchema);
    }
    if raw.events.len() > app.limits.max_batch_events {
        return Err(ApiError::TooManyEvents(app.limits.max_batch_events));
    }
    let source = validate_source(&raw.source, &app.limits)
        .map_err(|r| ApiError::InvalidSource(r.field.unwrap_or("source")))?;
    let count = u32::try_from(raw.events.len()).unwrap_or(u32::MAX);
    if let Err(retry_after_secs) =
        app.ingest_buckets
            .take(key.id, count.max(1), received_at.unix_micros())
    {
        Metrics::add(&app.metrics.rate_limited, 1);
        return Err(ApiError::RateLimited { retry_after_secs });
    }

    let (mut prepared, mut response) = validate_all(&app, &key, &raw, received_at)?;

    let names = admit_names(&app, key.project_id, key.id, &mut prepared, &mut response).await?;
    let refused_count = u32::try_from(response.refused.len()).unwrap_or(u32::MAX);
    let index_of: Vec<(aiops_domain::EventId, usize)> = prepared
        .iter()
        .map(|(i, p)| (p.event.event_id, *i))
        .collect();
    let batch = IngestBatch {
        project_id: key.project_id,
        ingest_key_id: key.id,
        source: &source,
        stats: raw.stats,
        received_at,
        events: prepared.into_iter().map(|(_, p)| p).collect(),
        refused: refused_count,
        new_names: names.new.clone(),
        touched_names: names.touched,
    };
    let stored =
        match tokio::time::timeout(app.config.store_deadline, app.store.ingest(batch)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                Metrics::add(&app.metrics.store_unavailable, 1);
                forget_names(&app, key.project_id, key.id, &names.new).await;
                return Err(e.into());
            }
            Err(_) => {
                // The transaction may still commit; the SDK's retry is idempotent (D-019),
                // and a name stored after all is inserted again harmlessly.
                Metrics::add(&app.metrics.store_unavailable, 1);
                forget_names(&app, key.project_id, key.id, &names.new).await;
                return Err(ApiError::StoreUnavailable);
            }
        };
    for id in &stored.conflicts {
        let index = index_of
            .iter()
            .find(|(e, _)| e == id)
            .map_or(0, |(_, i)| *i);
        response.refused.push(refused(
            index,
            Some(id.to_string()),
            Refusal::new(RefusalCode::ConflictingDuplicate),
        ));
    }
    response.refused.sort_by_key(|r| r.index);
    response.accepted = stored.accepted;
    response.duplicates += stored.duplicates;

    let m = &app.metrics;
    Metrics::add(&m.batches, 1);
    Metrics::add(&m.events_accepted, u64::from(stored.accepted));
    Metrics::add(&m.events_duplicate, u64::from(stored.duplicates));
    Metrics::add(&m.events_refused, u64::from(refused_count));
    Metrics::add(&m.events_conflicting, stored.conflicts.len() as u64);
    Metrics::add(
        &m.attributes_withheld,
        u64::from(response.withheld_attributes),
    );
    Metrics::add(&m.redactions, u64::from(response.redactions));
    Ok(Json(response))
}

/// Validates each event on its own: valid ones are prepared for the store, invalid ones
/// become named refusals in the response.
fn validate_all(
    app: &AppState,
    key: &IngestKey,
    raw: &RawBatch<'_>,
    received_at: aiops_domain::Timestamp,
) -> Result<(Vec<(usize, PreparedEvent)>, IngestResponse), ApiError> {
    let policy = Policy {
        opt_in_keys: key.opt_in_keys.clone(),
    };
    let validator = Validator {
        limits: &app.limits,
        policy: &policy,
        mode: Mode::Ingest { received_at },
    };
    let mut response = IngestResponse::default();
    let mut prepared = Vec::with_capacity(raw.events.len());
    for (index, event) in raw.events.iter().enumerate() {
        match validator.validate_raw(event.get()) {
            Ok(v) => {
                response.withheld_attributes += v.marks.withheld_attributes;
                response.redactions += v.marks.redactions;
                let p = PreparedEvent::new(v.event, v.marks)
                    .map_err(|e| ApiError::Internal(e.into()))?;
                prepared.push((index, p));
            }
            Err((id, refusal)) => {
                response
                    .refused
                    .push(refused(index, id.map(|i| i.to_string()), refusal));
            }
        }
    }
    // An ID repeated inside the batch (review finding 3): an identical copy is a
    // duplicate, a different one is refused at its own index; only the first goes on.
    let mut first: std::collections::HashMap<aiops_domain::EventId, usize> =
        std::collections::HashMap::with_capacity(prepared.len());
    let mut unique: Vec<(usize, PreparedEvent)> = Vec::with_capacity(prepared.len());
    for (index, p) in prepared {
        if let Some(&pos) = first.get(&p.event.event_id) {
            let (_, kept) = &unique[pos];
            if kept.body == p.body && kept.marks == p.marks {
                response.duplicates += 1;
            } else {
                response.refused.push(refused(
                    index,
                    Some(p.event.event_id.to_string()),
                    Refusal::new(RefusalCode::ConflictingDuplicate),
                ));
            }
            continue;
        }
        first.insert(p.event.event_id, unique.len());
        unique.push((index, p));
    }
    Ok((unique, response))
}

fn refused(index: usize, event_id: Option<String>, r: Refusal) -> RefusedEvent {
    RefusedEvent {
        index: u32::try_from(index).unwrap_or(u32::MAX),
        event_id,
        code: r.code.as_str().to_owned(),
        field: r.field.map(str::to_owned),
    }
}

/// Reads the body only under the in-flight byte budget (review N2); a body of unknown length
/// is charged the full cap. Past the budget, the request is shed before any byte is read.
async fn read_body(
    app: &AppState,
    headers: &HeaderMap,
    body: Body,
) -> Result<(axum::body::Bytes, tokio::sync::OwnedSemaphorePermit), ApiError> {
    let limit = app.limits.max_body_bytes;
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(limit)
        .min(limit);
    let kib = u32::try_from(declared.div_ceil(1024).max(1)).unwrap_or(u32::MAX);
    let Ok(permit) = app.ingest_body_budget.clone().try_acquire_many_owned(kib) else {
        Metrics::add(&app.metrics.shed, 1);
        return Err(ApiError::Overloaded);
    };
    match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => Ok((bytes, permit)),
        // Over the cap (no Content-Length, or a chunked body), or the client went away
        // mid-body. The limit error can sit several sources deep (two limiting layers).
        Err(e) => {
            let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&e);
            while let Some(c) = cause {
                if c.is::<http_body_util::LengthLimitError>() {
                    return Err(ApiError::BodyTooLarge);
                }
                cause = c.source();
            }
            Err(ApiError::MalformedBatch)
        }
    }
}

async fn authenticate(app: &AppState, headers: &HeaderMap) -> Result<IngestKey, ApiError> {
    let Some(secret) = bearer(headers, INGEST_KEY_PREFIX) else {
        Metrics::add(&app.metrics.unauthenticated, 1);
        return Err(ApiError::Unauthenticated);
    };
    let digest = hash(secret);
    let found = app
        .resolve(&app.ingest_keys, digest, true, || {
            app.store.find_ingest_key(&digest)
        })
        .await?;
    match found {
        Some(key) if !key.revoked => Ok(key),
        _ => {
            Metrics::add(&app.metrics.unauthenticated, 1);
            Err(ApiError::Unauthenticated)
        }
    }
}

/// Visits every name an event carries, by kind, mutably (an over-budget one is rewritten).
fn visit_names(event: &mut Event, mut f: impl FnMut(&'static str, &mut String)) {
    match &mut event.body {
        EventBody::OperationStarted(m) => {
            f("op", &mut m.name);
            if let Some(v) = m.release.as_mut() {
                f("release", v);
            }
            if let Some(v) = m.deployment.as_mut() {
                f("deploy", v);
            }
        }
        EventBody::OperationFinished(fin) => {
            if let Some(v) = fin.error_code.as_mut() {
                f("error", v);
            }
        }
        EventBody::StepStarted(st) => {
            f("step", &mut st.name);
            for (kind, v) in [
                ("model", st.model.as_mut()),
                ("prompt", st.prompt_version.as_mut()),
                ("config", st.config_version.as_mut()),
            ] {
                if let Some(v) = v {
                    f(kind, v);
                }
            }
        }
        EventBody::StepFinished(fin) => {
            if let Some(v) = fin.name.as_mut() {
                f("step", v);
            }
            if let Some(v) = fin.error_code.as_mut() {
                f("error", v);
            }
            if let Some(c) = fin.cost_estimate.as_mut() {
                f("basis", &mut c.basis);
            }
        }
        EventBody::GapReported(_) => {}
    }
}

/// What a batch did to the project's names.
struct NameUse {
    new: Vec<String>,
    touched: Vec<String>,
}

/// Takes back the names a batch admitted when the batch was not stored (review G1): they
/// were never written to `known_names`, so a later batch must admit and store them, not
/// only touch them. Rare (a failed store), so the exclusive lock is fine here.
async fn forget_names(app: &AppState, project: Uuid, key: Uuid, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let Ok(registry) = registry(app, project).await else {
        return;
    };
    if let Some(mut w) = app.names_exclusive_lock.write(&registry) {
        for n in names {
            w.forget(n, *key.as_bytes());
        }
    }
}

/// The project's name registry, loaded from the store on its first batch.
async fn registry(app: &AppState, project: Uuid) -> Result<Arc<RwLock<NameRegistry>>, ApiError> {
    let found = app
        .names_map_lock
        .read(&app.names)
        .and_then(|m| m.get(&project).cloned());
    if let Some(r) = found {
        return Ok(r);
    }
    let (cap, per_key) = (
        app.limits.max_names_per_project,
        app.limits.max_names_per_key,
    );
    let known = app.store.known_names(project, cap).await?;
    let mut map = app
        .names_map_lock
        .write(&app.names)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("name registry lock poisoned")))?;
    Ok(map
        .entry(project)
        .or_insert_with(|| Arc::new(RwLock::new(NameRegistry::new(cap, per_key, known))))
        .clone())
}

/// Bounded cardinality (D-014, review finding 11): a name over the project's or the key's
/// budget is stored as `_overflow`, labeled and counted, so a noisy host can never cost an
/// honest host its events, and no event is refused for its name.
///
/// The batch's names are formatted before any lock is taken. In the steady state every
/// one is already known, which a shared lock can tell: batches then never wait for each
/// other. Only a batch that carries a new name takes its project's registry exclusively,
/// and admits every name in order, so budgets are spent first come, first served (D-026).
async fn admit_names(
    app: &AppState,
    project: Uuid,
    key: Uuid,
    prepared: &mut [(usize, PreparedEvent)],
    response: &mut IngestResponse,
) -> Result<NameUse, ApiError> {
    let registry = registry(app, project).await?;
    let poisoned = || ApiError::Internal(anyhow::anyhow!("name registry lock poisoned"));
    let mut names = Vec::with_capacity(prepared.len() * 2);
    for (_, p) in prepared.iter_mut() {
        visit_names(&mut p.event, |kind, value| {
            names.push(format!("{kind}:{value}"));
        });
    }
    let all_known = {
        let r = app.names_shared_lock.read(&registry).ok_or_else(poisoned)?;
        names.iter().all(|n| r.is_known(n))
    };
    let verdicts: Vec<Admission> = if all_known {
        vec![Admission::Known; names.len()]
    } else {
        let key_bytes = *key.as_bytes();
        let mut w = app
            .names_exclusive_lock
            .write(&registry)
            .ok_or_else(poisoned)?;
        names.iter().map(|n| w.admit(n, key_bytes)).collect()
    };
    let mut new_names = Vec::new();
    let mut touched = std::collections::HashSet::new();
    let mut over_budget = false;
    for (name, verdict) in names.into_iter().zip(&verdicts) {
        match verdict {
            Admission::Known => {
                touched.insert(name);
            }
            Admission::New => new_names.push(name),
            Admission::OverBudget => over_budget = true,
        }
    }
    if over_budget {
        // The same visiting order, so the n-th name visited is the n-th verdict.
        let mut verdict = verdicts.iter();
        for (_, p) in prepared.iter_mut() {
            let mut overflowed = 0u32;
            visit_names(&mut p.event, |_, value| {
                if verdict.next() == Some(&Admission::OverBudget) {
                    OVERFLOW_NAME.clone_into(value);
                    overflowed += 1;
                }
            });
            if overflowed > 0 {
                p.marks.overflowed_names += overflowed;
                response.overflowed_names += overflowed;
                p.body = serde_json::to_string(&p.event.to_wire())
                    .map_err(|e| ApiError::Internal(e.into()))?;
            }
        }
    }
    Ok(NameUse {
        new: new_names,
        touched: touched.into_iter().collect(),
    })
}
