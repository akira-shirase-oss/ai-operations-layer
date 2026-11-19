# Controlled recovery (milestone A3) — design for review

Status: **proposal, not yet built** (2026-11-18). Everything below the line
marked "proposed" is open for the coordinator's review before code. The
binding text is the A3 row of `docs/ROADMAP.md` and the invariants in
`docs/END_GOAL.md` (1 telemetry is not truth, 4 safe control) and
`docs/ARCHITECTURE.md` (the control path). The gate A3 must prove:

> The lost-publish-response drill: the vanished effect reads as
> `uncertain`, reconcile runs against the authority before any rerun, the
> business key prevents a duplicate publish; an unauthorized retry is
> denied and audited; replayed control calls are no-ops; no generic
> "run again" exists.

## What a control action is

A **control record** is the layer's own authoritative record (in
PostgreSQL) of one operator's intent to act on one operation: *who*
(actor + token), *what* (reconcile, retry; cancel arrives with a host
that owns it), *on which operation and business key*, *what the operator
asserted the authority's state to be*, the *decision* (authorized or
denied, with the reason), the *authority snapshot* the decision was made
from, and the *outcome* observed afterwards. The record, its
authorization check and its audit row commit in one transaction;
**no transaction spans an external call** (ARCHITECTURE).

Two kinds exist in this milestone:

- **reconcile** — read the effect's authority and store its answer. It
  changes nothing outside the layer. For an `uncertain` publish it
  answers: did it land, or not?
- **retry** — submit replacement work to the host through its public
  contract, under the stable business key, only after the checks below.
  What "submit" means is the adapter's (below). There is no generic
  rerun: every retry names its operation, its business key and its
  expected state.

## Proposed model (aiops-domain, pure — no I/O)

A control record's `status` moves only forward, by named transitions the
domain validates (`control.rs`):

```
requested → reconciling → reconciled → (feeds retry)
requested → authorized → submitted → confirmed | submit_failed | observed_unknown
any non-terminal → denied(reason)          (terminal)
submitted stale on boot → reconciling again (restart re-drive)
```

- `denied` is a first-class, terminal status — a denial is a decision,
  not an absence.
- `submit_failed` means the host call failed before a definitive answer;
  it is retryable by the executor with a bounded budget, then terminal.
- `observed_unknown` means the submit was sent but its outcome is not
  proven — the record stays honest about it (it is never upgraded to
  confirmed without authority evidence).
- The resolution stored on a reconcile record: `effect_present`,
  `effect_absent`, `still_uncertain` (the authority could not answer —
  the record says so; it never guesses).

## Proposed API (control surface, separate pool — D-008/D-022)

On the query router's siblings, a control group behind the existing
auth stack; the control pool already exists (`control_connections`).

| Route | Scope | Body | Answer |
|---|---|---|---|
| `POST /v1/projects/{p}/operations/{op}/control/reconcile` | `operate` | `{business_key, expected_state}` | the record, incl. the authority snapshot |
| `POST /v1/projects/{p}/operations/{op}/control/retry` | `operate` | `{business_key, expected_state, step_id?}` | the record (`authorized`/`denied`, then `submitted` → observed) |
| `GET /v1/projects/{p}/operations/{op}/control` | `read` | — | the operation's control records |
| `GET /v1/projects/{p}/operations/{op}/control/{id}` | `read` | — | one record |

Request rules:

- **`expected_state`** (required, bounded JSON ≤ 4 KiB): the actor's
  assertion of the authority's current state — e.g. Durable Runner
  `{"run": 12345, "state": "Failed"}`; the reference host
  `{"published": false}`. The adapter re-reads the authority and refuses
  on mismatch (`expected_state_mismatch`, audited). It is the operator's
  seen-state made explicit: a stale operator cannot stomp a newer world.
- **`business_key`** (required, ≤ 256 bytes): the stable business key —
  the publisher's `article:<request_id>` key, the runner's `op_key`. It
  is what dedup hangs off at the *effect's* authority.
