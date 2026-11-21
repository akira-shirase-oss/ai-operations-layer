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
D = ROOT / "crates/aiops-domain/src"
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
                """    if let Some(key) = found {
        return Ok(key);
    }
    Metrics::add(&app.metrics.unauthenticated, 1);
    Err(ApiError::Unauthenticated)""",
                """    if let Some(key) = found {
        return Ok(key);
    }
    Ok(IngestKey {
        id: Uuid::nil(),
        name: "forged".into(),
        project_id: Uuid::nil(),
        project_slug: "forged".into(),
        opt_in_keys: vec![],
        revoked: false,
    })""",
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
                "    Metrics::add(&state.revocation_notices, 1);",
                "    Metrics::add(&state.revocation_notices, 1);\n    let _ = (payload, last_resync, resync_at);\n    return;",
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
                "    Metrics::add(&state.revocation_notices, 1);",
                "    Metrics::add(&state.revocation_notices, 1);\n    let _ = (payload, last_resync, resync_at);\n    return;",
            )
        ],
        ["a_peer_revoke_is_refused_on_the_next_request"],
    ),
    # The channel's recovery is four legs — gap detection, the resync's
    # catch-up eviction, live NOTIFY on the re-LISTENed conn, and `resolve`
    # distrusting the cache while unproven — and each can independently
    # produce the 401 this test asserts, so a mutant that cuts only one
    # stays green (measured: three did). Weaken exactly the tested
    # protection — "a dead backend must not strand a revocation" — by
    # cutting every eviction path at once: the resync clears nothing and
    # re-arms nothing, handle() is deaf to the re-LISTENed conn's
    # notices, and resolve trusts the entry for its full TTL as if the
    # channel were healthy. Detection still runs (the gap counter proves
    # the test reached its scenario); eviction is what's gone.
    "listener-gap-not-resynced": (
        [
            e(
                S / "notify.rs",
                "        state.tokens.clear();\n        state.ingest_keys.clear();",
                "        let _ = &state.tokens;",
            ),
            e(
                S / "notify.rs",
                """                for digest in digests {
                    state.tokens.put(digest, None);
                }""",
                """            let _ = digests;""",
            ),
            e(
                S / "notify.rs",
                "    Metrics::add(&state.revocation_notices, 1);",
                "    Metrics::add(&state.revocation_notices, 1);\n    let _ = (payload, last_resync, resync_at);\n    return;",
            ),
            e(
                S / "app.rs",
                "        let healthy = self.revocation_healthy.load(Ordering::Relaxed);",
                "        let healthy = true;",
            ),
        ],
        ["a_revocation_survives_the_listener_dying"],
    ),
    # The resync is the only enforcement for a write the channel never
    # heard: a trigger-disabled revoke and a replica-session delete both
    # leave no NOTIFY anywhere, so only the resync's clear + bounded
    # digest read removes the cached ghost (review N-C/N-G/N-H). A resync
    # that "succeeds" but evicts nothing leaves both tests at 200. The
    # overlapping legs must be cut too (review round 6, B3 — a green
    # mutant proves nothing): the V1 window must not shrink the fresh
    # TTL, so `resolve` keeps trusting the ghost for its full TTL.
    "resync-evicts-nothing": (
        [
            e(
                S / "notify.rs",
                "        state.tokens.clear();\n        state.ingest_keys.clear();",
                "        let _ = &state.tokens;",
            ),
            e(
                S / "notify.rs",
                """                for digest in digests {
                    state.tokens.put(digest, None);
                }""",
                """            let _ = digests;""",
            ),
            e(
                S / "app.rs",
                "        let healthy = self.revocation_healthy.load(Ordering::Relaxed);",
                "        let healthy = true;",
            ),
        ],
        [
            "a_revoke_inside_the_gap_only_the_catch_up_can_see",
            "a_delete_invisible_to_the_channel_is_caught_by_the_resync",
        ],
    ),
    # A failed catch-up must retry until it lands (review N-C); answering
    # the Err arm with `true` leaves the channel deaf and /readyz down
    # after the lock clears — the test's recovery never comes.
    "resync-never-retries": (
        [
            e(
                S / "notify.rs",
                '                tracing::warn!(error = %e, "revocation resync failed; caches cleared, retrying");',
                '                tracing::warn!(error = %e, "revocation resync failed; caches cleared, retrying");\n                return true;',
            ),
        ],
        ["a_failed_catch_up_recovers_instead_of_staying_deaf"],
    ),
    # (Removed — `heartbeat-skips-trigger`: under round 6 B2 the beat also
    # checks every trigger's arming, so bypassing the UPDATE is caught by
    # the tgrelid check the `api-token-trigger-unproven` mutant already
    # proves. The scenario the mutant exercised is redundant.)
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
    # REDACTION-POLICY (docs/REDACTION-POLICY.md) attack classes — the typed
    # field policy is the privacy boundary. Each mutant weakens exactly the
    # rule its attack test asserts.
    # The credential check must precede the opt-in list; moving it after
    # means an opted-in credential key stores its value raw.
    "opt-in-beats-credential": (
        [
            e(
                D / "policy.rs",
                """        if credential_key(key.as_bytes()) {
            return KeyTreatment::Withheld;
        }
        if DEFAULT_ALLOWED_ATTRIBUTES.contains(&key) {
            KeyTreatment::Default
        } else if self.opt_in_keys.iter().any(|k| k == key) {
            KeyTreatment::OptIn
        } else {
            KeyTreatment::Withheld
        }""",
                """        if DEFAULT_ALLOWED_ATTRIBUTES.contains(&key) {
            KeyTreatment::Default
        } else if self.opt_in_keys.iter().any(|k| k == key) {
            KeyTreatment::OptIn
        } else if credential_key(key.as_bytes()) {
            KeyTreatment::Withheld
        } else {
            KeyTreatment::Withheld
        }""",
            )
        ],
        ["a_credential_key_is_withheld_even_when_opted_in"],
    ),
    # A credential key's value must die whatever its shape — the key's name
    # decides. Answering OptIn instead puts the value through the free-text
    # scanner, where a benign-shaped `hunter2` survives raw.
    "credential-key-by-shape": (
        [
            e(
                D / "policy.rs",
                """        if credential_key(key.as_bytes()) {
            return KeyTreatment::Withheld;
        }""",
                """        if credential_key(key.as_bytes()) {
            return KeyTreatment::OptIn;
        }""",
            )
        ],
        ["a_credential_key_is_withheld_even_when_opted_in"],
    ),
    # An unknown attribute key is withheld by default; answering Default
    # stores every unlisted key the caller invents.
    "unknown-key-default-allowed": (
        [
            e(
                D / "policy.rs",
                """        } else if self.opt_in_keys.iter().any(|k| k == key) {
            KeyTreatment::OptIn
        } else {
            KeyTreatment::Withheld
        }""",
                """        } else if self.opt_in_keys.iter().any(|k| k == key) {
            KeyTreatment::OptIn
        } else {
            KeyTreatment::Default
        }""",
            )
        ],
        ["an_unknown_attribute_key_is_withheld"],
    ),
    # only() is what makes a field the event type does not carry a
    # field_not_allowed refusal; never matching admits it silently.
    "unclassified-field-admitted": (
        [
            e(
                D / "validate.rs",
                ".find(|f| field_present(w, f) && !allowed.contains(f))",
                ".find(|f| field_present(w, f) && !allowed.contains(f) && false)",
            )
        ],
        ["a_field_the_event_type_does_not_carry_is_refused"],
    ),
    # In free text, `key=value` under a credential name still loses the
    # value — the keyed arm is what strips it; a `password=hunter2` inside
    # an error_message otherwise stores `hunter2` raw.
    "keyed-text-kept": (
        [
            e(
                D / "policy.rs",
                "            && credential_key(&rest[..key_len])",
                "            && false && credential_key(&rest[..key_len])",
            )
        ],
        ["keyed_text_inside_free_text_loses_the_value"],
    ),
    # An identifier field refuses credential-keyed `name:value` — the tail
    # check deciding that is narrowed to state words and ONE port piece
    # under a non-credential last part. Admitting every tail stores
    # `password:hunter2` as a service name.
    "identifier-credential-stored": (
        [
            e(
                D / "policy.rs",
                """        !(state_tail || port_tail)""",
                """        false""",
            )
        ],
        ["a_credential_shaped_identifier_is_refused_not_rewritten"],
    ),
    # Review round 5, finding 1: the N-J skip must exempt the position after
    # a leading `_` — without it every `_`-wrapped credential survives.
    "underscore-boundary-skipped": (
        [
            e(
                D / "policy.rs",
                """            && !(view[i - 1] == b'_' && (i < 2 || !is_word(view[i - 2])))""",
                """            && true""",
            )
        ],
        ["an_underscore_wrapped_token_dies_in_free_text"],
    ),
    # The hex run's word-continuation check must skip `_` joiners —
    # `_<32hex>_` is italics decoration, not a word continuation.
    "hex-decoration-continues-word": (
        [
            e(
                D / "policy.rs",
                """        let continues = rest[hex_run..]
            .iter()
            .find(|b| **b != b'_')
            .is_some_and(|b| is_word(*b));""",
                """        let continues = rest.get(hex_run).is_some_and(|b| is_word(*b));""",
            )
        ],
        ["an_underscore_wrapped_token_dies_in_free_text"],
    ),
    # Review round 5, finding 8: `/` must open a run — a std-base64 token
    # may lead with it, and the 31-char tail otherwise survives.
    "slash-cant-open-run": (
        [
            e(
                D / "policy.rs",
                """        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(*b, b'_' | b'-' | b'+' | b'/'))""",
                """        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(*b, b'_' | b'-' | b'+'))""",
            )
        ],
        ["a_leading_slash_token_dies_in_free_text"],
    ),
    # Review round 6, A1: a capped decode residual can hide the key's own
    # separator, so the whole field fails closed — span-level work stored
    # `password[redacted]hunter2`. Drop the early return and the residue
    # leaks the value beside the span again.
    "residual-erases-field": (
        [
            e(
                D / "policy.rs",
                """        if percent_decode(&view).is_some() {
            return Some(REDACTED.to_owned());
        }""",
                """        let _ = percent_decode(&view);""",
            )
        ],
        ["a_nested_escape_erases_the_whole_field"],
    ),
    # Review round 6, A4: bare ≥32 hex is a credential shape in every
    # identifier field except `release`/`request_id` — without the check a
    # Twilio/Datadog-shaped token stores verbatim in deployment, model,
    # error_code, source.* and cost_estimate.basis.
    "bare-hex-admitted": (
        [
            e(
                D / "policy.rs",
                """    if bytes.len() >= 32
        && !BARE_HEX_KEPT.contains(&field)
        && bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return true;
    }""",
                """    let _ = (bytes.len(), BARE_HEX_KEPT, field);""",
            )
        ],
        ["a_bare_hex_credential_dies_outside_release_and_request_id"],
    ),
    # Review round 6, A4: the exception is `release`/`request_id` ONLY —
    # ignoring it refuses the commit SHA and the hex trace id the contract
    # declares.
    "hex-exceptions-ignored": (
        [
            e(
                D / "policy.rs",
                """        && !BARE_HEX_KEPT.contains(&field)""",
                """        && !["__none__"].contains(&field)""",
            )
        ],
        ["a_commit_sha_and_a_hex_trace_id_are_identifiers"],
    ),
    # Review round 5, finding 5: the `:`-tail keep is ONE digit piece under
    # a non-credential last part — an uncapped per-piece count admits PINs.
    "digit-tail-uncapped": (
        [
            e(
                D / "policy.rs",
                """        let port_tail = !last_is_credential
            && !tail.is_empty()
            && tail.len() <= 5
            && tail.iter().all(u8::is_ascii_digit);
        !(state_tail || port_tail)""",
                """        !(tail
            .split(|b| matches!(b, b':' | b'.' | b'-' | b'_' | b'/'))
            .all(|p| STATE_WORDS.contains(&p)
                || (p.len() <= 5 && !p.is_empty() && p.iter().all(u8::is_ascii_digit))))""",
            )
        ],
        ["pin_tails_and_new_credential_keys_still_refuse"],
    ),
    # `auth:`/`authorization:` carry no exemption — a non-state tail under
    # them is a credential, never a category.
    "auth-exempt-again": (
        [
            e(
                D / "policy.rs",
                """        let key = &rest[..key_len];
        if !credential_key(key) {
            return false;
        }
        if rest[key_len] == b'=' {""",
                """        let key = &rest[..key_len];
        if !credential_key(key)
            || key.eq_ignore_ascii_case(b"auth")
            || key.eq_ignore_ascii_case(b"authorization")
        {
            return false;
        }
        if rest[key_len] == b'=' {""",
            )
        ],
        ["pin_tails_and_new_credential_keys_still_refuse"],
    ),
    # `pin`/`otp`/`passcode`/`mfa`/`pass` are credential key names —
    # thinning the namespace admits them in identifier and attribute
    # roles. BOTH lists must thin: a bare name is a part, and every one of
    # these is also a suffix (`pin` dies either way).
    "credential-list-thinned": (
        [
            e(
                D / "policy.rs",
                '''    "otp",
    "pass",
    "passcode",''',
                '''    "passcode",''',
            ),
            e(
                D / "policy.rs",
                '''    "password",
    "pin",
    "pwd",''',
                '''    "password",
    "pwd",''',
            ),
            e(
                D / "policy.rs",
                '''    b"otp",
    b"pass",''',
                '''    b"pass",''',
            ),
            e(
                D / "policy.rs",
                '''    b"passwordhash",
    b"pin",''',
                '''    b"passwordhash",''',
            ),
        ],
        ["pin_tails_and_new_credential_keys_still_refuse"],
    ),
    # Review round 6, B1: any `*key` part names a credential outside the
    # safe list — drop the arm and `stripe_key`/`hmac_key`/`accesskey`/
    # `privatekey`/`OpenAIApiKey` opt in and apply.
    "key-suffix-not-credential": (
        [
            e(
                D / "policy.rs",
                """        && parts
            .iter()
            .any(|p| p.ends_with(b"key") && (p.len() > 3 || parts.len() > 1))""",
                """        && parts
            .iter()
            .any(|p| p.ends_with(b"key") && p.len() > 3 && parts.len() > 100)""",
            )
        ],
        ["credential_named_keys_die_everywhere_opt_in_cannot_reach"],
    ),
    # Review round 6, B1: `set-opt-in` must refuse a credential-named key —
    # without the check the CLI reports success on an opt-in ingestion can
    # never apply.
    "opt-in-trusts-name": (
        [
            e(
                S / "admin.rs",
                """            for key in &keys {
                anyhow::ensure!(
                    !aiops_domain::policy::credential_key(key.as_bytes()),
                    "opt-in key `{key}` names a credential — credential keys are always withheld"
                );
            }""",
                """            let _ = &keys;""",
            )
        ],
        ["credential_named_keys_die_everywhere_opt_in_cannot_reach"],
    ),
    # Review round 6, A3: `code` names a credential only after a qualifier —
    # unconditionally it withholds the default `http.status_code` attribute
    # and redacts ordinary "status code: 503" text.
    "code-names-credential": (
        [
            e(
                D / "policy.rs",
                """        if part == b"code" && CODE_QUALIFIERS.iter().any(|q| q.as_bytes() == prev) {""",
                """        if part == b"code" {""",
            )
        ],
        ["the_default_attribute_surface_survives_code"],
    ),
    # Review round 6, A3 follow-up: a qualified `code` key makes a digit
    # tail a PIN — checking only the bare last part lets `reset.code:4421`
    # ride the port exemption.
    "qualified-code-tail-a-port": (
        [
            e(
                D / "policy.rs",
                """        let last_is_credential = parts.last().is_some_and(|p| {
            credential_key(p)
                || (p.as_slice() == b"code"
                    && parts.len() > 1
                    && CODE_QUALIFIERS
                        .iter()
                        .any(|q| q.as_bytes() == parts[parts.len() - 2].as_slice()))
        });""",
                """        let last_is_credential = parts.last().is_some_and(|p| credential_key(p));""",
            )
        ],
        ["the_default_attribute_surface_survives_code"],
    ),
    # Review round 5, finding 2 + round 6, A2: a `clear` hint must schedule
    # the debounced resync — never scheduling it leaves a withdrawn opt-in
    # applying from cache and no flood resyncs at all.
    "clear-hints-dropped": (
        [
            e(
                S / "notify.rs",
                """            let pending = match last_resync.lock() {
                // A poisoned debounce lock resyncs at once — cheaper than
                // a deadlock.
                Err(_) => Instant::now(),
                Ok(last) => (*last + RESYNC_DEBOUNCE).max(Instant::now()),
            };
            let deadline = tokio::time::Instant::from_std(pending);
            *resync_at = Some(resync_at.map_or(deadline, |d| d.min(deadline)));""",
                """            let _ = (last_resync, resync_at);""",
            )
        ],
        [
            "two_clears_half_a_second_apart_both_land",
            "a_clear_flood_coalesces_and_digests_still_evict",
        ],
    ),
    # Review round 6, A2: the trailing-edge debounce is what bounds a clear
    # flood — scheduling every hint for `now` serialises one resync per
    # notice, so the resync count blows past the bound.
    "clear-debounce-removed": (
        [
            e(
                S / "notify.rs",
                "                Ok(last) => (*last + RESYNC_DEBOUNCE).max(Instant::now()),",
                "                Ok(_) => Instant::now(),",
            )
        ],
        ["a_clear_flood_coalesces_and_digests_still_evict"],
    ),
    # Review round 5, finding 3: the beat is bounded twice — the writer
    # pool's lock/statement timeouts cover a blocked row, the client's 2 s
    # timeout covers a half-open connection. Remove BOTH and a lock on the
    # heartbeat row freezes the watcher inside the select arm, green.
    "beat-unbounded": (
        [
            e(
                S / "notify.rs",
                """                match tokio::time::timeout(BEAT_TIMEOUT, revocation_beat(writer)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "revocation heartbeat failed; reconnecting");
                        Metrics::add(&state.revocation_gaps, 1);
                        return true;
                    }""",
                """                match revocation_beat(writer).await {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "revocation heartbeat failed; reconnecting");
                        Metrics::add(&state.revocation_gaps, 1);
                        return true;
                    }""",
            ),
            e(
                T / "lib.rs",
                """            .after_connect(|conn, _| {
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
            })""",
                """""",
            ),
        ],
        ["a_beat_under_a_row_lock_reads_as_a_gap"],
    ),
    # Review round 5, finding 7 + round 6, B2: the beat checks EVERY
    # revocation trigger's arming by table OID and requires 'A' — without
    # it a disabled or origin-only trigger stays green while deletes and
    # replica-session revokes go deaf.
    "api-token-trigger-unproven": (
        [
            e(
                T / "lib.rs",
                """    let unarmed: Vec<String> = sqlx::query_scalar(
        "SELECT v.tgname
         FROM (VALUES
             ('api_tokens', 'api_tokens_revoked_notify'),
             ('api_tokens', 'api_tokens_deleted_notify'),
             ('api_tokens', 'api_tokens_truncated_notify'),
             ('ingest_keys', 'ingest_keys_revoked_notify'),
             ('ingest_keys', 'ingest_keys_deleted_notify'),
             ('ingest_keys', 'ingest_keys_truncated_notify'),
             ('projects', 'projects_opt_in_notify'),
             ('revocation_heartbeat', 'revocation_heartbeat_notify')
         ) AS v(relname, tgname)
         LEFT JOIN pg_trigger t
           ON t.tgname = v.tgname
          AND t.tgrelid = v.relname::regclass
         WHERE t.tgenabled IS NULL OR t.tgenabled <> 'A'",
    )
    .fetch_all(pool)
    .await?;
    if !unarmed.is_empty() {
        return Err(StoreError::TriggerUnarmed(unarmed.join(",")));
    }
    Ok(())""",
                """    Ok(())""",
            )
        ],
        [
            "a_disabled_revoke_trigger_reads_as_a_gap",
            "an_unarmed_revocation_trigger_reads_as_a_gap",
        ],
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
