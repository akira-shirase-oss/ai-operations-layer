//! The pentest regression suite (standing rule, 2026-11-20): real attack
//! requests against a running instance — the real `aiops-server` binary on its
//! own migrated PostgreSQL database per test, credentials minted through the
//! `admin` CLI, every refusal asserted by status and by what the refusal left
//! behind (no audit row, no authority call, no stored event).
//!
//! Targets: `POST /v1/ingest`, the `/v1/projects/…` query surface, the
//! `/reconcile`/`/retry`/`/controls` control surface and its outbound authority
//! calls, `/v1/admin/*`, and the credentials themselves. Attacks: auth bypass
//! (absent, malformed, wrong-scheme, prefix-confused, expired and revoked
//! credentials — including `admin` minted under a `--project` list, which the
//! CLI must refuse); cross-tenant reads and writes (a token or ingest key for
//! project B must never read into, write into, or name-resolve through project
//! A); header spoofing (forwarded-for games must not move the per-token rate
//! bucket, conflicting `Authorization` lines, hostile `X-Request-Id` values);
//! rate-limit evasion (spraying unknown slugs, malformed bodies that used to be
//! free, burst past the bound); SSRF and path injection through `business_key`
//! and `operation_id` into the control adapters' outbound calls; oversized,
//! slow and malformed bodies (over the 1 MiB cap, >500-event batches,
//! truncated JSON, a trickling request the server must shed while staying
//! responsive); replay (the same batch twice counts duplicates, not double
//! writes; a control resend returns the same record; a revoked credential is
//! refused on the next call); and audit/log injection (forged slugs and
//! operation ids carrying newlines or bidi stored only as `<invalid>`, and
//! error bodies that never echo the attacker's bytes).
#![expect(
    clippy::unwrap_used,
    reason = "black-box attack tests: a failed setup step is a test failure"
)]
#![expect(
    clippy::expect_used,
    reason = "black-box attack tests: a failed setup step is a test failure"
)]

#[path = "../common/mod.rs"]
mod shared;

mod auth_bypass;
mod bodies;
mod common;
mod cross_tenant;
mod headers_spoof;
mod inject;
mod rate_limit;
mod replay;
mod ssrf_keys;
