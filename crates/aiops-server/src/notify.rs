//! Cross-process credential revocation (D-034, amended): a revoke is a row
//! update in ANY process — the admin CLI included — and the
//! `aiops_notify_revocation` trigger NOTIFYs the revoked digest on
//! [`aiops_store::REVOCATION_CHANNEL`]. A running collector LISTENs on its own
//! connection (off every pool) and evicts exactly that digest from both
//! credential caches, so a CLI revoke is refused on the very next request
//! instead of living out the serving peer's cache TTL (the Payment Sandbox's
//! D-102 model).
//!
//! Channel health is proved, not assumed (review V1): sqlx's `try_recv`
//! returns `Ok(None)` after an internal reconnect — a gap where notices may
//! be lost — so every `Ok(None)` is followed by a catch-up read
//! ([`Store::revoked_token_digests`]) plus an ingest-key clear (their cached
//! records carry `opt_in_attribute_keys`, which a `projects` update can
//! silently withdraw). A self-`NOTIFY` heartbeat through the serving pool
//! every [`HEARTBEAT_EVERY`] proves the whole path; none observed inside
//! [`HEARTBEAT_DEADLINE`] marks the channel unhealthy — `resolve` then
//! trusts entries only for seconds and serves nothing stale — and forces a
//! reconnect. `revocation_healthy`, `revocation_notices` and
//! `revocation_gaps` on [`AppState`] surface all of it to `/readyz` and
//! `/v1/collector/stats`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use aiops_store::{PgListener, revocation_listener};
use tokio_util::sync::CancellationToken;

use crate::app::{AppState, Metrics};

/// How long a dead connection waits before the reconnect attempt.
const RECONNECT_AFTER: Duration = Duration::from_secs(1);
/// The self-NOTIFY cadence through the serving pool.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
/// No observed notice (heartbeat or real) for this long means the channel
/// is dead — half-open connections surface no error otherwise.
const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(10);

/// The serving-time revocation watcher. Runs until `cancel`; a dropped
/// connection is retried forever. Every gap — a failed connect, an `Err`,
/// an `Ok(None)` internal reconnect, a missed heartbeat — is followed by a
/// catch-up: the ingest-key cache clears outright (its entries carry
/// `opt_in_attribute_keys`) and revoked token digests are evicted by name,
/// so healthy entries survive a blip (review S3's catch-up requirement).
pub async fn watch(state: Arc<AppState>, cancel: CancellationToken) {
    let url = state.config.database_url.clone();
    loop {
        // The first call races cancellation — startup must never hang on a
        // dead endpoint (review S3's shutdown list).
        let listener = tokio::select! {
            () = cancel.cancelled() => return,
            r = revocation_listener(&url) => r,
        };
        match listener {
            Ok(mut listener) => {
                resync(&state).await;
                if !pump(&state, &mut listener, &cancel).await {
                    return; // cancelled
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "revocation listener cannot connect; retrying");
            }
        }
        state.revocation_healthy.store(false, Ordering::Relaxed);
        tokio::select! {
            () = cancel.cancelled() => {}
            () = tokio::time::sleep(RECONNECT_AFTER) => {}
        }
        if cancel.is_cancelled() {
            return;
        }
    }
}

/// The post-gap resync: ingest keys drop everything (their records carry
/// `opt_in_attribute_keys` — a `projects` write during the gap is
/// indistinguishable from none), and tokens evict exactly the rows the
/// store says are revoked — catch-up, not clear-all.
async fn resync(state: &AppState) {
    state.ingest_keys.clear();
    match state.store.revoked_token_digests().await {
        Ok(digests) => {
            for digest in &digests {
                state.tokens.remove(digest);
            }
            state.revocation_healthy.store(true, Ordering::Relaxed);
            tracing::info!(revoked = digests.len(), "revocation channel up; resynced");
        }
        Err(e) => {
            // The catch-up read itself failed: nothing is proved, so the
            // token cache cannot be trusted either.
            state.tokens.clear();
            tracing::warn!(error = %e, "revocation resync failed; caches cleared");
        }
    }
}

/// `false` when `cancel` ended the pump, `true` when the channel did.
async fn pump(state: &AppState, listener: &mut PgListener, cancel: &CancellationToken) -> bool {
    let mut last_seen = Instant::now();
    let mut beat = tokio::time::interval(HEARTBEAT_EVERY);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return false,
            _ = beat.tick() => {
                if last_seen.elapsed() > HEARTBEAT_DEADLINE {
                    tracing::warn!("revocation heartbeat missed; reconnecting");
                    Metrics::add(&state.revocation_gaps, 1);
                    return true;
                }
                if let Err(e) = state.store.revocation_heartbeat().await {
                    tracing::warn!(error = %e, "revocation heartbeat failed; reconnecting");
                    Metrics::add(&state.revocation_gaps, 1);
                    return true;
                }
            }
            note = listener.try_recv() => match note {
                Ok(Some(note)) => {
                    last_seen = Instant::now();
                    handle(state, note.payload());
                }
                // sqlx dropped and re-LISTENed inside `try_recv`: notices in
                // the gap are lost silently — `Ok(None)` is the only sign.
                Ok(None) => {
                    tracing::warn!("revocation listener reconnected internally; resyncing");
                    Metrics::add(&state.revocation_gaps, 1);
                    resync(state).await;
                    last_seen = Instant::now();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "revocation listener dropped; reconnecting");
                    Metrics::add(&state.revocation_gaps, 1);
                    return true;
                }
            },
        }
    }
}

/// A payload is the revoked digest as hex — evict exactly that entry from
/// both credential caches (token and key digests share the SHA-256 space;
/// removing a non-member is a no-op). `clear` (DELETE/TRUNCATE/opt-in
/// writes, migration 0009) empties both; `heartbeat` only proves the path.
fn handle(state: &AppState, payload: &str) {
    Metrics::add(&state.revocation_notices, 1);
    match payload {
        "heartbeat" => {}
        "clear" => {
            state.tokens.clear();
            state.ingest_keys.clear();
            tracing::info!("revocation channel asked for a full cache clear");
        }
        _ => {
            let Ok(digest) = <[u8; 32]>::try_from(hex_bytes(payload).as_slice()) else {
                tracing::warn!(
                    payload_len = payload.len(),
                    "malformed revocation notice ignored"
                );
                return;
            };
            state.tokens.remove(&digest);
            state.ingest_keys.remove(&digest);
            tracing::info!("revoked credential evicted from caches");
        }
    }
}

fn hex_bytes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 2 <= bytes.len() {
        match (nibble(bytes[i]), nibble(bytes[i + 1])) {
            (Some(h), Some(l)) => out.push(h << 4 | l),
            _ => return Vec::new(),
        }
        i += 2;
    }
    out
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
