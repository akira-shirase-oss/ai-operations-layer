# AI Operations Layer — end goal

**Follow one application operation from request to outcome — across models,
tools and jobs — and tell the truth about what is not known.**

An add-on observability and controlled-recovery layer for existing AI-backed
applications, motivated by scattered failure handling: today a failed
generation means grepping logs, guessing whether the publish call landed, and
rerunning blind. This layer makes an operation's history readable — where it
failed, what was retried, what it cost, and which outcome remains uncertain —
without requiring the host application to migrate runtimes. Instrumented
applications keep working when the layer is down; telemetry loss is shown, not
hidden, and never authorizes anything.

Contract: `../../docs/Project_Guides/08_AI_Operations_Layer.docx` (the engineering
contract section is authoritative). When this file and the guide disagree, the
guide wins. Deliberate deviations are recorded in `docs/DECISIONS.md`.

## In scope (the first useful release)

- A versioned event schema correlating operation, trace, request, job, step,
  model, prompt/config version and deployment — idempotent events with bounded
  cardinality.
- A Rust SDK (`aiops-sdk`) plus a language-neutral HTTP/JSON ingestion
  contract (`docs/INGEST_CONTRACT.md`) for existing applications — the
  portfolio's hosts are Rust services (D-013; a TypeScript SDK is later).
  The SDK emits operation IDs, propagates OpenTelemetry-compatible context,
  buffers under a bounded drop policy when the collector is unreachable, and
  counts every drop. Server-side only.
- A Rust + Axum collector/API: bounded, authenticated ingestion and query
  paths, isolated from the control path.
- PostgreSQL for operation metadata and explicit control records. No object
  storage in the first release: events are metadata only, under hard caps
  (D-014).
- A SvelteKit investigation console: one timeline per operation — event time
  and receive time shown separately — with filtering, release comparison and
  explicit missing-data states.
- One execution adapter — the reference host's publisher first, then
  Durable Runner through its public API (D-012) — with safe control:
  retry/reconcile endpoints requiring actor scope, expected execution state
  and a stable business key; reconcile before rerunning when an external
  outcome is unknown. Controls exist only where the adapter owns execution.
- The pilot: one authorized, noncritical content workflow, observable from
  request → generation → validation → publishing in a single timeline, and
  recoverable through the controlled path. It runs first in the in-repo
  reference host (`fixtures/reference-host`), then on Durable Runner as the
  first real host (D-012).
- Provider usage/cost tracking with estimated cost labeled separately from
  reconciled billing; capped retention; drop counters everywhere loss occurs.

## Out of scope (explicit exclusions)

- A general APM/tracing product or an automatic root-cause oracle. This layer
  records and correlates evidence; it does not claim to find causes.
- A generic "run again" button. Every control is scoped, keyed and reconciled;
  anything that could duplicate an external effect (e.g. publishing) is a
  contract violation, not a UX choice.
- The optional native client — "later" per the guide; it consumes the same
  permissioned API whenever it arrives.
- Additional SDK languages, additional execution adapters, and the full
  direction's later items (evaluation results, anomaly signals,
  evidence-linked AI summaries) — each needs an explicit contract first.
- Production capacity claims: all numbers in docs are proposed lab workloads
  or acceptance targets until a committed run produces evidence.

## Non-negotiable invariants

1. **Telemetry is not truth for money or actions.** A trace describes observed
   behavior. Accepted jobs, budgets and business outcomes live in their own
   authoritative stores. Telemetry loss can never authorize a retry, refund
   or publication; control reads the authority, not the trace.
2. **Gaps are data.** Collector downtime, dropped events and incomplete traces
   are recorded and rendered explicitly as gaps with counts. A missing span
   is not proof that a job never ran — and is never rendered as "did not run".
3. **Two clocks, no fabricated chronology.** Every event carries event time
   and receive time. Duplicate and late events merge idempotently; ordering
   never invents causality the evidence does not contain.
4. **Safe control.** Retry requires actor scope, expected execution state and
   a stable business key; an unknown external effect is reconciled before
   rerunning. Every denied or authorized control action is audited. Controls
   apply only where the adapter actually owns execution.
5. **Bounded ingestion.** Authenticated, bounded bodies, bounded cardinality,
   sampling/export under explicit policy, drops counted, retention capped.
   Ingestion and query workloads are isolated from control actions so an
   expensive query or a burst can never starve a control record.
6. **Privacy by default.** Default metadata excludes prompts, customer
   content, tokens and secrets. Inclusion is explicit, per-field, and labeled.
7. **Estimated ≠ reconciled.** Model/provider cost figures are estimates,
   labeled as such, stored separately from reconciled billing, and never
   merged into one number.

## Required failure cases

The guide's four cases are contract, not options:

| Trigger | Required result |
|---|---|
| Collector unavailable | Application work continues under a bounded telemetry policy; dropped/gap counts remain visible |
| Duplicate or late events | Merge without false chronology; event time and receive time both shown |
| Model/publishing timeout | Differentiate transport error, validation failure and uncertain external effect |
| Unauthorized retry or cross-project query | Deny and audit; AI-generated investigation text cannot widen access (vacuous until AI summaries exist; tested when they arrive, never claimed before) |

## Ship gate

From the guide: **ship when one failed AI workflow can be diagnosed and
safely resolved with measurably less searching than the existing setup.**

"Measurably" is owed a method, not a vibe. The protocol is pre-registered
in `docs/DRILL.md` (D-017): seeded incidents, tickets with only a request ID
and a symptom, a sealed answer key, search actions and wall time per
ticket. The logs-only baseline is measured in A1, *before* the layer is
used for diagnosis; the layer run is A4, by a performer who did not seed
the incidents, and its result is committed under `perf/results/`.

## Security notes the design must keep

- Event text (error messages, names, attribute values) is
  attacker-influenced: the console renders it as text only, under a strict
  Content Security Policy (D-022).
- Anyone holding an ingest key can send misleading events. Telemetry
  authorizes nothing (invariant 1), and every event shows the key and
  source instance it arrived under, so provenance is visible (D-015).
- Error strings are the usual privacy leak: secret-shape redaction runs on
  every free-text string (error messages and string attributes), in the SDK
  and again in the collector (D-014). It covers known credential prefixes,
  `Bearer`/`Basic` credentials, JWTs, PEM blocks, values after keys that
  name a secret (`*_token`, `api-key`, `client_secret`, `password`,
  `cookie`, `sig`, `*sessionid`, camelCase forms such as `accessToken`, …)
  after `=`, `:`, `=>` or `%3D`, quoted values to their closing quote, the
  rest of an `Authorization:` line, URL passwords and long mixed-case
  alphanumeric tokens, with property tests
  (`crates/aiops-domain/tests/redaction.rs`). It is pattern matching, so it
  is partial by nature (a key spelled with look-alike Unicode letters, for
  one, is not caught); the A4 privacy red-team is still owed.

## Evidence plan

Reference load shape from the guide: 100 instrumented requests/s, ten events
each (≈1,000 events/s). Measure application latency with and without
instrumentation, ingestion p95, queue age, query latency over ≥1,000,000
metadata events, and storage/day. Proposed overhead budget: under 5% median
application latency in the reference fixture — **with dropped data disclosed
alongside the number**. Chaos coverage: burst ingestion, collector loss,
high-cardinality attack, slow queries. Never trade data loss for a latency
chart silently: a run that dropped events says so in its result file.

Per the workspace standard, full runs are million-scale (events ingested is
the unit here), thresholds fail on misses and regressions, and results are
committed under `perf/results/` with machine spec and commit hash. Laptop
numbers are development observations, never production capacity.
