# Architecture

## Sources of truth

1. **Host systems own their own state.** Accepted jobs, budgets, publishing
   outcomes and billing live in the applications and providers that own them.
   This layer never overrides them; control paths *read* the authority before
   acting.
2. **PostgreSQL is the layer's only authority** for what it stores: operation
   and event metadata, control records (retry/reconcile intents, approvals,
   outcomes) and the audit log. One schema, migrations in the store crate.
3. **No object storage in the first release** (D-014). Events are metadata
   under hard caps. If a measured need brings blobs back, they hold
   over-threshold detail only, referenced from Postgres; losing one degrades
   detail, never the recorded history.
4. **SDK-side queues are disposable buffers.** They exist to keep the host
   working when the collector is down; every drop is counted and surfaced.
   They are never a durability claim.
5. **Derived views are disposable.** Cached timelines, rollups and console
   projections are rebuilt from Postgres and never authoritative.

## Components

| Piece | Role |
|---|---|
| `aiops-sdk` | The first SDK, in Rust (D-013): emits operation IDs, propagates W3C `traceparent` context, enqueues events to a bounded queue without blocking, batches to the ingest API, counts drops, reports gaps (D-021). The wire format is `docs/INGEST_CONTRACT.md`, so other languages can post without it. Server-side only. |
| `aiops-domain` | Pure model: the versioned event schema, operation/step identities, outcome classification (transport / validation / uncertain-external-effect), cardinality limits, redaction rules, control-state machine. No I/O — enforced. |
| `aiops-store` | PostgreSQL access: idempotent event upserts (dedup by event ID), operation metadata, control records, audit log, retention/rollup jobs, drop counters. |
| `aiops-server` | Axum API: the ingest path, the query path and the control path as separately bounded, separately authenticated surfaces; health endpoints reporting ingestion and storage state honestly. |
| `aiops-adapter-*` | Execution adapters (A3; first the reference host's publisher, then Durable Runner through its public API — D-012). An adapter is the only component allowed to effect an external action, and only inside its ownership contract. |
| `apps/console` | SvelteKit investigation console: operation timeline, filtering, release comparison, missing-data states, control actions through the API only. |
| `fixtures/reference-host` | The in-repo pilot host (D-012): request → generation → validation → fact-check → publish with fake model/tool/publisher, injectable faults, cost estimates and an authoritative publication record. Also the drill's incident generator (`docs/DRILL.md`). |
| `perf/` | k6 scenarios + `results/` (machine + commit on every run). |

The collector serves the built console at `/console/` with a strict
Content Security Policy (D-022). The local stack (`deploy/compose.yaml`,
project `ai-operations-layer`) binds Postgres to `127.0.0.1:57432`; the
collector listens on `127.0.0.1:57400` and the reference host on
`127.0.0.1:57401`.

## Paths

**Emit:** host app starts an operation (SDK assigns/propagates operation ID +
OTel-compatible trace context) → each model/tool/job step records an event
(kind, status, event time, IDs, `seq`/`op_seq`, bounded metadata) → bounded
SDK queue (non-blocking enqueue; full → drop newest, counted) → batched
`POST /v1/ingest` with the host's ingest key → ack returns accepted /
duplicate / refused counts → queue drains or drops, counting either way;
pending gap reports are sent when the collector answers again.

**Ingest:** shed if too many requests are in flight → authenticate (the
ingest key's hash names one project) → per-key token bucket → bounded body
→ validate against the schema version (each bad event refused by name, the
rest accepted) → enforce caps → withhold non-allowlisted attributes and
redact secret shapes, counted (default-deny: no prompts, content, tokens,
secrets) → one transaction: idempotent multi-row upsert (dedup on
`(project, event_id)`; store event time *and* receive time and the key the
event arrived under), operation summaries, source-instance counters →
answer. Nothing in this path touches a control record or a host system.

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
crates/          aiops-domain (pure), aiops-store, aiops-server, aiops-sdk,
                 aiops-adapter-* (A3)
fixtures/        reference-host (pilot workflow + drill incident generator)
apps/console/    SvelteKit investigation console (served at /console/)
deploy/          compose.yaml — the local stack
docs/            END_GOAL, ARCHITECTURE, ROADMAP, DECISIONS, DRILL,
                 INGEST_CONTRACT
perf/            k6 scenarios + results/ (machine + commit on every run)
scripts/         check.sh and the gates it runs
```
