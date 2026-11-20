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
    assert!(rows.is_empty(), "a 401 never audits: {rows:?}");
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
    for (kind, id) in [("ingest-keys", key_id), ("tokens", token_id)] {
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
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn a_peer_revoke_dies_inside_the_credential_ttl(pool: PgPool) {
    // The admin CLI revokes in the database — another process. The running
    // collector only learns of it when the cached lookup ages out (D-034's
    // documented peer bound), so the attack surface is the TTL, nothing more.
    let s = stack(&pool, &[("AIOPS_CREDENTIAL_TTL_SECS", "1")]).await;
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

    // Within the fresh window the cached answer may still serve — that IS the
    // documented bound. Once it ages out, the revoked credential is refused.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (status, _, _) = post_ingest(&s.base, Some(&key), batch(&journey(2, 0))).await;
        if status == 401 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the peer revoke outlived its bound — still answering {status} after 10 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
}
