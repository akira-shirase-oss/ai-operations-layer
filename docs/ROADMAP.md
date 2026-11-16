# Roadmap

Milestones in dependency order, following the guide's four implementation
steps with the thin spine first (re-cut 2026-11-16, D-023). Each milestone
states what it builds and the proof gate it must produce before the next
starts. Numbers are proposed targets until a run produces evidence.

**Status rule.** A status cell may say *Done* only when it names the tests
and the evidence path that prove the gate. *Partly* names what is owed.

| Step | Builds | Proves (the gate) | Status |
|---|---|---|---|
| **A0 — the spine** | Workspace, pinned toolchain (1.99.1), `scripts/check.sh`, CI, local stack (`deploy/compose.yaml`). `aiops-domain`: versioned event schema v1 (operation / trace / request / step / attempt / model / prompt-config / deployment / release IDs; `model`, `tool`, `job`, `validation` step kinds; outcomes including `uncertain`; gaps as data), named refusals, cardinality caps, default-deny attribute allowlist, secret-shape redaction, timeline assembly — no I/O. `aiops-store`: migrations, idempotent batch upsert, source counters, timeline reads, audit. `aiops-server`: authenticated bounded ingest (body/batch caps, per-key token bucket, load shedding, deadlines, idempotent event IDs, out-of-order and skew handling), timeline API, admin CLI. `aiops-sdk` (Rust): bounded non-blocking queue, counted drops, timeouts, gap reports. `apps/console`: one operation's timeline, honestly | Every malformed event refused with a named refusal; caps enforced; schema versioned with a compatibility rule; domain pure (architecture check); duplicate delivery dedups; an SDK whose collector is down never blocks or fails its host | **Done** (2026-11-16) except the load evidence, which A1 owes. Tests: `crates/aiops-domain/tests/{validate,timeline}.rs` (every refusal code by name, caps, policy, redaction, timeline honesty), `crates/aiops-store/tests/store.rs` (re-delivery, conflicting duplicates, out-of-order, retention, pool isolation), `crates/aiops-server/tests/{api,binary}.rs` (auth, caps, shedding, rate limit, cardinality, scope and project denials audited, CSP console, the real binary with SIGINT drain and no secrets in logs), `crates/aiops-sdk/tests/sdk.rs` (dead collector, gap reports, idempotent re-send, shutdown deadline). Gate: `scripts/check.sh`, `.github/workflows/ci.yml` |
| **A1 — one workflow, investigated** | `fixtures/reference-host`: request → generation → validation → fact-check tool → publish, fake model/tool/publisher, injectable faults, cost estimates, uncertain publish. The investigation view (two clocks, retries, gaps, drops, uncertain, estimated cost, provenance). The contract failure cases as automated tests. An ingestion load scenario in `perf/k6/` with thresholds. The drill kit and the logs-only baseline (`docs/DRILL.md`) | A real operation's events land end to end and reassemble; tests for: collector down (host keeps working, drops and gaps visible after recovery), duplicate and out-of-order events (no false chronology, both clocks shown), a host flooding events (bounded, counted, others unaffected), an uncertain step shown as uncertain, a console user without scope or project denied and audited. Load result committed under `perf/results/` with machine and commit | See README status |
| **A2 — first real host + investigation breadth** | Durable Runner as the first real host (D-012): a workflow running as Durable Runner handlers, instrumented through the ingest contract, correlated with Durable Runner's own run state read through its public API (OQ7). Operation filtering (status, name, release, time), release comparison, query-path chaos (slow queries against the separate pool) | A second developer distinguishes a real failure from missing telemetry without guessing; filtering and release comparison read the same records; a slow query cannot delay ingest or audit | Planned |
| **A3 — controlled recovery** | Control records + audit in Postgres; retry/reconcile endpoints (actor scope `operate` + expected execution state + stable business key); reconcile before rerun; approval record; adapters: the reference host's publisher first, then Durable Runner (submit under `op_key` = business key, inspect = authority read); console control surface through the same API | The lost-publish-response drill: the vanished effect reads as `uncertain`, reconcile runs against the authority before any rerun, the business key prevents a duplicate publish; an unauthorized retry is denied and audited; replayed control calls are no-ops; no generic "run again" exists | Planned |
| **A4 — quality + adoption** | Retention sweep at measured cost, sampling/export policy, usage and estimated-vs-reconciled cost surfaces, redaction and privacy red-team (error fields first), the full load plan (100 req/s × 10 events; instrumentation overhead < 5% median with drops disclosed; ingestion p95; queue age; query latency over ≥ 1M events; storage/day; burst, collector-loss, high-cardinality and slow-query runs), a second independent workflow, the layer-mode diagnosis drill by a performer who did not seed it | Ship gate: one failed AI workflow diagnosed and safely resolved measurably faster than the logs-only baseline (protocol pre-registered in `docs/DRILL.md`); every failure case has committed evidence in `perf/results/` | Planned |

## Risks that are people, not code

- A2's gate needs a second developer and A4's drill needs a performer who
  did not write the incident generator. Neither exists yet; drill runs by
  the builder are labeled seeder-aware and do not count (D-017).

## Later items (each needs an explicit contract)

From the guide's "full direction": evaluation results, anomaly signals,
evidence-linked AI summaries, budgets beyond cost labeling, the optional
native client (consumes the same permissioned API), a TypeScript SDK
(D-013), additional adapters. None starts without its own ownership,
privacy and failure-case contract. Contract failure case four's clause
"AI-generated investigation text cannot widen access" is vacuous until AI
summaries exist — nothing produces AI text yet — and its test lands with
them; it is not claimed as proven before then.

## Open questions

OQ1–OQ6 are answered (see `DECISIONS.md`, D-012–D-017). OQ7 (Durable
Runner hand-off shape) is answered at A2 start.
