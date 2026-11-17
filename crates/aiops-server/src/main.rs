//! `aiops-server serve | migrate | admin …`.

use std::sync::Arc;

use aiops_server::retention::{self, SweepPlan};
use aiops_server::serve::{self, ServeLimits};
use aiops_server::{AppState, Config, admin, console, router, system_clock};
use aiops_store::{Retention, Store, StoreConfig};
use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[derive(Parser)]
#[command(version, about = "The AI Operations Layer's collector")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the collector (reads AIOPS_* from the environment).
    Serve,
    /// Apply database migrations and exit.
    Migrate,
    /// Keys, tokens and projects.
    #[command(subcommand)]
    Admin(admin::Admin),
    /// Run the retention sweep now, until nothing expired is left, and report it.
    Sweep,
}

fn store_config(cfg: &Config) -> StoreConfig {
    let mut s = StoreConfig::new(cfg.database_url.clone());
    s.ingest_connections = cfg.ingest_connections;
    s.query_connections = cfg.query_connections;
    s.control_connections = cfg.control_connections;
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AIOPS_LOG")
                .unwrap_or_else(|_| "info,tower_http=warn,sqlx=warn".into()),
        )
        .json()
        .init();
    let cli = Cli::parse();
    let config = Config::from_env().map_err(anyhow::Error::msg)?;
    let store = Store::connect(&store_config(&config))
        .await
        .context("connecting to PostgreSQL")?;
    match cli.command {
        Command::Migrate => {
            store.migrate().await?;
            println!("migrations applied");
            Ok(())
        }
        Command::Admin(cmd) => {
            store.migrate().await?;
            admin::run(&store, cmd, (system_clock())()).await
        }
        Command::Serve => serve(store, config).await,
        Command::Sweep => {
            store.migrate().await?;
            let plan = SweepPlan {
                max_duration: std::time::Duration::from_secs(3_600),
                ..SweepPlan::default()
            };
            let started = std::time::Instant::now();
            let t = retention::run(&store, (system_clock())(), Retention::default(), plan).await?;
            let secs = started.elapsed().as_secs_f64();
            println!(
                "{{\"events\":{},\"operations\":{},\"instances\":{},\"audit\":{},\"names\":{},\"calls\":{},\"complete\":{},\"seconds\":{secs:.3}}}",
                t.events, t.operations, t.instances, t.audit, t.names, t.calls, t.complete
            );
            Ok(())
        }
    }
}

async fn serve(store: Store, config: Config) -> anyhow::Result<()> {
    store.migrate().await?;
    let csp = match config.console_dir.clone() {
        Some(dir) => Some(
            tokio::task::spawn_blocking(move || console::content_security_policy(&dir))
                .await?
                .context("reading the console's index.html")?,
        ),
        None => None,
    };
    let listen = config.listen;
    let grace = config.shutdown_grace;
    let header_read_timeout = config.header_read_timeout;
    let max_connections = config.max_connections;
    let sweep_every = config.retention_sweep_every;
    let state = Arc::new(AppState::new(store.clone(), config, system_clock(), csp));
    let app = router(state.clone());

    let cancel = CancellationToken::new();
    let tracker = TaskTracker::new();
    if let Some(every) = sweep_every.filter(|d| !d.is_zero()) {
        let cancel = cancel.clone();
        let state = state.clone();
        tracker.spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        match retention::run(&state.store, state.now(), Retention::default(), SweepPlan::default()).await {
Ok(t) => {
                                tracing::info!(events = t.events, operations = t.operations, instances = t.instances, audit = t.audit, names = t.names, calls = t.calls, complete = t.complete, "retention sweep");
                                if t.names > 0 {
                                    // Swept names free their budget: reload the registries.
                                    if let Ok(mut names) = state.names.lock() {
                                        names.clear();
                                    }
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "retention sweep failed; will retry"),
                        }
                    }
                }
            }
        });
    }
    tracker.close();

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let limits = ServeLimits {
        header_read_timeout,
        max_connections: fit_to_file_limit(max_connections).await,
        drain_grace: grace,
    };
    tracing::info!(%listen, max_connections = limits.max_connections, "collector listening");
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            stop_signal().await;
            tracing::info!("shutting down: draining");
            cancel.cancel();
        });
    }
    serve::run(listener, app, limits, cancel.clone()).await?;
    tracker.wait().await;
    Ok(())
}

/// Keeps the connection cap under the process's open-file limit (with room for the
/// database pools and files), so accepting never fails for lack of descriptors.
async fn fit_to_file_limit(wanted: usize) -> usize {
    let limit = tokio::task::spawn_blocking(|| {
        std::process::Command::new("sh")
            .args(["-c", "ulimit -n"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<usize>().ok())
    })
    .await
    .ok()
    .flatten();
    match limit {
        Some(n) if n < wanted + 128 => {
            let fitted = n.saturating_sub(128).max(16);
            tracing::warn!(
                open_file_limit = n,
                wanted,
                fitted,
                "connection cap lowered to fit the open-file limit"
            );
            fitted
        }
        _ => wanted,
    }
}

/// SIGINT or SIGTERM (Docker and systemd stop services with SIGTERM).
async fn stop_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM; SIGINT only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
