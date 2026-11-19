# Second-developer review — A4 (2026-11-19)

A fresh walk of the repository as a new contributor: README → quick start →
architecture → one change end to end → the error surface. Findings are grouped
by whether this branch fixed them or listed them. Every claim is cited.

## The walk, honestly

- `README.md` → `docs/OPERATING.md` → `docs/ARCHITECTURE.md` is a coherent
  on-ramp: what it is, how to run it, where truth lives. The quick start
  (`README.md:57-68`) works as written: `stack.sh up`, `check.sh`, the admin
  trio, `AIOPS_CONSOLE_DIR=apps/console/build … serve`, console at `/console/`.
  `create-token` without `--scope` mints a `read` token — enough to sign in,
  not enough to use the Recovery form (`operate`); the README doesn't say so.
- One change end to end (e.g. adding a control action): `aiops-domain` types →
  `aiops-store` row + migration → `aiops-server` adapter impl + endpoint +
  audit → console form. The seams are where ARCHITECTURE says they are.
- `AGENTS.md` carries the toolchain trap (Homebrew cargo ignores
  `rust-toolchain.toml`) — a human contributor who never opens the
  agent-instructions file hits it cold; README:58 gives the same `PATH` export,
  so this is covered for shell users.

## Stale claims — fixed in this branch

1. **`docs/ROADMAP.md:16` marks A3 "Planned"** — the control path shipped:
   records + audit in Postgres, reconcile/retry under actor scope + expected
   state + business key, refhost and Durable Runner adapters, console control
   surface, crash-window re-drive — proven live
   (`perf/results/2026-11-19-drill-lost-publish.md`) and in
   `crates/aiops-server/tests/control.rs` (13 tests). Updated to Done.
2. **`README.md:3-20` predates A3 entirely** — the status block stops at
   "A2 partly" and "Decisions D-012–D-028". Updated: A3 recorded, decision
   range corrected to D-012–D-034.
3. **`docs/modules/control-recovery.md:3` says "proposal, not yet built"** —
   it is built and drilled. Status line corrected.
4. **The control surface is invisible to operators** — `docs/OPERATING.md`
   and `deploy/aiops-server.env.example` never mention `AIOPS_CONTROL_*`,
   so a production install cannot enable controlled recovery by the docs.
   The connection-hardening vars (`AIOPS_WRITE_STALL_TIMEOUT_SECS`,
   `AIOPS_HEADER_READ_TIMEOUT_SECS`, `AIOPS_MAX_REQUESTS_PER_CONNECTION`,
   `AIOPS_CONNECTION_LIFETIME_SECS`) were also missing from the env example.
   Both added.
5. **Untested control-path validation** — the 400 arms at
   `control.rs:572,575,591-592,595` (business-key size, `expected_state`
   size, adapter key shape, operation-id hex) and `795-797` (control-id
   uuid) had no coverage; `tests/control.rs` exercised only valid ids.
   One test added covering all of them.

## Stale claims that remain (listed, with an order)

1. **README:15-18 "a quiet-machine 1M run is owed"** — still true and should
   stay until the CI result lands; the claim is honest, just check it survives
   the merge of the dispatched run (workflow 44544883621).
2. **OQ8 stays open** (`ROADMAP.md:38-41`, DRILL.md:115-120): the runner's own
   state names the failure, so the registered comparison cannot pass on this
   catalog. Must be answered before any *counted* drill run — by design, not
   a bug.
3. **A2's second-developer gate** (`ROADMAP.md:15` tail): this review is the
   closest thing to it so far; a performer who did not seed incidents is still
   owed for the counted drill (D-017). People risk, not code.
4. **Docs site + changelog** — built by a separate worker in
   `planned/ai-operations-layer-docs`; not duplicated here.
5. **`Store::recent_audit`** (`lib.rs:1124`) — production API with only test
   callers since `audit_page` landed. Keep (tests lean on it) or move under
   `#[cfg(test)]` in a later pass; listed, not changed.
6. **`controls.list` truncates at 100 with no cursor**
   (`lib.rs:1287`): an operation with >100 control records shows only the
   newest 100 and offers no next page. Acceptable today (records are few per
   operation); the audit endpoint already has keyset paging if this ever bites.
7. **`ApiError::AuditUnavailable`** (control.rs:474, credentials.rs:149,
   query.rs:83): fail-closed when a *denial* can't be audited — untested
   because it needs store-failure injection mid-request. The right fix is a
   `sqlx::test`-level fault hook, not a mock; listed for the next pass.
8. **No CONTRIBUTING.md** — `AGENTS.md` doubles as it; README doesn't link
   either file. A one-line pointer in the README's docs list would help humans.
9. **Console browser coverage for `/audit`** — testids are the anchors
   (`audit-row`, `audit-empty`); Chromium is sandboxed out here, so the page
   has no spec yet (UI_AUDIT.md records this).

## Unaudited and unbounded spots — the audit

| Surface | Verdict |
|---|---|
| `GET /v1/admin/audit` | allowed reads unaudited **by design** (a read that grew the log would be a write); denials audited. Documented in code + UI_AUDIT. |
| `GET /v1/collector/stats` | `admin` scope, denials audited (query.rs:492-502) |
| `GET /v1/admin/credentials*` | `admin` scope via `authorize_admin`, denials audited |
| Control endpoints | `operate`/`read` scope under the control bucket; decisions + denials audited in the same transaction |
| Unauthenticated requests (any surface) | rejected at `authenticate` before an actor exists — nothing to audit; standard |
| Adapter `GET /stats` | unauthenticated, metadata-only; binds loopback per `deploy/adapter.example.json` — acceptable |
| `GET /v1/projects/…/controls` | `LIMIT 100`, no cursor — finding 6 above |
| All list/read endpoints | `limit` clamped (ops ≤ 200, audit ≤ 100); `ident_param`/`time_param` bound every filter; bodies bounded by the ingest contract |

Nothing new was found where telemetry could authorize an action or a read
could grow the audit log.

## Verification this branch

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked
-- -D warnings`, `scripts/check-isolated.sh`, and the touched test files run
through `heavy.sh` — results in the commit message footers.
