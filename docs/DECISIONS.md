# Decisions

Deliberate choices, newest last. D-NNN numbering continues the workspace
convention. These seed the directions the guide prescribes, each stated as
the trade-off it actually is. Open questions the guide leaves unspecified
are listed at the end, not guessed.

- **D-001 — Telemetry is evidence, never authority.** The event store records
  observed behavior; jobs, budgets and business outcomes stay in their
  authoritative stores, and control paths read those authorities. Trade-off:
  a control action costs an extra read against the host system, and the UI
  must sometimes say "the trace is not the truth" — accepted, because the
  alternative is telemetry loss silently authorizing retries, refunds or
  publication.
- **D-002 — Add-on, not a runtime migration.** Host applications instrument
  through an SDK talking to a separate collector; nothing in the host's
  request path may depend on the layer being up. Trade-off: best-effort
  delivery means gaps are inevitable — accepted, and made honest by bounded
  SDK queues with counted, displayed drops rather than hidden loss.
- **D-003 — OpenTelemetry-compatible IDs, our own event schema.** Context
  propagation and trace/span ID formats follow OTel so instrumented apps
  correlate with the wider ecosystem; the stored event model is a versioned
  schema defined here, not the full OTel data model or collector pipeline.
  Trade-off: no off-the-shelf OTel backend compatibility — accepted for
  direct control of cardinality bounds, redaction and the two-clock model.
- **D-004 — PostgreSQL for metadata and control records; bounded blobs
  elsewhere only on threshold.** Operation metadata, events, control records
  and audit live in Postgres under migrations; object storage is used only
  when a defined bound is exceeded. Trade-off: Postgres carries the event
  volume — accepted because operation metadata is the query surface, and the
  load plan measures it at ≥1M events before believing it.
- **D-005 — Two clocks on every event.** Event time and receive time are both
  stored and both displayed. Trade-off: every timeline view must handle
  disagreement between the two — accepted because merging late/duplicate
  events without false chronology is a contract failure case, and you cannot
  render what you did not keep.
- **D-006 — Default-deny metadata.** The allowlist excludes prompts, customer
  content, tokens and secrets unless explicitly opted in per field and
  labeled. Trade-off: less diagnostic context out of the box — accepted; a
  leaked prompt in a trace is an unforced privacy failure, and opt-in keeps
  the choice visible.
- **D-007 — No generic "run again".** Every control action requires actor
  scope, expected execution state read from the authority, and a stable
  business key; an unknown external effect is reconciled before rerunning.
  Trade-off: recovery UX is slower than a retry button — accepted; a
  duplicated publish is worse than a deliberate one.
- **D-008 — Ingest, query and control are isolated surfaces.** Separate
  pools, limits and credentials so an ingestion burst or an expensive query
  can never starve or delay a control record. Trade-off: three surfaces to
  operate instead of one — accepted because control correctness is the
  product's riskiest promise.
- **D-009 — Estimated cost is a labeled estimate.** Provider/model cost
  figures are stored and displayed as estimates, structurally separate from
  reconciled billing. Trade-off: two numbers where users want one — accepted;
  merging them would make a guess look like an invoice.
- **D-010 — TypeScript SDK first.** *Superseded by D-013 (2026-11-16).*
  The premise was wrong: the applications in this portfolio that could host
  an AI workflow are Rust services, not TypeScript.
- **D-011 — Missing data is a first-class render state.** Gaps, drops and
  `uncertain` outcomes render explicitly everywhere; nothing interpolates
  them into a clean timeline. Trade-off: the console sometimes looks worse
  than reality — accepted; a reassuring lie is the failure mode this whole
  layer exists to remove.

