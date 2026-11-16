//! Application state and the router. Ingest, query and console are separately bounded
//! surfaces (D-008, D-022); middleware order follows rust-mastery chapter 12.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aiops_domain::names::NameRegistry;
use aiops_domain::{Limits, Timestamp};
use aiops_store::{IngestKey, Store, Token};
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

use crate::auth::{Buckets, CredentialCache};
use crate::config::Config;
use crate::error::{REQUEST_ID_HEADER, error_envelope, sanitize_request_id};
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
}

impl Metrics {
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn view(&self) -> MetricsView {
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
        }
    }
}

pub struct AppState {
    pub store: Store,
    pub config: Config,
    pub limits: Limits,
    pub clock: Clock,
    pub ingest_gate: Arc<Semaphore>,
    pub ingest_keys: CredentialCache<IngestKey>,
    pub tokens: CredentialCache<Token>,
    pub ingest_buckets: Buckets<Uuid>,
    pub query_buckets: Buckets<Uuid>,
    /// Per project, loaded lazily from `known_names`.
    pub names: Mutex<HashMap<Uuid, NameRegistry>>,
    pub metrics: Metrics,
    pub console_csp: Option<HeaderValue>,
}

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
            ingest_keys: CredentialCache::new(
                Duration::from_secs(30),
                Duration::from_secs(5),
                10_000,
            ),
            tokens: CredentialCache::new(Duration::from_secs(30), Duration::from_secs(5), 10_000),
            ingest_buckets: Buckets::new(config.ingest_events_per_sec, config.ingest_burst_events),
            query_buckets: Buckets::new(config.query_requests_per_sec, config.query_burst),
            names: Mutex::new(HashMap::new()),
            metrics: Metrics::default(),
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
