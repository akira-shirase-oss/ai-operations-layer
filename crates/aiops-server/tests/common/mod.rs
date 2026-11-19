//! Shared fixtures for the collector's tests.
#![allow(
    dead_code,
    reason = "each test binary uses a different subset of the fixtures"
)]

use std::sync::Arc;

use aiops_domain::Timestamp;
use aiops_domain::wire::WireEvent;
use aiops_server::auth::{INGEST_KEY_PREFIX, TOKEN_PREFIX, generate, hash};
use aiops_server::{AppState, Config, SharedState, router, system_clock};
use aiops_store::Store;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

pub(crate) struct Env {
    pub(crate) app: Router,
    pub(crate) state: SharedState,
    pub(crate) store: Store,
    pub(crate) ingest_key: String,
    pub(crate) other_ingest_key: String,
    /// A second key of `pilot` (another host of the same project).
    pub(crate) other_ingest_key_same_project: String,
    /// read on `pilot`
    pub(crate) reader: String,
    /// `operate` only, on `pilot`
    pub(crate) no_read: String,
    /// read on `other`
    pub(crate) other_reader: String,
    /// `admin` (collector-wide counters)
    pub(crate) admin: String,
}

pub(crate) async fn env(pool: PgPool) -> Env {
    env_with(pool, |_| {}).await
}

pub(crate) async fn env_with(pool: PgPool, tune: impl FnOnce(&mut AppState)) -> Env {
    env_with_store(Store::from_pool(pool), tune).await
}

pub(crate) fn now() -> Timestamp {
    (system_clock())()
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: axum::http::HeaderMap,
    pub(crate) body: Value,
}

pub(crate) async fn send(app: &Router, req: Request<Body>) -> Reply {
    let res: Response<Body> = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        headers,
        body,
    }
}

pub(crate) fn post_ingest(key: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post("/v1/ingest")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}

pub(crate) fn get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

pub(crate) const INSTANCE: &str = "11111111111111111111111111111111";

pub(crate) fn batch(events: &[WireEvent]) -> String {
    batch_from(INSTANCE, events)
}

pub(crate) fn batch_from(instance: &str, events: &[WireEvent]) -> String {
    json!({
        "schema_version": 1,
        "source": {"instance_id": instance, "service": "reference-host", "sdk": "test/1"},
        "stats": {},
        "events": events,
    })
    .to_string()
}

/// One operation's events: started, a model step that fails (transport) and is
/// retried, a publish step that ends `uncertain`, and the declared finish.
#[expect(
    clippy::too_many_lines,
    reason = "a literal fixture: eight events spelled out"
)]
pub(crate) fn journey(op: u8, first_seq: u64) -> Vec<WireEvent> {
    let base_time = now().plus_micros(-30_000_000);
    let op_id = format!("{op:02x}{}", "ab".repeat(15));
    let mut seq = first_seq;
    let mut op_seq = 0u32;
    let mut ev = |ms: i64, ty: &str| {
        seq += 1;
        op_seq += 1;
        WireEvent {
            event_id: format!("{op:02x}{seq:030x}"),
            event_type: ty.into(),
            operation_id: op_id.clone(),
            seq,
            op_seq,
            event_time: base_time.plus_micros(ms * 1_000).to_rfc3339(),
            ..WireEvent::default()
        }
    };
    let step = |w: WireEvent, id: &str, kind: &str, name: &str, attempt: u32| WireEvent {
        step_id: Some(id.into()),
        kind: Some(kind.into()),
        name: Some(name.into()),
        attempt: Some(attempt),
        ..w
    };
    vec![
        WireEvent {
            name: Some("publish_article".into()),
            request_id: Some(format!("req-{op}")),
            release: Some("r1".into()),
            ..ev(0, "operation.started")
        },
        WireEvent {
            model: Some("fake-model-1".into()),
            ..step(
                ev(1, "step.started"),
                "1111111111111111",
                "model",
                "generate",
                1,
            )
        },
        WireEvent {
            outcome: Some("failed".into()),
            failure_class: Some("transport".into()),
            error_code: Some("connect_refused".into()),
            ..step(
                ev(2, "step.finished"),
                "1111111111111111",
                "model",
                "generate",
                1,
            )
        },
        step(
            ev(100, "step.started"),
            "1111111111111111",
            "model",
            "generate",
            2,
        ),
        WireEvent {
            outcome: Some("succeeded".into()),
            cost_estimate: Some(aiops_domain::wire::WireCost {
                micros: 2_500,
                currency: "USD".into(),
                basis: "list-price".into(),
            }),
            ..step(
                ev(400, "step.finished"),
                "1111111111111111",
                "model",
                "generate",
                2,
            )
        },
        WireEvent {
            external_effect: Some(true),
            ..step(
                ev(401, "step.started"),
                "2222222222222222",
                "tool",
                "publish",
                1,
            )
        },
        WireEvent {
            outcome: Some("uncertain".into()),
            uncertain_reason: Some("timeout_after_send".into()),
            ..step(
                ev(5_401, "step.finished"),
                "2222222222222222",
                "tool",
                "publish",
                1,
            )
        },
        WireEvent {
            outcome: Some("uncertain".into()),
            uncertain_reason: Some("timeout_after_send".into()),
            events_emitted: Some(8),
            ..ev(5_402, "operation.finished")
        },
    ]
}

