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

`scripts/drill.sh` (A1) does the following, from a seed the performer does not see:

1. Starts the stack and the reference host (layer on or off by mode).
2. Sends background traffic (200 normal requests by default) and six
   incidents drawn from the catalog (with repetition) at random positions.
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

Answers go in `answers.json`; `scripts/drill-score` compares them with the
key and writes the result file under `perf/results/drill/`.

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
