//! Cross-process credential revocation (D-034, amended): a revoke is a row
//! update in ANY process — the admin CLI included — and the
//! `aiops_notify_revocation` trigger NOTIFYs the revoked digest on
//! [`aiops_store::REVOCATION_CHANNEL`]. A running collector LISTENs on its own
//! connection (off every pool) and evicts exactly that digest from both
//! credential caches, so a CLI revoke is refused on the very next request
//! instead of living out the serving peer's cache TTL (the Payment Sandbox's
//! D-102 model). Notifications die with the connection, so a re-established
//! LISTEN clears both caches outright — the only safe resume.

use std::sync::Arc;
use std::time::Duration;

use aiops_store::{PgListener, revocation_listener};
use tokio_util::sync::CancellationToken;

use crate::app::AppState;

/// How long a dead connection waits before the reconnect attempt.
const RECONNECT_AFTER: Duration = Duration::from_secs(1);

/// The serving-time revocation watcher. Runs until `cancel`; a dropped
/// connection is retried forever. Every established LISTEN clears both
/// caches — entries cached outside the notification window (a dropped
/// channel, a slow first connect) cannot be trusted to be un-revoked.
pub async fn watch(state: Arc<AppState>, cancel: CancellationToken) {
    let url = state.config.database_url.clone();
    loop {
        match revocation_listener(&url).await {
            Ok(mut listener) => {
                state.tokens.clear();
                state.ingest_keys.clear();
                tracing::info!("revocation listener up; credential caches cleared");
                if !pump(&state, &mut listener, &cancel).await {
                    return; // cancelled
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "revocation listener cannot connect; retrying");
            }
        }
        tokio::select! {
            () = cancel.cancelled() => {}
            () = tokio::time::sleep(RECONNECT_AFTER) => {}
        }
        if cancel.is_cancelled() {
            return;
        }
    }
}

/// `false` when `cancel` ended the pump, `true` when the connection did.
async fn pump(state: &AppState, listener: &mut PgListener, cancel: &CancellationToken) -> bool {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return false,
            note = listener.recv() => match note {
                Ok(note) => handle(state, note.payload()),
                Err(e) => {
                    tracing::warn!(error = %e, "revocation listener dropped; reconnecting");
                    return true;
                }
            },
        }
    }
}

/// A payload is the revoked digest as hex — evict exactly that entry from
/// both credential caches (token and key digests share the SHA-256 space;
/// removing a non-member is a no-op).
fn handle(state: &AppState, payload: &str) {
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
