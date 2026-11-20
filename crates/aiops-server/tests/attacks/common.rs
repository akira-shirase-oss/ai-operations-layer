//! Harness for the attack suite: spawn the real `aiops-server` binary on the
//! test's own migrated database, mint credentials through the `admin` CLI, and
//! drive real HTTP against it. Everything a test needs is honest process I/O —
//! the same path an attacker on the network would take (plus a SQL read-back
//! for what the log stored).
#![allow(
    dead_code,
    reason = "each attack file uses a different subset of the harness"
)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sqlx::PgPool;

pub(crate) use crate::shared::{batch, batch_from, journey, op_hex};

pub(crate) fn database_url(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let db = pool.connect_options().get_database().unwrap().to_owned();
    let (prefix, _) = base.rsplit_once('/').unwrap();
    format!("{prefix}/{db}")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `aiops-server admin …` against the test's database: (stdout, stderr).
pub(crate) fn cli(url: &str, args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_aiops-server"))
        .env("AIOPS_DATABASE_URL", url)
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap().trim().to_owned(),
        String::from_utf8(out.stderr).unwrap().trim().to_owned(),
    )
}

/// The same, where failure is the assertion: (status, stdout, stderr).
pub(crate) fn cli_status(url: &str, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_aiops-server"))
        .env("AIOPS_DATABASE_URL", url)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap().trim().to_owned(),
        String::from_utf8(out.stderr).unwrap().trim().to_owned(),
    )
}

pub(crate) fn create_project(url: &str, slug: &str) {
    let (out, err) = cli(url, &["admin", "create-project", slug]);
    assert!(out.starts_with("project "), "{out} / {err}");
}

pub(crate) fn create_ingest_key(url: &str, project: &str, name: &str) -> String {
    let (key, err) = cli(
        url,
        &[
            "admin",
            "create-ingest-key",
            "--project",
            project,
            "--name",
            name,
        ],
    );
    assert!(key.starts_with("aiops_ik_"), "{key} / {err}");
    key
}

/// Mint a token; returns (secret, id) — the id rides stderr, the secret stdout.
pub(crate) fn create_token(
    url: &str,
    actor: &str,
    scopes: &[&str],
    projects: &[&str],
) -> (String, String) {
    let mut args = vec!["admin", "create-token", "--actor", actor];
    for p in projects {
        args.extend(["--project", p]);
    }
    for s in scopes {
        args.extend(["--scope", s]);
    }
    let (token, err) = cli(url, &args);
    assert!(token.starts_with("aiops_tk_"), "{token} / {err}");
    let id = err
        .strip_prefix("token ")
        .and_then(|s| s.split(' ').next())
        .unwrap_or_else(|| panic!("no token id on stderr: {err}"))
        .to_owned();
    (token, id)
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A running collector on its own database and port.
pub(crate) struct Stack {
    pub(crate) base: String,
    /// The database URL the server runs against (for minting + audit read-back).
    pub(crate) url: String,
    _server: Server,
}

/// Spawn `aiops-server serve` against `pool`'s database with extra `AIOPS_*`
/// env, and wait for `/readyz`.
pub(crate) async fn stack(pool: &PgPool, envs: &[(&str, &str)]) -> Stack {
    let url = database_url(pool);
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aiops-server"));
    cmd.arg("serve")
        .env("AIOPS_DATABASE_URL", &url)
        .env("AIOPS_LISTEN", format!("127.0.0.1:{port}"))
        .env("AIOPS_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let server = Server(cmd.spawn().unwrap());
    wait_ready(&base).await;
    Stack {
        base,
        url,
        _server: server,
    }
}

async fn wait_ready(base: &str) {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(r) = client.get(format!("{base}/readyz")).send().await
            && r.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("collector did not become ready");
}

pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// POST a raw body to /v1/ingest; returns (status, json-or-null, headers).
pub(crate) async fn post_ingest(
    base: &str,
    key: Option<&str>,
    body: impl Into<reqwest::Body>,
) -> (u16, Value, reqwest::header::HeaderMap) {
    let mut req = client()
        .post(format!("{base}/v1/ingest"))
        .header("content-type", "application/json")
        .body(body);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    let body: Value = r.json().await.unwrap_or(Value::Null);
    (status, body, headers)
}

/// GET a /v1 path under a bearer credential; returns (status, json-or-null).
pub(crate) async fn get(base: &str, path: &str, token: Option<&str>) -> (u16, Value) {
    let mut req = client().get(format!("{base}{path}"));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

/// POST a JSON control body; returns (status, json-or-null).
pub(crate) async fn post_json(
    base: &str,
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> (u16, Value) {
    let mut req = client()
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

/// The newest audit rows, straight from the database — what an operator
/// (or an injected value) actually left behind.
pub(crate) async fn audit_rows(
    pool: &PgPool,
    limit: i64,
) -> Vec<(String, String, Option<String>, Option<String>)> {
    sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT action, decision, project_slug, target FROM audit_log ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap()
}
