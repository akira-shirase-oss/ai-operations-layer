#!/usr/bin/env python3
"""Mutation proof for the attack suite (workspace AGENTS.md, 2026-11-20):
every attack test must be proved to fail without its protection.

Each named mutant below removes or weakens exactly the protection the
listed attack tests assert. `apply` swaps one exact source region for the
mutant; `restore` puts the file back byte-for-byte. `scripts/prove-attacks.sh`
runs each mutant, requires its tests to go red, then restores — a test that
stays green under its mutant is a suite bug, not a pass.
"""

import hashlib
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
S = ROOT / "crates/aiops-server/src"
T = ROOT / "crates/aiops-store/src"
# File-level backups (.scratch is gitignored): restore copies bytes back, so a
# mutant whose `new` is empty or ambiguous restores exactly, never by search.
BACKUP = ROOT / ".scratch" / "mutants-backup"

def e(path, old, new):
    return (path, old, new)

# name -> ([(file, old, new), ...], [attack tests that must go red])
MUTANTS = {
    # Every credential shape is refused unless the store vouches it. The
    # mutant admits any well-shaped unknown ingest key as a fabricated host —
    # the forged key in the test then answers, not 401.
    "unknown-ingest-key-admitted": (
        [
            e(
                S / "ingest.rs",
                """    match found {
        Some(key) if !key.revoked => Ok(key),
        _ => {
            Metrics::add(&app.metrics.unauthenticated, 1);
            Err(ApiError::Unauthenticated)
        }
    }""",
                """    match found {
        Some(key) if !key.revoked => Ok(key),
        _ => Ok(IngestKey {
            id: Uuid::nil(),
            name: "forged".into(),
            project_id: Uuid::nil(),
            project_slug: "forged".into(),
            opt_in_keys: vec![],
            revoked: false,
        }),
    }""",
            )
        ],
        ["every_absent_or_malformed_credential_is_refused_everywhere"],
    ),
    # A revoke in the serving process evicts the cache NOW; a peer revoke
    # arrives over NOTIFY and evicts the same digest. Removing BOTH (the
    # in-process evict AND the listener's handle) is the only way the test's
    # next-request 401 can fail — either path alone still lands it.
    "revocation-evicts-nothing": (
        [
            e(
                S / "credentials.rs",
                """    if let Some(digest) = &digest {
        match kind {
            Kind::Token => app.tokens.remove(digest),
            Kind::IngestKey => app.ingest_keys.remove(digest),
        }
    }""",
                """    let _ = &digest;""",
            ),
            e(
                S / "notify.rs",
                "fn handle(state: &AppState, payload: &str) {",
                "fn handle(state: &AppState, payload: &str) {\n    let _ = (state, payload);\n    return;",
            ),
        ],
        ["an_in_process_revoke_dies_before_the_next_request"],
    ),
    # The NOTIFY listener is the only way a peer-process revoke reaches this
    # collector's cache. Deafen handle() and the poll sees 200 until the TTL.
    "notify-deaf": (
        [
            e(
                S / "notify.rs",
                "fn handle(state: &AppState, payload: &str) {",
                "fn handle(state: &AppState, payload: &str) {\n    let _ = (state, payload);\n    return;",
            )
        ],
        ["a_peer_revoke_is_refused_on_the_next_request"],
    ),
    # The CLI refuses contradictory credential shapes before minting.
    "cli-mints-contradictions": (
        [
            e(
                S / "admin.rs",
                """            let admin_scope = scopes.iter().any(|s| s == "admin");
            anyhow::ensure!(
                !admin_scope || projects.is_empty(),
                "--scope admin is collector-wide: --project cannot narrow it"
            );
            anyhow::ensure!(
                admin_scope || !projects.is_empty(),
                "a read/operate token needs at least one --project"
            );""",
                """            let _admin_scope = scopes.iter().any(|s| s == "admin");""",
            )
        ],
        ["the_cli_will_not_mint_a_contradictory_admin_or_scopeless_token"],
    ),
    # Bodies are capped twice: the route layer refuses on Content-Length and
    # read_body limits what it will buffer. Uncap both and the 2 MiB body is
    # read and parsed — a 400, not the early 413.
    "ingest-body-uncapped": (
        [
            e(
                S / "app.rs",
                "        .layer(DefaultBodyLimit::max(body_limit))\n        .layer(RequestBodyLimitLayer::new(body_limit));",
                "        .layer(DefaultBodyLimit::disable())\n        .layer(RequestBodyLimitLayer::new(usize::MAX));",
            ),
            e(
                S / "ingest.rs",
                "    let limit = app.limits.max_body_bytes;",
                "    let limit = usize::MAX;",
            ),
        ],
        ["bodies_over_every_cap_are_shed_not_parsed"],
    ),
    # The request deadline is the only bound on a stalled body. Remove the
    # timeout layer and the trickling connection is held forever.
    "no-request-timeout": (
        [
            e(
                S / "app.rs",
                """        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            timeout,
        ));""",
                ";",
            )
        ],
        ["a_body_sent_one_byte_a_heartbeat_is_bounded"],
    ),
    # Project authorization checks the token's project_ids. Admitting every
    # project turns B's read token into A's.
    "any-project-readable": (
        [
            e(
                S / "query.rs",
                "        Some(p) if actor.token.project_ids.contains(&p.id) => Ok(p),",
                "        Some(p) => Ok(p),",
            )
        ],
        ["one_projects_credentials_never_touch_anothers"],
    ),
    # The ingest bucket is keyed by the credential and charged twice —
    # admission before the body read, the remaining events after parse.
    # Removing both charges makes the IP-rotation flood all 200s.
    "ingest-never-limits": (
        [
            e(
                S / "ingest.rs",
                """    if let Err(retry_after_secs) = app.ingest_buckets.take(key.id, 1, app.now().unix_micros()) {
        Metrics::add(&app.metrics.rate_limited, 1);
        return Err(ApiError::RateLimited { retry_after_secs });
    }""",
                "",
            ),
            e(
                S / "ingest.rs",
                """    if let Err(retry_after_secs) =
        app.ingest_buckets
            .take(key.id, count.saturating_sub(1), received_at.unix_micros())
    {
        Metrics::add(&app.metrics.rate_limited, 1);
        return Err(ApiError::RateLimited { retry_after_secs });
    }""",
                "",
            ),
        ],
        ["forwarded_headers_do_not_move_the_per_token_bucket"],
    ),
    # HeaderMap::get answers the FIRST Authorization line. Reading the last
    # instead lets a valid second credential rescue a garbage first.
    "last-authorization-wins": (
        [
            e(
                S / "auth.rs",
                """    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;""",
                """    let value = headers
        .get_all(axum::http::header::AUTHORIZATION)
        .iter()
        .last()?
        .to_str()
        .ok()?;""",
            )
        ],
        ["conflicting_authorization_lines_have_one_verdict"],
    ),
    # The request-id pipeline strips hostile client ids before a fresh one is
    # assigned. Trusting the client's header echoes it straight back.
    "request-id-trusted": (
        [
            e(
                S / "app.rs",
                """        .layer(middleware::from_fn(sanitize_request_id))
        .layer(SetRequestIdLayer::new(request_id.clone(), MakeRequestUuid))""",
                "",
            )
        ],
        ["a_hostile_request_id_never_reaches_the_wire"],
    ),
    # Audit columns store slugs and targets only after shape-sanitizing to
    # `<invalid>`. Passing them raw writes the attacker's bytes into the log.
    "audit-stores-raw": (
        [
            e(
                T / "lib.rs",
                "pub fn audit_slug(s: &str) -> &str {",
                "pub fn audit_slug(s: &str) -> &str {\n    return s;",
            ),
            e(
                T / "lib.rs",
                "pub fn audit_target(s: &str) -> &str {",
                "pub fn audit_target(s: &str) -> &str {\n    return s;",
            ),
        ],
        ["forged_slugs_and_ids_in_paths_are_never_stored_raw"],
    ),
    # Error envelopes are static text; the request path never leaks into a
    # body. Echoing it into the message returns the marker to the caller.
    "errors-echo-path": (
        [
            e(
                S / "error.rs",
                """pub async fn error_envelope(request: Request, next: Next) -> Response {
    let request_id = request""",
                """pub async fn error_envelope(request: Request, next: Next) -> Response {
    let echo_path = request.uri().path().to_owned();
    let request_id = request""",
            ),
            e(
                S / "error.rs",
                "    let public = match response.extensions().get::<PublicError>() {",
                "    let mut public = match response.extensions().get::<PublicError>() {",
            ),
            e(
                S / "error.rs",
                """        None => public_for_status(status),
    };
    let (mut parts, _) = response.into_parts();""",
                """        None => public_for_status(status),
    };
    public.message = format!("{} {echo_path}", public.message);
    let (mut parts, _) = response.into_parts();""",
            ),
        ],
        ["error_bodies_never_echo_attacker_bytes"],
    ),
    # The query bucket charges inside authenticate, before authorize knows
    # the slug. Removing the charge makes denied requests free.
    "query-never-limits": (
        [
            e(
                S / "query.rs",
                """            if let Err(retry_after_secs) = buckets.take(token.id, 1, now.unix_micros()) {
                Metrics::add(&app.metrics.rate_limited, 1);
                return Err(ApiError::RateLimited { retry_after_secs });
            }
            Ok(Actor { token })""",
                "            Ok(Actor { token })",
            )
        ],
        ["bursts_spend_their_own_budget_on_every_surface"],
    ),
    # Admission costs one token BEFORE the body is read; moving the charge
    # past the parse makes malformed floods free again (a refused-at-parse
    # body never spends).
    "charge-after-parse": (
        [
            e(
                S / "ingest.rs",
                """    if let Err(retry_after_secs) = app.ingest_buckets.take(key.id, 1, app.now().unix_micros()) {
        Metrics::add(&app.metrics.rate_limited, 1);
        return Err(ApiError::RateLimited { retry_after_secs });
    }""",
                "",
            ),
            e(
                S / "ingest.rs",
                "    let count = u32::try_from(raw.events.len()).unwrap_or(u32::MAX);",
                """    if let Err(retry_after_secs) = app.ingest_buckets.take(key.id, 1, app.now().unix_micros()) {
        Metrics::add(&app.metrics.rate_limited, 1);
        return Err(ApiError::RateLimited { retry_after_secs });
    }
    let count = u32::try_from(raw.events.len()).unwrap_or(u32::MAX);""",
            ),
        ],
        ["a_malformed_flood_still_spends_the_ingest_budget_over_http"],
    ),
    # Every authorize-denial writes an audit row before answering. Skipping
    # the write leaves the flood's denials out of the ledger.
    "denials-not-audited": (
        [
            e(
                S / "query.rs",
                """    match tokio::time::timeout(app.config.audit_deadline, app.store.audit(&entry)).await {
        Ok(Ok(())) => ApiError::Forbidden(reason),
        _ => ApiError::AuditUnavailable,
    }""",
                """    let _ = &entry;
    ApiError::Forbidden(reason)""",
            )
        ],
        ["an_unknown_slug_flood_stops_at_its_own_bucket"],
    ),
    # (project_id, event_id) is unique; the DO NOTHING is what makes a replay
    # merge instead of erroring. Removing it turns the second batch into a
    # constraint violation.
    "replay-double-writes": (
        [
            e(
                T / "lib.rs",
                "         ON CONFLICT (project_id, event_id) DO NOTHING",
                "",
            )
        ],
        ["a_replayed_batch_is_counted_not_rewritten"],
    ),
    # control_insert's DO NOTHING + re-select is the replay idempotence.
    # Removing the clause makes the second insert a hard conflict.
    "control-replay-reinserts": (
        [
            e(
                T / "lib.rs",
                "             ON CONFLICT (project_id, operation_id, action, business_key) DO NOTHING",
                "",
            )
        ],
        ["a_replayed_control_call_returns_the_same_record"],
    ),
    # business_key_id is the only gate between caller text and the adapter's
    # outbound URL. Accepting anything sends the forged keys to the stub.
    "business-key-unvalidated": (
        [
            e(
                S / "control.rs",
                """pub fn business_key_id(s: &str) -> bool {
    !s.is_empty()""",
                """pub fn business_key_id(s: &str) -> bool {
    let _ = s;
    return true;
    !s.is_empty()""",
            )
        ],
        ["a_forged_business_key_never_reaches_the_authority"],
    ),
}


