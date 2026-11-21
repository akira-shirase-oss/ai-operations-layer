//! PostgreSQL access for the AI Operations Layer.
//!
//! Three pools, never shared (D-008, D-022): **ingest** (batch upserts), **query**
//! (timelines and lists) and **control** (audit now, control records in A3). Each has its
//! own size, acquire timeout and statement timeout, so an ingest burst or a slow query
//! cannot starve an audit write.

use std::collections::HashSet;
use std::time::Duration;

use aiops_domain::summary::summarize;
use aiops_domain::timeline::{InstanceStats, StoredEvent};
use aiops_domain::validate::Source;
use aiops_domain::wire::{WireEvent, WireStats};
use aiops_domain::{
    Event, EventId, InstanceId, Limits, Mode, OperationId, Outcome, PolicyMarks, Timestamp,
    Validator,
};
pub use sqlx::PgPool as RevocationPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
pub use sqlx::postgres::{PgConnectOptions, PgListener};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

/// The columns of an [`OperationRow`], in the order [`operation_row`] reads them.
macro_rules! operation_columns {
    () => {
        "SELECT operation_id, name, request_id, release, first_event_time, last_event_time, last_received_at,
                host_events, gap_reports, reported_dropped, finish_outcome, finish_detail, events_emitted,
                failed_attempts, uncertain_attempts, cost_estimate_micros, cost_currency, finish_conflict,
                first_received_at, multi_source, finish_code"
    };
}

/// Store failures. `Display` is safe to show; the database's own message is only reachable
/// through `source()`, for logs.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database is unreachable, overloaded or timed out. Retryable.
    #[error("the store is unavailable")]
    Unavailable(#[source] sqlx::Error),
    /// Anything else the database refused. Not retryable as-is.
    #[error("the store refused the operation")]
    Database(#[source] sqlx::Error),
    #[error("a record with that name already exists")]
    AlreadyExists,
    /// The batch held text the database cannot store; nothing of it was written.
    #[error("the batch holds text the store cannot keep")]
    Unstorable(#[source] sqlx::Error),
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// The batch's source instance was first used by another ingest key (review finding
    /// 7): the batch is refused whole, so one host cannot write under another's identity.
    #[error("the source instance belongs to another ingest key")]
    InstanceOwnedByAnotherKey,
    #[error("migration failed")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// A stored row no longer validates (should be impossible; reported, never hidden).
    #[error("a stored event no longer validates")]
    Corrupt,
}

impl StoreError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// How a PostgreSQL error code (SQLSTATE) is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlStateClass {
    /// The database is temporarily unable to serve: retry (503).
    Unavailable,
    /// A unique constraint: the record exists.
    AlreadyExists,
    /// Text the database cannot store (U+0000 in jsonb, an unencodable character): the
    /// request is refused as invalid, never a 500 (review N1).
    Unstorable,
    /// Anything else: not retryable as-is.
    Other,
}

/// Classifies a SQLSTATE.
pub fn classify_sqlstate(code: &str) -> SqlStateClass {
    match code {
        "23505" => SqlStateClass::AlreadyExists,
        // 22P05 untranslatable character (U+0000 in jsonb), 22021 character not in repertoire.
        "22P05" | "22021" => SqlStateClass::Unstorable,
        // 57014 statement timeout; 53300 too many connections; 57P01 admin shutdown (a
        // restart), 57P02 crash shutdown, 57P03 cannot connect now (review finding 8).
        "57014" | "53300" | "57P01" | "57P02" | "57P03" => SqlStateClass::Unavailable,
        // Class 08: connection exceptions.
        c if c.starts_with("08") => SqlStateClass::Unavailable,
        _ => SqlStateClass::Other,
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            // A connection that died mid-statement surfaces as I/O or a protocol error.
            sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Io(_)
            | sqlx::Error::Protocol(_)
            | sqlx::Error::WorkerCrashed => Self::Unavailable(e),
            sqlx::Error::Database(db) => match db.code().as_deref().map(classify_sqlstate) {
                Some(SqlStateClass::AlreadyExists) => Self::AlreadyExists,
                Some(SqlStateClass::Unstorable) => Self::Unstorable(e),
                Some(SqlStateClass::Unavailable) => Self::Unavailable(e),
                _ => Self::Database(e),
            },
            _ => Self::Database(e),
        }
    }
}

/// One operation's summary by primary key (`$1` project, `$2` operation); public so its plan
/// can be checked (review N8).
pub const OPERATION_SUMMARY_SQL: &str = concat!(
    operation_columns!(),
    " FROM operations WHERE project_id = $1 AND operation_id = $2"
);

fn operation_row(r: &sqlx::postgres::PgRow) -> Result<OperationRow, StoreError> {
    let id: Uuid = r.get(0);
    Ok(OperationRow {
        operation_id: OperationId::from_bytes(*id.as_bytes()).ok_or(StoreError::Corrupt)?,
        name: r.get(1),
        request_id: r.get(2),
        release: r.get(3),
        first_event_time: from_odt(r.get(4)),
        last_event_time: from_odt(r.get(5)),
        last_received_at: from_odt(r.get(6)),
        host_events: r.get(7),
        gap_reports: r.get(8),
        reported_dropped: r.get(9),
        finish_outcome: r.get(10),
        finish_detail: r.get(11),
        events_emitted: r.get(12),
        failed_attempts: r.get(13),
        uncertain_attempts: r.get(14),
        cost_estimate_micros: r.get(15),
        cost_currency: r.get(16),
        finish_conflict: r.get(17),
        first_received_at: from_odt(r.get(18)),
        multi_source: r.get(19),
        finish_code: r.get(20),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_the_database_cannot_store_is_its_own_class() {
        // Review N1: U+0000 in jsonb (22P05) must not surface as a 500.
        assert_eq!(classify_sqlstate("22P05"), SqlStateClass::Unstorable);
        assert_eq!(classify_sqlstate("22021"), SqlStateClass::Unstorable);
    }

    #[test]
    fn a_database_restart_or_lost_connection_is_retryable() {
        // 57P01 admin_shutdown (a restart), 57P02 crash_shutdown, class 08 connection errors.
        for code in [
            "57P01", "57P02", "57P03", "08000", "08003", "08006", "08001", "08004", "57014",
            "53300",
        ] {
            assert_eq!(
                classify_sqlstate(code),
                SqlStateClass::Unavailable,
                "{code}"
            );
        }
        assert_eq!(classify_sqlstate("23505"), SqlStateClass::AlreadyExists);
        for code in ["42601", "23503", "22P02"] {
            assert_eq!(classify_sqlstate(code), SqlStateClass::Other, "{code}");
        }
    }
}

/// Pool sizes and deadlines. The defaults are development values until the A4 load runs
/// set them (D-022).
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub url: String,
    pub ingest_connections: u32,
    pub query_connections: u32,
    pub control_connections: u32,
    /// Credential lookups (keys, tokens): their own pool, so unknown-key floods never
    /// take an ingest or query connection.
    pub auth_connections: u32,
    /// The retention sweep's own connection (never an ingest, query or audit one).
    pub maintenance_connections: u32,
    /// Audit writes and reads: their own pool, so a burst of audited denials never takes
    /// the control connections an operator needs to revoke a key or a token.
    pub audit_connections: u32,
    pub acquire_timeout: Duration,
    pub ingest_statement_timeout: Duration,
    pub query_statement_timeout: Duration,
    pub control_statement_timeout: Duration,
}

impl StoreConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ingest_connections: 16,
            query_connections: 8,
            control_connections: 2,
            auth_connections: 4,
            maintenance_connections: 1,
            audit_connections: 2,
            acquire_timeout: Duration::from_secs(2),
            ingest_statement_timeout: Duration::from_secs(5),
            query_statement_timeout: Duration::from_secs(3),
            control_statement_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    ingest: PgPool,
    query: PgPool,
    control: PgPool,
    auth: PgPool,
    maintenance: PgPool,
    audit: PgPool,
    /// Fault hook, debug builds only (never compiled into a release binary): while
    /// set, every audit write fails — `deny`, `control_decide` and the admin revokes
    /// must then refuse, which is what their tests assert.
    #[cfg(debug_assertions)]
    fail_audit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Same, for the revocation catch-up read: the watcher's resync must
    /// retry through a failed read and recover, which its test proves by
    /// failing reads then clearing the hook.
    #[cfg(debug_assertions)]
    fail_revocation: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// One validated event ready to store: the event, what policy changed, its stored JSON.
#[derive(Debug, Clone)]
pub struct PreparedEvent {
    pub event: Event,
    pub marks: PolicyMarks,
    pub body: String,
}

impl PreparedEvent {
    pub fn new(event: Event, marks: PolicyMarks) -> Result<Self, serde_json::Error> {
        let body = serde_json::to_string(&event.to_wire())?;
        Ok(Self { event, marks, body })
    }
}

/// One authenticated, validated batch.
#[derive(Debug)]
pub struct IngestBatch<'a> {
    pub project_id: Uuid,
    pub ingest_key_id: Uuid,
    pub source: &'a Source,
    pub stats: WireStats,
    pub received_at: Timestamp,
    pub events: Vec<PreparedEvent>,
    /// Events of this batch the collector refused (kept on the source's counters).
    pub refused: u32,
    pub new_names: Vec<String>,
    /// Known names this batch used (their `last_seen` is refreshed, at most daily).
    pub touched_names: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IngestResult {
    pub accepted: u32,
    pub duplicates: u32,
    /// Event IDs already stored with different content or provenance (first write wins).
    pub conflicts: Vec<EventId>,
}

#[derive(Debug, Clone)]
pub struct IngestKey {
    pub id: Uuid,
    pub name: String,
    pub project_id: Uuid,
    pub project_slug: String,
    pub opt_in_keys: Vec<String>,
    pub revoked: bool,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub id: Uuid,
    pub actor: String,
    pub scopes: Vec<String>,
    pub project_ids: Vec<Uuid>,
    pub expires_at: Timestamp,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: Uuid,
    pub slug: String,
}

/// Credential metadata for the admin surface (D-034): everything about a key or a
/// token except the secret hash, which never leaves the store.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: Uuid,
    pub slug: String,
    pub opt_in_keys: Vec<String>,
    pub created_at: Timestamp,
}