- **Replay:** `(project, operation, action, business_key)` is unique.
  A replayed call with an identical body returns the existing record —
  a no-op (`replayed: true` in the answer). A same-key call with a
  different body is a `409` conflict, audited. A retried `POST` from a
  client that never saw the first answer is therefore safe.
- **Retry precondition:** when the operation carries an `uncertain`
  step for that business key, a prior `reconciled` record on the same
  key must exist whose resolution is `effect_absent`. `effect_present`
  → the retry is denied `effect_confirmed` (the right answer is "already
  done"). `still_uncertain` → denied `reconcile_uncertain`; the operator
  re-reconciles or stops. Failures by class `rejected`/`validation`/
  `transport` (no effect sent) need no reconcile — the class itself is
  the proof.
- **Every call audited**, allowed or denied (`control.reconcile`,
  `control.retry`; reasons: `scope_missing`, `project_not_in_scope`,
  `expected_state_mismatch`, `reconcile_required`, `effect_confirmed`,
  `reconcile_uncertain`, `adapter_unavailable`, `conflict`). A denied
  call whose audit row cannot be written fails closed — the existing
  pattern.
- **Approval record (proposed):** the record itself is the approval —
  the actor's identity, scope check, expected-state assertion and the
  decision commit together; `approved_by`/`approved_at` are columns. A
  second-approver field is reserved in the schema but unused: two-person
  sign-off is a later contract. *Decision point for review.*

## Proposed executor (in-server, bounded)

A `TaskTracker` task inside `aiops-server` drives records from
`authorized` to `submitted` to observed, on the **control pool**. Bounds:
a semaphore on in-flight external calls (16), a deadline per authority
read (2 s, same discipline as `authority.rs`) and per submit (5 s), and
no shared state with ingest or query. On startup the executor re-drives
records stuck in `submitted`/`reconciling` — safe because every submit
is idempotent under the business key, and re-reconcile is just a read.
Trade-off accepted: a crash between commit and submit is recovered by
re-drive, not prevented; the business key makes the worst case a
returned "already exists".

Alternative considered and not taken: a separate `aiops-control-executor`
binary. More isolation, but one more unit to deploy and keep version-
locked, for what is a handful of HTTP calls under deadlines. *Decision
point for review.*

## Proposed adapters

An adapter owns execution for one kind of host and is the only code that
may cause an external effect (ARCHITECTURE; `aiops-adapter-*`). It holds
the host's credentials; nothing else does (D-012: the adapter alone
holds Durable Runner's API key). Both adapters talk to hosts only
through public contracts.

| | Reference host's publisher (first) | Durable Runner |
|---|---|---|
| Reconcile | `GET /publications/{business_key}` → `effect_present` / `effect_absent` | `GET /jobs/{id}` (existing authority read) for run state; `GET /publications/{key}` on the worker for the publish effect |
| Expected state | the publication record vs the assertion | the run's `state` vs the assertion |
| Submit | `POST /articles` with the original `x-request-id` → the host runs the workflow again under the **same** business key | `POST /jobs` under a **new** `op_key`, the business key carried in the job input |
| Refuses to act when | the authority is unreachable or answers `still_uncertain` | the run is not inspectable (e.g. expired terminal history) |

- **The `op_key` finding (A2) stands:** an `op_key` returns its first
  run, so re-submitting under it cannot rerun a failed Durable Runner
  job. The rerun is a new job under a new `op_key`; the business key is
  carried by the effect's own authority (the publisher's record), not by
  the runner's dedup. The roadmap's shorthand "submit under `op_key` =
  business key" reads as: the submit is idempotent *at the effect's
  authority*, not at the runner. *Decision point for review.*
