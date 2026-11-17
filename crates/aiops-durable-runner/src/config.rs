//! The adapter's configuration: a JSON file for what is not secret, the environment for
//! the two credentials (the layer's ingest key and the runner's read-only key).

use std::net::SocketAddr;
use std::time::Duration;

use aiops_sdk::StepKind;
use serde::Deserialize;

/// Durable Runner as the adapter sees it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runner {
    /// Names this runner in operation IDs (D-027): `[A-Za-z0-9._-]{1,64}`, unique per
    /// runner journal the project observes.
    pub label: String,
    /// Where `GET /jobs/{id}` is answered (the authority for a run's state).
    pub url: String,
    /// The runner's `--step-timeout-ms`. A call that outlives it was given up by the
    /// runner: its outcome is unknown to the run even if the handler answered later.
    pub step_timeout_ms: u64,
}

/// One allowlisted handler the runner calls through the adapter (`POST /h/{route}`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub route: String,
    pub kind: RouteKind,
    /// The real handler: `http://` or `https://`.
    pub upstream: String,
    #[serde(default)]
    pub external_effect: bool,
    #[serde(default)]
    pub model: Option<String>,
    /// A list price, to label a model reply's token usage with an estimated cost.
    #[serde(default)]
    pub price: Option<Price>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Model,
    Tool,
    Job,
    Validation,
}

impl RouteKind {
    pub fn step_kind(self) -> StepKind {
        match self {
            Self::Model => StepKind::Model,
            Self::Tool => StepKind::Tool,
            Self::Job => StepKind::Job,
            Self::Validation => StepKind::Validation,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input_micros_per_token: u64,
    pub output_micros_per_token: u64,
    pub currency: String,
    pub basis: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub listen: SocketAddr,
    pub runner: Runner,
    /// The operation name every observed run is reported under.
    pub workflow: String,
    pub routes: Vec<Route>,
    #[serde(default = "default_max_runs")]
    pub max_runs_tracked: usize,
    #[serde(default = "default_poll_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_max_calls")]
    pub max_calls_in_flight: usize,
}

fn default_max_runs() -> usize {
    65_536
}
fn default_poll_ms() -> u64 {
    500
}
fn default_max_calls() -> usize {
    256
}

/// Everything the adapter runs with.
#[derive(Clone)]
pub struct Config {
    pub file: File,
    /// The runner's read-only key (`DURABLE_READ_KEY`), for `GET /jobs/{id}`.
    pub read_key: String,
    pub endpoint: Option<String>,
    pub ingest_key: Option<String>,
    pub release: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("file", &self.file)
            .field("read_key", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .field(
                "ingest_key",
                &self.ingest_key.as_ref().map(|_| "<redacted>"),
            )
            .field("release", &self.release)
            .finish()
    }
}

/// A handler reply is read up to this many bytes (the runner's own output limit).
pub const MAX_REPLY_BYTES: usize = 256 * 1024;
/// A call body is read up to this many bytes (the runner's 64 KiB input, as a JSON
/// array of numbers, plus the key).
pub const MAX_CALL_BYTES: usize = 1024 * 1024;

impl Config {
    /// Every problem at once, so a bad file is fixed in one pass.
    pub fn validate(&self) -> Result<(), String> {
        let mut errors = Vec::new();
        let f = &self.file;
        let label_ok = !f.runner.label.is_empty()
            && f.runner.label.len() <= 64
            && f.runner
                .label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !label_ok {
            errors.push("runner.label must be [A-Za-z0-9._-]{1,64}".to_owned());
        }
        if f.runner.step_timeout_ms == 0 {
            errors.push("runner.step_timeout_ms must be positive".to_owned());
        }
        let http = |u: &str| u.starts_with("http://") || u.starts_with("https://");
        if !http(&f.runner.url) {
            errors.push("runner.url must be http:// or https://".to_owned());
        }
        if f.routes.is_empty() {
            errors.push("at least one route is needed".to_owned());
        }
        let mut seen = std::collections::HashSet::new();
        for r in &f.routes {
            if !seen.insert(r.route.as_str()) {
                errors.push(format!("route {:?} is listed twice", r.route));
            }
            if r.route.is_empty()
                || !r
                    .route
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            {
                errors.push(format!("route {:?} must be [A-Za-z0-9_-]+", r.route));
            }
            if !http(&r.upstream) {
                errors.push(format!("route {}: upstream must be http(s)", r.route));
            }
        }
        if self.read_key.len() < 32 {
            errors.push("DURABLE_READ_KEY (at least 32 characters) is required: the adapter declares a run's outcome only from the runner's own answer".to_owned());
        }
        if f.max_runs_tracked == 0 || f.max_calls_in_flight == 0 || f.poll_interval_ms == 0 {
            errors.push(
                "max_runs_tracked, max_calls_in_flight and poll_interval_ms must be positive"
                    .to_owned(),
            );
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    pub fn step_timeout(&self) -> Duration {
        Duration::from_millis(self.file.runner.step_timeout_ms)
    }

    /// How long the adapter waits for a handler: past the runner's deadline, so the
    /// runner's own timeout decides the run (the adapter never answers it sooner with a
    /// status the real handler did not give), and the adapter still learns what the
    /// handler did.
    pub fn upstream_timeout(&self) -> Duration {
        self.step_timeout() + Duration::from_secs(1)
    }

    pub fn route(&self, name: &str) -> Option<&Route> {
        self.file.routes.iter().find(|r| r.route == name)
    }
}
