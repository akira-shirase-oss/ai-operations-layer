# AI Operations Layer

> **Status — A0 done; A1 done except the quiet-machine million-event run;
> A2 partly (2026-11-17).** Two independent reviews' findings are fixed, each
> with a test that fails without its fix; redaction remains pattern matching
> and partial by nature (`docs/END_GOAL.md`). Durable Runner is the first
> real host (D-027): the article workflow runs as its handlers through an
> adapter that never changes what the runner decides, and the console shows
> a real run end to end with the runner's own answer beside the timeline
> (checked in Chromium against the real runner); operations filter by status,
> name, release and time, and releases compare from the same records. The drill kit runs on
> Durable Runner (D-028); its first, seeder-aware baseline shows the
> registered comparison cannot pass on that catalog (open question OQ8).
> Decisions D-012–D-028. Load evidence is development observation on a busy
> laptop: two short smokes, the 1,000,000-event run that missed its
> thresholds (36,580 events shed and counted, p95 152 ms;
> `perf/results/2026-11-17-ingest-million.md`), and the lock measurement
> (`perf/results/2026-11-17-ingest-locks.md`). A quiet-machine 1M run is
> owed. Evidence per milestone: `docs/ROADMAP.md`. Development software, no
> production claim. Contract:
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
the first real host is Durable Runner, integrated only through its public
contracts (D-012, D-027): `aiops-dr-adapter` sits where the runner calls
its handlers, and the collector reads the runner's own answer with its
read-only key.

- `docs/END_GOAL.md` — what done means, scope in/out, the invariants, the
  four contract failure cases, the ship gate
- `docs/ARCHITECTURE.md` — sources of truth, components, emit/ingest/
  investigate/control paths, consistency story, layout
- `docs/ROADMAP.md` — A0 spine → A1 one workflow, investigated → A2 first
  real host → A3 controlled recovery → A4 quality/adoption
- `docs/DECISIONS.md` — D-001..D-028 and the open question OQ8
- `docs/DRILL.md` — the pre-registered diagnosis drill behind the ship gate
- `docs/INGEST_CONTRACT.md` — the language-neutral wire contract (schema v1)
- `docs/OPERATING.md` — install, first run, connecting a host (Durable Runner too),
  upgrade, backup/restore, troubleshooting; `deploy/` holds the env example, systemd
  units and a reverse-proxy example, tested by `scripts/production-smoke.sh`

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
| `crates/aiops-durable-runner` | the Durable Runner adapter (`aiops-dr-adapter`): wraps the runner's handler calls, reports them, finishes runs from the runner's answer |
| `apps/console` | the SvelteKit investigation console (`scripts/console-e2e.sh` runs it in Chromium) |
| `fixtures/reference-host` | the in-repo pilot host and its end-to-end failure-case tests |
| `fixtures/dr-article-worker` | the article workflow's steps as plain Durable Runner handlers (uninstrumented, no logs) |
| `perf/` | k6 scenarios, `run-ingest.sh`, committed results |
| `scripts/drill.py` | the diagnosis drill kit (`docs/DRILL.md`), on the reference host or Durable Runner |
| `scripts/durable-runner.sh` | builds Durable Runner from its public repository at the tested revision |
| `deploy/compose.yaml` | the local stack (project `ai-operations-layer`) |

**What counts as done (the ship gate):** one failed AI workflow can be
diagnosed and safely resolved with measurably less searching than the
existing setup — with the failure-case drills and the load evidence
committed under `perf/results/`.

## License

MIT; see [LICENSE](LICENSE).
