//! The credential surface (D-034) in-process: every credential's metadata under
//! `admin`, never a hash; audited revocation of keys and tokens; denials audited and
//! failing closed. Minting stays CLI — there is no create route to test.
#![expect(
    clippy::unwrap_used,
    reason = "in-process tests: a failed call is a test failure"
)]

mod common;

use aiops_server::auth::{TOKEN_PREFIX, generate, hash};
use axum::body::Body;
use axum::http::Request;
use common::{env, get as get_req, now, post_json, send};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Mint a fresh `read` token on `pilot` and return (secret, id).
async fn mint_read(e: &common::Env) -> (String, uuid::Uuid) {
    let t = generate(TOKEN_PREFIX).unwrap();
    let later = now().plus_micros(3_600_000_000);
    let id = e
        .store
        .create_token(
            "carol",
            &["read".into()],
            &["pilot".into()],
            &hash(&t),
            later,
        )
        .await
        .unwrap();
    (t, id)
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn credentials_show_every_metadata_field_and_never_a_hash(pool: PgPool) {
    let e = env(pool).await;
    let r = send(&e.app, get_req("/v1/admin/credentials", &e.admin)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);

    let slugs: Vec<&str> = r.body["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["slug"].as_str().unwrap())
        .collect();
    assert!(
        slugs.contains(&"pilot") && slugs.contains(&"other"),
        "{slugs:?}"
    );

    let keys = r.body["ingest_keys"].as_array().unwrap();
    assert!(keys.len() >= 3, "{keys:?}");
    for k in keys {
        assert!(k["id"].is_string() && k["name"].is_string() && k["project"].is_string());
        assert!(k["created_at"].is_string(), "{k:?}");
        // The hash is a column, not a field: it must never appear, under any name.
        assert!(
            k.get("key_hash").is_none() && k.get("hash").is_none(),
            "{k:?}"
        );
        assert!(
            k.get("revoked_at").is_some(),
            "the field is present, null or not"
        );
    }

    let tokens = r.body["tokens"].as_array().unwrap();
    let actors: Vec<&str> = tokens
        .iter()
        .map(|t| t["actor"].as_str().unwrap())
        .collect();
    for a in ["ana", "bob", "eve", "ops"] {
        assert!(actors.contains(&a), "missing token for {a}: {actors:?}");
    }
    for t in tokens {
        assert!(t["scopes"].is_array() && t["projects"].is_array() && t["expires_at"].is_string());
        assert!(
            t.get("token_hash").is_none() && t.get("hash").is_none(),
            "{t:?}"
        );
    }
    // No secret material anywhere in the response, however nested.
    let text = r.body.to_string();
    assert!(!text.contains("hash"), "hash material in response");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn credentials_need_admin_scope_and_every_denial_is_audited(pool: PgPool) {
    let e = env(pool).await;
    let before = e.store.recent_audit(50).await.unwrap().len();

    let r = send(&e.app, get_req("/v1/admin/credentials", &e.reader)).await;
    assert_eq!(r.status, 403);
    let r = send(&e.app, get_req("/v1/admin/credentials", &e.no_read)).await;
    assert_eq!(r.status, 403);
    let r = send(
        &e.app,
        Request::get("/v1/admin/credentials")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 401);

    let audits = e.store.recent_audit(50).await.unwrap();
    let denials = audits[before..]
        .iter()
        .filter(|a| a.action == "credentials.list" && a.decision == "denied")
        .count();
    assert_eq!(denials, 2, "two scope denials audited: {audits:?}");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn revoking_a_token_ends_its_calls_and_is_audited(pool: PgPool) {
    let e = env(pool).await;
    let (t, id) = mint_read(&e).await;

    // Alive before the revoke.
    let r = send(&e.app, get_req("/v1/projects", &t)).await;
    assert_eq!(r.status, 200);

    let r = send(
        &e.app,
        post_json(
            &format!("/v1/admin/credentials/tokens/{id}/revoke"),
            &e.admin,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["revoked"], true);

    // Dead after — authentication itself fails.
    let r = send(&e.app, get_req("/v1/projects", &t)).await;
    assert_eq!(r.status, 401);

    // The audit row names the acting token and the target credential.
    let audits = e.store.recent_audit(50).await.unwrap();
    let grant = audits
        .iter()
        .find(|a| a.action == "admin.revoke_token" && a.target.as_deref() == Some(&id.to_string()));
    assert!(grant.is_some_and(|a| a.decision == "allowed"), "{audits:?}");

    // A second revoke is not a grant: 404 and an audited denial.
    let r = send(
        &e.app,
        post_json(
            &format!("/v1/admin/credentials/tokens/{id}/revoke"),
            &e.admin,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status, 404);
    let audits = e.store.recent_audit(50).await.unwrap();
    let denial = audits
        .iter()
        .find(|a| a.action == "admin.revoke_token" && a.decision == "denied");
    assert!(denial.is_some(), "{audits:?}");

    // A non-admin's revoke is a 403, audited, and changes nothing.
    let (t2, id2) = mint_read(&e).await;
    let r = send(
        &e.app,
        post_json(
            &format!("/v1/admin/credentials/tokens/{id2}/revoke"),
            &e.reader,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = send(&e.app, get_req("/v1/projects", &t2)).await;
    assert_eq!(r.status, 200, "the denied revoke did not take");
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn revoking_an_ingest_key_ends_its_writes(pool: PgPool) {
    let e = env(pool).await;
    let key = aiops_server::auth::generate(aiops_server::auth::INGEST_KEY_PREFIX).unwrap();
    let id = e
        .store
        .create_ingest_key("pilot", "doomed", &hash(&key))
        .await
        .unwrap();

    let r = send(
        &e.app,
        post_json(
            &format!("/v1/admin/credentials/ingest-keys/{id}/revoke"),
            &e.admin,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status, 200);

    let r = send(&e.app, common::post_ingest(&key, common::batch(&[]))).await;
    assert_eq!(r.status, 401, "the revoked key cannot ingest");

    let audits = e.store.recent_audit(50).await.unwrap();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "admin.revoke_ingest_key" && a.decision == "allowed"),
        "{audits:?}"
    );
}

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn revoke_with_a_bad_id_is_a_clean_400(pool: PgPool) {
    let e = env(pool).await;
    let r = send(
        &e.app,
        post_json(
            "/v1/admin/credentials/tokens/not-a-uuid/revoke",
            &e.admin,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = send(
        &e.app,
        post_json(
            &format!("/v1/admin/credentials/tokens/{}/revoke", Uuid::new_v4()),
            &e.admin,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(r.status, 404);
}
