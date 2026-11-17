//! The AI Operations Layer's collector and API.
//!
//! - `POST /v1/ingest` — bounded, authenticated ingestion (`docs/INGEST_CONTRACT.md`).
//! - `GET /v1/projects…` — the query path: lists and one operation's timeline.
//! - `/console/` — the built investigation console, under a strict CSP.
//!
//! Telemetry stored here is evidence, never authority (invariant 1): nothing in this
//! crate authorizes a retry, refund or publication.

pub mod admin;
pub mod app;
pub mod auth;
pub mod authority;
pub mod config;
pub mod console;
pub mod control;
pub mod error;
pub mod ingest;
pub mod locks;
pub mod query;
pub mod retention;
pub mod serve;

pub use app::{AppState, SharedState, router, system_clock};
pub use config::Config;