/// One ingest key's metadata (the hash is never selected).
#[derive(Debug, Clone)]
pub struct IngestKeyInfo {
    pub id: Uuid,
    pub name: String,
    pub project_slug: String,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

/// One token's metadata (the hash is never selected).
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub id: Uuid,
    pub actor: String,
    pub scopes: Vec<String>,
    pub project_slugs: Vec<String>,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

/// One row of the operation list.
#[derive(Debug, Clone)]
pub struct OperationRow {
    pub operation_id: OperationId,
    pub name: Option<String>,
    pub request_id: Option<String>,
    pub release: Option<String>,
    pub first_event_time: Timestamp,
    pub last_event_time: Timestamp,
    pub first_received_at: Timestamp,
    pub last_received_at: Timestamp,
    pub host_events: i32,
    pub gap_reports: i32,
    pub reported_dropped: i64,
    pub finish_outcome: Option<String>,
    pub finish_detail: Option<String>,
    /// The host's `error_code` on the kept finish.
    pub finish_code: Option<String>,
    /// Hosts declared different outcomes.
    pub finish_conflict: bool,
    /// Events came from more than one source instance.
    pub multi_source: bool,
    pub events_emitted: Option<i32>,
    pub failed_attempts: i32,
    pub uncertain_attempts: i32,
    pub cost_estimate_micros: i64,
    pub cost_currency: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub limit: i64,
    /// Keyset cursor: operations that started strictly before this point.
    pub before: Option<(Timestamp, OperationId)>,
    pub request_id: Option<String>,
    /// Only this operation.
    pub operation_id: Option<OperationId>,
    /// The list's own status words: `succeeded`, `failed`, `uncertain`, `conflicting`,
    /// `no_finish_observed` (the caller validates them).
    pub status: Option<&'static str>,
    pub name: Option<String>,
    pub release: Option<String>,
    /// Operations whose first event time is in `[from, to)`.
    pub from: Option<Timestamp>,
    pub to: Option<Timestamp>,
}

/// One release's operations in a window, counted from the same summaries the list reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseCounts {
    pub release: String,
    pub operations: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub uncertain: i64,
    pub conflicting: i64,
    pub no_finish_observed: i64,
    /// Finished, single-source operations that received fewer events than they declared.
    pub missing_events: i64,
    pub reported_dropped: i64,
    pub failed_attempts: i64,
    pub uncertain_attempts: i64,
    pub cost_estimate_micros: i64,
    /// Distinct currencies among the estimates (more than one: not summable).
    pub currencies: i64,
}

/// The SQL every status filter shares with the list's status (`list_state`).
macro_rules! status_matches {
    ($p:literal) => {
        concat!(
            "(",
            $p,
            "::text IS NULL",
            " OR (",
            $p,
            " = 'conflicting' AND finish_conflict)",
            " OR (",
            $p,
            " = 'no_finish_observed' AND NOT finish_conflict AND finish_outcome IS NULL)",
            " OR (NOT finish_conflict AND finish_outcome = ",
            $p,
            "))"
        )
    };
}

#[derive(Debug, Clone)]
pub struct AuditEntry<'a> {
    pub actor: &'a str,
    pub token_id: Option<Uuid>,
    pub action: &'a str,
    pub project_slug: Option<&'a str>,
    pub target: Option<&'a str>,
    pub allowed: bool,
    pub reason: &'a str,
}

/// The audit read's filter: a keyset window plus equality bounds on the fields an
/// investigator actually narrows by.
#[derive(Debug, Default)]
pub struct AuditFilter<'a> {
    /// Rows strictly older than `(at, id)` — the next-page cursor.
    pub before: Option<(Timestamp, i64)>,
    pub action: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub token_id: Option<Uuid>,
    /// `at` in `[from, to)`.
    pub from: Option<Timestamp>,
    pub to: Option<Timestamp>,
    pub limit: i64,
}

/// One `audit_log` row as the read surface shows it: every column, metadata only
/// (the table holds no secrets, digests or bodies — this selects no such field).
#[derive(Debug, Clone)]
pub struct AuditPageRow {
    pub id: i64,
    pub at: Timestamp,
    pub actor: String,
    pub token_id: Option<Uuid>,
    pub action: String,
    pub project_slug: Option<String>,
    pub target: Option<String>,
    pub decision: String,
    pub reason: String,
}

/// What a new control record is committed as (`control_insert`'s input).
#[derive(Debug)]
pub struct NewControlRecord<'a> {
    pub project_id: Uuid,
    pub operation_id: Uuid,
    /// The `ControlKind` name: `reconcile` or `retry`.
    pub action: &'a str,
    pub business_key: &'a str,
    /// The adapter-validated assertion (bounded on the way in).
    pub expected_state: &'a serde_json::Value,
    pub actor: &'a str,
    pub token_id: Uuid,
    /// The first active status for the action: `reconciling` for a reconcile (its read is
    /// about to start), `requested` for a retry (its decision is pending).
    pub initial_status: &'a str,
}

/// One `control_records` row (A3): intent + decision + what the authority answered.
#[derive(Debug, Clone)]
pub struct ControlRecord {
    pub id: Uuid,
    pub project_id: Uuid,
    pub operation_id: Uuid,
    pub action: String,
    pub business_key: String,
    pub expected_state: serde_json::Value,
    pub actor: String,
    pub token_id: Uuid,
    pub status: String,
    pub resolution: Option<String>,
    pub authority_read: Option<serde_json::Value>,
    pub submitted_ref: Option<String>,
    pub approved_by: Option<String>,
    pub approved_at: Option<Timestamp>,
    pub deny_reason: Option<String>,
    pub attempts: i32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Whether `control_insert` created the row or found its unique key already taken
/// (`Existing` is the replay path — the caller compares bodies).
#[derive(Debug)]
pub enum ControlInsert {
    Created(ControlRecord),
    Existing(ControlRecord),
}

/// A resumable record plus its project's slug (for the executor's adapter pick and its
/// audit rows).
#[derive(Debug)]
pub struct ResumableControl {
    pub record: ControlRecord,
    pub project_slug: String,
}

/// Fields a committed transition may set; `None` leaves the column alone.
#[derive(Debug, Default)]
pub struct ControlPatch<'a> {
    pub status: Option<&'a str>,
    pub resolution: Option<&'a str>,
    pub authority_read: Option<&'a serde_json::Value>,
    pub submitted_ref: Option<&'a str>,
    pub deny_reason: Option<&'a str>,
    pub approved_by: Option<&'a str>,
    pub approved_at: Option<Timestamp>,
    /// Added to `attempts` (one per submit try; the re-drive budget).
    pub add_attempts: Option<i32>,
}

/// Retention values (D-016).
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub raw_events: Duration,
    pub summaries: Duration,
    pub audit: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        const DAY: u64 = 24 * 3_600;
        Self {
            raw_events: Duration::from_secs(14 * DAY),
            summaries: Duration::from_secs(90 * DAY),
            audit: Duration::from_secs(365 * DAY),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepResult {
    pub events: u64,
    pub operations: u64,
    pub instances: u64,
    pub audit: u64,
    /// Names unused for the summary retention period (their budget is freed).
    pub names: u64,
    /// The sweep stopped at its batch budget; the next run continues.
    pub more: bool,
}

pub fn to_odt(t: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(t.unix_micros()) * 1_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

pub fn from_odt(t: OffsetDateTime) -> Timestamp {
    let micros = t.unix_timestamp_nanos() / 1_000;
    Timestamp::from_unix_micros(i64::try_from(micros).unwrap_or(i64::MAX))
}

fn uuid128(bytes: &[u8; 16]) -> Uuid {
    Uuid::from_bytes(*bytes)
}

fn sat_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn sat_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

fn finish_parts(outcome: Option<Outcome>) -> (Option<&'static str>, Option<&'static str>) {
    match outcome {
        None => (None, None),
        Some(Outcome::Succeeded) => (Some("succeeded"), None),
        Some(Outcome::Failed(c)) => (Some("failed"), Some(c.as_str())),
        Some(Outcome::Uncertain(r)) => (Some("uncertain"), Some(r.as_str())),
    }
}

/// Audit fields are attacker text (the path's slug and operation id go in raw):
/// store only the two shapes the log is allowed to carry — a project slug
/// (`[a-z0-9][a-z0-9-]{0,62}`) — else `"<invalid>"`, never the bytes sent.
/// `pub` for the fuzz target: arbitrary input must never panic or pass through.
pub fn audit_slug(s: &str) -> &str {
    let b = s.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if ok { s } else { "<invalid>" }
}

/// An audit target is a 32-hex operation id or a 36-char UUID — anything else
/// (newlines, bidi, a raw path tail) is recorded as `"<invalid>"`.
/// `pub` for the fuzz target: arbitrary input must never panic or pass through.
pub fn audit_target(s: &str) -> &str {
    let b = s.as_bytes();
    let hex32 = b.len() == 32 && b.iter().all(u8::is_ascii_hexdigit);
    let uuid = b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    if hex32 || uuid { s } else { "<invalid>" }
}

async fn pool(
    cfg: &StoreConfig,
    base: &PgConnectOptions,
    size: u32,
    statement: Duration,
) -> Result<PgPool, StoreError> {
    let options = base
        .clone()
        .options([("statement_timeout", format!("{}", statement.as_millis()))]);
    Ok(PgPoolOptions::new()
        .max_connections(size)
        .acquire_timeout(cfg.acquire_timeout)
        .connect_with(options)
        .await?)
}

/// The channel the `aiops_notify_revocation` trigger (migration 0008) emits on;
/// the payload is the revoked credential digest as lowercase hex.
pub const REVOCATION_CHANNEL: &str = "aiops_credential_revoked";

/// The `application_name` the revocation listener connects under — how a
/// test (or an operator) finds exactly its backend in `pg_stat_activity`.
pub const REVOCATION_LISTENER_NAME: &str = "aiops-revocation-listener";

/// A dedicated LISTEN connection for credential-revocation notices, off every
/// pool: a saturated serving pool can never delay an eviction. Notifications
/// die with the connection — a caller re-establishing one must clear its
/// credential caches outright rather than trust anything cached in the gap.
/// Connect and LISTEN are bounded (review V1): a dead or half-open endpoint
/// must not hang the watcher.
pub async fn revocation_listener(database_url: &str) -> Result<PgListener, StoreError> {
    let options = database_url
        .parse::<PgConnectOptions>()
        .map_err(StoreError::Unavailable)?
        .application_name(REVOCATION_LISTENER_NAME);
    let pool = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect_with(options),
    )
    .await
    .map_err(|_| StoreError::Unavailable(sqlx::Error::PoolTimedOut))?
    .map_err(StoreError::Unavailable)?;
    let mut listener = PgListener::connect_with(&pool)
        .await
        .map_err(StoreError::Unavailable)?;
    // The pool is the listener's own — nothing else ever closes it.
    listener.ignore_pool_close_event(true);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        listener.listen(REVOCATION_CHANNEL),
    )
    .await
    .map_err(|_| StoreError::Unavailable(sqlx::Error::PoolTimedOut))?
    .map_err(StoreError::Unavailable)?;
    Ok(listener)
}

