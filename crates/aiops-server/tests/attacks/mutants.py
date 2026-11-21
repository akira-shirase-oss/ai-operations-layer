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
                """            state.tokens.remove(&digest);
            state.ingest_keys.remove(&digest);
            tracing::info!("revoked credential evicted from caches");""",
                "            let _ = digest;",
            ),
        ],
        ["an_in_process_revoke_dies_before_the_next_request"],
    ),
    # The NOTIFY listener is the only way a peer-process revoke reaches this
    # collector's cache. Cut the digest eviction and the poll sees 200 until
    # the TTL. (A whole-handle early `return` leaves unreachable match arms —
    # uncompilable under the harness's -D warnings, so the mutant cuts the
    # eviction calls themselves.)
    "notify-deaf": (
        [
            e(
                S / "notify.rs",
                """            state.tokens.remove(&digest);
            state.ingest_keys.remove(&digest);
            tracing::info!("revoked credential evicted from caches");""",
                "            let _ = digest;",
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
                """            state.tokens.remove(&digest);
            state.ingest_keys.remove(&digest);
            tracing::info!("revoked credential evicted from caches");""",
                "            let _ = digest;",
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
                """                if tokio::select! {
                    () = cancel.cancelled() => false,
                    () = tokio::time::sleep(backoff) => true,
                } {
                    backoff = (backoff * 2).min(RESYNC_BACKOFF_MAX);
                    continue;
                }
                return false;""",
                """                if backoff >= RESYNC_BACKOFF_MAX {
                    let _ = cancel.cancelled();
                }
                backoff = RESYNC_BACKOFF_MAX;
                return backoff < RESYNC_BACKOFF_MAX;""",
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
                "        .layer(DefaultBodyLimit::disable())\n        .layer(RequestBodyLimitLayer::new(body_limit.max(usize::MAX)));",
            ),
            e(
                S / "ingest.rs",
                "    let limit = app.limits.max_body_bytes;",
                "    let limit = app.limits.max_body_bytes.max(usize::MAX);",
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
                """        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(3600).max(timeout),
        ));""",
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
                """    let _ = app.ingest_buckets.take(key.id, 1, app.now().unix_micros());""",
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
                """    let _ = app
        .ingest_buckets
        .take(key.id, count.saturating_sub(1), received_at.unix_micros());""",
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
                S / "error.rs",
                """    if bad {
        request.headers_mut().remove(REQUEST_ID_HEADER);
    }""",
                """    if !bad {
        request.headers_mut().remove(REQUEST_ID_HEADER);
    }""",
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
                """            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if ok { s } else { "<invalid>" }""",
                """            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if ok || s.len() < usize::MAX {
        s
    } else {
        "<invalid>"
    }""",
            ),
            e(
                T / "lib.rs",
                "    if hex32 || uuid { s } else { \"<invalid>\" }",
                "    if hex32 || uuid || s.len() < usize::MAX { s } else { \"<invalid>\" }",
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
                """            let _ = buckets.take(token.id, 1, now.unix_micros());
            Ok(Actor { token })""",
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
    if s.len() < usize::MAX {
        return true;
    }
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
        [
            "a_credential_key_is_withheld_even_when_opted_in",
            "credential_codes_refuse_and_operational_keys_opt_in",
        ],
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
        [
            "a_credential_key_is_withheld_even_when_opted_in",
            "credential_codes_refuse_and_operational_keys_opt_in",
        ],
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
    # In free text, `name<sep>value` keeps its value only under a benign
    # name — the allowlist gate is what strips it; a `password=hunter2`
    # inside an error_message otherwise stores raw.
    "keyed-text-kept": (
        [
            e(
                D / "policy.rs",
                "            if !(benign_text_name(&name)",
                "            if !(benign_text_name(&name) || true",
            )
        ],
        [
            "keyed_text_inside_free_text_loses_the_value",
            "non_benign_names_and_bad_escapes_lose_the_field",
        ],
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
                """        !(state_tail || port_tail) && tail.is_empty()""",
            )
        ],
        [
            "a_credential_shaped_identifier_is_refused_not_rewritten",
            "credential_codes_refuse_and_operational_keys_opt_in",
        ],
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
                """        && !["__none__"].contains(&field) && BARE_HEX_KEPT.len() < usize::MAX""",
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
                """        !(state_tail
            || last_is_credential
            || tail
                .split(|b| matches!(b, b':' | b'.' | b'-' | b'_' | b'/'))
                .all(|p| {
                    STATE_WORDS.contains(&p)
                        || (p.len() <= 5 && !p.is_empty() && p.iter().all(u8::is_ascii_digit))
                }))""",
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
                """    parts
        .iter()
        .any(|p| p.ends_with(b"key") && (p.len() > 3 || parts.len() > 1))""",
                """    parts
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
        [
            "credential_named_keys_die_everywhere_opt_in_cannot_reach",
            "credential_codes_refuse_and_operational_keys_opt_in",
        ],
    ),
    # Review round 7, N1: the whole name decides through a CLOSED safe
    # list — removing it re-credentials `status_code`/`error_code` (the
    # default attribute surface dies) and `aws.s3.key`/`bypass`/`compass`
    # (the opt-in test's operational keeps refuse).
    "code-names-credential": (
        [
            e(
                D / "policy.rs",
                """    if NON_CREDENTIAL_NAMES.iter().any(|e| name_on(&parts, e)) {
        return false;
    }""",
                """    let _ = NON_CREDENTIAL_NAMES;""",
            )
        ],
        [
            "the_default_attribute_surface_survives_code",
            "credential_codes_refuse_and_operational_keys_opt_in",
        ],
    ),
    # Review round 6, A3 follow-up + round 7, N1: a credential `*code` key
    # makes a digit tail a PIN — checking only the bare last part lets
    # `reset.code:4421` ride the port exemption.
    "qualified-code-tail-a-port": (
        [
            e(
                D / "policy.rs",
                """        let last_is_credential = parts.last().is_some_and(|p| {
            credential_key(p)
                || (p.as_slice() == b"code"
                    && parts.len() > 1
                    && !NON_CREDENTIAL_NAMES.iter().any(|e| name_on(&parts, e)))
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
            // One log line per SCHEDULED resync, not per hint (review
            // round 7, N7): a `clear` inside the window only coalesces
            // the pending deadline, and a flood is attacker-shaped — it
            // must not buy a line per message.
            if resync_at.is_none() {
                tracing::info!("revocation channel hinted a clear; resync scheduled");
            }
            *resync_at = Some(resync_at.map_or(deadline, |d| d.min(deadline)));""",
                """            let _ = (last_resync, resync_at, RESYNC_DEBOUNCE);""",
            )
        ],
        [
            "two_clears_half_a_second_apart_both_land",
            "a_clear_flood_coalesces_and_digests_still_evict",
        ],
    ),
    # Review round 6, A2: the trailing-edge debounce is what bounds a clear
    # flood — scheduling every hint for `now` serialises one resync per
    # notice, so the resync count blows past the bound. The mutant keeps
    # `RESYNC_DEBOUNCE` referenced or the -D warnings build fails on the
    # unused constant and the kill is inconclusive.
    "clear-debounce-removed": (
        [
            e(
                S / "notify.rs",
                "                Ok(last) => (*last + RESYNC_DEBOUNCE).max(Instant::now()),",
                "                Ok(last) => {\n                    let _ = (last, RESYNC_DEBOUNCE);\n                    Instant::now()\n                }",
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
                """                match tokio::time::timeout(BEAT_TIMEOUT * 1000, revocation_beat(writer)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
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
    # Review round 7, N1: a skipped heartbeat trigger must read as a gap.
    # Two legs detect it — the beat's arming check (fast) and
    # HEARTBEAT_DEADLINE on the last observed notice (slow, still inside
    # the test's 30 s window) — so the mutant cuts both: the heartbeat
    # row leaves the arming list AND the deadline outlives the poll.
    "beat-skips-trigger": (
        [
            e(
                T / "lib.rs",
                """             ('projects', 'projects_opt_in_notify'),
             ('revocation_heartbeat', 'revocation_heartbeat_notify')""",
                """             ('projects', 'projects_opt_in_notify')""",
            ),
            e(
                S / "notify.rs",
                "const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(10);",
                "const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(3600);",
            ),
        ],
        ["a_beat_that_skips_the_trigger_reads_as_a_gap"],
    ),
    # Review round 7, ruling: a credential name must never enter the
    # benign list. Adding `backup_code` re-admits the N1 leak —
    # `backup_code=cv0sent` keeps its value.
    "benign-list-admits-credential": (
        [
            e(
                D / "policy.rs",
                """const BENIGN_TEXT_NAMES: &[&[&str]] = &[&["duration", "ms"]];""",
                """const BENIGN_TEXT_NAMES: &[&[&str]] = &[&["backup", "code"], &["duration", "ms"]];""",
            )
        ],
        ["non_benign_names_and_bad_escapes_lose_the_field"],
    ),
    # Review round 7, N2: only `%XX` decodes — every other escape form
    # fails the whole field. Answering `false` always stores
    # `password%u003Ahunter2` verbatim.
    "escape-forms-pass": (
        [
            e(
                D / "policy.rs",
                """fn undecodable_escape(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if matches!(bytes.get(i + 1), Some(b'u' | b'U')) {
                    return true;
                }
                i += 1;
            }
            b'\\\\' => {
                if matches!(bytes.get(i + 1), Some(b'x' | b'X' | b'u' | b'U')) {
                    return true;
                }
                i += 1;
            }
            b'&' => {
                // `&#…;` numeric/hex entities — `&#` is never plain text.
                if bytes.get(i + 1) == Some(&b'#') {
                    return true;
                }
                // `&name;` named entities: letters only, bounded like the
                // real vocabulary (`&colon;`, `&amp;`). An unclosed `&`
                // or a non-letter run is literal text and keeps.
                let name = bytes[i + 1..]
                    .iter()
                    .take(16)
                    .take_while(|b| b.is_ascii_alphabetic())
                    .count();
                if name > 0 && bytes.get(i + 1 + name) == Some(&b';') {
                    return true;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}""",
                """fn undecodable_escape(bytes: &[u8]) -> bool {
    let _ = bytes;
    false
}""",
            )
        ],
        ["non_benign_names_and_bad_escapes_lose_the_field"],
    ),
}

# The decisive-assertion pin of every attack test (review round 7, N4-b):
# a kill counts only when the named test's panic lands at one of these
# lines — the test's OWN verdict assertions, in its file. `audit` checks
# each pinned line still carries an assert/panic so a shifted file can't
# silently unpin a test; a panic anywhere else (a helper unwrap, setup,
# compile) is INCONCLUSIVE, not a kill.
PINS = {
    "every_absent_or_malformed_credential_is_refused_everywhere": (
        "auth_bypass.rs",
        [23, 24, 43, 53, 65, 70],
    ),
    "an_in_process_revoke_dies_before_the_next_request": (
        "auth_bypass.rs",
        [120, 127, 136, 137],
    ),
    "a_peer_revoke_is_refused_on_the_next_request": ("auth_bypass.rs", [184]),
    "a_revocation_survives_the_listener_dying": ("auth_bypass.rs", [246, 267]),
    "a_failed_catch_up_recovers_instead_of_staying_deaf": (
        "auth_bypass.rs",
        [374, 389],
    ),
    "a_revoke_inside_the_gap_only_the_catch_up_can_see": ("auth_bypass.rs", [437]),
    "a_beat_that_skips_the_trigger_reads_as_a_gap": ("auth_bypass.rs", [467]),
    "a_delete_invisible_to_the_channel_is_caught_by_the_resync": (
        "auth_bypass.rs",
        [529],
    ),
    "the_cli_will_not_mint_a_contradictory_admin_or_scopeless_token": (
        "auth_bypass.rs",
        [556, 570],
    ),
    "two_clears_half_a_second_apart_both_land": (
        "auth_bypass.rs",
        [632, 666, 679, 686],
    ),
    "a_clear_flood_coalesces_and_digests_still_evict": (
        "auth_bypass.rs",
        [770, 781, 793, 797],
    ),
    "an_unarmed_revocation_trigger_reads_as_a_gap": ("auth_bypass.rs", [826]),
    "a_beat_under_a_row_lock_reads_as_a_gap": ("auth_bypass.rs", [860]),
    "a_disabled_revoke_trigger_reads_as_a_gap": ("auth_bypass.rs", [889]),
    "bodies_over_every_cap_are_shed_not_parsed": (
        "bodies.rs",
        [31, 35, 51, 63, 79, 87],
    ),
    "a_body_sent_one_byte_a_heartbeat_is_bounded": ("bodies.rs", [173, 175, 179]),
    "one_projects_credentials_never_touch_anothers": (
        "cross_tenant.rs",
        [38, 53, 82, 83, 95],
    ),
    "forwarded_headers_do_not_move_the_per_token_bucket": (
        "headers_spoof.rs",
        [42, 48],
    ),
    "conflicting_authorization_lines_have_one_verdict": (
        "headers_spoof.rs",
        [78, 89, 95],
    ),
    "a_hostile_request_id_never_reaches_the_wire": (
        "headers_spoof.rs",
        [135, 136, 145],
    ),
    "forged_slugs_and_ids_in_paths_are_never_stored_raw": ("inject.rs", [31, 39]),
    "error_bodies_never_echo_attacker_bytes": ("inject.rs", [89, 96, 97]),
    "bursts_spend_their_own_budget_on_every_surface": (
        "rate_limit.rs",
        [36, 37, 38, 47, 50, 54],
    ),
    "a_malformed_flood_still_spends_the_ingest_budget_over_http": (
        "rate_limit.rs",
        [79, 80],
    ),
    "an_unknown_slug_flood_stops_at_its_own_bucket": (
        "rate_limit.rs",
        [111, 115, 116, 122],
    ),
    "a_credential_key_is_withheld_even_when_opted_in": (
        "redaction.rs",
        [48, 55, 59],
    ),
    "an_unknown_attribute_key_is_withheld": ("redaction.rs", [77, 79, 83, 87]),
    "a_field_the_event_type_does_not_carry_is_refused": ("redaction.rs", [110]),
    "keyed_text_inside_free_text_loses_the_value": ("redaction.rs", [131, 135]),
    "a_credential_shaped_identifier_is_refused_not_rewritten": (
        "redaction.rs",
        [156, 159, 167],
    ),
    "an_underscore_wrapped_token_dies_in_free_text": ("redaction.rs", [201, 205]),
    "a_leading_slash_token_dies_in_free_text": ("redaction.rs", [228]),
    "a_nested_escape_erases_the_whole_field": ("redaction.rs", [254, 258, 264]),
    "a_bare_hex_credential_dies_outside_release_and_request_id": (
        "redaction.rs",
        [296, 297, 314, 333, 348],
    ),
    "the_default_attribute_surface_survives_code": (
        "redaction.rs",
        [374, 375, 381, 385, 399, 408, 409, 422, 423],
    ),
    "credential_named_keys_die_everywhere_opt_in_cannot_reach": (
        "redaction.rs",
        [440, 441, 465, 473, 476, 487, 523],
    ),
    "a_commit_sha_and_a_hex_trace_id_are_identifiers": ("redaction.rs", [546, 550]),
    "pin_tails_and_new_credential_keys_still_refuse": (
        "redaction.rs",
        [585, 591, 607],
    ),
    "non_benign_names_and_bad_escapes_lose_the_field": (
        "redaction.rs",
        [646, 648, 672, 690, 692],
    ),
    "credential_codes_refuse_and_operational_keys_opt_in": (
        "redaction.rs",
        [722, 723, 738, 762, 768, 774, 801],
    ),
    "a_replayed_batch_is_counted_not_rewritten": ("replay.rs", [23, 27, 35, 36]),
    "a_replayed_control_call_returns_the_same_record": ("replay.rs", [65, 71]),
    "a_forged_business_key_never_reaches_the_authority": (
        "ssrf_keys.rs",
        [95, 96, 99, 113, 138],
    ),
}

# An attack test with NO mutant needs a written exemption here (N4-h) —
# the exemption IS the admission that the harness cannot prove it.
# Every test today carries a mutant, so this is empty on purpose.
EXEMPTIONS: dict[str, str] = {}


def attack_tests() -> dict[str, str]:
    """Every `#[sqlx::test]` attack test: name -> its file name."""
    import re

    out: dict[str, str] = {}
    for f in sorted((ROOT / "crates/aiops-server/tests/attacks").glob("*.rs")):
        if f.name == "common.rs":
            continue
        src = f.read_text()
        for m in re.finditer(r"#\[sqlx::test[^\]]*\]\s*\n\s*async fn (\w+)", src):
            out[m.group(1)] = f.name
    return out


def pin(test: str) -> None:
    """Print the test's pin as `file:l1,l2,...` for the shell to grep."""
    file, lines = PINS[test]
    print(f"{file}:{','.join(str(n) for n in lines)}")


def audit() -> int:
    """N4-(h): enumerate every attack test — each needs a mutant or a
    written exemption — plus pin sanity (N4-b): every pinned line must
    still carry an assert/panic, every pin must map to a real test, and
    every mutant's test list must pin to real tests."""
    tests = attack_tests()
    bad = 0
    covered = {t for _, ts in MUTANTS.values() for t in ts}
    for t, file in tests.items():
        if t not in covered and t not in EXEMPTIONS:
            print(f"audit: {t} ({file}) has no mutant and no exemption")
            bad += 1
        if t not in PINS:
            print(f"audit: {t} ({file}) has no decisive-assertion pin")
            bad += 1
    for t in PINS:
        if t not in tests:
            print(f"audit: pin {t} is not an attack test")
            bad += 1
    for m, (_, ts) in MUTANTS.items():
        for t in ts:
            if t not in tests:
                print(f"audit: mutant {m} lists non-test {t}")
                bad += 1
            elif t not in PINS:
                print(f"audit: mutant {m} lists unpinned test {t}")
                bad += 1
    for t, (file, lines) in PINS.items():
        path = ROOT / "crates/aiops-server/tests/attacks" / file
        if not path.exists():
            print(f"audit: pin {t} names missing file {file}")
            bad += 1
            continue
        src = path.read_text().splitlines()
        for n in lines:
            if not (0 < n <= len(src)):
                print(f"audit: pin {t}:{file}:{n} is out of range — the file moved")
                bad += 1
                continue
            if "assert" not in src[n - 1] and "panic" not in src[n - 1]:
                print(
                    f"audit: pin {t}:{file}:{n} is not an assertion "
                    f"({src[n - 1].strip()[:60]}) — the file moved"
                )
                bad += 1
    if bad:
        return 1
    print(
        f"audit: {len(tests)} attack tests — "
        f"{sum(1 for t in tests if t in covered)} mutant-covered, "
        f"{len(EXEMPTIONS)} exempted, {len(PINS)} pinned"
    )
    return 0


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
    """Put back every file `name` touched — byte-for-byte from its backup.
    A file with no backup was never edited (an apply killed mid-mutant),
    so skipping it is what makes an EXIT-trap restore safe."""
    edits, _ = MUTANTS[name]
    seen: set[Path] = set()
    for path, _, _ in edits:
        if path in seen:
            continue
        seen.add(path)
        bak = _bak(name, path)
        if not bak.exists():
            continue
        path.write_bytes(bak.read_bytes())
        bak.unlink()


def _test_lines(name: str) -> None:
    print(" ".join(MUTANTS[name][1]))


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "list":
        print("\n".join(MUTANTS))
        sys.exit(0)
    if cmd == "audit":
        sys.exit(audit())
    name = sys.argv[2] if len(sys.argv) > 2 else ""
    if cmd == "apply" and name:
        if name not in MUTANTS:
            sys.exit(f"unknown mutant: {name}")
        mutate(name)
    elif cmd == "restore" and name:
        if name not in MUTANTS:
            sys.exit(f"unknown mutant: {name}")
        restore(name)
    elif cmd == "tests" and name:
        if name not in MUTANTS:
            sys.exit(f"unknown mutant: {name}")
        _test_lines(name)
    elif cmd == "pin" and name:
        if name not in PINS:
            sys.exit(f"unpinned test: {name}")
        pin(name)
    else:
        sys.exit("usage: mutants.py apply|restore|tests <name> | pin <test> | list | audit")
