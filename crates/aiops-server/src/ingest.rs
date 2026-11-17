//! `POST /v1/ingest` (`docs/INGEST_CONTRACT.md`): shed → authenticate → bound → rate-limit
//! → validate each event (named refusals) → admit names (bounded cardinality) → one
//! store transaction under a deadline. Nothing here touches control records or hosts.

use aiops_domain::event::EventBody;
use aiops_domain::names::{Admission, NameRegistry};
use aiops_domain::policy::Policy;
use aiops_domain::validate::{Refusal, RefusalCode, validate_source};
use aiops_domain::wire::{IngestResponse, RawBatch, RefusedEvent, SCHEMA_VERSION};
use aiops_domain::{Event, Mode, Validator};
use aiops_store::{IngestBatch, IngestKey, PreparedEvent};
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use uuid::Uuid;

use crate::app::{AppState, Metrics, SharedState};
use crate::auth::{INGEST_KEY_PREFIX, bearer, hash};
use crate::error::ApiError;

pub async fn ingest(
    State(app): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IngestResponse>, ApiError> {
    // Shed first: the cheapest refusal, before any parsing or lookup.
    let Ok(_permit) = app.ingest_gate.clone().try_acquire_owned() else {
        Metrics::add(&app.metrics.shed, 1);
        return Err(ApiError::Overloaded);
    };
    let key = authenticate(&app, &headers).await?;
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

    let new_names = admit_names(&app, key.project_id, &mut prepared, &mut response).await?;
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
        new_names,
    };
    let stored =
        match tokio::time::timeout(app.config.store_deadline, app.store.ingest(batch)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                Metrics::add(&app.metrics.store_unavailable, 1);
                return Err(e.into());
            }
            Err(_) => {
                // The transaction may still commit; the SDK's retry is idempotent (D-019).
                Metrics::add(&app.metrics.store_unavailable, 1);
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

async fn authenticate(app: &AppState, headers: &HeaderMap) -> Result<IngestKey, ApiError> {
    let Some(secret) = bearer(headers, INGEST_KEY_PREFIX) else {
        Metrics::add(&app.metrics.unauthenticated, 1);
        return Err(ApiError::Unauthenticated);
    };
    let digest = hash(secret);
    let found = if let Some(cached) = app.ingest_keys.get(&digest) {
        cached
    } else {
        let found = app.store.find_ingest_key(&digest).await?;
        app.ingest_keys.put(digest, found.clone());
        found
    };
    match found {
        Some(key) if !key.revoked => Ok(key),
        _ => {
            Metrics::add(&app.metrics.unauthenticated, 1);
            Err(ApiError::Unauthenticated)
        }
    }
}

/// The names an event introduces, prefixed by kind so the cap counts each dimension.
fn names_of(event: &Event) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |kind: &str, v: Option<&String>| {
        if let Some(v) = v {
            out.push(format!("{kind}:{v}"));
        }
    };
    match &event.body {
        EventBody::OperationStarted(m) => {
            push("op", Some(&m.name));
            push("release", m.release.as_ref());
            push("deploy", m.deployment.as_ref());
        }
        EventBody::OperationFinished(f) => push("error", f.error_code.as_ref()),
        EventBody::StepStarted(s) => {
            push("step", Some(&s.name));
            push("model", s.model.as_ref());
            push("prompt", s.prompt_version.as_ref());
            push("config", s.config_version.as_ref());
        }
        EventBody::StepFinished(f) => {
            push("step", f.name.as_ref());
            push("error", f.error_code.as_ref());
            push("basis", f.cost_estimate.as_ref().map(|c| &c.basis));
        }
        EventBody::GapReported(_) => {}
    }
    out
}

/// Bounded cardinality (D-014): events that would introduce a name past the project's
/// cap are refused by name; everything else goes on.
async fn admit_names(
    app: &AppState,
    project: Uuid,
    prepared: &mut Vec<(usize, PreparedEvent)>,
    response: &mut IngestResponse,
) -> Result<Vec<String>, ApiError> {
    let loaded = app.names.lock().is_ok_and(|m| m.contains_key(&project));
    if !loaded {
        let cap = app.limits.max_names_per_project;
        let known = app.store.known_names(project, cap).await?;
        if let Ok(mut map) = app.names.lock() {
            map.entry(project)
                .or_insert_with(|| NameRegistry::new(cap, known));
        }
    }
    let mut new_names = Vec::new();
    let Ok(mut map) = app.names.lock() else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "name registry lock poisoned"
        )));
    };
    let Some(registry) = map.get_mut(&project) else {
        return Err(ApiError::Internal(anyhow::anyhow!("name registry missing")));
    };
    prepared.retain(|(index, p)| {
        let names = names_of(&p.event);
        let mut fresh = Vec::new();
        for name in &names {
            match registry.admit(name) {
                Admission::Known => {}
                Admission::New => fresh.push(name.clone()),
                Admission::Refused => {
                    response.refused.push(refused(
                        *index,
                        Some(p.event.event_id.to_string()),
                        Refusal::on(RefusalCode::CardinalityExceeded, "name"),
                    ));
                    new_names.extend(fresh);
                    return false;
                }
            }
        }
        new_names.extend(fresh);
        true
    });
    Ok(new_names)
}
