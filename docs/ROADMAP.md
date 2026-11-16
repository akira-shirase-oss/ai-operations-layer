# Roadmap

Milestones in dependency order, following the guide's four implementation
steps with a scaffold first. Each milestone states what it builds and the
proof gate it must produce before the next starts; a milestone merges when
its evidence exists. Numbers are proposed targets until a run produces
evidence.

| Step | Builds | Proves (the gate) | Status |
|---|---|---|---|
| **A0 — scaffold + contract** | Workspace, pinned toolchain, check scripts, docs. `aiops-domain`: the versioned event schema (operation/trace/request/job/step/model/prompt-config/deployment IDs), outcome classification (transport vs validation vs uncertain external effect), cardinality limits, default-deny metadata allowlist, control-state machine | Every malformed event refused with a named refusal; cardinality bound enforced; schema versioned with a compatibility rule; domain is pure (no I/O) | Planned |
| **A1 — one workflow** | `aiops-store` + migrations, `aiops-server` ingest path (authenticated, bounded bodies, idempotent upsert, drop counters), the TypeScript SDK (operation IDs, OTel-compatible context propagation, bounded queue, drop counting), the pilot: one authorized noncritical content workflow emitting request → generation → validation → publishing | A real operation's events land end-to-end and reassemble in event-time order; duplicate delivery dedups; the collector-loss drill: SDK drops bounded, host work continues, gap + drop counts visible after recovery | Planned |
| **A2 — investigation** | Query API (project-scoped, separately bounded), `apps/console`: per-operation timeline showing event time and receive time, filtering, release comparison, explicit missing-data states | A second developer distinguishes a real failure from missing telemetry without guessing; late/duplicate events display without fabricated chronology; a cross-project query is denied and audited | Planned |
| **A3 — controlled recovery** | First execution adapter (workflow runner or hosted jobs — open question OQ2), control records + audit in Postgres, retry/reconcile endpoints (actor scope + expected execution state + stable business key), reconcile-before-rerun, approval record; console control surface through the same API | The lost-publishing-response drill: vanished external effect reads as `uncertain`, reconcile runs against the authority before any rerun, the business key prevents a duplicate publish; an unauthorized retry is denied and audited; no generic "run again" exists | Planned |
| **A4 — quality + adoption** | Retention caps + rollup jobs, sampling/drop policy, estimated-vs-reconciled cost labeling, usage/cost surfaces, redaction/privacy tests, the full load plan (100 req/s × 10 events; instrumentation overhead <5% median with drops disclosed; ingestion p95; queue age; query latency over ≥1M events; storage/day; burst, collector-loss, high-cardinality, slow-query chaos runs), a second independent workflow instrumented, the diagnosis-time drill | Ship gate: one failed AI workflow diagnosed and safely resolved measurably faster than the existing setup — drill protocol and result committed; every A1–A4 failure case has committed evidence in `perf/results/` | Planned |

## Later items (each needs an explicit contract)

From the guide's "full direction": evaluation results, anomaly signals,
evidence-linked AI summaries, budgets beyond cost labeling, the optional
native client (consumes the same permissioned API), additional SDKs and
adapters. None starts without its own ownership, privacy and failure-case
contract — AI-generated investigation text in particular can never widen
access (contract failure case four).

## Open questions (see DECISIONS.md)

- **OQ1** — which host application carries the pilot workflow.
- **OQ2** — which execution adapter is first (the workspace's durable-runner
  vs hosted jobs).
- **OQ3** — the blob-offload threshold ("bounded blobs when needed").
- **OQ4** — the auth model's concrete shape (project scopes, actor identity,
  SDK credentials).
- **OQ5** — retention values ("cap retention" gives no numbers).
- **OQ6** — the diagnosis-time drill protocol behind "measurably less
  searching".