/// The `application_name` the revocation heartbeat writer connects under.
pub const REVOCATION_WRITER_NAME: &str = "aiops-revocation-writer";

/// A dedicated write pool for the revocation heartbeat, off every tenant
/// pool (review N-I: a beat sent on the serving pool turns a saturated
/// pool into a fake channel gap). Its UPDATE goes through the
/// `revocation_heartbeat` trigger (migration 0010), so an observed beat
/// proves the triggers are live, not just the connection — a backend
/// running `session_replication_role = replica` stays silent and reads
/// as a gap (review N-H).
pub async fn revocation_writer(database_url: &str) -> Result<PgPool, StoreError> {
    let options = database_url
        .parse::<PgConnectOptions>()
        .map_err(StoreError::Unavailable)?
        .application_name(REVOCATION_WRITER_NAME);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    // Server-side deadlines under the client's 2 s beat
                    // timeout (review round 5, finding 3): a lock on the
                    // heartbeat row fails at 1 s, a slow statement at 2 s —
                    // either way the beat returns, never freezes the
                    // watcher.
                    sqlx::query("SET lock_timeout = '1s'")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("SET statement_timeout = '2s'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options),
    )
    .await
    .map_err(|_| StoreError::Unavailable(sqlx::Error::PoolTimedOut))?
    .map_err(StoreError::Unavailable)
}

/// The heartbeat write: fire the channel's UPDATE trigger over `pool`
/// (the watcher's own, never a tenant pool). The trigger emits `heartbeat`
/// on [`REVOCATION_CHANNEL`]. The beat also proves the WHOLE revoke chain,
/// not just its own trigger (review round 5, finding 7):
/// `api_tokens_revoked_notify` disabled (`tgenabled = 'D'`) or armed
/// replica-only (`'R'`) leaves deletes deaf while this table's trigger
/// still fires green — `O` (origin) and `A` (always) are the sound states.
pub async fn revocation_beat(pool: &PgPool) -> Result<(), StoreError> {
    sqlx::query("UPDATE revocation_heartbeat SET beat = now() WHERE id")
        .execute(pool)
        .await?;
    let armed: Option<String> = sqlx::query_scalar(
        "SELECT tgenabled::text FROM pg_trigger
         WHERE tgname = 'api_tokens_revoked_notify'",
    )
    .fetch_optional(pool)
    .await?;
    if !matches!(armed.as_deref(), Some("O" | "A")) {
        return Err(StoreError::Unavailable(sqlx::Error::RowNotFound));
    }
    Ok(())
}

impl Store {
    pub async fn connect(cfg: &StoreConfig) -> Result<Self, StoreError> {
        let base: PgConnectOptions = cfg.url.parse().map_err(StoreError::Unavailable)?;
        Self::connect_with(cfg, &base).await
    }

