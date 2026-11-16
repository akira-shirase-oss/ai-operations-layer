# AI Operations Layer

> **Status — decisions recorded, A0 starting.** OQ1–OQ6 are answered
> (`docs/DECISIONS.md`, D-012–D-023) and the diagnosis drill is
> pre-registered (`docs/DRILL.md`). No code yet. Milestones are in
> `docs/ROADMAP.md`. Contract:
> `../../docs/Project_Guides/08_AI_Operations_Layer.docx` (extract with
> `textutil -convert txt -stdout`).

An observability and controlled-recovery layer for AI-backed applications:
follow one application operation — request → model/tool/job steps → outcome —
in a single honest timeline that shows where it failed, what was retried,
what it cost, and which outcome remains uncertain. It is an add-on to
existing backends, not a runtime migration: hosts instrument through a Rust
SDK or the language-neutral HTTP/JSON ingestion contract, a Rust/Axum
collector stores events in PostgreSQL, a SvelteKit console renders the
investigation, and execution adapters provide safe, scoped, reconciled
retry — never a generic "run again". Telemetry is evidence, not authority:
it can never authorize a retry, refund or publish, and its own gaps are
displayed as gaps.

The first host is an in-repo reference workflow (`fixtures/reference-host`);
the first real host and execution adapter is Durable Runner, integrated only
through public APIs (D-012).

- `docs/END_GOAL.md` — what done means, scope in/out, the invariants, the
  four contract failure cases, the ship gate
- `docs/ARCHITECTURE.md` — sources of truth, components, emit/ingest/
  investigate/control paths, consistency story, layout
- `docs/ROADMAP.md` — A0 spine → A1 one workflow, investigated → A2 first
  real host → A3 controlled recovery → A4 quality/adoption
- `docs/DECISIONS.md` — D-001..D-023 and any open question
- `docs/DRILL.md` — the pre-registered diagnosis drill behind the ship gate

**What counts as done (the ship gate):** one failed AI workflow can be
diagnosed and safely resolved with measurably less searching than the
existing setup — with the failure-case drills and the load evidence
committed under `perf/results/`.

## License

MIT; see [LICENSE](LICENSE).
