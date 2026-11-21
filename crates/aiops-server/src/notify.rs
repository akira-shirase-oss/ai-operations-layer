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
//! silently withdraw). The heartbeat rides the watcher's OWN pool through
//! the `revocation_heartbeat` trigger (never a tenant pool — review N-I),
//! so an observed beat proves the trigger path live, not just the
//! connection (review N-H). None observed inside [`HEARTBEAT_DEADLINE`]
//! marks the channel unhealthy — `resolve` then trusts entries only for
//! seconds and serves nothing stale — and forces a reconnect.
//! `revocation_healthy`, `revocation_notices` and `revocation_gaps` on
//! [`AppState`] surface all of it to `/readyz` and `/v1/collector/stats`.
//!
//! Payloads are hints, never commands (review N-H): a digest evicts one
//! entry, `clear` schedules a debounced resync rather than flushing on a
//! message any database role can forge, and the catch-up read itself is
//! retried with backoff until it lands (review N-C) — a failed resync is
//! a degraded instance, not a permanently deaf one.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aiops_store::{
    PgListener, RevocationPool, revocation_beat, revocation_listener, revocation_writer,
};
use tokio_util::sync::CancellationToken;

use crate::app::{AppState, Metrics};

/// How long a dead connection waits before the reconnect attempt.
const RECONNECT_AFTER: Duration = Duration::from_secs(1);
/// The heartbeat cadence over the watcher's own pool.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
/// A beat must answer within this — a blocked UPDATE on the heartbeat row
/// or a half-open writer connection is a gap, never a hang (review round
/// 5, finding 3). Longer than the writer pool's server-side timeouts so
/// the server's error lands first when it can.
const BEAT_TIMEOUT: Duration = Duration::from_secs(2);
/// No observed notice (heartbeat or real) for this long means the channel
/// is dead — half-open connections surface no error otherwise.
const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(10);
/// The resync's first retry wait after a failed catch-up read.
const RESYNC_BACKOFF_MIN: Duration = Duration::from_millis(200);
/// …and its ceiling — a broken read keeps the instance degraded, so the
/// retry stays cheap but never stops (review N-C).
const RESYNC_BACKOFF_MAX: Duration = Duration::from_secs(2);
/// `clear` payloads resync at most this often — a forged `clear` buys one
/// bounded catch-up read, not a cache flush per message (review N-H).
const RESYNC_DEBOUNCE: Duration = Duration::from_secs(1);

