//! The keep-alive contract with what sits in front of the collector
//! (D-035). hyper's `header_read_timeout` bounds the idle wait between
//! requests on a kept-alive connection as well as the header read itself,
//! and when it fires the connection closes with no answer: a client or
//! proxy that reuses a connection idle for longer than that races the
//! close (Go/k6: `readLoopPeekFailLocked`, status 0; a proxy: a 502 for a
//! non-idempotent request). The collector's bound is the server default —
//! so every client in this repository must bound its own reuse below it.
#![expect(
    clippy::unwrap_used,
    reason = "tests: a failed setup step is a test failure"
)]

use std::path::PathBuf;

/// The server-side bound every client-side reuse bound must stay under:
/// `ServeLimits::default().header_read_timeout` (config.rs, serve.rs).
const SERVER_IDLE_CLOSE_SECS: f64 = 10.0;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
}

// The Caddy upstream pool must retire an idle connection before the
// collector would close it mid-reuse: `keepalive` below the header bound.
#[test]
fn the_caddy_upstream_idles_shorter_than_the_collector_closes() {
    let caddy = read("deploy/Caddyfile.example");
    let transport = caddy
        .split("transport http")
        .nth(1)
        .unwrap_or_else(|| panic!("Caddyfile.example has no `transport http` block"))
        .split('}')
        .next()
        .unwrap_or_else(|| panic!("the transport block closes"));
    let keepalive = transport
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("keepalive"))
        .unwrap_or_else(|| {
            panic!(
                "transport http must set `keepalive` below the collector's \
                 header_read_timeout, or pooled idle connections race its close"
            )
        })
        .trim();
    let secs: f64 = keepalive
        .strip_suffix('s')
        .and_then(|n| n.parse().ok())
        .or_else(|| {
            keepalive
                .strip_suffix("ms")
                .and_then(|n| n.parse::<f64>().ok())
                .map(|ms| ms / 1_000.0)
        })
        .unwrap_or_else(|| panic!("keepalive must be a duration, found {keepalive:?}"));
    assert!(
        secs > 0.0 && secs < SERVER_IDLE_CLOSE_SECS,
        "upstream idle keepalive {secs}s must be under the collector's \
         {SERVER_IDLE_CLOSE_SECS}s idle close (D-035)"
    );
}

// A k6 scenario's client may not carry an idle connection longer than the
// bound either: `noVUConnectionReuse` retires each connection at the
// iteration boundary, so a VU's idle time is only its intra-iteration gaps
// (the poll-loop sleeps, well under 10 s) — never an inter-iteration wait.
#[test]
fn every_load_client_bounds_reuse_below_the_idle_close() {
    let k6 = root().join("perf/k6");
    let mut scenarios = 0;
    for entry in std::fs::read_dir(&k6).unwrap_or_else(|e| panic!("perf/k6 exists: {e}")) {
        let entry = entry.unwrap_or_else(|e| panic!("a k6 entry: {e}"));
        if entry.path().extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let text =
            std::fs::read_to_string(entry.path()).unwrap_or_else(|e| panic!("read {name}: {e}"));
        // Only a scenario file (one that exports `options`) owns a client;
        // helper libraries like `ops.js` carry no connection.
        if !text.contains("export const options") {
            continue;
        }
        scenarios += 1;
        assert!(
            text.contains("noVUConnectionReuse: true"),
            "{name}: set `noVUConnectionReuse: true` so a VU's idle reuse \
             cannot outlive the collector's {SERVER_IDLE_CLOSE_SECS}s close (D-035)"
        );
    }
    assert!(scenarios > 0, "no k6 scenarios found under perf/k6");
}
