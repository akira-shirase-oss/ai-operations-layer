# AI Operations Layer

> **Status — planned.** Documentation set only; no code yet. Milestones A0–A4
> in `docs/ROADMAP.md` are all Planned. Contract:
> `../../docs/Project_Guides/08_AI_Operations_Layer.docx` (extract with
> `textutil -convert txt -stdout`).

An observability and controlled-recovery layer for AI-backed applications:
follow one application operation — request → model/tool/job steps → outcome —
in a single honest timeline that shows where it failed, what was retried,
what it cost, and which outcome remains uncertain. It is an add-on to
existing backends, not a runtime migration: hosts instrument through a
TypeScript SDK, a Rust/Axum collector stores events in PostgreSQL, a
SvelteKit console renders the investigation, and execution adapters provide
safe, scoped, reconciled retry — never a generic "run again". Telemetry is
evidence, not authority: it can never authorize a retry, refund or publish,
and its own gaps are displayed as gaps.

- `docs/END_GOAL.md` — what done means, scope in/out, the invariants, the
  four contract failure cases, the ship gate
- `docs/ARCHITECTURE.md` — sources of truth, components, emit/ingest/
  investigate/control paths, consistency story, layout
- `docs/ROADMAP.md` — A0 contract → A1 one workflow → A2 investigation →
  A3 controlled recovery → A4 quality/adoption, each with its proof gate
- `docs/DECISIONS.md` — D-001..D-011 seeded by the guide, plus the open
  questions the guide leaves unspecified

**What counts as done (the ship gate):** one failed AI workflow can be
diagnosed and safely resolved with measurably less searching than the
existing setup — with the failure-case drills and the load evidence
committed under `perf/results/`.
