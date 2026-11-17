//! The runner's own answer beside a timeline (A2, D-027): `GET /v1/projects/{p}/operations/
//! {id}/authority` reads `GET /jobs/{run}` from Durable Runner with its read-only key and
//! returns it, labeled as the runner's answer and the time it was read. The trace never
//! overrides it, and nothing here acts on it (invariant 1).
//!
//! The link from an operation to a run is telemetry's claim (`job.runner`, `job.id` on the
//! operation's start), so it is checked before the runner is asked: the operation ID must
//! be the one derived from that runner and run. A forged link can only make the console say
//! "unverified", never show another run's state. The call is on the query path, under its
//! own concurrency cap and a 2 s deadline, and uses no database connection.

use std::time::Duration;

use aiops_domain::OperationId;
use aiops_domain::correlate::operation_for_run;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::app::SharedState;
use crate::config::AuthoritySource;
use crate::error::ApiError;
use crate::query::{authenticate, authorize};

/// What `GET /jobs/{id}` answers (Durable Runner's public contract).
#[derive(Debug, Deserialize)]
struct RunAnswer {
    id: u64,
    key: String,
    state: String,
    steps: u32,
    finished_at: Option<u64>,
}

/// The runner's answer, as the console shows it.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RunView {
    pub state: String,
    pub key: String,
    pub steps: u32,
    /// Milliseconds since the epoch, by the runner's clock.
    pub finished_at_ms: Option<u64>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Authority {
    /// No runner is configured on this collector.
    NotConfigured,
    /// The operation's start events name no run.
    NotLinked,
    /// The events name a runner this collector does not know.
    UnknownRunner { runner: String },
    /// The named run does not derive to this operation: the link is not believed.
    LinkUnverified { runner: String, run_id: i64 },
    /// The runner could not be asked or did not answer usably; nothing is inferred.
    Unavailable {
        source: &'static str,
        runner: String,
        run_id: u64,
        reason: &'static str,
        read_at: aiops_domain::Timestamp,
    },
    Answered {
        source: &'static str,
        runner: String,
        run_id: u64,
        read_at: aiops_domain::Timestamp,
        answer: Box<RunView>,
    },
}

const SOURCE: &str = "durable-runner";

/// Which run the operation stands for, if its link holds. The configured runner's link is
/// the one checked when the events name several.
fn verify(
    source: &AuthoritySource,
    operation: OperationId,
    links: &[(String, i64)],
) -> Result<u64, Authority> {
    let Some((runner, run)) = links
        .iter()
        .find(|(r, _)| *r == source.label)
        .or(links.first())
        .cloned()
    else {
        return Err(Authority::NotLinked);
    };
    if runner != source.label {
        return Err(Authority::UnknownRunner { runner });
    }
    match u64::try_from(run) {
        Ok(id) if operation_for_run(&runner, id) == operation => Ok(id),
        _ => Err(Authority::LinkUnverified {
            runner,
            run_id: run,
        }),
    }
}

/// Reads the runner's answer up to `max` bytes; `None` past it.
async fn read_bounded(mut res: reqwest::Response, max: usize) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.ok()? {
        if body.len() + chunk.len() > max {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

pub async fn authority(
    State(app): State<SharedState>,
    headers: HeaderMap,
    Path((slug, operation_id)): Path<(String, String)>,
) -> Result<Json<Authority>, ApiError> {
    let actor = authenticate(&app, &headers).await?;
    let target = (operation_id.len() <= 32).then_some(operation_id.as_str());
    let project = authorize(&app, &actor, "authority.read", &slug, target).await?;
    let op = OperationId::parse(&operation_id).map_err(|_| ApiError::BadRequest("operation_id"))?;
    let Some(source) = app.config.authority.clone() else {
        return Ok(Json(Authority::NotConfigured));
    };
    let links = app.store.operation_links(project.id, op).await?;
    let run = match verify(&source, op, &links) {
        Ok(run) => run,
        Err(a) => return Ok(Json(a)),
    };
    let unavailable = |reason: &'static str| Authority::Unavailable {
        source: SOURCE,
        runner: source.label.clone(),
        run_id: run,
        reason,
        read_at: app.now(),
    };
    let Ok(_permit) = app.authority_gate.try_acquire() else {
        return Ok(Json(unavailable("busy")));
    };
    let res = app
        .authority_client
        .get(format!("{}/jobs/{run}", source.url))
        .bearer_auth(&source.read_key)
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    let read_at = app.now();
    let res = match res {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Ok(Json(unavailable("timeout"))),
        Err(e) if e.is_connect() => return Ok(Json(unavailable("unreachable"))),
        Err(_) => return Ok(Json(unavailable("unreachable"))),
    };
    match res.status().as_u16() {
        200 => {}
        404 => return Ok(Json(unavailable("not_found"))),
        401 | 403 => return Ok(Json(unavailable("unauthorized"))),
        _ => return Ok(Json(unavailable("bad_answer"))),
    }
    let Ok(Some(body)) =
        tokio::time::timeout(Duration::from_secs(2), read_bounded(res, 64 * 1024)).await
    else {
        return Ok(Json(unavailable("bad_answer")));
    };
    match serde_json::from_slice::<RunAnswer>(&body) {
        Ok(a) if a.id == run && a.state.len() <= 64 && a.key.len() <= 512 => {
            Ok(Json(Authority::Answered {
                source: SOURCE,
                runner: source.label,
                run_id: run,
                read_at,
                answer: Box::new(RunView {
                    state: a.state,
                    key: a.key,
                    steps: a.steps,
                    finished_at_ms: a.finished_at,
                }),
            }))
        }
        _ => Ok(Json(unavailable("bad_answer"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> AuthoritySource {
        AuthoritySource {
            label: "dr-local".into(),
            url: "http://127.0.0.1:1".into(),
            read_key: "k".repeat(32),
        }
    }

    #[test]
    fn a_link_is_believed_only_when_the_operation_derives_from_it() {
        let op = operation_for_run("dr-local", 7);
        assert_eq!(verify(&source(), op, &[("dr-local".into(), 7)]), Ok(7));
        assert_eq!(
            verify(&source(), op, &[("dr-local".into(), 8)]),
            Err(Authority::LinkUnverified {
                runner: "dr-local".into(),
                run_id: 8
            }),
            "a forged run ID cannot borrow another run's state"
        );
        assert_eq!(verify(&source(), op, &[]), Err(Authority::NotLinked));
        assert_eq!(
            verify(&source(), op, &[("dr-other".into(), 7)]),
            Err(Authority::UnknownRunner {
                runner: "dr-other".into()
            })
        );
        assert_eq!(
            verify(
                &source(),
                op,
                &[("dr-other".into(), 1), ("dr-local".into(), 7)]
            ),
            Ok(7),
            "the configured runner's link is the one checked"
        );
    }
}
