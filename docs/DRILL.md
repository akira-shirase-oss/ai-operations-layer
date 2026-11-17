# The diagnosis drill (pre-registered, D-017)

The ship gate says the layer must let one failed AI workflow be "diagnosed
and safely resolved with measurably less searching than the existing
setup". This file fixes, before the layer is used for diagnosis, what is
measured, how, and what result counts. Changing the protocol after a run
needs a decision entry and invalidates the earlier comparison.

## The workflow

The reference host's article pipeline (`fixtures/reference-host`):
request → generation (fake model) → validation → fact-check (fake tool) →
publish (fake publisher, which keeps its own authoritative record and
answers `GET /publications/{business_key}`). The host writes JSON log lines
the way a typical service does: one line per step outcome, with the request
ID, mixed with every other request's lines.

## The incident catalog

Each incident is a fault injected into one request. The answer key holds
the class and the one correct recovery choice.

| Class | What happens | Correct recovery choice |
|---|---|---|
| `model_retry_ok` | generation attempt 1 fails with a transport error; attempt 2 succeeds; the article is published | `none` — it was slow, not broken |
| `validation_failed` | the draft fails validation; nothing reaches the publisher | `resubmit` — no external effect happened |
| `publish_lost_response` | the publisher records the article, the response is lost (timeout after send) | `reconcile_first` — check the publisher by business key before anything else; never rerun blind |
| `publish_rejected` | the publisher answers with a definitive refusal; nothing is recorded | `resubmit` — the refusal proves no effect |
| `tool_timeout` | the fact-check tool times out; the operation stops before publishing | `resubmit` — publishing never started |
| `model_exhausted` | every generation attempt times out after send; the operation fails; estimated model cost is incurred | `resubmit` — nothing published; note the cost |

## A run

`scripts/drill.py seed --mode baseline|layer` (A1) does the following, from a seed the
performer does not see:

1. Starts the stack and the reference host (layer on or off by mode).
2. Sends background traffic (200 normal requests by default) and six
   incidents at random positions: at least one `publish_lost_response`, the
   other five drawn from the catalog with repetition (D-024).
