# Next slice — A4a: usage and cost surfaces (estimated vs reconciled)

Planning only (2026-11-21). The slice is A4's "usage and
estimated-vs-reconciled cost surfaces" line — the largest END_GOAL claim
the branch does not yet prove. The redaction/privacy red-team portion of
A4 landed with the security drill; the retention sweep and its throughput
evidence landed at `db1d1ed`
(`perf/results/2026-11-17-retention-sweep.md`, development observation).
What remains unbuilt is a readable surface: what the layer saw, dropped
and estimated per project — and a reconciled-cost record it never merges
with.

## Scope

- **Usage surface** (read scope): counters the store already accumulates
  — `source_instances.events_received/events_refused`,
  `dropped_queue_full/send_failed/rejected`, `gap_overflow`,
  `truncated_events` — aggregated per project and per ingest key, plus
  collector-observed drops (rate-limited/shed batches attributed to the
  key where known). New endpoint `GET /v1/projects/{p}/usage` on the
  query pool; numbers carry the two-clock caveat (counters are
  receive-time facts).
- **Estimated cost rollup**: `cost_estimate_micros` already merges
  per-operation with `MIXED` currency marking; the surface rolls it up
  per project, per release and per model where present — never summing
  across currencies.
- **Reconciled cost record**: the missing half of invariant 7. Minimal
  shape: an admin-entered, audited record
  (`project, period, amount_micros, currency, source_label, entered_by`)
  with its own table and admin-scope write path — there is no billing
  authority to read, so it is operator-entered authority data, not
  telemetry. Stored and rendered beside estimates, never summed with
  them. Needs a named decision (OQ9 candidate: where reconciled numbers
  come from) before code.
- **Console**: a usage/cost page per project (kit states: loading, empty,
  error-with-retry, and multi-currency as a first-class render — two
  rows, not a sum).

## Failure cases and attack classes (each → prove-attacks.sh)

Each is a test plus a named mutant that removes the protection:

1. **Replay inflation** — a replayed batch must not re-add to usage
   counters or cost rollups. Mutant: apply `EXCLUDED` deltas for
   already-known event IDs (the merge summarizes only `fresh` events —
   the attack proves the rollup inherits that).
2. **Currency laundering** — estimates in two currencies surface as two
   rows (or `MIXED`), never one number. Mutant: `sum()` ignoring
   currency.
3. **Estimate/reconcile merge** — no read path may add reconciled amounts
   into an estimate. Mutant: combine the fields in the response.
4. **Cross-project usage read** — a token scoped to project B reading
   project A's usage/cost: denied and audited. Mutant: drop the project
   binding in `authorize`.
5. **Unscoped reconciled write** — a non-admin token entering a
   reconciled record: denied and audited. Mutant: widen the scope check.
6. **Drop laundering** — usage must present drops as counters, never net
   them out of "received" silently. Mutant: report `received - dropped`
   as received.
7. **Rollup immortality** — usage rollup rows must be inside the
   retention sweep's remit (they are event-derived; an immortal rollup
   outlives the privacy contract). Mutant: sweep skips the rollup table.
8. **Reconciled tampering by telemetry** — ingest payloads must not be
   able to write reconciled fields (shape validation refuses a `cost`
   key outside `cost_estimate`). Mutant: accept the field.

## Load scenario at 1M requests

`perf/k6/usage.js` (+ `perf.yml` entry), riding the operator's
million-scale run:

- Ingest: the committed control-million shape (≥1M requests total),
  events spanning ≥3 currencies and several models/releases.
- Concurrent `GET /v1/projects/{p}/usage` at a modest fixed rate from a
  second token, plus a few reconciled-record writes.
- Thresholds: ingest p95/loss unchanged versus the run without usage
  reads (isolation on the query pool); usage-read p95 bounded;
  **the post-run reconciliation is the real gate** —
  `stored == sent - refused - shed`, per-currency sums exact, drop
  counters equal the reported losses. Losses disclosed in the result
  file per the workspace rule.

## Risks already visible

- **Write amplification.** `ingest_batch` is one multi-CTE statement per
  batch; adding project×period rollup writes must stay in that shape or
  ingest p95 regresses — the lock-order work (D-026) bounds how much new
  write surface is safe on the hot path.
- **Reconciled records are authority data with no upstream authority.**
  Who enters them, and whether an API write is the right shape at all, is
  a genuine open question — resolving it silently would violate the OQ
  rule; it needs a named decision first.
- **Counters vs retention.** Whether rollups derived from swept events
  outlive the raw rows must be decided explicitly: aggregate counters can
  outlive raw events (aggregates keep drop counters per ARCHITECTURE),
  but per-model/per-release rollup rows probably should not — pick, then
  pin with a test.
- **This slice does not advance the ship gate.** OQ8 is still open and
  no non-seeder performer exists; it is parallel quality work, not
  progress toward the counted drill — the roadmap update should say so.
- **Adjacent, deliberately excluded:** sampling/export policy (invariant
  5's unbuilt half — sampled-out events look exactly like loss, so the
  policy needs its own contract discussion before any code) and the
  second independent workflow.

## END_GOAL claims this branch makes but has not proven

Honest list, strongest first:

1. **"Estimated cost labeled separately from reconciled billing"** —
   estimates flow and are labeled; no reconciled store or surface exists,
   so "never merged" is currently vacuous. → this slice.
2. **"Instrumentation overhead < 5% median"** — no evidence anywhere;
   needs a with/without measurement on the reference fixture.
3. **Quiet-machine ≥1M ingest run** — A1's leftover; `630f533` missed
   (36,580 lost on a busy laptop), the CI control-mix run
   (`ci-44763875623`) is a different shape. README carries the debt
   honestly.
4. **Query latency over ≥1M events** — committed runs are laptop-scale
   (`2026-11-20-ingest-queryrun{,2}`); the million-scale query phase is
   owed (A2 row).
5. **"storage/day"** — no measurement.
6. **Burst and high-cardinality runs** — the flood failure case is
   tested; no committed `perf/results/` run at scale for either.
7. **Collector-loss at scale** — the pg-restart and lost-publish drills
   are committed at drill scale; no million-scale collector-loss run.
8. **"Sampling/export under explicit policy"** (invariant 5) — no policy
   or mechanism exists; excluded from this slice pending its own
   contract.
9. **Retention sweep at production-shape load** — throughput is a
   committed laptop observation; not yet measured under the reference
   ingest shape.
10. **A2's second-developer gate** — REVIEW-A4 is the closest thing; a
    true second developer is still owed (people, not code).
11. **The ship gate itself** — no counted drill run; OQ8 open, and no
    performer who did not seed the incidents (people + design).

Not claimed: "AI-generated investigation text cannot widen access" —
correctly vacuous until AI summaries exist; nothing produces AI text.
