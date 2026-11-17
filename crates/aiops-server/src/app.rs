//! Application state and the router. Ingest, query and console are separately bounded
//! surfaces (D-008, D-022); middleware order follows rust-mastery chapter 12.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aiops_domain::names::NameRegistry;
use aiops_domain::{Limits, Timestamp};
use aiops_store::{IngestKey, Project, Store, Token};
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::Serialize;
use tokio::sync::Semaphore;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::auth::{Buckets, Cached, CredentialCache};
use crate::config::Config;
use crate::error::ApiError;
use crate::error::{REQUEST_ID_HEADER, error_envelope, sanitize_request_id};
use crate::locks::{LockStats, LockView};
use crate::{console, ingest, query};

pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        Timestamp::from_unix_micros(micros)
    })
}

/// Counters the collector keeps about itself (`GET /v1/collector/stats`).
#[derive(Default)]
pub struct Metrics {
    pub batches: AtomicU64,
    pub events_accepted: AtomicU64,
    pub events_duplicate: AtomicU64,
    pub events_refused: AtomicU64,
    pub events_conflicting: AtomicU64,
    pub attributes_withheld: AtomicU64,
    pub redactions: AtomicU64,
    pub unauthenticated: AtomicU64,
    pub shed: AtomicU64,
    pub rate_limited: AtomicU64,
    pub denied: AtomicU64,
    pub store_unavailable: AtomicU64,
    /// Credential lookups refused because the lookup budget was spent.
    pub auth_shed: AtomicU64,
}

#[derive(Serialize)]
pub struct MetricsView {
    pub batches: u64,
    pub events_accepted: u64,
    pub events_duplicate: u64,
    pub events_refused: u64,
    pub events_conflicting: u64,
    pub attributes_withheld: u64,
    pub redactions: u64,
    pub unauthenticated: u64,
    pub shed: u64,
    pub rate_limited: u64,
    pub denied: u64,
    pub store_unavailable: u64,
    pub auth_shed: u64,
    /// Wait and hold times of the in-process locks (D-026).
    pub locks: LocksView,
}

/// The in-process locks: the ingest path's (the global name registry, the ingest-key
/// cache, the per-key rate limiter) and the query path's.
#[derive(Serialize)]
pub struct LocksView {
    pub names: LockView,
    pub ingest_key_cache: LockView,
    pub ingest_rate_limit: LockView,
    pub token_cache: LockView,
    pub query_rate_limit: LockView,
    pub project_cache: LockView,
}

impl Metrics {
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn view(&self, locks: LocksView) -> MetricsView {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        MetricsView {
            batches: g(&self.batches),
            events_accepted: g(&self.events_accepted),
            events_duplicate: g(&self.events_duplicate),
            events_refused: g(&self.events_refused),
            events_conflicting: g(&self.events_conflicting),
            attributes_withheld: g(&self.attributes_withheld),
            redactions: g(&self.redactions),
            unauthenticated: g(&self.unauthenticated),
            shed: g(&self.shed),
            rate_limited: g(&self.rate_limited),
            denied: g(&self.denied),
            store_unavailable: g(&self.store_unavailable),
            auth_shed: g(&self.auth_shed),
            locks,
        }
    }
}

pub struct AppState {
    pub store: Store,
    pub config: Config,
    pub limits: Limits,
    pub clock: Clock,
    pub ingest_gate: Arc<Semaphore>,
    /// In-flight ingest body bytes, in KiB permits (review N2): a body is read only under
    /// this budget, after authentication.
    pub ingest_body_budget: Arc<Semaphore>,
    /// Credential lookups that missed the cache and may query PostgreSQL, at most this
    /// many at once (review finding 4): unknown keys can never tie up ingest.
    pub lookup_gate: Arc<Semaphore>,
    pub ingest_keys: CredentialCache<IngestKey>,
    pub tokens: CredentialCache<Token>,
    pub ingest_buckets: Buckets<Uuid>,
    pub query_buckets: Buckets<Uuid>,
    /// Per project, loaded lazily from `known_names`. One lock for every project, held
    /// while a batch's names are admitted; its cost is measured (D-026).
    pub names: Mutex<HashMap<Uuid, NameRegistry>>,
    pub names_lock: LockStats,
    pub metrics: Metrics,
    pub console_csp: Option<HeaderValue>,
    /// Slug → project. Projects are never renamed or deleted, so entries never go stale;
    /// unknown slugs are not cached (they are denied and audited every time).
    pub projects: Mutex<HashMap<String, Project>>,
    pub projects_lock: LockStats,
}

/// Distinct projects held in the cache at most.
const PROJECT_CACHE_CAP: usize = 1_000;

pub type SharedState = Arc<AppState>;

