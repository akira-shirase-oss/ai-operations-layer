# AI Operations Layer — agent instructions

An add-on observability and controlled-recovery layer for AI-backed
applications. Telemetry describes observed behavior; it is never the
authority for jobs, budgets or business outcomes, and its own gaps are
shown, not hidden. Contract:
`../../docs/Project_Guides/08_AI_Operations_Layer.docx` (extract with
`textutil -convert txt -stdout`; the engineering-contract section is
authoritative).

## Read first

1. `docs/END_GOAL.md` — what done means, scope, the seven invariants, the
   four required failure cases, the ship gate.
2. `docs/ARCHITECTURE.md` — sources of truth, emit/ingest/investigate/
   control paths, consistency story.
3. `docs/DECISIONS.md` — seeded directions (evidence not authority, add-on
   not migration, two clocks, default-deny metadata, no generic rerun), the
   answers to OQ1–OQ6 (D-012–D-017) and any new open question — never
   silently resolve an open question.
4. `docs/ROADMAP.md` — A0–A4 milestone order and each milestone's proof gate.
5. `docs/INGEST_CONTRACT.md` — the versioned wire contract; `docs/DRILL.md`
   — the pre-registered diagnosis drill.

## Non-negotiables

1. **Telemetry is not truth for money/actions.** Control paths read the
   authoritative store, never the trace. Telemetry loss can never authorize
   a retry, refund or publication.
2. **Gaps are data.** Collector downtime, drops and incomplete traces are
   recorded and rendered explicitly with counts. A missing span is not proof
   a job never ran — never render it as "did not run".
3. **Two clocks, no fabricated chronology.** Event time and receive time on
   every event; duplicates/late events merge idempotently.
4. **Safe control.** Actor scope + expected execution state + stable
   business key on every control action; reconcile before rerun; every
   denial and authorization audited; controls only where the adapter owns
   execution. No generic "run again" — ever.
5. **Bounded ingestion.** Authenticated, bounded bodies and cardinality,
   explicit sampling/export policy, drops counted, retention capped.
   Ingest/query workloads are isolated from control actions.
6. **Privacy by default.** No prompts, customer content, tokens or secrets
   in default metadata; opt-in inclusion is per-field and labeled. No
   secrets or payloads in logs.
7. **Estimated ≠ reconciled.** Cost figures are labeled estimates, stored
   and shown separately from billing.
8. **Evidence over claims** (workspace rule). `perf/` results carry machine
   + commit; full runs are million-scale with events as the unit; misses and
   dropped data are disclosed in the result file, never rerun until green.
   **A latency chart that hides data loss is not evidence.** Laptop numbers
   are development observations.

## Rust rules

- `export PATH="$(brew --prefix rustup)/bin:$PATH"` before every cargo
  command — Homebrew's cargo ignores the toolchain pin.
- Typed errors (`thiserror`) in libraries; `anyhow` only in binaries.
- No `unwrap`/`expect` on external input. Deadlines on every I/O await.
- `aiops-domain` carries no I/O — enforced like the other projects' domain
  crates.

## SDK rules (Rust `aiops-sdk` now; TypeScript later, same rules — D-013)

- The SDK must never block the host request path beyond its bound; the
  queue, batch and flush all have limits. Emitting is a non-blocking
  enqueue; a full queue drops and counts (D-021).
- Every drop is counted and every counter is exposable to the host.
- The SDK returns errors and never panics; it keeps working (and its host
  keeps working) when the collector is down.
- Context propagation follows OTel-compatible formats; event payloads
  conform to the versioned schema in `docs/INGEST_CONTRACT.md` — no schema
  drift without a version bump.
- Server-side only: an ingest key in a browser is a published key.

## Frontend rules

- SvelteKit + TypeScript (strict). The console is a client of the same
  permissioned API everything else uses; it holds no authority.
- `unknown`, `uncertain`, gap and drop states are first-class renders —
  never interpolated into a clean timeline.
- Event text is attacker-influenced: render it as text, never as HTML
  (no `{@html}`), under the CSP the build emits.

## Testing rule

Every feature ships, in the same branch: regression tests (domain unit,
in-process API, black-box against the real binary), a fixture/testbed path
exercising the real HTTP API, and a load scenario in `perf/k6/` with
thresholds. The four contract failure cases each get a drill that is run
and recorded. Every fixed bug gets a test that fails without the fix.

## Verification

`scripts/check.sh` runs all of it: the toolchain pin, `cargo fmt --all
--check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo
test --workspace` (against real PostgreSQL: `scripts/stack.sh up`), the
architecture check, and the console's type-check and build — before every
commit, plus the failure-case drills when
ingest, store or control code changed. Report exactly what ran and what
passed.

## Landing

Commit on your branch. The coordinator lands on `main` after review —
never move the `main` ref yourself, not even locally (no merge, rebase or
`update-ref` against it), and never push.
