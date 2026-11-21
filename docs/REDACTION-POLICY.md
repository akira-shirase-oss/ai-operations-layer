# Redaction policy — typed field roles (design, 2026-11-21)

Design only; the ruling adopted the structural assessment from the
round-4 remediation: four heuristic rounds each found a new leak shape
because the free-text scanner must recognize *every* credential shape in
*any* context. The fix inverts the default — the schema classifies each
field, the role decides the treatment, and the heuristics survive only
as the free-text backstop. This slice sits ahead of A4a usage/cost.

## Roles

Every string-bearing field gets exactly one role. The role is a closed
rule, never a guess:

| Role | Rule | Fields |
|---|---|---|
| `id` | Parsed to a closed format; content never stored raw, never shown raw — only the parsed form. | `event_id`, `operation_id`, `trace_id`, `step_id`, `parent_step_id`, `source.instance_id` |
| `enum` | Parsed against a closed vocabulary; anything else is refused by name. | `event_type`, `kind`, `outcome`, `failure_class`, `uncertain_reason`, `reason` (gap), `cost_estimate.currency` (3-uppercase) |
| `scalar` | Numbers, bools, counts — type-checked, never text. | `seq`, `op_seq`, `attempt`, `events_emitted`, `dropped`, `truncated`, `usage.*`, `cost_estimate.micros`, `external_effect`, `stats.*` |
| `time` | RFC 3339 into `Timestamp`. | `event_time`, `first_dropped_at`, `last_dropped_at` |
| `identifier` | Bounded charset `[A-Za-z0-9._:/@-]` + `looks_like_secret` — the CLOSED refuse check: a credential-keyed `k:v` tail outside the state-word allowlist, a bare ≥32-hex run, or a known token shape refuses by name. Stored verbatim otherwise (identifiers are never rewritten — `[redacted]` would corrupt what the caller reads back). | `name`, `release`, `deployment`, `request_id`, `error_code`, `model`, `prompt_version`, `config_version`, `source.service`, `source.sdk`, `cost_estimate.basis` |
| `key` | Attribute keys: `[a-z0-9._]` + `Policy::treat` — a `CREDENTIAL_KEYS` name is **always Withheld** (an opt-in cannot whitelist a credential — new rule); then the default allowlist, then opt-in; anything else is Withheld — neither key nor value stored. | `attributes` keys |
| `keyed_value` | An attribute value lives or dies by its key's treatment: Withheld and credential-named keys store nothing, *whatever the value's shape* (`hunter2` under `attributes.password` dies by rule). Stored string values then pass the free-text backstop as defense in depth. | `attributes` values |
| `free_text` | `redact()` — the heuristic scanner. This is the only heuristic role left. | `error_message` |

Numeric/bool/`Option` fields and `Refusal.field` (`&'static str` — the
schema name, never client data) carry no text and need no role.

## How a field gets a role — and what an unclassified one does

- `WireEvent` is `deny_unknown_fields`: a wire field the schema does
  not declare is `malformed`, never seen.
- A declared field reaches an event only through `only()`'s allowed
  list; anything else is `field_not_allowed`.
- The table above becomes `FIELD_ROLES` — a const in `policy.rs` with a
  coverage test: every name in `only()`'s `present` array and every
  `allowed` list must appear in the table, and every table row must
  name a live field (no stale rows). The test fails a compile-clean but
  unclassified field — a role is not optional paperwork.
- A new attribute key needs nothing new: unclassified is already
  Withheld. The closed order — credential-named → default → opt-in —
  means an opt-in can never admit `password`, `api_key`, `token`,
  `authorization`, `cookie` or the rest of the `CREDENTIAL_KEYS` set
  (today's `SECRET_PARTS`/`SECRET_SUFFIXES`/`KEY_QUALIFIERS` word lists,
  renamed to say what they are: a key *namespace*, not a value guess).