3. Writes `tickets.md` — for each incident only the request ID and the
   customer-visible symptom ("my article did not appear", "it took very
   long") — and `answer-key.json`, whose SHA-256 is printed in
   `tickets.md` so the key cannot change after the run.

Two modes:

- **Baseline (logs only).** The layer is not installed. The performer may
  use any shell tool on the host log and may call the publisher's
  `GET /publications/{business_key}` (it is part of the existing setup).
- **Layer.** The same host with the SDK installed; the performer uses the
  console. Log and publisher access remain allowed and are counted.

## Measures (per ticket)

- **Search actions**: each shell command, each console page load or filter
  change, each publisher lookup. This is the primary measure.
- **Wall time** from opening the ticket to writing the answer. Reported,
  not gated (it also measures typing speed).
- **Correctness**: the written class matches the key, and the recovery
  choice matches exactly. A wrong recovery choice on
  `publish_lost_response` (anything but `reconcile_first`) is a failed
  ticket regardless of the class.

Answers go in `answers.json`; `scripts/drill.py score` checks the key's hash
against the tickets, compares, and writes the result file under
`perf/results/drill/`. Background traffic also carries unticketed noise (about
5% of requests retry the model once and succeed), as real logs do.

## What counts

- **Performer.** Someone who did not write the incident generator or read
  the key. A run by the builder (human or agent) is labeled
  *seeder-aware*: it is a dry run of the kit and never counts toward the
  ship gate.
- **The comparison.** Baseline and layer runs use the same catalog and
  different seeds. The gate passes when, over at least six tickets each,
  the layer run's median search actions per ticket is at least 30% lower
  than the baseline's **and** its correctness is not lower. Every run,
  passing or failing, is committed with its seed, machine and commit.
- **Order.** The baseline is run before the layer is used for diagnosis
  (A1); the layer run is A4.
- **Which workflow counts (D-024).** The comparison that decides the ship
  gate runs on a workflow whose evidence is split across at least two
  services' logs — the Durable Runner-hosted workflow (A2) — and its
  baseline uses those services' logs as they ship, not lines written for
  the drill. On the reference host the logs-only baseline needs about one
  search action per ticket (dry runs in `perf/results/drill/`), while the
  console needs at least two (a list search and a timeline load) plus the
  publisher lookup for a lost publish, so reference-host runs are dry runs
  of the kit only.

## The Durable Runner workflow (D-028)

`scripts/drill.py seed --host durable-runner --mode baseline|layer` runs the
same article pipeline as Durable Runner jobs (`generate`, `validate`,
`factcheck`, `publish`). The handlers are `fixtures/dr-article-worker`
(uninstrumented, no log lines). In layer mode the runner calls the
adapter, which forwards to the worker. The six classes and their correct
recoveries are as above. "Resubmit" means a new job under a new `op_key`,
because an `op_key` returns its first run. Tickets give the `op_key` and a
symptom. The baseline may use only what the runner ships: its server log,
`durable-cli list/inspect` with the read key, and the publisher's
`GET /publications/{op_key}`.

The first dry run (seeder-aware, `perf/results/drill/20261117T121345Z-dr-
baseline.md`) needed a median of 1.5 search actions, 6/6 correct. The
runner's state names the failed step, and the tickets' symptoms nearly
name the class, so on this catalog (catalog A) the comparison turns on
half an action. D-029 records the registered comparison as missed here,
keeps that result as evidence, and pre-registers catalog B.

## Catalog B (pre-registered 2026-11-17, D-029; not yet run)

Registered before any run on it. Its generator and the worker faults it
needs are built at A4; until a run is committed, nothing here is evidence.

**Arms.** Catalog A (above) and catalog B, each in both modes. Every
counted run reports both arms; the criterion ("What counts") is unchanged
and applies to each arm on its own.

**Incidents that share a runner state but differ in cause and recovery.**

| Runner state | Class | What happened | Correct recovery |
|---|---|---|---|
| `failed:step:0` | `generate_refused` | the provider refused the prompt on every attempt | `fix_prompt` (the same input fails again) |
| `failed:step:0` | `generate_timeout` | every attempt timed out after sending | `resubmit` |
| `failed:step:0` | `generate_quota` | a 429 on every attempt exhausted the retries | `wait_or_raise_quota` |
| `failed:step:3` | `publish_refused` | the publisher refused every attempt; nothing was recorded | `fix_and_resubmit` |
| `failed:step:3` | `publish_landed_then_refused` | attempt 1 published and its reply was lost; later attempts were refused as duplicates | `none_published` (do not resubmit) |
| `done` | `published_twice` | two effectful attempts each published | `report_duplicate` |
| `failed:step:1` | `validate_after_release` | validation fails on a new release or prompt version for inputs the old one passed | `roll_back` |
| `failed:step:1` | `validate_bad_input` | the input itself is invalid; other runs of the same release pass | `fix_input` |
| any | `telemetry_gap` | the operation has an unknown tail or a gap; only the runner's answer decides | `ask_runner` ("unknown; ask the runner") |

**Tickets.** At least 12 per run, drawn with a fixed, recorded seed and
stratified so every runner state above appears, with at least 3 lost
publishes (`publish_lost_response` from catalog A or
`publish_landed_then_refused`). Each ticket gives the `op_key` and the
user-visible symptom only ("my article did not appear", "it appeared
twice"), written by someone who has not seen the class.

**Counting, frozen per arm.** Baseline: each shell command, each
`durable-cli` call, each publisher lookup. Layer: each console page load,
each search or filter change, each toggle opened (details, codes), each
publisher lookup. Copying an ID is not an action.

**Performer.** Neither the seeder nor the ticket writer.

**Measures.** Search actions (primary), correctness, wall time (reported,
not gated) and **wrong-recovery risk**: per arm, the number of answers
whose recovery could duplicate or lose an external effect (`resubmit` on
`publish_landed_then_refused`, anything but `report_duplicate` on
`published_twice`, anything but `ask_runner` on `telemetry_gap`). It is
registered now and reported beside the criterion; it does not change it.

