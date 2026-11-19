//! Typed configuration, read once from the environment and validated with every error
//! reported together.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// A job runner whose own answer the console may show beside a timeline (D-027): Durable
/// Runner, read with its read-only key.
#[derive(Clone)]
pub struct AuthoritySource {
    /// The runner label the adapter reports under (`job.runner`).
    pub label: String,
    pub url: String,
    pub read_key: String,
}

impl std::fmt::Debug for AuthoritySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthoritySource")
            .field("label", &self.label)
            .field("url", &self.url)
            .field("read_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    pub console_dir: Option<PathBuf>,
    /// Ingest requests in flight before the collector sheds (503 + Retry-After).
    pub ingest_max_in_flight: usize,
    /// Ingest body bytes in flight at most, in KiB (review N2): bodies are read after
    /// authentication and only under this budget; past it the request is shed (503).
    pub ingest_body_budget_kib: usize,
    /// Uncached credential lookups (each a PostgreSQL query) in flight at most.
    pub auth_lookups_in_flight: usize,
    /// Per ingest key: sustained events per second and burst.
    pub ingest_events_per_sec: u32,
    pub ingest_burst_events: u32,
    /// Per API token: sustained requests per second and burst.
    pub query_requests_per_sec: u32,
    pub query_burst: u32,
    pub request_timeout: Duration,
    /// Deadline for one batch's store transaction.
    pub store_deadline: Duration,
    /// Deadline for one audit write (denials fail closed past it).
    pub audit_deadline: Duration,
    pub ingest_connections: u32,
    pub query_connections: u32,
    pub control_connections: u32,
    /// Events loaded for one timeline at most.
    pub timeline_max_events: usize,
    pub retention_sweep_every: Option<Duration>,
    pub shutdown_grace: Duration,
    /// A request's headers must arrive within this; idle keep-alive connections close
    /// after it too.
    pub header_read_timeout: Duration,
    /// `AIOPS_AUTHORITY_DURABLE_RUNNER=<label>=<url>` and
    /// `AIOPS_AUTHORITY_DURABLE_RUNNER_READ_KEY`: the runner the console may ask.
    pub authority: Option<AuthoritySource>,
    /// `AIOPS_CONTROL_REFERENCE_HOST=<slug>=<url>`: the project's control adapter (D-007).
    /// v1 wires one project to the reference host; a project without an adapter denies
    /// every control call, audited.
    pub control_reference_host: Option<(String, String)>,
    /// Accepted connections at most.
    pub max_connections: usize,
    /// A response write making no progress for this long ends its connection (review N3).
    pub write_stall_timeout: Duration,
    /// Requests served on one connection at most (review N3).
    pub max_requests_per_connection: u32,
    /// A connection is closed after this long, after the request in flight (review N3).
    pub connection_lifetime: Duration,
}