def _bak(name: str, path: Path) -> Path:
    tag = hashlib.sha256(str(path).encode()).hexdigest()[:12]
    return BACKUP / name / f"{tag}_{path.name}"


def mutate(name: str) -> None:
    """Apply `name`'s edit(s); `old` must appear in its file exactly once.
    The pre-edit file is backed up first so `restore` is byte-exact."""
    edits, _ = MUTANTS[name]
    for path, old, new in edits:
        text = path.read_text()
        count = text.count(old)
        if count != 1:
            sys.exit(
                f"mutant {name}: {old[:50]!r}… found {count}× in {path}"
                " — the source drifted; redraw the mutant"
            )
        bak = _bak(name, path)
        # One backup per file per mutant, taken BEFORE the first edit — a
        # multi-edit mutant must not overwrite it with a half-mutated file.
        if not bak.exists():
            bak.parent.mkdir(parents=True, exist_ok=True)
            bak.write_bytes(path.read_bytes())
        path.write_text(text.replace(old, new, 1))


def restore(name: str) -> None:
    """Put back every file `name` touched — byte-for-byte from its backup."""
    edits, _ = MUTANTS[name]
    seen: set[Path] = set()
    for path, _, _ in edits:
        if path in seen:
            continue
        seen.add(path)
        bak = _bak(name, path)
        if not bak.exists():
            sys.exit(f"restore {name}: no backup for {path}")
        path.write_bytes(bak.read_bytes())
        bak.unlink()


def tests(name: str) -> None:
    print(" ".join(MUTANTS[name][1]))


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "list":
        print("\n".join(MUTANTS))
        sys.exit(0)
    name = sys.argv[2] if len(sys.argv) > 2 else ""
    if cmd == "apply" and name:
        mutate(name)
    elif cmd == "restore" and name:
        restore(name)
    elif cmd == "tests" and name:
        tests(name)
    else:
        sys.exit("usage: mutants.py apply|restore|tests <name> | list")