    /// Like [`Store::connect`], from parsed options (`cfg.url` is ignored).
    pub async fn connect_with(
        cfg: &StoreConfig,
        base: &PgConnectOptions,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            ingest: pool(
                cfg,
                base,
                cfg.ingest_connections,
                cfg.ingest_statement_timeout,
            )
            .await?,
            query: pool(
                cfg,
                base,
                cfg.query_connections,
                cfg.query_statement_timeout,
            )
            .await?,
            control: pool(
                cfg,
                base,
                cfg.control_connections,
                cfg.control_statement_timeout,
            )
            .await?,
            auth: pool(cfg, base, cfg.auth_connections, cfg.query_statement_timeout).await?,
            // A bounded DELETE batch can take longer than a query; 30 s per statement.
            maintenance: pool(
                cfg,
                base,
                cfg.maintenance_connections,
                Duration::from_secs(30),
            )
            .await?,
            audit: pool(
                cfg,
                base,
                cfg.audit_connections,
                cfg.control_statement_timeout,
            )
            .await?,
            #[cfg(debug_assertions)]
            fail_audit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(debug_assertions)]
            fail_revocation: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// One pool for every surface (tests only: it gives up the isolation).
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            ingest: pool.clone(),
            query: pool.clone(),
            auth: pool.clone(),
            maintenance: pool.clone(),
            audit: pool.clone(),
            control: pool,
            #[cfg(debug_assertions)]
            fail_audit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(debug_assertions)]
            fail_revocation: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Debug-build fault hook: while set, `audit` and `control_decide`'s audit
    /// insert fail as an unavailable store — release builds do not carry it.
    #[cfg(debug_assertions)]
    pub fn fail_audit_writes(&self, on: bool) {
        self.fail_audit
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Debug-build fault hook: while set, the revocation catch-up read
    /// fails — the watcher must retry through it (review N-C).
    #[cfg(debug_assertions)]
    pub fn fail_revocation_reads(&self, on: bool) {
        self.fail_revocation
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(debug_assertions)]
    fn audit_failing(&self) -> bool {
        self.fail_audit.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(debug_assertions)]
    fn revocation_failing(&self) -> bool {
        self.fail_revocation
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations")
            .run(&self.control)
            .await
            .map_err(StoreError::Migrate)
    }

    /// Readiness: can the query pool reach the database.
    pub async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1").execute(&self.query).await?;
        Ok(())
    }

    // ---- ingest ---------------------------------------------------------------------

    pub async fn find_ingest_key(&self, hash: &[u8; 32]) -> Result<Option<IngestKey>, StoreError> {
        let row = sqlx::query(
            "SELECT k.id, k.name, k.project_id, p.slug, p.opt_in_attribute_keys, k.revoked_at IS NOT NULL AS revoked
             FROM ingest_keys k JOIN projects p ON p.id = k.project_id WHERE k.key_hash = $1",
        )
        .bind(&hash[..])
        .fetch_optional(&self.auth)
        .await?;
        Ok(row.map(|r| IngestKey {
            id: r.get(0),
            name: r.get(1),
            project_id: r.get(2),
            project_slug: r.get(3),
            opt_in_keys: r.get(4),
            revoked: r.get(5),
        }))
    }

    pub async fn known_names(
        &self,
        project_id: Uuid,
        cap: usize,
    ) -> Result<Vec<(String, Option<[u8; 16]>)>, StoreError> {
        let rows = sqlx::query(
            "SELECT name, ingest_key_id FROM known_names WHERE project_id = $1 LIMIT $2",
        )
        .bind(project_id)
        .bind(i64::try_from(cap).unwrap_or(i64::MAX))
        .fetch_all(&self.ingest)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let key: Option<Uuid> = r.get(1);
                (r.get(0), key.map(|k| *k.as_bytes()))
            })
            .collect())
    }

    /// Stores one batch in one transaction: idempotent event inserts (dedup on
    /// `(project, event_id)`), summaries from the newly stored events only, source
    /// counters, new names, and a content check on duplicates.
    pub async fn ingest(&self, mut batch: IngestBatch<'_>) -> Result<IngestResult, StoreError> {
        let mut sorted = std::mem::take(&mut batch.events);
        // One global lock order for concurrent batches: event IDs ascending (stable, so the
        // first copy of a repeated ID comes first).
        sorted.sort_by_key(|p| p.event.event_id);
        // An ID repeated inside the batch: an identical copy is one duplicate, a different
        // one is a conflict; only the first copy is stored or counted (review finding 3).
        let mut in_batch_duplicates = 0u32;
        let mut in_batch_conflicts = Vec::new();
        let mut events: Vec<PreparedEvent> = Vec::with_capacity(sorted.len());
        for p in sorted {
            match events.last() {
                Some(prev) if prev.event.event_id == p.event.event_id => {
                    if prev.body == p.body && prev.marks == p.marks {
                        in_batch_duplicates += 1;
                    } else {
                        in_batch_conflicts.push(p.event.event_id);
                    }
                }
                _ => events.push(p),
            }
        }
        let received = to_odt(batch.received_at);
        let instance = uuid128(batch.source.instance_id.as_bytes());

        let mut tx = self.ingest.begin().await?;
        let inserted = insert_events(&mut tx, &batch, instance, received, &events).await?;
        let fresh: Vec<&Event> = events
            .iter()
            .filter(|p| inserted.contains(&uuid128(p.event.event_id.as_bytes())))
            .map(|p| &p.event)
            .collect();
        let max_seq = events.iter().map(|p| p.event.seq).max().unwrap_or(0);
        upsert_summaries(&mut tx, &batch, instance, received, &fresh, max_seq).await?;
        // Duplicates: identical content and provenance merge; anything else is a conflict.
        let conflicts = if inserted.len() < events.len() {
            find_conflicts(&mut tx, &batch, instance, &events, &inserted).await?
        } else {
            Vec::new()
        };
        tx.commit().await?;

        let accepted = u32::try_from(inserted.len()).unwrap_or(u32::MAX);
        let total = u32::try_from(events.len()).unwrap_or(u32::MAX);
        let conflict_count = u32::try_from(conflicts.len()).unwrap_or(u32::MAX);
        let mut conflicts = conflicts;
        conflicts.extend(in_batch_conflicts);
        Ok(IngestResult {
            accepted,
            duplicates: total - accepted - conflict_count + in_batch_duplicates,
            conflicts,
        })
    }

    // ---- query ----------------------------------------------------------------------

    pub async fn find_token(&self, hash: &[u8; 32]) -> Result<Option<Token>, StoreError> {
        let row = sqlx::query(
            "SELECT id, actor, scopes, project_ids, expires_at, revoked_at IS NOT NULL
             FROM api_tokens WHERE token_hash = $1",
        )
        .bind(&hash[..])
        .fetch_optional(&self.auth)
        .await?;
        Ok(row.map(|r| Token {
            id: r.get(0),
            actor: r.get(1),
            scopes: r.get(2),
            project_ids: r.get(3),
            expires_at: from_odt(r.get(4)),
            revoked: r.get(5),
        }))
    }

    /// Token digests revoked within `max_age` — the revocation listener's
    /// catch-up read after a missed-notification gap (review V1). The
    /// window is the credential cache's max entry age: a row revoked
    /// longer ago cannot still be cached, so reading further back only
    /// makes the blip bigger as revoked rows accumulate (review N-C);
    /// migration 0010's partial index keeps the read at window size.
    pub async fn revoked_token_digests(
        &self,
        max_age: std::time::Duration,
    ) -> Result<Vec<[u8; 32]>, StoreError> {
        #[cfg(debug_assertions)]
        if self.revocation_failing() {
            return Err(StoreError::Unavailable(sqlx::Error::PoolClosed));
        }
        let rows = sqlx::query(
            "SELECT token_hash FROM api_tokens
             WHERE revoked_at IS NOT NULL
               AND revoked_at > now() - make_interval(secs => $1)",
        )
        .bind(max_age.as_secs_f64())
        .fetch_all(&self.auth)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| <[u8; 32]>::try_from(r.get::<Vec<u8>, _>(0).as_slice()).ok())
            .collect())
    }

    pub async fn project_by_slug(&self, slug: &str) -> Result<Option<Project>, StoreError> {
        let row = sqlx::query("SELECT id, slug FROM projects WHERE slug = $1")
            .bind(slug)
            .fetch_optional(&self.query)
            .await?;
        Ok(row.map(|r| Project {
            id: r.get(0),
            slug: r.get(1),
        }))
    }

    pub async fn projects_by_ids(&self, ids: &[Uuid]) -> Result<Vec<Project>, StoreError> {
        let rows = sqlx::query("SELECT id, slug FROM projects WHERE id = ANY($1) ORDER BY slug")
            .bind(ids)
            .fetch_all(&self.query)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Project {
                id: r.get(0),
                slug: r.get(1),
            })
            .collect())
    }

    pub async fn list_operations(
        &self,
        project_id: Uuid,
        filter: &ListFilter,
    ) -> Result<Vec<OperationRow>, StoreError> {
        let (before_t, before_id) = filter.before.map_or((None, None), |(t, id)| {
            (Some(to_odt(t)), Some(uuid128(id.as_bytes())))
        });
        let rows = sqlx::query(concat!(
            operation_columns!(),
            " FROM operations
             WHERE project_id = $1
               AND ($2::timestamptz IS NULL OR (first_event_time, operation_id) < ($2, $3::uuid))
               AND ($4::text IS NULL OR request_id = $4)
               AND ($6::uuid IS NULL OR operation_id = $6)
               AND ($7::text IS NULL OR name = $7)
               AND ($8::text IS NULL OR release = $8)
               AND ($9::timestamptz IS NULL OR first_event_time >= $9)
               AND ($10::timestamptz IS NULL OR first_event_time < $10)
               AND ",
            status_matches!("$11"),
            "
             ORDER BY first_event_time DESC, operation_id DESC
             LIMIT $5"
        ))
        .bind(project_id)
        .bind(before_t)
        .bind(before_id)
        .bind(filter.request_id.as_deref())
        .bind(filter.limit)
        .bind(filter.operation_id.map(|o| uuid128(o.as_bytes())))
        .bind(filter.name.as_deref())
        .bind(filter.release.as_deref())
        .bind(filter.from.map(to_odt))
        .bind(filter.to.map(to_odt))
        .bind(filter.status)
        .fetch_all(&self.query)
        .await?;
        rows.into_iter().map(|r| operation_row(&r)).collect()
    }

    /// Releases side by side over `[from, to)`: the same summaries, statuses and window the
    /// list filters by, so a comparison and a filtered list never disagree.
    pub async fn compare_releases(
        &self,
        project_id: Uuid,
        releases: &[String],
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<ReleaseCounts>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT release, count(*),
                    count(*) FILTER (WHERE ",
            status_matches!("'succeeded'"),
            "),
                    count(*) FILTER (WHERE ",
            status_matches!("'failed'"),
            "),
                    count(*) FILTER (WHERE ",
            status_matches!("'uncertain'"),
            "),
                    count(*) FILTER (WHERE ",
            status_matches!("'conflicting'"),
            "),
                    count(*) FILTER (WHERE ",
            status_matches!("'no_finish_observed'"),
            "),
                    count(*) FILTER (WHERE events_emitted IS NOT NULL AND NOT multi_source
                                       AND host_events < events_emitted),
                    coalesce(sum(reported_dropped), 0)::bigint,
                    coalesce(sum(failed_attempts), 0)::bigint,
                    coalesce(sum(uncertain_attempts), 0)::bigint,
                    coalesce(sum(cost_estimate_micros), 0)::bigint,
                    count(DISTINCT cost_currency)
             FROM operations
             WHERE project_id = $1 AND release = ANY($2)
               AND first_event_time >= $3 AND first_event_time < $4
             GROUP BY release
             ORDER BY release"
        ))
        .bind(project_id)
        .bind(releases)
        .bind(to_odt(from))
        .bind(to_odt(to))
        .fetch_all(&self.query)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ReleaseCounts {
                release: r.get(0),
                operations: r.get(1),
                succeeded: r.get(2),
                failed: r.get(3),
                uncertain: r.get(4),
                conflicting: r.get(5),
                no_finish_observed: r.get(6),
                missing_events: r.get(7),
                reported_dropped: r.get(8),
                failed_attempts: r.get(9),
                uncertain_attempts: r.get(10),
                cost_estimate_micros: r.get(11),
                currencies: r.get(12),
            })
            .collect())
    }

    /// The operation's stored events, oldest event time first, at most `limit`; the flag
    /// says whether more exist.
    pub async fn operation_events(
        &self,
        project_id: Uuid,
        operation_id: OperationId,
        limit: usize,
    ) -> Result<(Vec<StoredEvent>, bool), StoreError> {
        let rows = sqlx::query(
            "SELECT e.instance_id, e.received_at, e.withheld_attributes, e.redactions, e.body::text, k.name,
                    e.overflowed_names
             FROM events e JOIN ingest_keys k ON k.id = e.ingest_key_id
             WHERE e.project_id = $1 AND e.operation_id = $2
             ORDER BY e.event_time, e.seq
             LIMIT $3",
        )
        .bind(project_id)
        .bind(uuid128(operation_id.as_bytes()))
        .bind(i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX))
        .fetch_all(&self.query)
        .await?;
        let truncated = rows.len() > limit;
        let limits = Limits::default();
        let policy = aiops_domain::policy::Policy::default();
        let validator = Validator {
            limits: &limits,
            policy: &policy,
            mode: Mode::Stored,
        };
        let mut out = Vec::with_capacity(rows.len().min(limit));
        for r in rows.into_iter().take(limit) {
            let instance: Uuid = r.get(0);
            let body: String = r.get(4);
            let wire: WireEvent = serde_json::from_str(&body).map_err(|_| StoreError::Corrupt)?;
            // Stored attributes already passed the policy; keep them all on re-read.
            let keep_all = aiops_domain::policy::Policy {
                opt_in_keys: wire
                    .attributes
                    .as_ref()
                    .map(|a| a.keys().cloned().collect())
                    .unwrap_or_default(),
            };
            let v = Validator {
                policy: &keep_all,
                ..validator
            }
            .validate(wire)
            .map_err(|_| StoreError::Corrupt)?;
            out.push(StoredEvent {
                event: v.event,
                received_at: from_odt(r.get(1)),
                instance_id: InstanceId::from_bytes(*instance.as_bytes())
                    .ok_or(StoreError::Corrupt)?,
                ingest_key: r.get(5),
                marks: PolicyMarks {
                    withheld_attributes: u32::try_from(r.get::<i16, _>(2)).unwrap_or(0),
                    redactions: u32::try_from(r.get::<i16, _>(3)).unwrap_or(0),
                    overflowed_names: u32::try_from(r.get::<i16, _>(6)).unwrap_or(0),
                },
            });
        }
        Ok((out, truncated))
    }

    pub async fn instances(
        &self,
        project_id: Uuid,
        ids: &[InstanceId],
    ) -> Result<Vec<InstanceStats>, StoreError> {
        let ids: Vec<Uuid> = ids.iter().map(|i| uuid128(i.as_bytes())).collect();
        let rows = sqlx::query(
            "SELECT s.instance_id, s.service, s.sdk, k.name, s.first_seen, s.last_seen, s.max_seq, s.events_received,
                    s.dropped_queue_full, s.dropped_send_failed, s.dropped_rejected, s.gap_overflow, s.truncated_events,
                    s.events_refused
             FROM source_instances s JOIN ingest_keys k ON k.id = s.ingest_key_id
             WHERE s.project_id = $1 AND s.instance_id = ANY($2)",
        )
        .bind(project_id)
        .bind(&ids)
        .fetch_all(&self.query)
        .await?;
        let u = |v: i64| u64::try_from(v).unwrap_or(0);
        rows.into_iter()
            .map(|r| {
                let id: Uuid = r.get(0);
                Ok(InstanceStats {
                    instance_id: InstanceId::from_bytes(*id.as_bytes())
                        .ok_or(StoreError::Corrupt)?,
                    service: r.get(1),
                    sdk: r.get(2),
                    ingest_key: r.get(3),
                    first_seen: from_odt(r.get(4)),
                    last_seen: from_odt(r.get(5)),
                    max_seq: u(r.get(6)),
                    events_received: u(r.get(7)),
                    events_refused: u(r.get(13)),
                    reported: WireStats {
                        dropped_queue_full: u(r.get(8)),
                        dropped_send_failed: u(r.get(9)),
                        dropped_rejected: u(r.get(10)),
                        gap_overflow: u(r.get(11)),
                        truncated_events: u(r.get(12)),
                    },
                })
            })
            .collect()
    }

    /// The job-runner runs an operation's start events say it stands for (`job.runner`,
    /// `job.id`, D-027), at most four distinct: telemetry's claim, which the caller checks
    /// before acting on it.
    pub async fn operation_links(
        &self,
        project_id: Uuid,
        operation_id: OperationId,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT body->'attributes'->>'job.runner', (body->'attributes'->>'job.id')::bigint
             FROM events
             WHERE project_id = $1 AND operation_id = $2
               AND body->>'type' = 'operation.started'
               AND body->'attributes' ? 'job.runner'
               AND body->'attributes'->>'job.id' ~ '^[0-9]{1,18}$'
             LIMIT 4",
        )
        .bind(project_id)
        .bind(uuid128(operation_id.as_bytes()))
        .fetch_all(&self.query)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.get::<Option<String>, _>(0)?, r.get::<Option<i64>, _>(1)?)))
            .collect())
    }

    /// One operation's summary (it outlives the raw events, D-016).
    pub async fn operation_summary(
        &self,
        project_id: Uuid,
        operation_id: OperationId,
    ) -> Result<Option<OperationRow>, StoreError> {
        // Its own primary-key lookup (review N8): the list's catch-all filter, run as a
        // generic plan, scanned every operation of the project.
        let row = sqlx::query(OPERATION_SUMMARY_SQL)
            .bind(project_id)
            .bind(uuid128(operation_id.as_bytes()))
            .fetch_optional(&self.query)
            .await?;
        row.map(|r| operation_row(&r)).transpose()
    }

    /// Raw events received before this may have been deleted by retention (`None`: the
    /// sweep never ran).
    pub async fn retention_watermark(&self) -> Result<Option<Timestamp>, StoreError> {
        let t: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT events_swept_before FROM retention_state WHERE id = 1")
                .fetch_optional(&self.query)
                .await?;
        Ok(t.map(from_odt))
    }

    // ---- control (audit) --------------------------------------------------------------

    /// Writes one audit row on the audit pool. Callers fail closed when this fails.
    pub async fn audit(&self, entry: &AuditEntry<'_>) -> Result<(), StoreError> {
        #[cfg(debug_assertions)]
        if self.audit_failing() {
            return Err(StoreError::Unavailable(sqlx::Error::PoolClosed));
        }
        sqlx::query(
            "INSERT INTO audit_log (actor, token_id, action, project_slug, target, decision, reason)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(entry.actor)
        .bind(entry.token_id)
        .bind(entry.action)
        .bind(entry.project_slug.map(audit_slug))
        .bind(entry.target.map(audit_target))
        .bind(if entry.allowed { "allowed" } else { "denied" })
        .bind(entry.reason)
        .execute(&self.audit)
        .await?;
        Ok(())
    }

    /// A page through `audit_log`, newest first, on the CONTROL pool: a query-path
    /// flood cannot starve an incident read. The `(at, id)` keyset keeps pages
    /// stable while rows arrive — a newer row can only fall ahead of the cursor.
    pub async fn audit_page(
        &self,
        filter: &AuditFilter<'_>,
    ) -> Result<Vec<AuditPageRow>, StoreError> {
        let (before_t, before_id) = filter
            .before
            .map_or((None, None), |(t, id)| (Some(to_odt(t)), Some(id)));
        let rows = sqlx::query(
            "SELECT id, at, actor, token_id, action, project_slug, target, decision, reason
             FROM audit_log
             WHERE ($1::timestamptz IS NULL OR (at, id) < ($1, $2::bigint))
               AND ($3::text IS NULL OR action = $3)
               AND ($4::text IS NULL OR actor = $4)
               AND ($5::uuid IS NULL OR token_id = $5)
               AND ($6::timestamptz IS NULL OR at >= $6)
               AND ($7::timestamptz IS NULL OR at < $7)
             ORDER BY at DESC, id DESC
             LIMIT $8",
        )
        .bind(before_t)
        .bind(before_id)
        .bind(filter.action)
        .bind(filter.actor)
        .bind(filter.token_id)
        .bind(filter.from.map(to_odt))
        .bind(filter.to.map(to_odt))
        .bind(filter.limit)
        .fetch_all(&self.control)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AuditPageRow {
                    id: r.get(0),
                    at: from_odt(r.get(1)),
                    actor: r.get(2),
                    token_id: r.get(3),
                    action: r.get(4),
                    project_slug: r.get(5),
                    target: r.get(6),
                    decision: r.get(7),
                    reason: r.get(8),
                })
            })
            .collect()
    }

    // ---- control records (A3; invariant 4: everything on the control pool) --------------

    /// The control path's own read of `projects`, on the control pool: a saturated query
    /// pool cannot starve a control action's authorization.
    pub async fn control_project_by_slug(&self, slug: &str) -> Result<Option<Project>, StoreError> {
        let row = sqlx::query("SELECT id, slug FROM projects WHERE slug = $1")
            .bind(slug)
            .fetch_optional(&self.control)
            .await?;
        Ok(row.map(|r| Project {
            id: r.get(0),
            slug: r.get(1),
        }))
    }

    /// Whether the operation exists in this project — the control path's own read.
    pub async fn control_operation_exists(
        &self,
        project_id: Uuid,
        operation_id: Uuid,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE project_id = $1 AND operation_id = $2)",
        )
        .bind(project_id)
        .bind(operation_id)
        .fetch_one(&self.control)
        .await
        .map_err(StoreError::from)
        .map(|exists: bool| exists)
    }

    /// Commits a new record at its first active status, or returns the row the replay
    /// key already holds — a repeated call gets `Existing` and re-drives nothing.
    pub async fn control_insert(
        &self,
        n: &NewControlRecord<'_>,
    ) -> Result<ControlInsert, StoreError> {
        let id = Uuid::new_v4();
        let row = sqlx::query(
            "INSERT INTO control_records (id, project_id, operation_id, action, business_key,
                expected_state, actor, token_id, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (project_id, operation_id, action, business_key) DO NOTHING
             RETURNING id",
        )
        .bind(id)
        .bind(n.project_id)
        .bind(n.operation_id)
        .bind(n.action)
        .bind(n.business_key)
        .bind(n.expected_state)
        .bind(n.actor)
        .bind(n.token_id)
        .bind(n.initial_status)
        .fetch_optional(&self.control)
        .await?;
        if row.is_some() {
            // The row was just committed by this statement; not finding it means the
            // store no longer tells a consistent story — the same class as Corrupt.
            let created = self.control_get(id).await?.ok_or(StoreError::Corrupt)?;
            return Ok(ControlInsert::Created(created));
        }
        let existing = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {CONTROL_COLUMNS} FROM control_records
             WHERE project_id = $1 AND operation_id = $2 AND action = $3 AND business_key = $4",
        )))
        .bind(n.project_id)
        .bind(n.operation_id)
        .bind(n.action)
        .bind(n.business_key)
        .fetch_one(&self.control)
        .await?;
        Ok(ControlInsert::Existing(control_row(&existing)))
    }

    pub async fn control_get(&self, id: Uuid) -> Result<Option<ControlRecord>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {CONTROL_COLUMNS} FROM control_records WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.control)
        .await?;
        Ok(row.map(|r| control_row(&r)))
    }

    /// One operation's control records, newest first — a `(created_at, id)` keyset
    /// page like `audit_page`: records created mid-walk can only fall ahead of the
    /// cursor, so pages neither repeat nor skip.
    pub async fn control_list(
        &self,
        project_id: Uuid,
        operation_id: Uuid,
        before: Option<(Timestamp, Uuid)>,
        limit: i64,
    ) -> Result<Vec<ControlRecord>, StoreError> {
        let (before_t, before_id) =
            before.map_or((None, None), |(t, id)| (Some(to_odt(t)), Some(id)));
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {CONTROL_COLUMNS} FROM control_records
             WHERE project_id = $1 AND operation_id = $2
               AND ($3::timestamptz IS NULL OR (created_at, id) < ($3, $4::uuid))
             ORDER BY created_at DESC, id DESC LIMIT $5"
        )))
        .bind(project_id)
        .bind(operation_id)
        .bind(before_t)
        .bind(before_id)
        .bind(limit)
        .fetch_all(&self.control)
        .await?;
        Ok(rows.iter().map(control_row).collect())
    }

    /// Records a crash could have stranded mid-work — the executor's boot re-drive and
    /// its wake-up queue (`is_resumable` in the domain). Each row carries its project's
    /// slug so a re-driven decision audits under the project's own name.
    pub async fn control_resumable(&self, limit: i64) -> Result<Vec<ResumableControl>, StoreError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {CONTROL_COLUMNS_C}, p.slug FROM control_records c
             JOIN projects p ON p.id = c.project_id
             WHERE c.status IN ('requested', 'reconciling', 'authorized', 'submitted')
             ORDER BY c.created_at LIMIT $1"
        )))
        .bind(limit)
        .fetch_all(&self.control)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ResumableControl {
                record: control_row(r),
                project_slug: r.get(18),
            })
            .collect())
    }

    /// A transition without its own audit row (in-flight moves: `submitted`, the post-
    /// submit observation — the authorization decision was already committed and audited).
    /// `expect_status` is optimistic concurrency: `Ok(false)` means another driver moved
    /// the record first and the caller must re-read it rather than assume the world.
    pub async fn control_update(
        &self,
        id: Uuid,
        expect_status: Option<&str>,
        patch: &ControlPatch<'_>,
    ) -> Result<bool, StoreError> {
        let mut tx = self.control.begin().await?;
        let done = control_apply(&mut tx, id, expect_status, patch).await?;
        tx.commit().await?;
        Ok(done)
    }

    /// The decision commit (invariant 4): the transition and its audit row in ONE
    /// transaction on the control pool — a denial and an authorization are equally
    /// durable before the external world is ever touched.
    pub async fn control_decide(
        &self,
        id: Uuid,
        expect_status: Option<&str>,
        patch: &ControlPatch<'_>,
        audit: &AuditEntry<'_>,
    ) -> Result<bool, StoreError> {
        let mut tx = self.control.begin().await?;
        let done = control_apply(&mut tx, id, expect_status, patch).await?;
        if done {
            #[cfg(debug_assertions)]
            if self.audit_failing() {
                return Err(StoreError::Unavailable(sqlx::Error::PoolClosed));
            }
            sqlx::query(
                "INSERT INTO audit_log (actor, token_id, action, project_slug, target, decision, reason)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(audit.actor)
            .bind(audit.token_id)
            .bind(audit.action)
            .bind(audit.project_slug.map(audit_slug))
            .bind(audit.target.map(audit_target))
            .bind(if audit.allowed { "allowed" } else { "denied" })
            .bind(audit.reason)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(done)
    }

    // ---- admin (CLI only; no HTTP surface) ----------------------------------------------

    pub async fn create_project(&self, slug: &str) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO projects (id, slug) VALUES ($1, $2)")
            .bind(id)
            .bind(slug)
            .execute(&self.control)
            .await?;
        Ok(id)
    }

    pub async fn set_opt_in_keys(&self, slug: &str, keys: &[String]) -> Result<(), StoreError> {
        let done = sqlx::query("UPDATE projects SET opt_in_attribute_keys = $2 WHERE slug = $1")
            .bind(slug)
            .bind(keys)
            .execute(&self.control)
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("project"));
        }
        Ok(())
    }

    pub async fn create_ingest_key(
        &self,
        project_slug: &str,
        name: &str,
        hash: &[u8; 32],
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        let done = sqlx::query(
            "INSERT INTO ingest_keys (id, project_id, name, key_hash)
             SELECT $1, id, $3, $4 FROM projects WHERE slug = $2",
        )
        .bind(id)
        .bind(project_slug)
        .bind(name)
        .bind(&hash[..])
        .execute(&self.control)
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("project"));
        }
        Ok(id)
    }

    pub async fn revoke_ingest_key(
        &self,
        project_slug: &str,
        name: &str,
    ) -> Result<(), StoreError> {
        let done = sqlx::query(
            "UPDATE ingest_keys SET revoked_at = now()
             WHERE name = $2 AND revoked_at IS NULL AND project_id = (SELECT id FROM projects WHERE slug = $1)",
        )
        .bind(project_slug)
        .bind(name)
        .execute(&self.control)
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("active ingest key"));
        }
        Ok(())
    }

    pub async fn create_token(
        &self,
        actor: &str,
        scopes: &[String],
        project_slugs: &[String],
        hash: &[u8; 32],
        expires_at: Timestamp,
    ) -> Result<Uuid, StoreError> {
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM projects WHERE slug = ANY($1)")
            .bind(project_slugs)
            .fetch_all(&self.control)
            .await?;
        if ids.len() != project_slugs.len() {
            return Err(StoreError::NotFound("project"));
        }
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO api_tokens (id, actor, token_hash, scopes, project_ids, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(actor)
        .bind(&hash[..])
        .bind(scopes)
        .bind(&ids)
        .bind(to_odt(expires_at))
        .execute(&self.control)
        .await?;
        Ok(id)
    }

    /// Revokes the token and returns its digest, so the caller can evict it from the
    /// credential cache — an incident action must stop the credential NOW, not when
    /// the cache TTL runs out (D-034). The digest never crosses the API.
    pub async fn revoke_token(&self, id: Uuid) -> Result<[u8; 32], StoreError> {
        let row = sqlx::query(
            "UPDATE api_tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL
             RETURNING token_hash",
        )
        .bind(id)
        .fetch_optional(&self.control)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound("active token"));
        };
        let hash: Vec<u8> = row.get(0);
        <[u8; 32]>::try_from(hash.as_slice()).map_err(|_| StoreError::Corrupt)
    }

    // ---- the credential surface (D-034) ------------------------------------------------
    //
    // The console's admin view: every credential's metadata — name, actor, scopes,
    // projects, expiry, revocation — and never a hash. All on the control pool; these
    // are bounded lists (credentials are counted in tens, not millions).

    /// Every project, oldest first.
    pub async fn list_project_infos(&self) -> Result<Vec<ProjectInfo>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, slug, opt_in_attribute_keys, created_at FROM projects ORDER BY created_at LIMIT 1000",
        )
        .fetch_all(&self.control)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ProjectInfo {
                id: r.get(0),
                slug: r.get(1),
                opt_in_keys: r.get(2),
                created_at: from_odt(r.get(3)),
            })
            .collect())
    }

    /// Every ingest key's metadata, oldest first — `key_hash` is never selected.
    pub async fn list_ingest_key_infos(&self) -> Result<Vec<IngestKeyInfo>, StoreError> {
        let rows = sqlx::query(
            "SELECT k.id, k.name, p.slug, k.created_at, k.revoked_at
             FROM ingest_keys k JOIN projects p ON p.id = k.project_id
             ORDER BY k.created_at LIMIT 2000",
        )
        .fetch_all(&self.control)
        .await?;
        Ok(rows
            .iter()
            .map(|r| IngestKeyInfo {
                id: r.get(0),
                name: r.get(1),
                project_slug: r.get(2),
                created_at: from_odt(r.get(3)),
                revoked_at: r.get::<Option<OffsetDateTime>, _>(4).map(from_odt),
            })
            .collect())
    }

    /// Every token's metadata, oldest first — `token_hash` is never selected;
    /// `project_ids` are joined back to their slugs.
    pub async fn list_token_infos(&self) -> Result<Vec<TokenInfo>, StoreError> {
        let rows = sqlx::query(
            "SELECT t.id, t.actor, t.scopes,
                    COALESCE((SELECT array_agg(p.slug ORDER BY p.slug) FROM projects p
                              WHERE p.id = ANY(t.project_ids)), '{}') AS slugs,
                    t.created_at, t.expires_at, t.revoked_at
             FROM api_tokens t ORDER BY t.created_at LIMIT 2000",
        )
        .fetch_all(&self.control)
        .await?;
        Ok(rows
            .iter()
            .map(|r| TokenInfo {
                id: r.get(0),
                actor: r.get(1),
                scopes: r.get(2),
                project_slugs: r.get(3),
                created_at: from_odt(r.get(4)),
                expires_at: from_odt(r.get(5)),
                revoked_at: r.get::<Option<OffsetDateTime>, _>(6).map(from_odt),
            })
            .collect())
    }

    /// Revoke an ingest key by its id and return its digest for cache eviction —
    /// the same incident-action immediacy as token revocation (D-034).
    pub async fn revoke_ingest_key_id(&self, id: Uuid) -> Result<[u8; 32], StoreError> {
        let row = sqlx::query(
            "UPDATE ingest_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL
             RETURNING key_hash",
        )
        .bind(id)
        .fetch_optional(&self.control)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound("active ingest key"));
        };
        let hash: Vec<u8> = row.get(0);
        <[u8; 32]>::try_from(hash.as_slice()).map_err(|_| StoreError::Corrupt)
    }

    // ---- retention (D-016) ------------------------------------------------------------

    /// Deletes expired rows in bounded batches (at most `max_batches` of `batch` rows per
    /// table per call), so a sweep never holds long locks or starves ingest.
    pub async fn sweep(
        &self,
        now: Timestamp,
        retention: Retention,
        batch: i64,
        max_batches: u32,
    ) -> Result<SweepResult, StoreError> {
        let cutoff = |d: Duration| {
            to_odt(now.plus_micros(-i64::try_from(d.as_micros()).unwrap_or(i64::MAX)))
        };
        let mut result = SweepResult::default();
        // Literal statements only (no SQL is built from strings).
        let tables: [(&'static str, Duration); 5] = [
            (
                "DELETE FROM events WHERE ctid = ANY(ARRAY(SELECT ctid FROM events WHERE received_at < $1 LIMIT $2))",
                retention.raw_events,
            ),
            (
                "DELETE FROM operations WHERE ctid = ANY(ARRAY(SELECT ctid FROM operations WHERE last_received_at < $1 LIMIT $2))",
                retention.summaries,
            ),
            (
                "DELETE FROM source_instances WHERE ctid = ANY(ARRAY(SELECT ctid FROM source_instances WHERE last_seen < $1 LIMIT $2))",
                retention.summaries,
            ),
            (
                "DELETE FROM audit_log WHERE ctid = ANY(ARRAY(SELECT ctid FROM audit_log WHERE at < $1 LIMIT $2))",
                retention.audit,
            ),
            (
                "DELETE FROM known_names WHERE ctid = ANY(ARRAY(SELECT ctid FROM known_names WHERE last_seen < $1 LIMIT $2))",
                retention.summaries,
            ),
        ];
        // Record the cutoff before deleting anything, so a timeline never calls a deleted
        // event "never received" (review finding 10). It only moves forward.
        sqlx::query(
            "INSERT INTO retention_state (id, events_swept_before) VALUES (1, $1)
             ON CONFLICT (id) DO UPDATE
             SET events_swept_before = GREATEST(retention_state.events_swept_before, EXCLUDED.events_swept_before)",
        )
        .bind(cutoff(retention.raw_events))
        .execute(&self.maintenance)
        .await?;
        let mut counts = [0u64; 5];
        for (i, (sql, keep)) in tables.into_iter().enumerate() {
            let mut rounds = 0;
            loop {
                let n = sqlx::query(sql)
                    .bind(cutoff(keep))
                    .bind(batch)
                    .execute(&self.maintenance)
                    .await?
                    .rows_affected();
                counts[i] += n;
                rounds += 1;
                if n < u64::try_from(batch).unwrap_or(0) {
                    break;
                }
                if rounds >= max_batches {
                    result.more = true;
                    break;
                }
            }
        }
        [
            result.events,
            result.operations,
            result.instances,
            result.audit,
            result.names,
        ] = counts;
        Ok(result)
    }
}

