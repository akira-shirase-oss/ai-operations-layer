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
| `identifier` | Bounded charset `[A-Za-z0-9._:/@-]` + `looks_like_secret` — the CLOSED refuse check: a credential-keyed `k:v` tail outside the state-word allowlist, a bare ≥32-hex run (allowed ONLY in `release` and `request_id` — the two fields declared to carry hex), or a known token shape refuses by name. Stored verbatim otherwise (identifiers are never rewritten — `[redacted]` would corrupt what the caller reads back). | `name`, `release`, `deployment`, `request_id`, `error_code`, `model`, `prompt_version`, `config_version`, `source.service`, `source.sdk`, `cost_estimate.basis` |
| `key` | Attribute keys: `[a-z0-9._]` + `Policy::treat` — a `CREDENTIAL_KEYS` name is **always Withheld** (an opt-in cannot whitelist a credential — new rule); then the default allowlist, then opt-in; anything else is Withheld — neither key nor value stored. | `attributes` keys |
| `keyed_value` | An attribute value lives or dies by its key's treatment: Withheld and credential-named keys store nothing, *whatever the value's shape* (`hunter2` under `attributes.password` dies by rule). Stored string values then pass the free-text backstop as defense in depth. | `attributes` values |
| `free_text` | `redact()` — the heuristic scanner. Inside it, `name<sep>value` is an **allowlist** (round 7): only a benign name keeps its value. | `error_message` |

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

## Free text is an allowlist (round 7)

Seven rounds of credential-word lists each opened a new hole (round 7
N1: `backup_code`, `security_code`, `device_code`, `login_code`,
`access_code`, `oauth_code`, `one_time_code`, `2fa_code` all slipped a
seven-name qualifier list, and free-text `verification code: 482913`
with them). The keep is now the closed side: in `free_text`, every
`name<sep>value` segment keeps its value **only if the name is on the
benign list** — every other name loses its value, whatever the value's
shape. A name containing a benign word is a different name:
`the status code: 503` is not `status code`.

### The benign free-text list — and why each entry earns it

Single words (`BENIGN_WORDS`), each the generic operational vocabulary
an error message carries with no credential interpretation:

- `status`, `state`, `reason`, `error` — outcome words.
- `method`, `route`, `path`, `url` — request vocabulary.
- `host`, `port` — endpoint vocabulary.
- `attempt`, `retry`, `count`, `timeout`, `duration` — scheduling words.
- `id`, `name`, `job`, `message`, `user` — entity words.
- `key` — a lone `key=x` is a map lookup (`cache key=article:42`), not a
  credential name; any *compound* `*key` name is a different name and
  dies.
- `sha256` — digest labels keep digests (`sha256: <hex>` is a content
  identifier, not a credential).

Multi-word names (`NON_CREDENTIAL_NAMES` ∪ `BENIGN_TEXT_NAMES`), each
declared non-credential for the identifier roles too:

- `status code`, `http status code`, `error code`, `exit code`,
  `reason code`, `result code` — outcome codes, not credentials.
- `zip code`, `area code`, `postal code`, `country code`,
  `currency code`, `iso code` — data codes.
- `business key`, `cache key`, `foreign key`, `idempotency key`,
  `partition key`, `primary key`, `public key`, `sort key` — data-model
  and idempotence vocabulary.
- `aws s3 key` (`aws.s3.key`), `messaging kafka message key` — real OTel
  attribute names for an object key and a record key (round 7 N6).
- `bypass`, `compass` — real words that end in `pass` and name no secret
  (round 7 N6).
- `duration ms` — the elapsed-time label.

Bare `code` is **not** benign in free text — `code: 482913` is a PIN
shape too often. Every `*code` compound is a credential unless listed
above. Every `*key` compound is a credential unless listed above. Every
word ending in `pass` is a credential unless listed. The list is CLOSED:
adding a name is a policy change, and mutant
`benign-list-admits-credential` proves the suite notices (it re-admits
`backup_code` and must go red).

### Escapes: decodable or dead (round 7, N2)

`%XX` is the only encoding the scanner decodes (bounded passes). Any
other escape form — `%u`, `&#…;`, `&name;`, `\x`, `\u`, NUL, a literal
`\`, or residue a decode pass produces (`%25u` → `%u`) — is
undecodable-by-contract, so the **whole field** dies as `[redacted]`,
name and all. Span-level work once stored `password[redacted]hunter2`
beside a marker; the whole-field wipe removes the ambiguity class
entirely. A `<scheme>://` URL is exempt only for a listed scheme —
`access_token://x` is a credential in scheme's clothing.

### Identifier roles and the same closed list

`credential_key` consults `NON_CREDENTIAL_NAMES` first — the same list
— so `aws.s3.key`, `messaging.kafka.message.key`, `bypass` and `compass`
opt in, store as attributes and keep as `source.service`-shaped
identifiers, while `backup_code`, `one_time_code`, `stripe_key`,
`pgpass` and every unlisted `*code`/`*key`/`*pass` refuse opt-in at the
CLI, withhold as attributes even behind the CLI's back, and refuse as
identifiers (never rewritten — a refused identifier is a `400`, not a
`[redacted]` that would corrupt what the caller reads back).

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
- **the benign list admits nothing new** — mutant
  `benign-list-admits-credential` (a credential name appended to the
  allowlist) must red on `non_benign_names_and_bad_escapes_lose_the_field`.
- **undecodable escapes wipe the field** — mutant `escape-forms-pass`
  (`undecodable_escape` answers `false`) must red on the same test.
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