- **D-012 — The first real host and first execution adapter is Durable
  Runner (answers OQ1 and OQ2); an in-repo reference host comes first.**
  No AI workflow exists anywhere in the portfolio yet, so a pilot "in an
  existing backend" cannot start today. Durable Runner is the one system
  here that already runs multi-step jobs with attempts, per-attempt
  deadlines, a business idempotency key (`op_key` on `POST /jobs`) and an
  authoritative inspect endpoint (`GET /jobs/{id}`), with an API key on
  its mutating routes. It integrates **only** through public contracts:
  workflow code running as Durable Runner handlers emits through this
  layer's HTTP ingestion contract (or the Rust SDK), and the adapter reads
  run state from and submits work to Durable Runner's public API, as any
  customer would. Nothing in Durable Runner may depend on this layer, and
  no milestone gate here may require Durable Runner to ship a change — a
  missing capability is recorded as a request and the adapter waits.
  Until then, everything is built and proven against
  `fixtures/reference-host`: a small Rust workflow (request → generation →
  validation → fact-check tool → publish) with fake model and tool steps,
  injectable failures, cost estimates and a fake publisher that keeps its
  own authoritative record. Trade-off: the reference host is not an
  "existing backend", so it proves the mechanism, not the guide's
  adoption claim — the ship gate's comparison must also run on a workflow
  hosted on Durable Runner (milestone A2/A4). Durable Runner's reads are
  unauthenticated and its API key is all-or-nothing, so for control
  actions this layer is the actor-scope boundary; the adapter alone holds
  that key, it never reaches the console, and it is treated as an operator
  credential (least privilege is one key per adapter deployment).
- **D-013 — The first SDK is a Rust crate plus a language-neutral HTTP/JSON
  ingestion contract (corrects D-010).** `crates/aiops-sdk` is the first
  SDK because every candidate host here is Rust; `docs/INGEST_CONTRACT.md`
  is the versioned wire contract any language can implement, so a host
  can also post events without any SDK. A TypeScript SDK follows later,
  under the same contract and its own drop/bound tests. Both SDKs are
  server-side only: an ingest key in a browser is a published key.
  Trade-off: the guide's wording ("TypeScript SDK first") is not followed —
  deliberately, because a TypeScript SDK would have no host to run in.
- **D-014 — No object storage in the first release; hard metadata caps
  instead (answers OQ3).** Events are metadata only. Hard caps, enforced by
  the collector with named refusals: serialized event ≤ 8 KiB, ≤ 32
  attributes, attribute key ≤ 64 bytes, string attribute value ≤ 256 bytes,
  error message ≤ 512 bytes, names ≤ 128 bytes; batch ≤ 500 events and
  body ≤ 1 MiB. The SDK truncates over-long strings *before* sending and
  marks the event (`truncated` count), so nothing is silently shortened.
  The collector never rewrites an event except in two labeled, counted
  ways: it withholds attributes whose keys are not allowed (D-006) and
  redacts secret-shaped strings. Trade-off: less detail per event and no
  payload capture — accepted; it removes a whole privacy and storage
  surface. A blob store returns only with a measured need and a threshold
  above these caps.
- **D-015 — Auth: per-host ingest keys stored hashed; scoped, expiring
  console tokens; denials audited (answers OQ4).** Ingest keys
  (`aiops_ik_` + 32 random bytes, base64url) are issued one per host or
  service, bound to one project, and can only call the ingest route. The
  server stores SHA-256 of the key: keys are 256-bit random, so a slow
  password hash would add hot-path CPU and no security. Every stored event
  records the key it arrived under, so a forged event shows its
  provenance. API tokens (`aiops_tk_`) carry an actor name, a project list,
  scopes (`read`; `admin` for the collector-wide counters, which span every
  project; `operate` reserved for A3 control), and an expiry
  (default 30 days); they are stored hashed too. The console keeps its
  token in session storage and sends it as a bearer header — no cookies,
  so no CSRF surface; A3 control calls additionally require expected state
  and a business key, which make a replayed call a no-op. A valid token
  asking for a project or scope it lacks is denied with 403 and audited
  synchronously; if the audit row cannot be written the request fails
  closed (503). Unauthenticated requests (401) are counted, not audited
  one by one, so a flood cannot become an audit-table write flood. Keys
  and tokens are minted by `aiops-server admin` with direct database
  access; there is no admin HTTP surface. Revocation takes effect within the
  30 s credential cache lifetime, or up to 5½ minutes while a flood has
  spent the lookup budget (a recently valid credential is then served from
  the cache rather than locked out; `docs/INGEST_CONTRACT.md`). Trade-off: no SSO/user accounts
  yet — accepted until the console has more than one operator.
