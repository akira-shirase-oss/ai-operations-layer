# AI Operations Layer

> **Status — A0 reopened, fixes in progress; A1 partly (2026-11-16).** An
> independent review found timeline-honesty and ingestion defects; A0 stays in
> progress until they are fixed (`docs/ROADMAP.md`). Decisions D-012–D-024
> answer OQ1–OQ6 and amend the drill before any counted run. Built and
> tested: the pure domain crate, the PostgreSQL store, the bounded collector
> and query API, the Rust SDK, the investigation console (checked in
> Chromium), and the in-repo reference host with the contract failure cases
> as end-to-end tests. Load evidence so far is two short smokes on a heavily
> loaded laptop (no loss; ingest latency target missed); the 1,000,000-event
> run is owed. Evidence per milestone: `docs/ROADMAP.md`. Development
> software, no production claim. Contract:
> `../../docs/Project_Guides/08_AI_Operations_Layer.docx` (extract with
> `textutil -convert txt -stdout`).

An observability and controlled-recovery layer for AI-backed applications:
follow one application operation — request → model/tool/job steps → outcome —
in a single honest timeline that shows where it failed, what was retried,
what it cost, and which outcome remains uncertain. It is an add-on to
existing backends, not a runtime migration: hosts instrument through a Rust
SDK or the language-neutral HTTP/JSON ingestion contract, a Rust/Axum
collector stores events in PostgreSQL, a SvelteKit console renders the
investigation, and execution adapters provide safe, scoped, reconciled
retry — never a generic "run again". Telemetry is evidence, not authority:
it can never authorize a retry, refund or publish, and its own gaps are
displayed as gaps.

The first host is an in-repo reference workflow (`fixtures/reference-host`);
the first real host and execution adapter is Durable Runner, integrated only
through public APIs (D-012).

- `docs/END_GOAL.md` — what done means, scope in/out, the invariants, the
  four contract failure cases, the ship gate
- `docs/ARCHITECTURE.md` — sources of truth, components, emit/ingest/
  investigate/control paths, consistency story, layout
- `docs/ROADMAP.md` — A0 spine → A1 one workflow, investigated → A2 first
  real host → A3 controlled recovery → A4 quality/adoption
- `docs/DECISIONS.md` — D-001..D-024 and any open question
- `docs/DRILL.md` — the pre-registered diagnosis drill behind the ship gate
- `docs/INGEST_CONTRACT.md` — the language-neutral wire contract (schema v1)

## Run it locally

```sh
export PATH="$(brew --prefix rustup)/bin:$PATH"   # the pinned toolchain (1.99.1)
scripts/stack.sh up                                # PostgreSQL on 127.0.0.1:57432
eval "$(scripts/stack.sh env)"
scripts/check.sh                                   # everything CI runs
npm run build --workspace @aiops/console
cargo run -p aiops-server -- admin create-project pilot
cargo run -p aiops-server -- admin create-ingest-key --project pilot --name my-host
cargo run -p aiops-server -- admin create-token --actor me --project pilot
AIOPS_CONSOLE_DIR=apps/console/build cargo run -p aiops-server -- serve
# console: http://127.0.0.1:57400/console/   ingest: POST http://127.0.0.1:57400/v1/ingest
```

| Path | What |
|---|---|
| `crates/aiops-domain` | schema v1, refusals, policy, timeline assembly — pure, no I/O |
| `crates/aiops-store` | PostgreSQL: idempotent ingest, summaries, keys, tokens, audit, retention |
| `crates/aiops-server` | the collector and API (`serve`, `migrate`, `admin`) |
| `crates/aiops-sdk` | the Rust SDK for hosts |
| `apps/console` | the SvelteKit investigation console (`scripts/console-e2e.sh` runs it in Chromium) |
| `fixtures/reference-host` | the in-repo pilot host and its end-to-end failure-case tests |
| `perf/` | k6 scenarios, `run-ingest.sh`, committed results |
| `scripts/drill.py` | the diagnosis drill kit (`docs/DRILL.md`) |
| `deploy/compose.yaml` | the local stack (project `ai-operations-layer`) |

**What counts as done (the ship gate):** one failed AI workflow can be
diagnosed and safely resolved with measurably less searching than the
existing setup — with the failure-case drills and the load evidence
committed under `perf/results/`.

## License

MIT; see [LICENSE](LICENSE).