async fn insert_events(
    tx: &mut PgConnection,
    batch: &IngestBatch<'_>,
    instance: Uuid,
    received: OffsetDateTime,
    events: &[PreparedEvent],
) -> Result<HashSet<Uuid>, StoreError> {
    let ids: Vec<Uuid> = events
        .iter()
        .map(|p| uuid128(p.event.event_id.as_bytes()))
        .collect();
    let ops: Vec<Uuid> = events
        .iter()
        .map(|p| uuid128(p.event.operation_id.as_bytes()))
        .collect();
    let seqs: Vec<i64> = events.iter().map(|p| sat_i64(p.event.seq)).collect();
    let op_seqs: Vec<i32> = events.iter().map(|p| sat_i32(p.event.op_seq)).collect();
    let times: Vec<OffsetDateTime> = events.iter().map(|p| to_odt(p.event.event_time)).collect();
    let withheld: Vec<i16> = events
        .iter()
        .map(|p| i16::try_from(p.marks.withheld_attributes).unwrap_or(i16::MAX))
        .collect();
    let redactions: Vec<i16> = events
        .iter()
        .map(|p| i16::try_from(p.marks.redactions).unwrap_or(i16::MAX))
        .collect();
    let overflowed: Vec<i16> = events
        .iter()
        .map(|p| i16::try_from(p.marks.overflowed_names).unwrap_or(i16::MAX))
        .collect();
    let bodies: Vec<&str> = events.iter().map(|p| p.body.as_str()).collect();
    Ok(sqlx::query_scalar(
        "INSERT INTO events (project_id, event_id, operation_id, instance_id, ingest_key_id, seq, op_seq,
                             event_time, received_at, withheld_attributes, redactions, body, overflowed_names)
         SELECT $1, e.id, e.op, $2, $3, e.seq, e.op_seq, e.t, $4, e.w, e.r, e.body::jsonb, e.o
         FROM UNNEST($5::uuid[], $6::uuid[], $7::int8[], $8::int4[], $9::timestamptz[], $10::int2[], $11::int2[],
                     $12::text[], $13::int2[])
              AS e(id, op, seq, op_seq, t, w, r, body, o)
         ON CONFLICT (project_id, event_id) DO NOTHING
         RETURNING event_id",
    )
    .bind(batch.project_id)
    .bind(instance)
    .bind(batch.ingest_key_id)
    .bind(received)
    .bind(&ids)
    .bind(&ops)
    .bind(&seqs)
    .bind(&op_seqs)
    .bind(&times)
    .bind(&withheld)
    .bind(&redactions)
    .bind(&bodies)
    .bind(&overflowed)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect())
}

