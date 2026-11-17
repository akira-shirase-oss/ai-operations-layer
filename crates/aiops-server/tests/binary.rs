//! Black-box: the real `aiops-server` binary against real PostgreSQL. The admin CLI mints
//! a project, an ingest key and a token; events go in over HTTP and come back as a
//! timeline; SIGINT drains and exits cleanly. Needs `DATABASE_URL`.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "black-box tests: a failed setup step is a test failure"
)]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sqlx::PgPool;

fn database_url(pool: &PgPool) -> String {
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

fn cli(url: &str, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_aiops-server"))
        .env("AIOPS_DATABASE_URL", url)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn the_real_binary_ingests_serves_timelines_and_drains_on_sigint(pool: PgPool) {
    let url = database_url(&pool);
    cli(&url, &["admin", "create-project", "pilot"]);
    let key = cli(
        &url,
        &[
            "admin",
            "create-ingest-key",
            "--project",
            "pilot",
            "--name",
            "reference-host",
        ],
    );
    let token = cli(
        &url,
        &[
            "admin",
            "create-token",
            "--actor",
            "ana",
            "--project",
            "pilot",
        ],
    );
    assert!(key.starts_with("aiops_ik_") && token.starts_with("aiops_tk_"));

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_aiops-server"))
            .arg("serve")
            .env("AIOPS_DATABASE_URL", &url)
            .env("AIOPS_LISTEN", format!("127.0.0.1:{port}"))
            .env("AIOPS_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait_ready(&base).await;

    let client = reqwest::Client::new();
    let r = client
        .post(format!("{base}/v1/ingest"))
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .body(common::batch(&common::journey(1, 0)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-request-id"));
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["accepted"], 8);

    let t: serde_json::Value = client
        .get(format!(
            "{base}/v1/projects/pilot/operations/{}",
            common::op_hex(1)
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(t["status"]["state"], "uncertain");
    assert_eq!(t["entries"].as_array().unwrap().len(), 8);

    // SIGINT: the server drains and exits on its own, well inside its grace period.
    let pid = server.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "did not exit after SIGINT"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(status.success(), "exit status {status:?}");
    let mut stderr = String::new();
    std::io::Read::read_to_string(server.0.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(
        !stderr.contains(&key),
        "the ingest key must never reach the logs"
    );
    assert!(
        !stderr.contains(&token),
        "the token must never reach the logs"
    );
}

/// Review finding 16: Docker and systemd stop a service with SIGTERM; it must drain like
/// SIGINT, finishing the request in flight.
#[sqlx::test(migrations = "../aiops-store/migrations")]
async fn sigterm_drains_like_sigint(pool: PgPool) {
    let url = database_url(&pool);
    cli(&url, &["admin", "create-project", "pilot"]);
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_aiops-server"))
            .arg("serve")
            .env("AIOPS_DATABASE_URL", &url)
            .env("AIOPS_LISTEN", format!("127.0.0.1:{port}"))
            .env("AIOPS_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_ready(&base).await;
    let pid = server.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "did not exit after SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        status.success(),
        "SIGTERM must drain and exit 0, got {status:?}"
    );
    let mut out = String::new();
    std::io::Read::read_to_string(server.0.stdout.as_mut().unwrap(), &mut out).unwrap();
    assert!(out.contains("shutting down: draining"), "no drain log line");
}
