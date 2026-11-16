//! `aiops-server serve | migrate | admin …`.

use std::sync::Arc;

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
                        match state.store.sweep(state.now(), Retention::default(), 5_000, 20).await {
                            Ok(r) => tracing::info!(events = r.events, operations = r.operations, instances = r.instances, audit = r.audit, more = r.more, "retention sweep"),
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
    tracing::info!(%listen, "collector listening");
    let shutdown = {
        let cancel = cancel.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down: draining");
            cancel.cancel();
        }
    };
    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown);
    let drained = {
        let cancel = cancel.clone();
        async move {
            cancel.cancelled().await;
            tokio::time::sleep(grace).await;
        }
    };
    tokio::select! {
        r = server => r?,
        () = drained => tracing::warn!("drain deadline passed; exiting with requests in flight"),
    }
    tracker.wait().await;
    Ok(())
}