/// Operation summaries, the source instance's counters and new names, in one statement
/// (one round trip on the ingest hot path).
#[expect(
    clippy::too_many_lines,
    reason = "one SQL statement and its binds; splitting it would add round trips to every batch"
)]
async fn upsert_summaries(
    tx: &mut PgConnection,
    batch: &IngestBatch<'_>,
    instance: Uuid,
    received: OffsetDateTime,
    fresh: &[&Event],
    max_seq: u64,
) -> Result<(), StoreError> {
    let deltas = summarize(fresh.iter().copied());

    let op_ids: Vec<Uuid> = deltas
        .iter()
        .map(|d| uuid128(d.operation_id.as_bytes()))
        .collect();
    let op_names: Vec<Option<&str>> = deltas.iter().map(|d| d.name.as_deref()).collect();
    let request_ids: Vec<Option<&str>> = deltas.iter().map(|d| d.request_id.as_deref()).collect();
    let releases: Vec<Option<&str>> = deltas.iter().map(|d| d.release.as_deref()).collect();
    let earliest: Vec<OffsetDateTime> = deltas.iter().map(|d| to_odt(d.min_event_time)).collect();
    let latest: Vec<OffsetDateTime> = deltas.iter().map(|d| to_odt(d.max_event_time)).collect();
    let host_counts: Vec<i32> = deltas.iter().map(|d| sat_i32(d.host_events)).collect();
    let gap_report_counts: Vec<i32> = deltas.iter().map(|d| sat_i32(d.gap_reports)).collect();
    let dropped_sums: Vec<i64> = deltas.iter().map(|d| sat_i64(d.reported_dropped)).collect();
    let finish_outcomes: Vec<Option<&str>> =
        deltas.iter().map(|d| finish_parts(d.finish).0).collect();
    let finish_details: Vec<Option<&str>> =
        deltas.iter().map(|d| finish_parts(d.finish).1).collect();
    let emitted_counts: Vec<Option<i32>> = deltas
        .iter()
        .map(|d| d.events_emitted.map(sat_i32))
        .collect();
    let failed_counts: Vec<i32> = deltas.iter().map(|d| sat_i32(d.failed_attempts)).collect();
    let uncertain_counts: Vec<i32> = deltas
        .iter()
        .map(|d| sat_i32(d.uncertain_attempts))
        .collect();
    let cost_sums: Vec<i64> = deltas
        .iter()
        .map(|d| sat_i64(d.cost_estimate_micros))
        .collect();
    let currencies: Vec<Option<&str>> = deltas.iter().map(|d| d.cost_currency.as_deref()).collect();
    let conflicts: Vec<bool> = deltas.iter().map(|d| d.finish_conflict).collect();
    let finish_codes: Vec<Option<&str>> = deltas.iter().map(|d| d.finish_code.as_deref()).collect();

    let counts = sqlx::query(
        "WITH ops AS (
            INSERT INTO operations AS o (project_id, operation_id, name, request_id, release,
                first_event_time, last_event_time, first_received_at, last_received_at,
                host_events, gap_reports, reported_dropped, finish_outcome, finish_detail,
                events_emitted, failed_attempts, uncertain_attempts, cost_estimate_micros, cost_currency,
                finish_conflict, first_instance_id, finish_code)
            SELECT $1, d.op, d.name, d.req, d.rel, d.tmin, d.tmax, $2, $2, d.host, d.gaps, d.dropped,
                   d.fin, d.detail, d.emitted, d.failed, d.unc, d.cost, d.cur, d.conflict, $19, d.code
            FROM UNNEST($3::uuid[], $4::text[], $5::text[], $6::text[], $7::timestamptz[], $8::timestamptz[],
                        $9::int4[], $10::int4[], $11::int8[], $12::text[], $13::text[], $14::int4[],
                        $15::int4[], $16::int4[], $17::int8[], $18::text[], $32::bool[], $34::text[])
                 AS d(op, name, req, rel, tmin, tmax, host, gaps, dropped, fin, detail, emitted, failed, unc, cost, cur,
                      conflict, code)
            ORDER BY d.op
            ON CONFLICT (project_id, operation_id) DO UPDATE SET
                name = COALESCE(o.name, EXCLUDED.name),
                request_id = COALESCE(o.request_id, EXCLUDED.request_id),
                release = COALESCE(o.release, EXCLUDED.release),
                first_event_time = LEAST(o.first_event_time, EXCLUDED.first_event_time),
                last_event_time = GREATEST(o.last_event_time, EXCLUDED.last_event_time),
                last_received_at = GREATEST(o.last_received_at, EXCLUDED.last_received_at),
                host_events = o.host_events + EXCLUDED.host_events,
                gap_reports = o.gap_reports + EXCLUDED.gap_reports,
                reported_dropped = o.reported_dropped + EXCLUDED.reported_dropped,
                multi_source = o.multi_source OR o.first_instance_id IS DISTINCT FROM EXCLUDED.first_instance_id,
                first_instance_id = COALESCE(o.first_instance_id, EXCLUDED.first_instance_id),
                -- Finishes that disagree make the operation conflicting (SET reads the old row).
                finish_conflict = o.finish_conflict OR EXCLUDED.finish_conflict OR (
                    o.finish_outcome IS NOT NULL AND EXCLUDED.finish_outcome IS NOT NULL
                    AND (o.finish_outcome, COALESCE(o.finish_detail, ''))
                        <> (EXCLUDED.finish_outcome, COALESCE(EXCLUDED.finish_detail, ''))),
                finish_detail = CASE WHEN o.finish_outcome IS NULL THEN EXCLUDED.finish_detail ELSE o.finish_detail END,
                finish_code = CASE WHEN o.finish_outcome IS NULL THEN EXCLUDED.finish_code ELSE o.finish_code END,
                finish_outcome = COALESCE(o.finish_outcome, EXCLUDED.finish_outcome),
                events_emitted = GREATEST(o.events_emitted, EXCLUDED.events_emitted),
                failed_attempts = o.failed_attempts + EXCLUDED.failed_attempts,
                uncertain_attempts = o.uncertain_attempts + EXCLUDED.uncertain_attempts,
                cost_estimate_micros = o.cost_estimate_micros + EXCLUDED.cost_estimate_micros,
                cost_currency = CASE
                    WHEN o.cost_currency IS NULL THEN EXCLUDED.cost_currency
                    WHEN EXCLUDED.cost_currency IS NULL OR EXCLUDED.cost_currency = o.cost_currency THEN o.cost_currency
                    ELSE 'MIXED' END
            RETURNING 1
        ), src AS (
            INSERT INTO source_instances AS s (project_id, instance_id, ingest_key_id, service, sdk,
                first_seen, last_seen, max_seq, events_received, events_refused,
                dropped_queue_full, dropped_send_failed, dropped_rejected, gap_overflow, truncated_events)
            VALUES ($1, $19, $20, $21, $22, $2, $2, $23, $24, $25, $26, $27, $28, $29, $30)
            ON CONFLICT (project_id, instance_id) DO UPDATE SET
                last_seen = GREATEST(s.last_seen, EXCLUDED.last_seen),
                max_seq = GREATEST(s.max_seq, EXCLUDED.max_seq),
                events_received = s.events_received + EXCLUDED.events_received,
                events_refused = s.events_refused + EXCLUDED.events_refused,
                dropped_queue_full = GREATEST(s.dropped_queue_full, EXCLUDED.dropped_queue_full),
                dropped_send_failed = GREATEST(s.dropped_send_failed, EXCLUDED.dropped_send_failed),
                dropped_rejected = GREATEST(s.dropped_rejected, EXCLUDED.dropped_rejected),
                gap_overflow = GREATEST(s.gap_overflow, EXCLUDED.gap_overflow),
                truncated_events = GREATEST(s.truncated_events, EXCLUDED.truncated_events)
            WHERE s.ingest_key_id = EXCLUDED.ingest_key_id
            RETURNING 1
        ), names AS (
            INSERT INTO known_names (project_id, name, ingest_key_id, last_seen)
            SELECT $1, n, $20, $2 FROM UNNEST($31::text[]) AS n
            ON CONFLICT DO NOTHING RETURNING 1
        ), touched AS (
            -- At most one write per name per day keeps last_seen current for the sweep.
            UPDATE known_names SET last_seen = $2
            WHERE project_id = $1 AND name = ANY($33::text[]) AND last_seen < $2 - interval '1 day'
            RETURNING 1
        )
        SELECT (SELECT count(*) FROM ops), (SELECT count(*) FROM src), (SELECT count(*) FROM names)",
    )
    .bind(batch.project_id)
    .bind(received)
    .bind(&op_ids)
    .bind(&op_names)
    .bind(&request_ids)
    .bind(&releases)
    .bind(&earliest)
    .bind(&latest)
    .bind(&host_counts)
    .bind(&gap_report_counts)
    .bind(&dropped_sums)
    .bind(&finish_outcomes)
    .bind(&finish_details)
    .bind(&emitted_counts)
    .bind(&failed_counts)
    .bind(&uncertain_counts)
    .bind(&cost_sums)
    .bind(&currencies)
    .bind(instance)
    .bind(batch.ingest_key_id)
    .bind(&batch.source.service)
    .bind(&batch.source.sdk)
    .bind(sat_i64(max_seq))
    .bind(sat_i64(fresh.len() as u64))
    .bind(i64::from(batch.refused))
    .bind(sat_i64(batch.stats.dropped_queue_full))
    .bind(sat_i64(batch.stats.dropped_send_failed))
    .bind(sat_i64(batch.stats.dropped_rejected))
    .bind(sat_i64(batch.stats.gap_overflow))
    .bind(sat_i64(batch.stats.truncated_events))
    .bind(&batch.new_names)
    .bind(&conflicts)
    .bind(&batch.touched_names)
    .bind(&finish_codes)
    .fetch_one(&mut *tx)
    .await?;
    let src_rows: i64 = counts.get(1);
    if src_rows == 0 {
        return Err(StoreError::InstanceOwnedByAnotherKey);
    }
    Ok(())
}