impl AppState {
    /// `console_csp` comes from [`console::content_security_policy`], computed off the
    /// async workers by the caller (it reads a file).
    pub fn new(
        store: Store,
        config: Config,
        clock: Clock,
        console_csp: Option<HeaderValue>,
    ) -> Self {
        Self {
            ingest_gate: Arc::new(Semaphore::new(config.ingest_max_in_flight)),
            ingest_body_budget: Arc::new(Semaphore::new(config.ingest_body_budget_kib)),
            lookup_gate: Arc::new(Semaphore::new(config.auth_lookups_in_flight)),
            ingest_keys: CredentialCache::new(
                Duration::from_secs(30),
                Duration::from_secs(5),
                10_000,
            ),
            tokens: CredentialCache::new(Duration::from_secs(30), Duration::from_secs(5), 10_000),
            ingest_buckets: Buckets::new(config.ingest_events_per_sec, config.ingest_burst_events),
            query_buckets: Buckets::new(config.query_requests_per_sec, config.query_burst),
            names: Mutex::new(HashMap::new()),
            names_lock: LockStats::default(),
            metrics: Metrics::default(),
            projects: Mutex::new(HashMap::new()),
            projects_lock: LockStats::default(),
            limits: Limits::default(),
            store,
            config,
            clock,
            console_csp,
        }
    }

    pub fn now(&self) -> Timestamp {
        (self.clock)()
    }

    /// A credential's record: from the cache when fresh, else from PostgreSQL under the
    /// lookup budget. With the budget spent, a recently valid credential is still served
    /// from the cache (stale) and anything else is refused as overloaded, without a query.
    pub async fn resolve<V, Fut>(
        &self,
        cache: &CredentialCache<V>,
        digest: [u8; 32],
        lookup: impl FnOnce() -> Fut,
    ) -> Result<Option<V>, ApiError>
    where
        V: Clone,
        Fut: Future<Output = Result<Option<V>, aiops_store::StoreError>>,
    {
        let stale = match cache.get(&digest) {
            Cached::Fresh(found) => return Ok(found),
            Cached::Stale(v) => Some(v),
            Cached::Miss => None,
        };
        let Ok(_permit) = self.lookup_gate.try_acquire() else {
            Metrics::add(&self.metrics.auth_shed, 1);
            return stale.map(Some).ok_or(ApiError::Overloaded);
        };
        match tokio::time::timeout(Duration::from_secs(2), lookup()).await {
            Ok(Ok(found)) => {
                cache.put(digest, found.clone());
                Ok(found)
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(ApiError::StoreUnavailable),
        }
    }

    pub fn project_cache(&self, slug: &str) -> Option<Project> {
        self.projects_lock.lock(&self.projects)?.get(slug).cloned()
    }

    /// Every counter, with the locks' wait and hold times.
    pub fn metrics_view(&self) -> MetricsView {
        self.metrics.view(LocksView {
            names: self.names_lock.view(),
            ingest_key_cache: self.ingest_keys.lock_stats.view(),
            ingest_rate_limit: self.ingest_buckets.lock_stats.view(),
            token_cache: self.tokens.lock_stats.view(),
            query_rate_limit: self.query_buckets.lock_stats.view(),
            project_cache: self.projects_lock.view(),
        })
    }

    pub fn remember_project(&self, project: &Project) {
        if let Some(mut map) = self.projects_lock.lock(&self.projects) {
            if map.len() >= PROJECT_CACHE_CAP {
                map.clear();
            }
            map.insert(project.slug.clone(), project.clone());
        }
    }
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct Ready {
    ready: bool,
    store: &'static str,
}

async fn readyz(
    axum::extract::State(app): axum::extract::State<SharedState>,
) -> (StatusCode, Json<Ready>) {
    let ok = matches!(
        tokio::time::timeout(Duration::from_secs(1), app.store.ping()).await,
        Ok(Ok(()))
    );
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(Ready {
            ready: ok,
            store: if ok { "reachable" } else { "unreachable" },
        }),
    )
}

pub fn router(state: SharedState) -> Router {
    let body_limit = state.limits.max_body_bytes;
    let ingest_routes = Router::new()
        .route("/v1/ingest", post(ingest::ingest))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(RequestBodyLimitLayer::new(body_limit));
    let query_routes = Router::new()
        .route("/v1/projects", get(query::projects))
        .route(
            "/v1/projects/{project}/operations",
            get(query::list_operations),
        )
        .route(
            "/v1/projects/{project}/operations/{operation_id}",
            get(query::timeline),
        )
        .route("/v1/collector/stats", get(query::stats))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ));
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(ingest_routes)
        .merge(query_routes);
    if let Some(console) = console::routes(&state) {
        app = app.merge(console);
    }
    let request_id = HeaderName::from_static(REQUEST_ID_HEADER);
    let timeout = state.config.request_timeout;
    let stack = ServiceBuilder::new()
        .layer(middleware::from_fn(sanitize_request_id))
        .layer(SetRequestIdLayer::new(request_id.clone(), MakeRequestUuid))
        .layer(PropagateRequestIdLayer::new(request_id))
        .layer(SetSensitiveRequestHeadersLayer::new([
            header::AUTHORIZATION,
        ]))
        .layer(TraceLayer::new_for_http())
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(middleware::from_fn(error_envelope))
        .layer(CatchPanicLayer::new())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            timeout,
        ));
    app.layer(stack).with_state(state)
}
