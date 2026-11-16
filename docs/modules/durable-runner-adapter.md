# The Durable Runner host and adapter (next milestone: A2, then A3)

Status: **planned**. Nothing here is built. Durable Runner is changing while
this is written, so every fact about it below was read from its public docs
(`docs/OPERATING.md`, `docs/ARCHITECTURE.md`, its run-state type) on
2026-11-16 and must be re-read at milestone start (OQ7). This layer never
edits Durable Runner and no gate here waits for it to ship a change (D-012).

## Why Durable Runner

It is the one system in the portfolio that already runs multi-step work
with attempts, per-attempt deadlines and its own reconcile of a vanished
handler reply, under a business idempotency key (`op_key` on
`POST /jobs`), and that answers "what state is this run in" from its own
authority (`GET /jobs/{id}`: `Queued`, `Running`, `RetryWaiting`,
`Reconciling`, `Done`, `Failed`, `Cancelled`). That is exactly the
evidence/authority split this layer needs, in a real product.

## A2 — Durable Runner as the first real host (observe only)

- **Where the events come from.** The workflow's steps run as Durable
  Runner's allowlisted HTTP handlers. The handler service links
  `aiops-sdk` (or posts the ingest contract) and emits the operation's
  steps; Durable Runner itself is not instrumented and does not know the
  layer exists.
- **Correlation (OQ7).** The operation ID must cross the boundary in a
  public field: the job's input (the submitter puts a W3C `traceparent`
  in it) is the candidate that needs no change in Durable Runner. Whether
  the runner forwards anything to handlers (a header, the run ID) is read
  from its contract at A2 start; if nothing is forwarded, the handler reads
  the `traceparent` from the job input.
- **Authority beside evidence.** The console shows the run's state as
  read from `GET /jobs/{id}` next to the timeline, labeled as Durable
  Runner's answer, read at a stated time. The trace never overrides it.
- **The drill that decides the ship gate (D-024)** runs here: evidence is
  split across the runner's journal and inspector, the handler service's
  logs and the publisher.
- **Gate:** a second developer tells a real failure from missing telemetry
  on this workflow without guessing.

## A3 — the Durable Runner adapter (controlled recovery)

The adapter is the only component that may cause an external action, and
only inside Durable Runner's ownership contract:

| Control | What the adapter does | What it never does |
|---|---|---|
| Inspect | `GET /jobs/{id}` (the authority) before any decision | infer state from the trace |
| Reconcile | wait while the run is `Reconciling`; read the publisher (the effect's authority) for an `uncertain` publish | treat a missing span as "did not run" |
| Retry | `POST /jobs` with the **same** `op_key` (the business key) only when the authority says the run is terminal and the effect is absent | submit a new `op_key`; offer a generic "run again" |
| Cancel | `POST /jobs/{id}/cancel` (stops scheduling; never undoes) | claim a cancel undid an effect |

- **Every control call** carries actor scope (`operate`), the expected run
  state read from the authority (refused if it changed), and the business
  key; the decision and its audit row commit together in PostgreSQL before
  the adapter calls Durable Runner; the outcome is observed through both
  the trace and the authority.
- **Credentials.** Durable Runner's API key is all-or-nothing and its reads
  are open, so this layer is the actor-scope boundary for the controls it
  exposes. The adapter alone holds the key (one per adapter deployment),
  it never reaches the console or logs, and it is treated as an operator
  credential.
- **A known edge:** Durable Runner expires terminal history after seven
  days, and an `op_key` reused after that re-executes. The adapter refuses
  a retry when the run is no longer inspectable rather than resubmitting
  into a possibly duplicated effect; the refusal is audited.

## Failure cases the adapter milestone must test

- Durable Runner down or `503`: controls refuse with a retryable answer;
  nothing is recorded as done.
- The run state changed between read and act: refused (expected state).
- A replayed control call: a no-op (same business key, same expected
  state).
- An unauthorized retry (no `operate`, another project): denied and
  audited.
- A lost publish response: reconcile against the publisher first; the
  business key prevents a duplicate publish.
