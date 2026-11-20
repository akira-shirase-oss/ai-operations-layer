//! `aiops-server admin …`: keys, tokens and projects, through direct database access.
//! There is no admin HTTP surface (D-015). Secrets are printed once and never stored.

use aiops_domain::Timestamp;
use aiops_store::Store;
use clap::Subcommand;
use uuid::Uuid;

use crate::auth::{INGEST_KEY_PREFIX, TOKEN_PREFIX, generate, hash};

#[derive(Debug, Subcommand)]
pub enum Admin {
    /// Create a project (slug: lowercase letters, digits, dashes).
    CreateProject {
        slug: String,
    },
    /// Mint an ingest key for one host or service; prints it once.
    CreateIngestKey {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
    },
    RevokeIngestKey {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
    },
    /// Mint an API token for the console; prints it once.
    CreateToken {
        #[arg(long)]
        actor: String,
        /// Required unless `--scope admin` — admin is collector-wide and a
        /// project list can never narrow it (white-hat S2-6).
        #[arg(long = "project")]
        projects: Vec<String>,
        #[arg(long = "scope", default_value = "read")]
        scopes: Vec<String>,
        #[arg(long, default_value_t = 30)]
        ttl_days: u32,
    },
    RevokeToken {
        id: Uuid,
    },
    /// Opt a project in to extra attribute keys (comma-separated; empty clears).
    SetOptIn {
        #[arg(long)]
        project: String,
        keys: String,
    },
}

pub async fn run(store: &Store, command: Admin, now: Timestamp) -> anyhow::Result<()> {
    match command {
        Admin::CreateProject { slug } => {
            let id = store.create_project(&slug).await?;
            println!("project {slug} {id}");
        }
        Admin::CreateIngestKey { project, name } => {
            let key = generate(INGEST_KEY_PREFIX)?;
            store
                .create_ingest_key(&project, &name, &hash(&key))
                .await?;
            println!("{key}");
        }
        Admin::RevokeIngestKey { project, name } => {
            store.revoke_ingest_key(&project, &name).await?;
            println!("revoked ingest key {name} of {project}");
        }
        Admin::CreateToken {
            actor,
            projects,
            scopes,
            ttl_days,
        } => {
            for s in &scopes {
                anyhow::ensure!(
                    matches!(s.as_str(), "read" | "operate" | "admin"),
                    "unknown scope {s} (read, operate, admin)"
                );
            }
            let admin_scope = scopes.iter().any(|s| s == "admin");
            anyhow::ensure!(
                !admin_scope || projects.is_empty(),
                "--scope admin is collector-wide: --project cannot narrow it"
            );
            anyhow::ensure!(
                admin_scope || !projects.is_empty(),
                "a read/operate token needs at least one --project"
            );
            let token = generate(TOKEN_PREFIX)?;
            let expires = now.plus_micros(i64::from(ttl_days) * 24 * 3_600 * 1_000_000);
            let id = store
                .create_token(&actor, &scopes, &projects, &hash(&token), expires)
                .await?;
            eprintln!("token {id} for {actor}, expires {expires}");
            println!("{token}");
        }
        Admin::RevokeToken { id } => {
            store.revoke_token(id).await?;
            println!("revoked token {id}");
        }
        Admin::SetOptIn { project, keys } => {
            let keys: Vec<String> = keys
                .split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_owned)
                .collect();
            store.set_opt_in_keys(&project, &keys).await?;
            println!("{project} opt-in keys: {keys:?}");
        }
    }
    Ok(())
}