- The console shows only stored fields, so roles apply once, at ingest;
  the render path inherits the decision (text-as-text rendering stays).

## Corpus migration — two tiers

`MUST_REDACT` splits by *why* a row must die:

- **`KEYED_REDACT` — deterministic, 100% by construction.** A failure
  here is always a bug; there is no measured residual.
  - Attribute under a withheld key → not stored (exists).
  - Attribute under a credential-named key → withheld even when the
    project opted the name in; `attributes.password = "hunter2"` is the
    canonical case — no credential *shape* present, the key alone kills.
  - `opt_ident` credential-keyed refusals (the N-E set):
    `password:hunter2`, `token:<hex>`, bare ≥32 hex, `password:123456`.
  - Keyed text inside free text: `k=v`/`k:v`/`"k":"v"` pairs under a
    `CREDENTIAL_KEYS` name, quoted or not, JSON-nested, escaped-JSON,
    percent-encoded key or separator. The key name is a closed word —
    matching it is mechanical; the value dies whatever its shape.
- **`BACKSTOP_REDACT` — measured, with the documented residual.** The
  unkeyed shapes only: bare ≥32 runs, JWTs, provider-prefixed tokens,
  URL userinfo. The R2 residual stands — a `/`-separated strict-name
  piece (with a real word piece for a whole-run keep) may survive —
  and it is now the *whole* residual: the properties keep their
  ≥8-byte windows and the `None` mirror, covering only what the
  heuristics still own.

`MUST_KEEP` is unchanged in spirit but annotated: keeps are identifier
fields (Mistral ids, `collector-2026-11-20`, SHA digests, UUIDs) or
free text with no credential shape. A keep that is credential-*keyed*
is a bug regardless of shape.

## Attack classes → `prove-attacks.sh`

- **opt-in cannot whitelist a credential** — `set-opt-in password`,
  then `attributes.password`: withheld. Mutant `opt-in-beats-credential`
  (`treat()` consults `opt_in_keys` before `CREDENTIAL_KEYS`) must red.
- **unclassified attribute key stays withheld** — exists today; pin it
  with mutant `unknown-key-default-allowed` (`treat()` returns Default
  for unrecognized keys).
- **shape-less value under a credential key** — `hunter2` under
  `attributes.password` dies with no secret shape to match; mutant
  `credential-key-by-shape` (check the value's shape, not the key's
  name) must red.
- **unclassified wire field refused** — the coverage test plus a
  FieldNotAllowed case; mutant `unclassified-field-admitted` must red.
- **keyed patterns still die in free text** — the keyed corpus rows in
  `error_message` and stored attribute strings; mutant
  `keyed-text-kept` (the key-match arm of `match_secret` skipped) must
  red.
- **identifier credential refuses** — the N-E set; mutant
  `identifier-credential-stored` (the `k:v` tail rule removed) must red.

## What it removes from the heuristics

- `looks_like_secret` stops being "the heuristic, narrowed for
  identifiers" and becomes the declared identifier rule-set: keyed tail,
  ≥32 hex, known token shapes. Its job description shrinks to the rows
  above.
- Value-shape analysis no longer decides anything under a credential
  *key*: `treat()` consults the key's name, not the value's contents —
  the leak class N-E named (realistic values under a key) is closed by
  construction, not by one more heuristic.
- `redact()` keeps only the backstop tier: unkeyed runs, JWTs,
  provider prefixes, URL userinfo, and the keyed-text patterns inside
  free text (a `password=x` inside an error string has no schema key to
  classify — it is text, so it stays measured).
- The corpus header's residual statement narrows to the backstop tier —
  the honest class left is "exotic credential spellings in free text",
  a bounded, explainable miss rather than a recurring zero-day.

## Sequencing

Ahead of A4a. Lands behind the same discipline as round 4: corpus
split + role table red-first, then the `treat()` ordering change and
`FIELD_ROLES` coverage test, then the mutants; full gate; the exact
commit re-reviewed.
