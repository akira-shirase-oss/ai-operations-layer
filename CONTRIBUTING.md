# Contributing

The path a new contributor walks. `AGENTS.md` is the contract; this file is
the how.

## Setup

```sh
export PATH="$(brew --prefix rustup)/bin:$PATH"   # Homebrew cargo ignores rust-toolchain.toml
scripts/stack.sh up                              # PostgreSQL on :57432 (AIOPS_PG_PORT to move it)
eval "$(scripts/stack.sh env)"                   # exports DATABASE_URL
npm install                                      # console toolchain
```

## The stack

| Piece | Where | What it is |
| --- | --- | --- |
| `aiops-domain` | `crates/aiops-domain` | Schema v1, validation, policy, control model. No I/O — enforced by `scripts/check-isolated.sh`. |
| `aiops-store` | `crates/aiops-store` | PostgreSQL: idempotent ingest, summaries, credentials, audit, controls, retention. Six isolated pools. |
| `aiops-server` | `crates/aiops-server` | Axum collector + API + admin CLI (`aiops-server admin …`). |
| `aiops-sdk` | `crates/aiops-sdk` | Rust SDK hosts link. Bounded queue; drops counted, never blocks. |
| `aiops-durable-runner` | `crates/aiops-durable-runner` | `aiops-dr-adapter`: control + telemetry adapter for Durable Runner. |
| console | `apps/console` | SvelteKit investigation UI; a client of the same API, holds no authority. |
| fixtures | `fixtures/` | `reference-host` (pilot app with seeded faults), `dr-article-worker`. |
| perf | `perf/` | k6 scenarios + `run-*.sh` harnesses; results land in `perf/results/`. |

## The gates — run before every commit

```sh
scripts/check.sh          # fmt, clippy -D warnings, workspace tests (real PG),
                          # architecture + isolated-member checks, console check+build
scripts/check-isolated.sh # every workspace member compiles alone (dependency hygiene)
```

Console alone: `npm run check && npm run build` in `apps/console`.
E2E (real collector + reference host + Chromium):
`scripts/console-e2e.sh` — needs Playwright browsers installed.

## Commit and evidence rules

- **Every fixed bug gets a test that fails without the fix.** Run it red first,
  keep the failure output.
- **Small, focused commits** — one concern each, the way the log already reads.
- **Evidence over claims** (`AGENTS.md` non-negotiable 8): `perf/` results carry
  machine + commit; laptop numbers are "development observation", never
  production capacity; a miss is disclosed in the result file, not rerun until
  green.
- **Telemetry is never authority.** Control paths read the host's authoritative
  state; a trace alone can never authorize a retry, refund or publication.
- **Gaps are data.** Never render a missing span as "did not run" —
  `no_finish_observed`, drops and gaps are first-class UI states.
- **No generic "run again".** Controls need actor scope + expected execution
  state + a stable business key; reconcile precedes retry; every decision is
  audited before an external submission.
- **Bounds on everything that takes input** — bodies, batches, cardinality,
  page sizes, queue depths, per-token rates.
- **Fault hooks are debug-only** — `#[cfg(debug_assertions)]`, never compiled
  into a release binary.

## Where the decisions live

`docs/DECISIONS.md` (D-001…), `docs/ROADMAP.md` (milestone gates),
`docs/REVIEW-A4.md` (the last second-developer pass — its remaining list is
the honest backlog). If a README/ROADMAP claim disagrees with the code, the
code is right and the doc is a bug — fix the doc in the same commit.
