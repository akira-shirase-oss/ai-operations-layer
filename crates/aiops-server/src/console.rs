//! Serves the built console at `/console/` with a strict Content Security Policy
//! (D-022). The SPA's inline bootstrap script is allowed by its SHA-256 hash, computed
//! from the built `index.html` at startup, so no `'unsafe-inline'` is needed for scripts.

use std::path::Path;

use axum::Router;
use axum::http::{HeaderValue, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::app::{AppState, SharedState};

/// The CSP for the console in `dir`: every inline `<script>` in `index.html` by hash.
pub fn content_security_policy(dir: &Path) -> std::io::Result<HeaderValue> {
    let html = std::fs::read_to_string(dir.join("index.html"))?;
    let mut hashes = Vec::new();
    let mut rest = html.as_str();
    while let Some(start) = rest.find("<script") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let tag = &after[..open_end];
        let body_start = open_end + 1;
        let Some(close) = after[body_start..].find("</script>") else {
            break;
        };
        let body = &after[body_start..body_start + close];
        if !tag.contains("src=") && !body.trim().is_empty() {
            hashes.push(format!(
                "'sha256-{}'",
                STANDARD.encode(Sha256::digest(body.as_bytes()))
            ));
        }
        rest = &after[body_start + close..];
    }
    let policy = format!(
        "default-src 'none'; script-src 'self' {}; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        hashes.join(" ")
    );
    HeaderValue::from_str(&policy).map_err(std::io::Error::other)
}

pub fn routes(state: &AppState) -> Option<Router<SharedState>> {
    let dir = state.config.console_dir.as_ref()?;
    let csp = state.console_csp.clone()?;
    let index = dir.join("index.html");
    let service = ServeDir::new(dir).fallback(ServeFile::new(index));
    Some(
        Router::new()
            .nest_service("/console", service)
            .layer(SetResponseHeaderLayer::overriding(
                header::CONTENT_SECURITY_POLICY,
                csp,
            )),
    )
}