pub(crate) fn op_hex(op: u8) -> String {
    format!("{op:02x}{}", "ab".repeat(15))
}

/// A fresh, migrated database for a test that needs a multi-threaded runtime (where
/// `sqlx::test`'s single-threaded one would let load-generating tasks starve the code
/// under test). Dropped by [`ScratchDb::drop`]; leftovers from failed runs older than an
/// hour are dropped by the next one.
pub(crate) struct ScratchDb {
    pub(crate) pool: PgPool,
    /// The database's URL, for a store built with its own pools.
    pub(crate) url: String,
    admin: PgPool,
    name: String,
}

impl ScratchDb {
    pub(crate) async fn new() -> Self {
        let admin_url = std::env::var("DATABASE_URL").unwrap();
        let admin = PgPool::connect(&admin_url).await.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale: Vec<String> =
            sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname LIKE 'aiops_mt_%'")
                .fetch_all(&admin)
                .await
                .unwrap();
        for name in stale {
            let born = name
                .split('_')
                .nth(2)
                .and_then(|t| t.parse::<u64>().ok())
                .unwrap_or(0);
            if now.saturating_sub(born) > 3_600 {
                let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                )))
                .execute(&admin)
                .await;
            }
        }
        let name = format!(
            "aiops_mt_{now}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let (prefix, _) = admin_url.rsplit_once('/').unwrap();
        let url = format!("{prefix}/{name}");
        let pool = PgPool::connect(&url).await.unwrap();
        Store::from_pool(pool.clone()).migrate().await.unwrap();
        Self {
            pool,
            url,
            admin,
            name,
        }
    }

    pub(crate) async fn drop(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await
        .unwrap();
    }
}

/// `env` with a caller-built `Store` (e.g. `Store::connect_with` for real pools — the
/// isolation tests need the control path on its own connections, not `from_pool`'s one).
pub(crate) async fn env_with_store(store: Store, tune: impl FnOnce(&mut AppState)) -> Env {
    store.create_project("pilot").await.unwrap();
    store.create_project("other").await.unwrap();
    let ingest_key = generate(INGEST_KEY_PREFIX).unwrap();
    store
        .create_ingest_key("pilot", "reference-host", &hash(&ingest_key))
        .await
        .unwrap();
    let other_ingest_key_same_project = generate(INGEST_KEY_PREFIX).unwrap();
    store
        .create_ingest_key(
            "pilot",
            "second-host",
            &hash(&other_ingest_key_same_project),
        )
        .await
        .unwrap();
    let other_ingest_key = generate(INGEST_KEY_PREFIX).unwrap();
    store
        .create_ingest_key("other", "other-host", &hash(&other_ingest_key))
        .await
        .unwrap();
    let later = now().plus_micros(3_600_000_000);
    let token = |actor: &'static str, scopes: &'static [&'static str], project: &'static str| {
        let store = store.clone();
        async move {
            let t = generate(TOKEN_PREFIX).unwrap();
            let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
            store
                .create_token(actor, &scopes, &[project.to_owned()], &hash(&t), later)
                .await
                .unwrap();
            t
        }
    };
    let reader = token("ana", &["read"], "pilot").await;
    let no_read = token("bob", &["operate"], "pilot").await;
    let other_reader = token("eve", &["read"], "other").await;
    let admin = token("ops", &["admin"], "pilot").await;
    let mut state = AppState::new(store.clone(), Config::local(""), system_clock(), None).unwrap();
    tune(&mut state);
    let state = Arc::new(state);
    Env {
        app: router(state.clone()),
        state,
        store,
        ingest_key,
        other_ingest_key,
        other_ingest_key_same_project,
        reader,
        no_read,
        other_reader,
        admin,
    }
}

/// A JSON POST to a `/v1` API route under a token.
pub(crate) fn post_json(path: &str, token: &str, body: &serde_json::Value) -> Request<Body> {
    Request::post(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body.to_string().into())
        .unwrap()
}