impl Config {
    /// Defaults for tests and local development.
    pub fn local(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
            listen: SocketAddr::from(([127, 0, 0, 1], 57_400)),
            console_dir: None,
            ingest_max_in_flight: 64,
            ingest_body_budget_kib: 32 * 1024,
            auth_lookups_in_flight: 4,
            ingest_events_per_sec: 20_000,
            ingest_burst_events: 100_000,
            query_requests_per_sec: 20,
            query_burst: 60,
            request_timeout: Duration::from_secs(10),
            store_deadline: Duration::from_secs(5),
            audit_deadline: Duration::from_secs(2),
            ingest_connections: 16,
            query_connections: 8,
            control_connections: 2,
            timeline_max_events: 5_000,
            retention_sweep_every: Some(Duration::from_secs(300)),
            shutdown_grace: Duration::from_secs(10),
            header_read_timeout: Duration::from_secs(10),
            max_connections: 2_048,
            write_stall_timeout: Duration::from_secs(10),
            max_requests_per_connection: 1_000,
            connection_lifetime: Duration::from_secs(300),
            authority: None,
            control_reference_host: None,
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one env var per stanza; splitting obscures the surface it describes"
    )]
    pub fn from_env() -> Result<Self, String> {
        let mut errors = Vec::new();
        let database_url = std::env::var("AIOPS_DATABASE_URL").unwrap_or_else(|_| {
            errors.push("AIOPS_DATABASE_URL is required".to_owned());
            String::new()
        });
        let mut cfg = Self::local(database_url);
        let mut num = |name: &str, target: &mut u64| {
            if let Ok(v) = std::env::var(name) {
                match v.parse::<u64>() {
                    Ok(n) if n > 0 => *target = n,
                    _ => errors.push(format!("{name} must be a positive integer")),
                }
            }
        };
        let mut in_flight = cfg.ingest_max_in_flight as u64;
        let mut body_budget_mib = (cfg.ingest_body_budget_kib / 1024) as u64;
        let mut rate = u64::from(cfg.ingest_events_per_sec);
        let mut burst = u64::from(cfg.ingest_burst_events);
        let mut qrate = u64::from(cfg.query_requests_per_sec);
        let mut qburst = u64::from(cfg.query_burst);
        let mut ingest_conns = u64::from(cfg.ingest_connections);
        let mut sweep_secs = cfg.retention_sweep_every.map_or(0, |d| d.as_secs());
        let mut header_secs = cfg.header_read_timeout.as_secs();
        let mut max_conns = cfg.max_connections as u64;
        let mut stall_secs = cfg.write_stall_timeout.as_secs();
        let mut per_conn = u64::from(cfg.max_requests_per_connection);
        let mut lifetime_secs = cfg.connection_lifetime.as_secs();
        num("AIOPS_INGEST_MAX_IN_FLIGHT", &mut in_flight);
        num("AIOPS_INGEST_BODY_BUDGET_MIB", &mut body_budget_mib);
        num("AIOPS_INGEST_EVENTS_PER_SEC", &mut rate);
        num("AIOPS_INGEST_BURST_EVENTS", &mut burst);
        num("AIOPS_QUERY_REQUESTS_PER_SEC", &mut qrate);
        num("AIOPS_QUERY_BURST", &mut qburst);
        num("AIOPS_INGEST_CONNECTIONS", &mut ingest_conns);
        num("AIOPS_RETENTION_SWEEP_SECS", &mut sweep_secs);
        num("AIOPS_HEADER_READ_TIMEOUT_SECS", &mut header_secs);
        num("AIOPS_MAX_CONNECTIONS", &mut max_conns);
        num("AIOPS_WRITE_STALL_TIMEOUT_SECS", &mut stall_secs);
        num("AIOPS_MAX_REQUESTS_PER_CONNECTION", &mut per_conn);
        num("AIOPS_CONNECTION_LIFETIME_SECS", &mut lifetime_secs);
        let to_u32 = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        cfg.ingest_max_in_flight = usize::try_from(in_flight).unwrap_or(usize::MAX);
        cfg.ingest_body_budget_kib =
            usize::try_from(body_budget_mib.saturating_mul(1024)).unwrap_or(usize::MAX);
        cfg.ingest_events_per_sec = to_u32(rate);
        cfg.ingest_burst_events = to_u32(burst);
        cfg.query_requests_per_sec = to_u32(qrate);
        cfg.query_burst = to_u32(qburst);
        cfg.ingest_connections = to_u32(ingest_conns);
        cfg.retention_sweep_every = Some(Duration::from_secs(sweep_secs));
        cfg.header_read_timeout = Duration::from_secs(header_secs);
        cfg.max_connections = usize::try_from(max_conns).unwrap_or(usize::MAX);
        cfg.write_stall_timeout = Duration::from_secs(stall_secs);
        cfg.max_requests_per_connection = to_u32(per_conn);
        cfg.connection_lifetime = Duration::from_secs(lifetime_secs);
        if let Ok(v) = std::env::var("AIOPS_LISTEN") {
            match v.parse() {
                Ok(addr) => cfg.listen = addr,
                Err(_) => errors.push("AIOPS_LISTEN must be host:port".to_owned()),
            }
        }
        if let Ok(v) = std::env::var("AIOPS_CONSOLE_DIR") {
            cfg.console_dir = Some(PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("AIOPS_AUTHORITY_DURABLE_RUNNER") {
            let key = std::env::var("AIOPS_AUTHORITY_DURABLE_RUNNER_READ_KEY").unwrap_or_default();
            match v.split_once('=') {
                Some((label, url))
                    if !label.is_empty()
                        && (url.starts_with("http://") || url.starts_with("https://")) =>
                {
                    if key.len() < 32 {
                        errors.push(
                            "AIOPS_AUTHORITY_DURABLE_RUNNER_READ_KEY (the runner's read-only key, at least 32 characters) is required with AIOPS_AUTHORITY_DURABLE_RUNNER".to_owned(),
                        );
                    }
                    cfg.authority = Some(AuthoritySource {
                        label: label.to_owned(),
                        url: url.trim_end_matches('/').to_owned(),
                        read_key: key,
                    });
                }
                _ => errors.push(
                    "AIOPS_AUTHORITY_DURABLE_RUNNER must be <label>=http(s)://host:port".to_owned(),
                ),
            }
        }
        if let Ok(v) = std::env::var("AIOPS_CONTROL_REFERENCE_HOST") {
            match v.split_once('=') {
                Some((slug, url))
                    if !slug.is_empty()
                        && (url.starts_with("http://") || url.starts_with("https://")) =>
                {
                    cfg.control_reference_host =
                        Some((slug.to_owned(), url.trim_end_matches('/').to_owned()));
                }
                _ => errors.push(
                    "AIOPS_CONTROL_REFERENCE_HOST must be <project-slug>=http(s)://host:port"
                        .to_owned(),
                ),
            }
        }
        if errors.is_empty() {
            Ok(cfg)
        } else {
            Err(errors.join("; "))
        }
    }
}
