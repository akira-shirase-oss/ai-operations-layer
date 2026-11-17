//! `aiops-dr-adapter`: the Durable Runner adapter (`docs/modules/durable-runner-adapter.md`).
//!
//! ```text
//! AIOPS_DR_ADAPTER_CONFIG=adapter.json   # listen, runner, workflow, routes (no secrets)
//! DURABLE_READ_KEY=…                     # the runner's read-only key (required)
//! AIOPS_ENDPOINT=http://127.0.0.1:57400  # the collector; with AIOPS_INGEST_KEY
//! AIOPS_INGEST_KEY=aiops_ik_…            # without it the adapter forwards and reports nothing
//! ```
//!
//! Register each route with the runner as `--handler <route>=http://<listen>/h/<route>`.

use std::time::Duration;

use aiops_durable_runner::adapter::{Adapter, serve, watch};
use aiops_durable_runner::config::{Config, File};
use aiops_sdk::Client;
use anyhow::Context;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AIOPS_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .json()
        .init();
    let path = std::env::var("AIOPS_DR_ADAPTER_CONFIG")
        .context("AIOPS_DR_ADAPTER_CONFIG (the adapter's JSON file) is required")?;
    let read = path.clone();
    let text = tokio::task::spawn_blocking(move || std::fs::read_to_string(read))
        .await?
        .with_context(|| format!("reading {path}"))?;
    let file: File = serde_json::from_str(&text).with_context(|| format!("parsing {path}"))?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let config = Config {
        file,
        read_key: env("DURABLE_READ_KEY").unwrap_or_default(),
        endpoint: env("AIOPS_ENDPOINT"),
        ingest_key: env("AIOPS_INGEST_KEY"),
        release: env("AIOPS_RELEASE"),
    };
    config.validate().map_err(anyhow::Error::msg)?;
    let sdk = match (&config.endpoint, &config.ingest_key) {
        (Some(endpoint), Some(key)) => {
            let mut c = aiops_sdk::Config::new(endpoint, key, "durable-runner-adapter");
            c.release.clone_from(&config.release);
            c.deployment = Some(config.file.runner.label.clone());
            // Every message this adapter sends is its own wording about timing and status
            // (classify.rs): never the handler's text, input or reply.
            c.include_error_messages = true;
            Client::start(&c).map_err(|e| anyhow::anyhow!("telemetry: {e}"))?
        }
        _ => Client::disabled(),
    };
    let listener = tokio::net::TcpListener::bind(config.file.listen)
        .await
        .with_context(|| format!("binding {}", config.file.listen))?;
    let adapter = Adapter::new(config, sdk.clone())?;
    let cancel = CancellationToken::new();
    let tracker = TaskTracker::new();
    tracker.spawn(watch(adapter.clone(), cancel.clone()));
    let stop = cancel.clone();
    tracker.spawn(async move {
        shutdown_signal().await;
        stop.cancel();
    });
    tracker.close();
    serve(adapter, listener, cancel.clone()).await?;
    cancel.cancel();
    if tokio::time::timeout(Duration::from_secs(5), tracker.wait())
        .await
        .is_err()
    {
        tracing::warn!("background tasks did not stop within 5 s");
    }
    // What could not be sent is counted, never claimed as delivered.
    let stats = sdk.shutdown(Duration::from_secs(2)).await;
    tracing::info!(dropped = stats.dropped(), "telemetry flushed");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = ctrl_c.await;
}
