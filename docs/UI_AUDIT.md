# UI-for-every-operation audit (2026-11-19, A3/A4)

The workspace rule: every operation the system offers has a UI. This file walks the
whole surface — HTTP API, admin actions, automatic behavior — and records where each
is shown and which are deliberately not UI, with the decision that says why. D-034
(the D-015 amendment) governs credentials: minting stays CLI; everything is shown;
revocation is UI under `admin`, audited.

## HTTP API → console

| Operation | Scope | Console surface | Status |
|---|---|---|---|
| `GET /healthz`, `GET /readyz` | none | — | **Excluded**: machine probes for the process supervisor, not operator surface |
| `POST /v1/ingest` | ingest key | — | **Excluded**: a machine write path; its *effects* (dropped/refused events) ARE shown — per-source loss on the op page's Sources table and the collector counters |
| `GET /v1/projects` | read | `/` projects list | ✓ |
| `GET /v1/projects/{p}/operations` | read | `/p/{p}` operations list | ✓ |
| `GET /v1/projects/{p}/operations/{id}` | read | `/p/{p}/op/{id}` timeline | ✓ |
| `GET …/authority` | read | authority card on the op page | ✓ |
| `GET …/releases/compare` | read | `/p/{p}/releases` | ✓ |
| `POST …/reconcile` | operate | op page Recovery form → result in the records table | ✓ (this branch) |
| `POST …/retry` | operate | op page Recovery form → result in the records table | ✓ (this branch) |
| `GET …/controls` | read | op page Recovery records table | ✓ (this branch) |
| `GET …/controls/{id}` | read | same fields as the list rows render (the single-get is for API clients; the console's per-record row shows the same `ControlView`) | ✓ covered via list |
| `GET /v1/collector/stats` | admin | `/credentials` collector-counters card — drops, refusals, denials shown (gaps are data) | ✓ (this branch) |
| `GET /v1/admin/credentials` | admin | `/credentials` — projects, ingest keys, tokens | ✓ (this branch) |
| `GET /v1/admin/audit` | admin | `/audit` — filterable log, keyset "Older" paging; the read itself is not audited (a denial always is) | ✓ (this branch) |
| `POST …/tokens/{id}/revoke` | admin | `/credentials` confirm-then-revoke | ✓ (this branch) |
| `POST …/ingest-keys/{id}/revoke` | admin | `/credentials` confirm-then-revoke | ✓ (this branch) |

## Admin CLI → UI (D-034)

| Operation | Console | Status |
|---|---|---|
| `admin create-project` | projects table on `/credentials` shows every project | ✓ shown; **minting stays CLI** (a secret shown once in a terminal is a smaller surface than a browser) |
| `admin create-ingest-key` | ingest-keys table shows name, project, created, revoked state | ✓ shown; minting CLI-only |
| `admin create-token` | tokens table shows actor, scopes, projects, expiry, revoked state | ✓ shown; minting CLI-only |
| `admin revoke-ingest-key` | confirm-then-revoke on `/credentials`, audited `admin.revoke_ingest_key` | ✓ (this branch) |
| `admin revoke-token` | confirm-then-revoke on `/credentials`, audited `admin.revoke_token` | ✓ (this branch) |
| `admin set-opt-in` | `opt_in_attribute_keys` column on `/credentials` | ✓ shown; **toggling stays CLI** (a policy change, not an incident action — coordinator's ruling) |

## Automatic / lifecycle behavior

| Behavior | Where it's visible |
|---|---|
| Retention expiry | `expired` counts in op coverage + the expired-notice on the timeline |
| Collector downtime | gap records on the timeline (kind `unknown_tail`, `expired`, …) |
| Control executor re-drive | `attempts` counts and the `submitted`/`confirmed` states on control records |
| Revocation taking effect | the token/key's `revoked` state; the cache's remaining minutes are noted in the page preamble |

## Honest gaps and residual work

- **Playwright/Chromium proof**: the audit's walkthrough screenshots (390 px and
  1440 px, light and dark, per the workspace rule) need a browser — sandboxed out
  here. `scripts/console-e2e.sh` runs them on a workstation; the data-testids added
  this branch (`control-form`, `control-note`, `control-record`, `ingest-key`,
  `token`, `collector-stats`, `revoke-note`, `audit-row`, `audit-empty`) are the
  anchors.
- **Denial visibility**: `GET /v1/admin/audit` + the `/audit` page closed this —
  every denial is now readable under `admin` (filters on action, actor, token id
  and time window; keyset pages stay stable while rows arrive). The read is not
  itself audited — a read that grew the log would be a write in disguise — while
  a denied read is.
- The op page's Recovery form uses the `published` assertion shape both adapters
  validate in v1; new adapters with different `expected_state` schemas will need
  their own form inputs (recorded in `docs/modules/control-recovery.md`).
