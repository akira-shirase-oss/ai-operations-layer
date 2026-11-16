# Architecture

## Sources of truth

1. **Host systems own their own state.** Accepted jobs, budgets, publishing
   outcomes and billing live in the applications and providers that own them.
   This layer never overrides them; control paths *read* the authority before
   acting.
2. **PostgreSQL is the layer's only authority** for what it stores: operation
   and event metadata, control records (retry/reconcile intents, approvals,
   outcomes) and the audit log. One schema, migrations in the store crate.
3. **Object storage holds bounded blobs only** (e.g. over-threshold payloads),
   written under an explicit size bound and referenced from Postgres metadata.
   Losing a blob degrades detail, never the operation's recorded history.
4. **SDK-side queues are disposable buffers.** They exist to keep the host
   working when the collector is down; every drop is counted and surfaced.
   They are never a durability claim.
5. **Derived views are disposable.** Cached timelines, rollups and console
   projections are rebuilt from Postgres and never authoritative.

## Components

| Piece | Role |
|---|---|
| `sdk/typescript` | Host-side instrumentation: emits operation IDs, propagates OpenTelemetry-compatible context, enqueues events to a bounded buffer, batches to the ingest API, counts drops. Never blocks the host request path beyond its bound. |
| `aiops-domain` | Pure model: the versioned event schema, operation/step identities, outcome classification (transport / validation / uncertain-external-effect), cardinality limits, redaction rules, control-state machine. No I/O — enforced. |
| `aiops-store` | PostgreSQL access: idempotent event upserts (dedup by event ID), operation metadata, control records, audit log, retention/rollup jobs, drop counters. |
| `aiops-server` | Axum API: the ingest path, the query path and the control path as separately bounded, separately authenticated surfaces; health endpoints reporting ingestion and storage state honestly. |
| `aiops-adapter-*` | Execution adapters (first: workflow runner or hosted jobs — see open questions). An adapter is the only component allowed to effect an external action, and only inside its ownership contract. |
| `apps/console` | SvelteKit investigation console: operation timeline, filtering, release comparison, missing-data states, control actions through the API only. |
| `fixtures/` | The instrumented pilot workflow (if not hosted in an existing workspace app) and fault fixtures: hung model, lost publish response, duplicate/lated event emitters, high-cardinality generators. |
| `perf/` | k6 scenarios + `results/` (machine + commit on every run). |

Names are provisional until the workspace exists; the roles are not.

## Paths

**Emit:** host app starts an operation (SDK assigns/propagates operation ID +
OTel-compatible trace context) → each model/tool/job step records an event
(kind, status, event time, IDs, bounded metadata) → bounded SDK queue →
batched `POST /ingest` with auth → ack returns accepted/dropped counts →
queue drains or drops, counting either way.

**Ingest:** authenticate (project-scoped credential) → validate against the
schema version → enforce body/cardinality bounds → apply the metadata
allowlist (default-deny: no prompts, content, tokens, secrets) → idempotent
upsert (dedup on event ID; store event time *and* receive time) → count drops
and rejects. Nothing in this path touches a control record or a host system.

**Investigate:** console → query API (same auth, project scope) → timeline
assembled ordered by event time with receive time displayed → explicit gap
markers where coverage is missing → filtering/release comparison read the
same records. The console holds no authority and renders `unknown` where the
data is absent.

**Control:** user requests retry/reconcile on an operation → API checks actor
scope and project → adapter reads the *authoritative* execution state (not
the trace) → if the external effect is unknown, reconcile first → a control
record + audit row commits in Postgres → adapter executes under the business
key → the result is observed through both telemetry and the authority. No
transaction spans an external call.

**Degraded:** collector down → SDK drops under its bound, increments counters
→ host work continues → when the collector returns, the gap is a recorded,
visible gap — including the counter evidence — never backfilled fiction.

**Retention:** capped by explicit policy; expiry/rollup runs as store jobs;
aggregates keep drop counters so retention never launders loss into silence.

## Consistency story

- **Idempotent events.** Every event carries a stable event ID; re-delivery
  merges to one record. Late events land in event-time position without
  rewriting history — receive time proves arrival order.
- **Control is transactional where it matters.** A control decision, its
  authorization check and its audit row commit together in Postgres; the
  adapter's external call happens after commit and its outcome is reconciled,
  never assumed.
- **Unknown stays `uncertain`.** A timed-out model call or vanished publish
  response is stored as an uncertain external effect, classified differently
  from transport errors and validation failures, and resolved only by
  reconciling against the authority.
- **Separation by construction.** Ingest, query and control use separate
  pools, limits and credentials; a slow query or ingestion burst cannot
  delay or drop a control record.

## Failure cases

See `END_GOAL.md` — the four contract cases (collector unavailable,
duplicate/late events, model/publishing timeout, unauthorized control or
cross-project query) plus the degraded path above.

## Layout

```
crates/        aiops-domain, aiops-store, aiops-server, aiops-adapter-*
sdk/typescript/  host instrumentation SDK (the first SDK per the guide)
apps/console/    SvelteKit investigation console
docs/            END_GOAL, ARCHITECTURE, ROADMAP, DECISIONS, modules/
fixtures/        pilot workflow and/or fault fixtures
perf/            k6 scenarios + results/
```
