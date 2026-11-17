# Ingestion contract — schema v1

The language-neutral wire contract (D-013). Any host can implement it over
HTTP/JSON without an SDK; `crates/aiops-sdk` is the Rust implementation. The
source of truth for every rule below is `crates/aiops-domain` (`wire.rs`,
`validate.rs`, `limits.rs`, `policy.rs`); this file must change with it.

**Compatibility rule.** A collector accepts exactly the schema versions it
lists (v1 today) and refuses unknown fields, so a new field is a new version.
New versions are added beside old ones, never in place of them.

## The request

```
POST /v1/ingest
Authorization: Bearer aiops_ik_<43 base64url characters>
Content-Type: application/json
```

The ingest key names the project; it can only call this route. Keys are
server-side secrets: never ship one to a browser.

**Authentication happens before the request takes ingest capacity.** A key
is checked for shape, then against the collector's cache; only a cache miss
queries the database, and at most 4 such lookups run at once (on their own database connections) — beyond that
an unknown key is answered `503 overloaded` without a query, so a flood of
made-up keys cannot take a known host's capacity. Valid and invalid keys
are cached apart and evicted oldest first. **Revocation takes effect within
30 s** (the cache's lifetime for a valid key); while the lookup budget is
exhausted, a key that was valid in the last 5 minutes is still served from
the cache, so revocation can then take up to 5 minutes 30 seconds.

```json
{
  "schema_version": 1,
  "source": { "instance_id": "<32 hex>", "service": "reference-host", "sdk": "aiops-sdk-rust/0.1.0" },
  "stats": { "dropped_queue_full": 0, "dropped_send_failed": 0, "dropped_rejected": 0, "gap_overflow": 0, "truncated_events": 0 },
  "events": [ { … }, … ]
}
```

- `source.instance_id`: random per process start (128-bit, lowercase hex,
  not all zero). `service`, `sdk`: identifiers (below). An instance belongs
  to the first ingest key that used it: a batch from another key under the
  same instance is refused whole with `409 instance_owned_by_another_key`,
  so one host cannot fill another's gaps or change its counters.
- `stats`: the sender's **cumulative** counters since its process started.
  Sent with every batch, so the latest values survive lost batches.

## Events

Every event carries:

| Field | Rule |
|---|---|
| `event_id` | 32 lowercase hex, not all zero; unique per project; **the same on every re-delivery** (the dedup key) |
| `type` | `operation.started`, `operation.finished`, `step.started`, `step.finished`, `gap.reported` |
| `operation_id` | 32 lowercase hex (W3C trace-ID width) |
| `seq` | ≥ 1, per source instance, in emission order |
| `op_seq` | ≥ 1, per operation within the instance, in emission order; `0` only on `gap.reported` |
| `event_time` | RFC 3339 with an offset; within 1 hour ahead of and 14 days behind the collector's receive time |
| `attributes` | optional map, ≤ 32 entries; keys `[a-z0-9._]{1,64}`; values bool, integer, finite number or string ≤ 256 bytes |
| `truncated` | optional: how many string fields the sender cut to the caps |

Per type, only the listed fields may appear (anything else is
`field_not_allowed`):

| Type | Required | Optional |
|---|---|---|
| `operation.started` | `name` | `trace_id`, `request_id`, `release`, `deployment` |
| `operation.finished` | `outcome`, `events_emitted` (≥ `op_seq`) | `failure_class` / `uncertain_reason`, `error_code`, `error_message` |
| `step.started` | `step_id` (16 hex), `kind`, `name`, `attempt` (1–1000) | `parent_step_id`, `model`, `prompt_version`, `config_version`, `external_effect` (bool) |
| `step.finished` | `step_id`, `attempt`, `outcome` | `kind`, `name`, `failure_class` / `uncertain_reason`, `error_code`, `error_message`, `usage`, `cost_estimate` |
| `gap.reported` | `dropped` (≥ 1), `reason`, `first_dropped_at`, `last_dropped_at` | — (no attributes) |

- `kind`: `model`, `tool`, `job`, `validation`.
- **Outcomes are declared by the host, never inferred** (D-020):
  `succeeded` (no class, no reason); `failed` + `failure_class` —
  `transport` (the request provably never reached the remote), `rejected`
  (the remote answered with a definitive error), `validation`, `internal`;
  `uncertain` + `uncertain_reason` — `timeout_after_send`,
  `connection_lost_after_send`, `response_unreadable` (the request left the
  host; the effect may have happened).
- `gap.reported` `reason`: `queue_full`, `send_failed`, `rejected`,
  `shutdown`.
- `usage`: `{input_tokens, output_tokens}` ≤ 10⁹ each.
- `cost_estimate`: `{micros, currency (ISO 4217, 3 uppercase), basis}` — an
  estimate, never a bill (invariant 7).
- Identifiers (`name`, `release`, `deployment`, `model`, `prompt_version`,
  `config_version`, `error_code`, `request_id`, `basis`, `source.service`,
  `source.sdk`): `[A-Za-z0-9._:/@-]`, non-empty, ≤ 128 bytes (`error_code`
  ≤ 64). No spaces or markup, so they are safe to index and to render.
- `error_message`: free text ≤ 512 bytes; secret-shaped substrings are
  replaced with `[redacted]` and counted.
- One event's JSON ≤ 8 KiB.

## Limits

| Bound | Value | Answer when exceeded |
|---|---|---|
| Body | 1 MiB | 413 `payload_too_large` |
| Events per batch | 500 | 413 `too_many_events` |
| Uncached key lookups in flight | 4 by default (own pool of 4 connections) | 503 `overloaded`, `Retry-After: 1` (no database query) |
| Ingest body bytes in flight | 32 MiB by default (`AIOPS_INGEST_BODY_BUDGET_MIB`); a body is read only after its key is authenticated from the headers, and is charged its `Content-Length` (the 1 MiB cap if none) | 503 `overloaded`, before any byte of the body is read |
| Ingest requests in flight (whole collector, after authentication) | 64 by default | 503 `overloaded`, `Retry-After: 1` |
| Events per second per key | 20,000 sustained, 100,000 burst by default | 429 `rate_limited`, `Retry-After: <s>` |
| Distinct names per project (`op:`, `step:`, `model:`, `release:`, …), of which per ingest key | 2,000, 500 | the name is stored as `_overflow` (counted in `overflowed_names`, labeled on the event); the event is kept. Names unused for 90 days are swept and free their budget |
| Store transaction | 5 s | 503 `store_unavailable` (the batch may have committed; re-send — it is idempotent) |
| Whole request | 10 s | 503 `timeout` |

## The answer

`200` when the envelope was accepted, even if some events were refused:

```json
{ "accepted": 7, "duplicates": 1, "withheld_attributes": 2, "redactions": 0,
  "refused": [ { "index": 3, "event_id": "…", "code": "invalid_outcome", "field": "outcome" } ] }
```

- `duplicates`: already stored with identical content and provenance
  (merged). A re-used `event_id` with different content, instance or key is
  refused as `conflicting_duplicate`; the first write wins.
- `withheld_attributes`: attributes not in the default allowlist or the
  project's opt-in list — neither key nor value is stored (D-006).
- `overflowed_names`: names over the project's or the key's budget, stored
  as `_overflow`.
- Refusal codes (stable): `malformed`, `event_too_large`,
  `unknown_event_type`, `invalid_id`, `missing_field`, `field_not_allowed`,
  `invalid_value`, `too_long`, `invalid_time`, `event_time_out_of_range`,
  `invalid_sequence`, `invalid_outcome`, `too_many_attributes`,
  `invalid_attribute`, `conflicting_duplicate` (`cardinality_exceeded` is no
  longer used: an over-budget name is stored as `_overflow` instead).
- Free text (`error_message`, string attribute values) must not contain
  U+0000, which the store cannot keep: such an event is refused alone as
  `invalid_value` on that field; the rest of the batch is stored. The Rust
  SDK replaces U+0000 with U+FFFD before sending.

Errors use one envelope:
`{"error": {"code": "…", "message": "…", "request_id": "…"}}` — `400`
`malformed_batch`, `unsupported_schema_version`, `invalid_source`,
`unstorable_text` (the store could not keep one event's text; nothing was
written; split the batch and resend the halves); `401`
`unauthenticated`; `409` `instance_owned_by_another_key`; `413`; `429`;
`503`.

**What a sender should retry:** 429 (after `Retry-After`), 503, and
connection errors — with the same events (same `event_id`s). Never retry
400 or 401 unchanged. Split and resend on `413` and on `400
unstorable_text`: they are caused by the batch's size or by one event, and
halving isolates it (the Rust SDK does this down to single events).

## Default attribute allowlist (v1)

`business_key`, `cache.hit`, `content.kind`, `content.length`, `error.type`,
`http.method`, `http.route`, `http.status_code`, `provider`,
`provider.region`, `queue.name`, `retry.reason`, `sampled`, `tool.name`,
`validation.rule`. Anything else needs a per-project opt-in
(`aiops-server admin set-opt-in`) and is labeled *opt-in* in the console.

## Context propagation

The SDK uses the operation ID as the W3C trace ID and step IDs as span IDs,
and reads/writes `traceparent` version `00`
(`00-<32 hex>-<16 hex>-<2 hex flags>`), so an operation correlates with
OpenTelemetry traces (D-003).