- **The reference host's publish becomes idempotent under its key:**
  `publish` consults the `publications` record under the business key
  before acting (check-then-act inside the host — the pattern every
  adapter requires of a real host). `times_published` then counts only
  genuine duplicate effects, and the lost-publish drill's assertion is
  `times_published == 1` after reconcile + a refused retry.
- **Cancel is reserved:** Durable Runner exposes
  `POST /jobs/{id}/cancel`; it lands when the drill needs it, under the
  same record and checks. Not in the first cut.

## Schema sketch

```sql
CREATE TABLE control_records (
  id uuid PRIMARY KEY,
  project_id uuid NOT NULL REFERENCES projects,
  operation_id text NOT NULL,
  action text NOT NULL,            -- reconcile | retry (| cancel later)
  business_key text NOT NULL,
  expected_state jsonb NOT NULL,
  actor text NOT NULL, token_id uuid NOT NULL,
  status text NOT NULL,            -- the state machine above
  resolution text,                 -- effect_present|effect_absent|still_uncertain
  authority_read jsonb,            -- source, answer, read_at
  submitted_ref text,              -- new operation/run id or publication key
  approved_by text, approved_at timestamptz,   -- the record is the approval
  second_approver text,            -- reserved, unused
  created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
  UNIQUE (project_id, operation_id, action, business_key)
);
```

Audit rows reuse `audit_log` (new action names). Denials and
authorizations both write rows, synchronously, in the deciding
transaction.

## Console

On the operation page: a control section listing the records (status,
resolution, actor, times), and for `operate`-scoped sessions two
actions — *Reconcile* and *Retry* — behind a confirm dialog that
restates the business key, the expected state and "the trace is not the
authority". `read`-only sessions see the records, never the buttons.
All states render honestly: `denied`, `observed_unknown`,
`still_uncertain` are first-class renders (D-011).

## Bounds and isolation

Control endpoints: per-token bucket (proposed 5/s, burst 10 — an
operator action, not a data path), ≤ 16 in flight, request deadline.
Authority reads and submits: deadlines as above; the authority gate is
reused for reads. Nothing on this path touches the ingest or query
pools; a burst of control calls cannot starve them, and a flooded
control caller only spends its own bucket.

## Tests (same branch, per the testing rule)

- **domain:** every legal transition, every refused transition, replay
  equivalence.
- **store:** decision+audit atomicity; idempotent replay by the unique
  key; restart-resume selection; conflict on same-key-different-body.
- **API:** unauthorized retry denied+audited; cross-project denied;
  replay no-op; `expected_state_mismatch` 409; `reconcile_required` on an
  uncertain effect; `effect_confirmed` denies retry; audit-unavailable
  fails closed.
- **fixture (the gate):** `publish_lost_response` → reconcile answers
  `effect_present` → retry refused (`effect_confirmed`) →
  `times_published` stays 1; and a rejected publish → reconcile
  `effect_absent` → retry submitted → publish lands once.
- **console:** an `operate` journey through reconcile→denied-retry; a
  `read` journey sees records, no buttons.
- **binary:** the real `aiops-server` against the real host.
- **perf/k6/control.js:** replayed + denied control calls while ingest
  runs — ingest thresholds unchanged, control p95 bounded, zero
  duplicate control records. The lost-publish drill is recorded under
  `perf/results/`.

## Decision points for the coordinator

1. Approval = the record itself (single actor), second approver
   reserved — or does the contract want two-person sign-off now?
2. Reference-host retry = resubmit the workflow under the same business
   key with the host's publish made idempotent by key — or a narrower
   "republish only" endpoint?
3. DR retry = new job, new `op_key`, business key in the input (the A2
   finding), dedup at the effect's authority.
4. Executor in-server on the control pool — or a separate binary?
5. Naming/shape of `expected_state` (adapter-typed JSON asserted by the
   operator) and whether `step_id` scoping belongs in v1.

Each accepted point lands as a D-entry in `docs/DECISIONS.md` with the
implementation; rejected ones get replaced here first.
