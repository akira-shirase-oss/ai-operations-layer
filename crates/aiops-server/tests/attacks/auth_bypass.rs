//! Auth bypass attempts: every credential shape that must not work, on every
//! surface — plus the CLI refusing to mint a contradictory credential.

use crate::common::{
    audit_rows, batch, cli_status, create_ingest_key, create_project, create_token, get, journey,
    post_ingest, stack,
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn every_absent_or_malformed_credential_is_refused_everywhere(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (reader, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);
    let key = create_ingest_key(&s.url, "pilot", "h");
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&journey(1, 0))).await;
    assert_eq!(status, 200, "{body}");
    let op = crate::common::op_hex(1);
    let op_path = format!("/v1/projects/pilot/operations/{op}");
    let ingest_body = batch(&journey(2, 100));

    // No credential at all.
    assert_eq!(post_ingest(&s.base, None, ingest_body.clone()).await.0, 401);
    assert_eq!(get(&s.base, &op_path, None).await.0, 401);
    assert_eq!(get(&s.base, "/v1/admin/audit", None).await.0, 401);

    // Malformed, wrong scheme, wrong prefix, right prefix + wrong body.
    for bad in [
        "Bearer",
        "Bearer garbage",
        "Basic aGVsbG8=",
        "Bearer aiops_tk_",
        "Bearer aiops_tk_short",
        "Bearer aiops_tk_!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!",
        "Bearer aiops_ik_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let r = crate::common::client()
            .get(format!("{}{op_path}", s.base))
            .header("authorization", bad)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401, "{bad}");
        // An ingest key shape is a key, not a token — the path's own rules apply.
        let r = crate::common::client()
            .post(format!("{}/v1/ingest", s.base))
            .header("authorization", bad)
            .header("content-type", "application/json")
            .body(ingest_body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401, "ingest {bad}");
    }

    // A token that is everything but the one that was minted.
    let mut forged = reader.clone();
    forged.replace_range(9..12, "zzz");
    let r = crate::common::client()
        .get(format!("{}{op_path}", s.base))
        .bearer_auth(&forged)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401, "a forged token body is refused");

    // Every refusal above was unauthenticated: no actor, no audit row — the
    // log only ever carries decisions that reached authorize.
    let rows = audit_rows(&pool, 20).await;
    assert_eq!(rows, Vec::new(), "a 401 never audits: {rows:?}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_in_process_revoke_dies_before_the_next_request(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "dead");
    let (token, token_id) = create_token(&s.url, "bob", &["read"], &["pilot"]);
    let (admin, _) = create_token(&s.url, "root", &["admin"], &[]);

    assert_eq!(
        post_ingest(&s.base, Some(&key), batch(&journey(1, 0)))
            .await
            .0,
        200
    );
    assert_eq!(
        get(&s.base, "/v1/projects/pilot/operations", Some(&token))
            .await
            .0,
        200
    );

    // The serving-process revoke (the console's path) evicts the resolver
    // cache itself: the very next call is refused (D-034).
    let (status, body) = crate::common::get(&s.base, "/v1/admin/credentials", Some(&admin)).await;
    assert_eq!(status, 200, "{body}");
    let key_id = body["ingest_keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "dead")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (kind, id) in [
        ("ingest-keys", key_id.clone()),
        ("tokens", token_id.clone()),
    ] {
        let (status, body) = crate::common::post_json(
            &s.base,
            &format!("/v1/admin/credentials/{kind}/{id}/revoke"),
            Some(&admin),
            &serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "{kind} revoke: {body}");
    }
    assert_eq!(
        post_ingest(&s.base, Some(&key), batch(&journey(2, 0)))
            .await
            .0,
        401,
        "a revoked ingest key"
    );
    assert_eq!(
        get(&s.base, "/v1/projects/pilot/operations", Some(&token))
            .await
            .0,
        401,
        "a revoked token"
    );
    // The ledger re-read: both rows carry revoked_at — the 401s reflect a
    // real state change, not a stale answer.
    assert!(crate::common::is_revoked(&pool, "ingest_keys", &key_id).await);
    assert!(crate::common::is_revoked(&pool, "api_tokens", &token_id).await);
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_peer_revoke_is_refused_on_the_next_request(pool: PgPool) {
    // The admin CLI revokes in the database — another process, so the running
    // collector's cache cannot see the row. The aiops_notify_revocation
    // trigger NOTIFYs the digest and the server's listener evicts it: the
    // refusal lands on the next request, nowhere near the 30 s cache TTL
    // the peer bound used to be (D-034, amended).
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "dead");
    assert_eq!(
        post_ingest(&s.base, Some(&key), batch(&journey(1, 0)))
            .await
            .0,
        200
    );
    let (ok, _, err) = cli_status(
        &s.url,
        &[
            "admin",
            "revoke-ingest-key",
            "--project",
            "pilot",
            "--name",
            "dead",
        ],
    );
    assert!(ok, "revoke-ingest-key: {err}");
    // The ledger re-read first: the CLI's UPDATE landed in this database.
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM ingest_keys WHERE name = 'dead'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked, "the CLI revoke landed in the ledger");

    // NOTIFY → evict is a hop, not a deadline — a short poll proves the
    // refusal arrived via the listener, not via the 30 s TTL.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (status, _, _) = post_ingest(&s.base, Some(&key), batch(&journey(2, 0))).await;
        if status == 401 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the CLI revoke never reached the running collector — still {status} after 5 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_revocation_survives_the_listener_dying(pool: PgPool) {
    // Review V1: the LISTEN connection is a separate backend — killing it
    // must not strand revocations. `try_recv` surfaces the dead conn as
    // `Ok(None)`, the watcher's resync evicts by digest, and the next
    // request dies in 2 s — never the 30 s cache TTL.
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (token, token_id) = create_token(&s.url, "reader", &["read"], &["pilot"]);
    assert_eq!(
        get(&s.base, "/v1/projects/pilot/operations", Some(&token))
            .await
            .0,
        200
    );
    // Kill ONLY the listener's backend — the serving pools keep working.
    // `stack` waited on /readyz, which already gates on the listener.
    // `pg_terminate_backend` must never appear in a WHERE: Postgres does
    // not short-circuit `AND`, so the call can evaluate on every row in
    // pg_stat_activity — including this query's own backend (it did, and
    // took the suite with it). Resolve the pid first, terminate by value.
    let mut pid: Option<i32> = None;
    for _ in 0..20 {
        pid = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity
             WHERE application_name = 'aiops-revocation-listener'
               AND datname = current_database()",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        if pid.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let pid = pid.expect("the listener backend never appeared in pg_stat_activity");
    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(killed, "exactly the listener backend was terminated");
    // Prove the kill was observed before revoking: the channel's gap
    // counter is on /v1/collector/stats — the admin scope reads it. A gap
    // the server never detected is a different (and worse) failure than a
    // lost eviction.
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);
    let gap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let (_, stats) = get(&s.base, "/v1/collector/stats", Some(&admin)).await;
        if stats["revocation"]["gaps"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < gap_deadline,
            "the dead listener never registered as a gap: {stats:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // Revoke while the channel is recovering: whether the trigger's NOTIFY
    // hits the dead conn or the just-re-LISTENed one, the collector must
    // not serve the credential past the deadline.
    let (ok, _, err) = cli_status(&s.url, &["admin", "revoke-token", &token_id]);
    assert!(ok, "revoke-token: {err}");
    assert!(
        crate::common::is_revoked(&pool, "api_tokens", &token_id).await,
        "the CLI revoke landed in the ledger"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let (status, _) = get(&s.base, "/v1/projects/pilot/operations", Some(&token)).await;
        if status == 401 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a revoke in the listener gap never landed — still {status} after 2 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Find and terminate the revocation listener's backend, returning the
/// pid killed — the database-side half of the gap tests' prologue.
async fn terminate_revocation_listener(pool: &PgPool) -> i32 {
    let mut pid: Option<i32> = None;
    for _ in 0..20 {
        pid = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity
             WHERE application_name = 'aiops-revocation-listener'
               AND datname = current_database()",
        )
        .fetch_optional(pool)
        .await
        .unwrap();
        if pid.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let pid = pid.expect("the listener backend never appeared in pg_stat_activity");
    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(killed, "exactly the listener backend was terminated");
    pid
}

/// Kill only the revocation listener's backend and prove the kill was
/// seen (`revocation.gaps >= 1`), then return — the shared prologue for
/// the gap tests below.
async fn kill_revocation_listener(pool: &PgPool, admin_base: &str, admin: &str) {
    terminate_revocation_listener(pool).await;
    let gap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let (_, stats) = get(admin_base, "/v1/collector/stats", Some(admin)).await;
        if stats["revocation"]["gaps"].as_u64().unwrap_or(0) >= 1 {
            return;
        }
        assert!(
            std::time::Instant::now() < gap_deadline,
            "the dead listener never registered as a gap: {stats:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_failed_catch_up_recovers_instead_of_staying_deaf(pool: PgPool) {
    // Review N-C: a catch-up read that fails must not leave the instance
    // degraded until the next gap — a DB blip used to mean the fleet served
    // 503 / never trusted the cache until some later event. The catch-up
    // read is on `api_tokens`: `LOCK TABLE` blocks exactly that SELECT,
    // the auth pool's statement timeout fails it, and the retry must land
    // once the lock is released — `/readyz` recovers and revokes enforce.
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (token, token_id) = create_token(&s.url, "reader", &["read"], &["pilot"]);
    assert_eq!(
        get(&s.base, "/v1/projects/pilot/operations", Some(&token))
            .await
            .0,
        200
    );
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);
    // The stats poll inside `kill_revocation_listener` authenticates —
    // while `api_tokens` is locked a cache miss there would block, so the
    // admin token must be positively cached before the lock is taken.
    get(&s.base, "/v1/collector/stats", Some(&admin)).await;

    // Block the catch-up read BEFORE the kill so the reconnecting
    // watcher's first resync provably hits the lock and dies on the auth
    // pool's statement timeout (3 s) — taking the lock after the gap was
    // observed could land after the resync and prove nothing. Held on
    // its own connection inside an open transaction.
    let mut locker = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *locker).await.unwrap();
    sqlx::query("LOCK TABLE api_tokens IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *locker)
        .await
        .unwrap();
    // The stats endpoint can't be read while `api_tokens` is locked — it
    // answers `store_unavailable` — so this test can't watch the gaps
    // counter. It doesn't need to: the lock is held first, so the
    // reconnecting watcher's resync attempt is guaranteed to hit it.
    // The window covers Err → ~1 s reconnect → resync attempt → the 3 s
    // statement-timeout failure.
    terminate_revocation_listener(&pool).await;
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    sqlx::query("ROLLBACK").execute(&mut *locker).await.unwrap();
    drop(locker);

    // Recovery is observable, not assumed: /readyz gates on the channel.
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (status, _) = get(&s.base, "/readyz", None).await;
        if status == 200 {
            break;
        }
        assert!(
            std::time::Instant::now() < ready_deadline,
            "the watcher never recovered after the catch-up read failed — /readyz still {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // And the recovered resync actually enforces a revoke.
    let (ok, _, err) = cli_status(&s.url, &["admin", "revoke-token", &token_id]);
    assert!(ok, "revoke-token: {err}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let (status, _) = get(&s.base, "/v1/projects/pilot/operations", Some(&token)).await;
        if status == 401 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a revoke after the recovered resync never landed — still {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_revoke_inside_the_gap_only_the_catch_up_can_see(pool: PgPool) {
    // Review N-C's ruling test: `a_revocation_survives_the_listener_dying`
    // revokes while the listener RECOVERS — its NOTIFY can land on the
    // just-re-LISTENed connection and pass with the catch-up cut. This
    // revoke is provably inside the gap: the row trigger is disabled, so
    // no NOTIFY ever exists, and the only thing that can enforce it is
    // the resync's authoritative read. Cut the catch-up loop and the
    // token keeps answering.
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (token, token_id) = create_token(&s.url, "reader", &["read"], &["pilot"]);
    assert_eq!(
        get(&s.base, "/v1/projects/pilot/operations", Some(&token))
            .await
            .0,
        200
    );
    sqlx::query("ALTER TABLE api_tokens DISABLE TRIGGER api_tokens_revoked_notify")
        .execute(&pool)
        .await
        .unwrap();
    let (ok, _, err) = cli_status(&s.url, &["admin", "revoke-token", &token_id]);
    assert!(ok, "revoke-token: {err}");
    assert!(
        crate::common::is_revoked(&pool, "api_tokens", &token_id).await,
        "the CLI revoke landed in the ledger"
    );
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);
    sqlx::query("ALTER TABLE api_tokens ENABLE ALWAYS TRIGGER api_tokens_revoked_notify")
        .execute(&pool)
        .await
        .unwrap();
    kill_revocation_listener(&pool, &s.base, &admin).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        let (status, _) = get(&s.base, "/v1/projects/pilot/operations", Some(&token)).await;
        if status == 401 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a revoke the channel never heard kept answering after the resync — still {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_beat_that_skips_the_trigger_reads_as_a_gap(pool: PgPool) {
    // Review N-H: a heartbeat sent by `pg_notify` proves only that the
    // channel can carry bytes — `session_replication_role = replica` (or
    // a disabled trigger) would leave the fleet healthy and deaf. The
    // beat goes through the `revocation_heartbeat` table's own trigger,
    // so disabling it must silence the observed beats and surface a gap.
    let s = stack(&pool, &[]).await;
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);
    sqlx::query(
        "ALTER TABLE revocation_heartbeat
         DISABLE TRIGGER revocation_heartbeat_notify",
    )
    .execute(&pool)
    .await
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, stats) = get(&s.base, "/v1/collector/stats", Some(&admin)).await;
        if stats["revocation"]["gaps"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "beats kept arriving with the heartbeat trigger disabled: {stats:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    sqlx::query(
        "ALTER TABLE revocation_heartbeat
         ENABLE ALWAYS TRIGGER revocation_heartbeat_notify",
    )
    .execute(&pool)
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_delete_invisible_to_the_channel_is_caught_by_the_resync(pool: PgPool) {
    // Review N-G + N-H: a credential write made with triggers disabled
    // (`session_replication_role = replica`) sends no NOTIFY at all — a
    // forged-replication session is deaf channel-side. The token stays
    // cached until a resync; the resync must catch what the digest set
    // cannot name: a DELETE leaves no `revoked_at` row to evict, so only
    // a resync that re-derives its cache from the store removes it.
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (token, token_id) = create_token(&s.url, "reader", &["read"], &["pilot"]);
    for _ in 0..2 {
        assert_eq!(
            get(&s.base, "/v1/projects/pilot/operations", Some(&token))
                .await
                .0,
            200,
            "the token must be positively cached before the invisible delete"
        );
    }
    // Delete with the trigger DISABLED: no delete NOTIFY exists anywhere.
    // (`session_replication_role = replica` no longer silences it —
    // migration 0011 arms every revocation trigger ALWAYS, which is the
    // round-6 B2 point; a disabled trigger is the invisible write now.)
    sqlx::query("ALTER TABLE api_tokens DISABLE TRIGGER api_tokens_deleted_notify")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM api_tokens WHERE id = $1::uuid")
        .bind(&token_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE api_tokens ENABLE ALWAYS TRIGGER api_tokens_deleted_notify")
        .execute(&pool)
        .await
        .unwrap();

    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);
    kill_revocation_listener(&pool, &s.base, &admin).await;
    // The resync after the kill must erase the cached ghost.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        let (status, _) = get(&s.base, "/v1/projects/pilot/operations", Some(&token)).await;
        if status == 401 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a deleted token kept answering after the resync — still {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_cli_will_not_mint_a_contradictory_admin_or_scopeless_token(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    // --scope admin with a --project list: admin is collector-wide, the list
    // can never narrow it (white-hat S2-6).
    let (ok, _, err) = cli_status(
        &s.url,
        &[
            "admin",
            "create-token",
            "--actor",
            "root",
            "--scope",
            "admin",
            "--project",
            "pilot",
        ],
    );
    assert!(!ok, "admin+project must refuse: {err}");
    assert!(err.contains("collector-wide"), "{err}");
    // A read token with no project either.
    let (ok, _, err) = cli_status(
        &s.url,
        &[
            "admin",
            "create-token",
            "--actor",
            "ghost",
            "--scope",
            "read",
        ],
    );
    assert!(!ok, "scoped token without a project must refuse: {err}");
    // The honest shapes still mint.
    let (_, id) = create_token(&s.url, "root", &["admin"], &[]);
    assert_ne!(id, "");
    let (tok, _) = create_token(&s.url, "ana", &["read"], &["pilot"]);
    assert!(tok.starts_with("aiops_tk_"));
    // The ledger re-read: exactly the two honest mints — the refused shapes
    // wrote no token rows at all.
    assert_eq!(
        crate::common::table_count(&pool, "api_tokens").await,
        2,
        "the refused mints stored nothing"
    );
}

/// Review round 5, finding 2: a `clear` hint inside the 1 s debounce must
/// be DEFERRED (trailing edge), never dropped — a dropped hint leaves the
/// withdrawn opt-in applying from cache until the 30 s TTL. Deterministic:
/// the first `clear` is sent by hand so the second is provably inside the
/// window whatever the CLI's timing is.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn two_clears_half_a_second_apart_both_land(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let key = create_ingest_key(&s.url, "pilot", "h");
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);

    // Opt in `customer.tier` and prove it applies — the ingest-key cache
    // now holds a positive entry with opt_in = [customer.tier].
    let (out, err) = crate::common::cli(
        &s.url,
        &["admin", "set-opt-in", "--project", "pilot", "customer.tier"],
    );
    assert!(out.contains("customer.tier"), "{out} / {err}");
    let mut e = journey(0x40, 0);
    e[0].attributes = serde_json::json!({"customer.tier": "v0"})
        .as_object()
        .cloned();
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&e)).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        stored_body_contains(&pool, "v0").await,
        "the opted-in key must store first"
    );

    // Clear #1 by hand — the resync lands now and stamps the debounce
    // window. Then re-warm the cache so a *stale* positive entry is what a
    // dropped hint would leave behind.
    sqlx::query("SELECT pg_notify('aiops_credential_revoked', 'clear')")
        .execute(&pool)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&e)).await;
    assert_eq!(status, 200, "{body}");

    // Clear #2 lands ~0.3 s inside the window — the opt_in UPDATE itself
    // fires `projects_opt_in_notify`'s `clear`, so the hint is real and
    // timed. Dropped under a leading-edge debounce; deferred under the fix.
    sqlx::query("UPDATE projects SET opt_in_attribute_keys = '{other.flag}'")
        .execute(&pool)
        .await
        .unwrap();

    // The deferred resync must land: `customer.tier` is withdrawn and
    // `other.flag` applies. Each poll uses fresh markers so an early,
    // still-stale ingest can't alias the check.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    for n in 0..30u64 {
        let withdrawn = format!("c{n}");
        let applied = format!("o{n}");
        let mut events = journey(0x41, 100 + n * 10);
        events[0].attributes = serde_json::json!({
            "customer.tier": withdrawn,
            "other.flag": applied,
        })
        .as_object()
        .cloned();
        let (status, body, _) = post_ingest(&s.base, Some(&key), batch(&events)).await;
        assert_eq!(status, 200, "{body}");
        if stored_body_contains(&pool, &applied).await {
            assert!(
                !stored_body_contains(&pool, &withdrawn).await,
                "the withdrawn key kept applying — the second clear was dropped"
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            let (_, stats) = get(&s.base, "/v1/collector/stats", Some(&admin)).await;
            let opt: Option<Vec<String>> =
                sqlx::query_scalar("SELECT opt_in_attribute_keys FROM projects")
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            panic!(
                "the deferred clear never landed; revocation: {} opt_in: {opt:?}",
                stats["revocation"]
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("the deferred clear never landed — `other.flag` never applied");
}

async fn stored_body_contains(pool: &PgPool, needle: &str) -> bool {
    let rows: Vec<String> = sqlx::query_scalar("SELECT body::text FROM events")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.join("\n").contains(needle)
}

/// The resync count the revocation watcher reports — the metric the clear
/// debounce is bounded by (review round 6, A2).
async fn resync_count(base: &str, admin: &str) -> u64 {
    let (_, stats) = get(base, "/v1/collector/stats", Some(admin)).await;
    stats["revocation"]["resyncs"].as_u64().unwrap_or(0)
}

/// Review round 6, A2: a `clear` hint's resync waits on the pump's own
/// deadline arm, never inside the `try_recv` arm — a queued clear must not
/// cost the socket a second of unread time. 500 `clear` notices (each its
/// own transaction, the flood the finding describes) coalesce into a
/// bounded count of resyncs while a real digest notice still evicts inside
/// 2 s.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_clear_flood_coalesces_and_digests_still_evict(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (token, token_id) = create_token(&s.url, "reader", &["read"], &["pilot"]);
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);
    // Warm the token cache so the digest eviction is observable.
    assert_eq!(
        get(&s.base, "/v1/projects/pilot/operations", Some(&token))
            .await
            .0,
        200
    );
    let resyncs0 = resync_count(&s.base, &admin).await;

    // The flood: 500 `clear` notices, one per transaction — the attacker's
    // shape (a NOTIFY needs a commit; one per tx is how the global queue
    // backs up). The send's own duration sets the resync bound below: a
    // coalesced flood costs ~one resync per debounce window the send
    // spans, never one per notice.
    let send = std::time::Instant::now();
    for _ in 0..500 {
        sqlx::query("SELECT pg_notify('aiops_credential_revoked', 'clear')")
            .execute(&pool)
            .await
            .unwrap();
    }
    let sent_in = send.elapsed();

    // A real revoke mid-drain — by SQL, so the admin path's in-process
    // evict cannot fake a pass. The digest notice is the LAST entry in the
    // queue: `notices` can only reach 501+ once every queued clear was
    // processed and the digest handled — reaching it within 2 s is the
    // "the digest still evicts within 2 s" bound (a resync also flushes
    // the token cache, so a 401 alone cannot tell the two paths apart).
    sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE id = $1::uuid")
        .bind(&token_id)
        .execute(&pool)
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut notices = 0u64;
    while std::time::Instant::now() < deadline {
        let (_, stats) = get(&s.base, "/v1/collector/stats", Some(&admin)).await;
        notices = stats["revocation"]["notices"].as_u64().unwrap_or(0);
        if notices >= 501 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        notices >= 501,
        "the digest notice queued behind the clear flood — {notices} notices in 2 s"
    );
    // The eviction it stood for: the cached token is refused.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let (status, _) = get(&s.base, "/v1/projects/pilot/operations", Some(&token)).await;
        if status == 401 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the digest notice was handled but the token still answers: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // …and the flood coalesced into a bounded resync count: let the last
    // debounce window close, then count. A resync per message would read
    // as hundreds.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let resyncs = resync_count(&s.base, &admin).await - resyncs0;
    assert!(
        resyncs >= 1,
        "500 clears produced no resync — the deadline arm never fired"
    );
    assert!(
        resyncs <= sent_in.as_secs() + 2,
        "500 clears in {sent_in:?} cost {resyncs} resyncs — each is a serial second of socket stall"
    );
}

/// Review round 6, B2: 'O' (origin-only) is not armed enough — a writer
/// with `session_replication_role = replica` silences an 'O' trigger. The
/// migration arms every revocation trigger ALWAYS, and the beat requires
/// 'A': a trigger dropped back to 'O' must read as a gap, not green.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn an_unarmed_revocation_trigger_reads_as_a_gap(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);

    // 'O' — the pre-round-6 state: fires in an ordinary session, silent
    // under a replica-role one.
    sqlx::query("ALTER TABLE api_tokens ENABLE TRIGGER api_tokens_revoked_notify")
        .execute(&pool)
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(14);
    loop {
        let (_, stats) = get(&s.base, "/v1/collector/stats", Some(&admin)).await;
        if stats["revocation"]["gaps"].as_u64().unwrap_or(0) >= 1 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an origin-only trigger stayed green — 'A' was never required: {stats:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Review round 5, finding 3 + 7: the beat is a bounded write that ALSO
/// proves the `api_tokens` trigger is armed — a lock on the heartbeat row or
/// a disabled trigger reads as a channel gap, never a frozen-green watcher.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_beat_under_a_row_lock_reads_as_a_gap(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);

    // Hold a lock the beat's UPDATE cannot pass: `revocation_heartbeat` is
    // one row, written every 5 s. The writer pool's lock/statement timeout
    // plus the 2 s client timeout must turn this into Err → gap, never a
    // watcher stuck inside the select arm.
    let mut locker = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *locker).await.unwrap();
    sqlx::query("LOCK TABLE revocation_heartbeat IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *locker)
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(14);
    loop {
        let (_, stats) = get(&s.base, "/v1/collector/stats", Some(&admin)).await;
        if stats["revocation"]["gaps"].as_u64().unwrap_or(0) >= 1 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a locked heartbeat row froze the watcher green — no gap: {stats:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Review round 5, finding 7: the heartbeat proves only its own trigger —
/// `api_tokens_revoked_notify` disabled (or a replica-role revoker) leaves
/// the channel deaf while the beat stays green. The beat must check the
/// revoke trigger's arming, not just its own.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_disabled_revoke_trigger_reads_as_a_gap(pool: PgPool) {
    let s = stack(&pool, &[]).await;
    create_project(&s.url, "pilot");
    let (admin, _) = create_token(&s.url, "ops", &["admin"], &[]);

    sqlx::query("ALTER TABLE api_tokens DISABLE TRIGGER api_tokens_revoked_notify")
        .execute(&pool)
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(14);
    loop {
        let (_, stats) = get(&s.base, "/v1/collector/stats", Some(&admin)).await;
        if stats["revocation"]["gaps"].as_u64().unwrap_or(0) >= 1 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a disabled revoke trigger stayed green — the beat proved only itself: {stats:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
