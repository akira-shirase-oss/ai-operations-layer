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
- **D-010 — TypeScript SDK first.** Existing applications in scope are
  TypeScript; Rust and other SDKs follow only under contract. Trade-off:
  eventual multi-SDK maintenance — accepted per the guide's "TypeScript SDK
  first".
- **D-011 — Missing data is a first-class render state.** Gaps, drops and
  `uncertain` outcomes render explicitly everywhere; nothing interpolates
  them into a clean timeline. Trade-off: the console sometimes looks worse
  than reality — accepted; a reassuring lie is the failure mode this whole
  layer exists to remove.

## Open questions

The guide leaves these open; they are answered by named decisions when the
relevant milestone starts, not assumed here:

- **OQ1 — Pilot host.** "One authorized noncritical content workflow" is not
  named. Candidates: an existing workspace application's content path (e.g.
  colophon's generate→validate→publish path) vs. a purpose-built fixture.
  The guide's "add-on to existing backends" wording favors a real app.
- **OQ2 — First execution adapter.** "The workflow runner or hosted jobs."
  The workspace's durable-runner is a candidate but is not prescribed.
- **OQ3 — Blob threshold.** "Bounded blobs in object storage when needed" —
  what size/kind triggers offload is unspecified.
- **OQ4 — Auth model.** "Bounded authenticated ingestion", "actor scope" and
  "cross-project query" imply project-scoped credentials and actor identity,
  but the concrete shape (token kinds, project model, console auth) is open.
- **OQ5 — Retention values.** "Cap retention" without durations; the
  ingest/query cost of any cap is part of the A4 load evidence.
- **OQ6 — Diagnosis metric.** "Measurably less searching" needs a drill
  protocol: which workflow, what baseline, who performs it, what is timed.