/// The serving-time revocation watcher. Runs until `cancel`; a dropped
/// connection is retried forever. Every gap — a failed connect, an `Err`,
/// an `Ok(None)` internal reconnect, a missed heartbeat — is followed by a
/// catch-up: both caches drop everything (a gap can hide DELETEs and
/// `opt_in` withdrawals alike — review N-G) and the revoked digests the
/// store still knows inside the cache's max entry age come back as
/// negative entries, so they refuse without a re-read.
pub async fn watch(state: Arc<AppState>, cancel: CancellationToken) {
    let url = state.config.database_url.clone();
    let last_resync = Mutex::new(Instant::now());
    loop {
        // The first call races cancellation — startup must never hang on a
        // dead endpoint (review S3's shutdown list).
        let listener = tokio::select! {
            () = cancel.cancelled() => return,
            r = revocation_listener(&url) => r,
        };
        match listener {
            Ok(mut listener) => {
                // The heartbeat writer is the watcher's own — a tenant pool
                // that stalls must never fake a channel gap (review N-I).
                let writer = tokio::select! {
                    () = cancel.cancelled() => return,
                    r = revocation_writer(&url) => r,
                };
                match writer {
                    Ok(writer) => {
                        if !resync(&state, &cancel, &last_resync).await {
                            return; // cancelled
                        }
                        if !pump(&state, &mut listener, &writer, &cancel, &last_resync).await {
                            return; // cancelled
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "revocation writer cannot connect; retrying");
                    }
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

/// The post-gap resync: both caches drop everything — the gap may hide a
/// DELETE or an `opt_in` withdrawal the digest set cannot name (review
/// N-G) — then the bounded revoked set comes back as negative entries.
/// A failed catch-up retried once under V1 left the instance degraded
/// until the next gap; now it retries with backoff until the read lands
/// or `cancel` ends it (review N-C). `false` means cancelled mid-retry.
///
/// The window is stamped BEFORE the work (review round 6, A2): a `clear`
/// processed while the catch-up runs schedules the next window, and the
/// caller — the pump's deadline arm or a channel gap — owns every wait.
async fn resync(
    state: &AppState,
    cancel: &CancellationToken,
    last_resync: &Mutex<Instant>,
) -> bool {
    if let Ok(mut last) = last_resync.lock() {
        *last = Instant::now();
    }
    Metrics::add(&state.revocation_resyncs, 1);
    let mut backoff = RESYNC_BACKOFF_MIN;
    loop {
        state.revocation_healthy.store(false, Ordering::Relaxed);
        state.tokens.clear();
        state.ingest_keys.clear();
        match state
            .store
            .revoked_token_digests(state.tokens.max_entry_age())
            .await
        {
            Ok(digests) => {
                let revoked = digests.len();
                for digest in digests {
                    state.tokens.put(digest, None);
                }
                state.revocation_healthy.store(true, Ordering::Relaxed);
                tracing::info!(revoked, "revocation channel up; resynced");
                return true;
            }
            Err(e) => {
                tracing::warn!(error = %e, "revocation resync failed; caches cleared, retrying");
                if tokio::select! {
                    () = cancel.cancelled() => false,
                    () = tokio::time::sleep(backoff) => true,
                } {
                    backoff = (backoff * 2).min(RESYNC_BACKOFF_MAX);
                    continue;
                }
                return false;
            }
        }
    }
}

/// `false` when `cancel` ended the pump, `true` when the channel did.
async fn pump(
    state: &AppState,
    listener: &mut PgListener,
    writer: &RevocationPool,
    cancel: &CancellationToken,
    last_resync: &Mutex<Instant>,
) -> bool {
    let mut last_seen = Instant::now();
    let mut beat = tokio::time::interval(HEARTBEAT_EVERY);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A queued `clear` waits on THIS arm, never inside the `try_recv` arm
    // (review round 6, A2): while the deadline sleeps the select keeps
    // polling the listener socket, so digests still evict and a clear
    // flood coalesces instead of serialising a resync per message.
    let mut resync_at: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return false,
            () = async {
                match resync_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                resync_at = None;
                if !resync(state, cancel, last_resync).await {
                    return false;
                }
                // Notices that queued during the catch-up are already in
                // the listener's buffer — drain them before the next
                // select round so their deadlines re-base promptly.
                while let Some(note) = listener.next_buffered() {
                    last_seen = Instant::now();
                    handle(state, note.payload(), last_resync, &mut resync_at);
                }
            }
            _ = beat.tick() => {
                if last_seen.elapsed() > HEARTBEAT_DEADLINE {
                    tracing::warn!("revocation heartbeat missed; reconnecting");
                    Metrics::add(&state.revocation_gaps, 1);
                    return true;
                }
                match tokio::time::timeout(BEAT_TIMEOUT, revocation_beat(writer)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "revocation heartbeat failed; reconnecting");
                        Metrics::add(&state.revocation_gaps, 1);
                        return true;
                    }
                    // A half-open writer connection or a lock on the
                    // heartbeat row must not freeze the watcher inside
                    // this select arm (review round 5, finding 3): the
                    // timeout turns it into a gap + reconnect.
                    Err(_) => {
                        tracing::warn!("revocation heartbeat timed out; reconnecting");
                        Metrics::add(&state.revocation_gaps, 1);
                        return true;
                    }
                }
            }
            note = listener.try_recv() => match note {
                Ok(Some(note)) => {
                    last_seen = Instant::now();
                    handle(state, note.payload(), last_resync, &mut resync_at);
                }
                // sqlx dropped and re-LISTENed inside `try_recv`: notices in
                // the gap are lost silently — `Ok(None)` is the only sign.
                Ok(None) => {
                    tracing::warn!("revocation listener reconnected internally; resyncing");
                    Metrics::add(&state.revocation_gaps, 1);
                    if !resync(state, cancel, last_resync).await {
                        return false;
                    }
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

/// A payload is a hint, not a command (review N-H): `heartbeat` proves the
/// trigger path; a digest evicts exactly that entry from both credential
/// caches (token and key digests share the SHA-256 space; removing a
/// non-member is a no-op); `clear` (DELETE/TRUNCATE/opt-in writes,
/// migration 0009) schedules a debounced resync — any database role can
/// forge a `clear`, so its effect must be a bounded re-derivation from
/// the store, never a per-message flush, and never a sleep that leaves
/// the socket unread (review round 6, A2).
fn handle(
    state: &AppState,
    payload: &str,
    last_resync: &Mutex<Instant>,
    resync_at: &mut Option<tokio::time::Instant>,
) {
    Metrics::add(&state.revocation_notices, 1);
    match payload {
        "heartbeat" => {}
        "clear" => {
            // pending = max(last + 1 s, now): at most one resync per
            // window, and a clear inside the window waits for its
            // TRAILING edge (review round 5, finding 2) — the pump's
            // deadline arm owns the sleep.
            let pending = match last_resync.lock() {
                // A poisoned debounce lock resyncs at once — cheaper than
                // a deadlock.
                Err(_) => Instant::now(),
                Ok(last) => (*last + RESYNC_DEBOUNCE).max(Instant::now()),
            };
            let deadline = tokio::time::Instant::from_std(pending);
            // One log line per SCHEDULED resync, not per hint (review
            // round 7, N7): a `clear` inside the window only coalesces
            // the pending deadline, and a flood is attacker-shaped — it
            // must not buy a line per message.
            if resync_at.is_none() {
                tracing::info!("revocation channel hinted a clear; resync scheduled");
            }
            *resync_at = Some(resync_at.map_or(deadline, |d| d.min(deadline)));
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