async fn find_conflicts(
    tx: &mut PgConnection,
    batch: &IngestBatch<'_>,
    instance: Uuid,
    events: &[PreparedEvent],
    inserted: &HashSet<Uuid>,
) -> Result<Vec<EventId>, StoreError> {
    let (dup_ids, dup_bodies): (Vec<Uuid>, Vec<&str>) = events
        .iter()
        .map(|p| (uuid128(p.event.event_id.as_bytes()), p.body.as_str()))
        .filter(|(id, _)| !inserted.contains(id))
        .unzip();
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT e.event_id FROM events e
             JOIN UNNEST($2::uuid[], $3::text[]) AS i(id, body) ON e.event_id = i.id
             WHERE e.project_id = $1
               AND (e.body <> i.body::jsonb OR e.instance_id <> $4 OR e.ingest_key_id <> $5)",
    )
    .bind(batch.project_id)
    .bind(&dup_ids)
    .bind(&dup_bodies)
    .bind(instance)
    .bind(batch.ingest_key_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|u| EventId::from_bytes(*u.as_bytes()))
        .collect())
}

/// The column list every control-record read shares (one order, one mapping).
const CONTROL_COLUMNS: &str = "id, project_id, operation_id, action, business_key,
    expected_state, actor, token_id, status, resolution, authority_read, submitted_ref,
    approved_by, approved_at, deny_reason, attempts, created_at, updated_at";