- **D-016 — Retention values (answers OQ5).** Raw events: 14 days by
  receive time. Operation summaries and source counters (including drop
  and gap counts): 90 days, so expiring detail never launders loss into
  silence. Audit log: 365 days. A store sweep deletes in bounded batches.
  Trade-off: two weeks of raw detail is short for slow-burning incidents —
  accepted until storage per day is measured at the reference load (A4);
  the values change only with that measurement and a decision entry.
- **D-017 — The diagnosis drill is pre-registered before the layer is used
  (answers OQ6).** Protocol in `docs/DRILL.md`: seeded incidents drawn from
  a fixed catalog in the reference host's workflow (including a lost
  publish response), tickets that give only a request ID and a symptom, a
  sealed answer key whose hash is printed with the tickets, and two
  measures per ticket — search actions and wall time — plus correctness of
  the diagnosis and the recovery choice. The logs-only baseline is run
  first, in A1; the layer run uses new seeds. Runs by the person or agent
  that wrote the generator are labeled seeder-aware and never count toward
  the ship gate. Trade-off: the ship gate depends on a second person,
  which is now a stated dependency, not a hidden one.
- **D-018 — Loss is measured by sequence numbers, not inferred from
  silence.** Each SDK process is a source instance with a random ID and a
  per-instance sequence number on every event; every event also carries a
  per-operation sequence number (`op_seq`), and an operation's finish
  event states how many events the SDK emitted for it. Holes in `op_seq`
  are definitive evidence of lost events; the SDK's own drop reports
  (`gap.reported` events and batch counters) say why; loss after the last
  received event of an unfinished operation is rendered as *unknown size*,
  never zero (a host restart erases the SDK's counters). Trade-off: a few
  bytes per event and one counter per operation in the SDK — accepted;
  it turns "missing" from a guess into a count.
- **D-019 — Two clocks; order by emission where it is known, by event time
  where it is not, and flag skew.** Within one source instance, `seq` is
  emission order; across instances the timeline orders by event time,
  shows receive time beside it, and flags an event whose event time is
  ahead of its receive time beyond the skew tolerance, or a child step that
  starts before its parent. Late and duplicate events merge idempotently on
  `(project, event_id)`; a duplicate with different content is refused as a
  conflict and counted (first write wins). Trade-off: cross-host order can
  be shown as uncertain rather than as a clean list — accepted (D-011).
- **D-020 — The host declares an outcome's class; the collector never
  infers it.** A step finishes `succeeded`, `failed` with a class —
  `transport` (the request provably never reached the remote), `rejected`
  (the remote answered with a definitive error), `validation` (the output
  failed a check) or `internal` — or `uncertain` with a reason
  (`timeout_after_send`, `connection_lost_after_send`,
  `response_unreadable`). Only the host knows whether a request left the
  process, so the SDK makes the host say so. Trade-off: hosts must be
  honest and precise — accepted; the alternative is a collector that
  guesses about external effects.
- **D-021 — The SDK never blocks or crashes its host.** Emitting is an O(1)
  non-blocking enqueue into a bounded queue (default 8,192 events, so at
  most 8,192 × 8 KiB in memory); when it is full the newest event is dropped
  and counted, per operation where known. A background task batches
  (≤ 500 events or 200 ms), sends with a 2 s timeout, retries only
  retryable answers (429, 503, connection errors) with jittered backoff
  inside a bounded budget, and then drops the batch and counts it. Pending
  gap reports live in a bounded map and are delivered when the collector
  returns. The SDK returns errors, never panics, and `shutdown` has a
  deadline. Trade-off: telemetry is best-effort — accepted (D-002); every
  loss is counted.
- **D-022 — Isolation, concretely.** Six separate PostgreSQL pools, each
  with an acquire timeout and a statement timeout: ingest, query, control
  (operator commands: projects, keys, tokens, revocation), audit (so a burst
  of audited denials cannot hold up a revocation), credential lookups (so an
  unknown-key flood cannot take an ingest or query connection) and
  maintenance (the retention sweep). Amended 2026-11-17 after an independent
  review; the first cut had three. Ingest sheds load beyond a fixed number of in-flight
  requests (503 + `Retry-After`) and applies a token bucket per ingest key
  (429 + `Retry-After`); the query path has its own per-token limit and
  caps on page size and events per timeline. The collector serves the
  built console at `/console/` with a strict Content Security Policy, and
  the console renders all event text as text (no raw HTML), because event
  metadata is attacker-influenced. Trade-off: six pools to size — the
  defaults are development values until the A4 load runs set them.
- **D-023 — The roadmap is re-cut so the spine comes first.** A0 now builds
  the whole thin spine — schema, pure domain, bounded collector, store,
  timeline API, Rust SDK, minimal console, CI, local stack — because
  performance and reliability have to be designed in from the first
  commit, not bolted on after a schema-only milestone. A1 is the reference
  host's full journey, the investigation view, the contract failure cases
  as automated tests, an ingestion load scenario and the drill baseline.
  A2 is Durable Runner as the first real host plus filtering and release
  comparison; A3 controlled recovery; A4 quality and adoption. Trade-off:
  A0 is larger than the original — accepted, because a schema without a
  collector cannot prove bounded ingestion.

- **D-024 — The drill's comparison runs on a multi-service workflow whose
  logs the builder did not write, and every run includes a lost publish
  response (amends D-017 before any counted run).** Two dry runs of the kit
  on the reference host (`perf/results/drill/`, seeder-aware) needed a
  median of 1 and 1.5 search actions per ticket with logs alone: the host
  writes one clean log with the request ID on every line, so `grep <request
  id>` answers most tickets, and a lost publish adds one publisher lookup.
  The layer cannot beat that there: finding an operation in the console is
  at least a list search plus a timeline load (two counted actions), and a
  lost publish still needs the publisher lookup, because the trace is
  evidence, not the authority. So the reference host cannot decide the ship
  gate; it proves the kit and the mechanism. A further risk: the builder
  wrote the baseline's log lines (and chose not to log cost), so the
  baseline's difficulty was under the seeder's control. The deciding
  comparison therefore runs on the Durable Runner-hosted workflow (A2), and
  its baseline uses Durable Runner's and the handlers' logs **as they ship**,
  not lines written for the drill. The first dry run also drew no
  `publish_lost_response` (draws were with repetition), the class the
  guide's first release is about, so every run now includes at least one.
  Trade-off: the gate moves later, behind A2 — accepted; a gate whose
  baseline the builder controls measures nothing.

## Open questions

All six questions the guide left open are answered above (2026-11-16):
OQ1 and OQ2 by D-012, OQ3 by D-014, OQ4 by D-015, OQ5 by D-016 and OQ6 by
D-017. New questions are added here with an `OQ` number and answered by a
named decision when the relevant milestone starts.

- **OQ7 — Durable Runner hand-off shape.** Which of Durable Runner's
  public surfaces (handler request headers, `GET /jobs/{id}` fields) carry
  the operation ID across the boundary. Answered at A2 start, from that
  repository's then-current public contract.
- **D-025 — Joined services' completeness waits for schema v2.** A service
  that joins an operation with `continue_operation` never sends
  `operation.started` or `operation.finished`, so schema v1 carries no count
  for its events and its tail can never be proven complete. Until v2, the
  timeline labels that tail `undeclared_count` ("does not declare how many it
  sent") instead of "no finish observed", and still counts it as unknown.
  Schema v2 adds a per-instance contribution count (sent when the continued
  handle ends) so a joined service can close its tail. Trade-off: multi-service
  operations keep one unknown until v2 — accepted over a guess (review N7,
  2026-11-17).