/// The same list, aliased for the resumable scan's join with `projects`.
const CONTROL_COLUMNS_C: &str = "c.id, c.project_id, c.operation_id, c.action,
    c.business_key, c.expected_state, c.actor, c.token_id, c.status, c.resolution,
    c.authority_read, c.submitted_ref, c.approved_by, c.approved_at, c.deny_reason,
    c.attempts, c.created_at, c.updated_at";

fn control_row(r: &sqlx::postgres::PgRow) -> ControlRecord {
    ControlRecord {
        id: r.get(0),
        project_id: r.get(1),
        operation_id: r.get(2),
        action: r.get(3),
        business_key: r.get(4),
        expected_state: r.get(5),
        actor: r.get(6),
        token_id: r.get(7),
        status: r.get(8),
        resolution: r.get(9),
        authority_read: r.get(10),
        submitted_ref: r.get(11),
        approved_by: r.get(12),
        approved_at: r.get::<Option<OffsetDateTime>, _>(13).map(from_odt),
        deny_reason: r.get(14),
        attempts: r.get(15),
        created_at: from_odt(r.get(16)),
        updated_at: from_odt(r.get(17)),
    }
}

/// The UPDATE both `control_update` and `control_decide` run. `expect_status` is
/// optimistic concurrency (`false`: another driver moved the record first).
async fn control_apply(
    tx: &mut PgConnection,
    id: Uuid,
    expect_status: Option<&str>,
    patch: &ControlPatch<'_>,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE control_records SET
            status = COALESCE($2, status),
            resolution = COALESCE($3, resolution),
            authority_read = COALESCE($4, authority_read),
            submitted_ref = COALESCE($5, submitted_ref),
            deny_reason = COALESCE($6, deny_reason),
            approved_by = COALESCE($7, approved_by),
            approved_at = COALESCE($8, approved_at),
            attempts = attempts + COALESCE($9, 0),
            updated_at = now()
         WHERE id = $1 AND ($10::text IS NULL OR status = $10)",
    )
    .bind(id)
    .bind(patch.status)
    .bind(patch.resolution)
    .bind(patch.authority_read)
    .bind(patch.submitted_ref)
    .bind(patch.deny_reason)
    .bind(patch.approved_by)
    .bind(patch.approved_at.map(to_odt))
    .bind(patch.add_attempts)
    .bind(expect_status)
    .execute(&mut *tx)
    .await?;
    Ok(done.rows_affected() > 0)
}
