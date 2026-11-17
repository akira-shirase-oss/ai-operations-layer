# Operating the AI Operations Layer

For the person who installs and runs it. The production path below is tested end
to end by `scripts/production-smoke.sh` (locally and in CI): release binaries,
configuration only from `deploy/aiops-server.env.example`, migrate, admin, the
console under its CSP, one host's operation read back, and a SIGTERM drain.

## What runs

| Piece | Binary | Listens | State |
|---|---|---|---|
| Collector, query API and console | `aiops-server serve` | `AIOPS_LISTEN` (loopback; a proxy in front) | PostgreSQL |
| Durable Runner adapter (optional) | `aiops-dr-adapter` | its config's `listen` | none (in memory; a restart leaves an unknown tail, D-027) |

PostgreSQL holds everything the layer knows: events, summaries, keys, tokens,
audit. Hosts keep their own state; the layer never needs it back.

## Install

```sh
export PATH="$(brew --prefix rustup)/bin:$PATH"     # macOS; elsewhere rustup's cargo
cargo build --release --locked -p aiops-server -p aiops-durable-runner
npm ci && npm run build --workspace @aiops/console
sudo install -D target/release/aiops-server target/release/aiops-dr-adapter -t /opt/aiops/bin
sudo cp -r apps/console/build /opt/aiops/console
sudo useradd --system --home /nonexistent aiops
sudo install -D -m 0640 -g aiops deploy/aiops-server.env.example /etc/aiops/aiops-server.env
sudo install -m 0644 deploy/aiops-server.service /etc/systemd/system/
```

Edit `/etc/aiops/aiops-server.env`: the database URL (a role that owns its
database), and nothing else unless a measurement says so. PostgreSQL 16 or later.

## First run

```sh
sudo systemctl enable --now aiops-server          # migrates, then serves
journalctl -u aiops-server -n 20                  # "collector listening"
curl -s http://127.0.0.1:57400/readyz             # {"ready":true,"store":"reachable"}
# Admin commands talk to the database directly (there is no admin HTTP surface, D-015):
set -a; . /etc/aiops/aiops-server.env; set +a
/opt/aiops/bin/aiops-server admin create-project pilot
/opt/aiops/bin/aiops-server admin create-token --actor you --project pilot   # for the console
```

Put TLS in front with `deploy/Caddyfile.example` (Caddy obtains certificates). Open
`https://<host>/console/` and sign in with the token. A token is shown once: keep it
in a password manager; it expires in 30 days by default.

## Connect a host

Mint one ingest key per host (`aiops-server admin create-ingest-key --project pilot
--name <host>`), then:

- **A Rust host**: link `aiops-sdk`, set `AIOPS_ENDPOINT` (the proxy's URL) and
  `AIOPS_INGEST_KEY`. `cargo run -p aiops-sdk --example first_operation` is a host in
  forty lines; its operation appears under request ID `example-1`.
- **Any language**: post the wire contract (`docs/INGEST_CONTRACT.md`).
- **Durable Runner** (needs its D-050, `9d240a7` or later):
  1. Copy `deploy/adapter.example.json` to `/etc/aiops/adapter.json`. List each
     handler as a route (its real URL as `upstream`), and set `runner.step_timeout_ms`
     to the runner's `--step-timeout-ms` exactly (a mismatch mislabels late calls).
  2. `/etc/aiops/aiops-dr-adapter.env`: `AIOPS_DR_ADAPTER_CONFIG=/etc/aiops/adapter.json`,
     `AIOPS_ENDPOINT`, `AIOPS_INGEST_KEY`, and `DURABLE_READ_KEY` (the runner's
     read-only key; required, since the adapter takes a run's outcome only from the
     runner). Install `deploy/aiops-dr-adapter.service` and start it.
  3. Register each route with the runner through the adapter:
     `--handler <route>=http://<adapter listen>/h/<route>`. Restart the runner.
  4. On the collector, set `AIOPS_AUTHORITY_DURABLE_RUNNER=<label>=<runner URL>` and
     `AIOPS_AUTHORITY_DURABLE_RUNNER_READ_KEY`, so the console shows the runner's own
     answer beside each run. Restart the collector.

  `GET http://<adapter>/stats` counts calls and runs. `calls_unidentified` means an
  older runner without the call headers; `runs_untracked_capacity` means the
  tracking bound was reached.

## Upgrade

Migrations are forward-only and run on start. To upgrade:

1. Back up (below).
2. Install the new binaries and console.
3. `systemctl restart aiops-server`, which drains the old process (up to its 10 s
   grace) and migrates.

The SDK keeps working while the collector restarts: it queues, then drops and
counts, and the gap shows in the console. Upgrade the adapter by restarting it.
Runs it was observing get an unknown tail, never a false finish.

## Back up and restore

Everything is in PostgreSQL:

```sh
pg_dump --format=custom --file=aiops-$(date +%F).dump "$AIOPS_DATABASE_URL"
# restore into an empty database, then point AIOPS_DATABASE_URL at it:
createdb aiops_restored && pg_restore --dbname=aiops_restored aiops-2026-11-17.dump
```

Raw events are kept 14 days and summaries 90. The audit log is kept 365 days
(D-016), so a backup older than that window restores history the live system has
already expired. Keys and tokens are stored hashed: a restore brings back exactly
the credentials that existed, never their secrets.

## Troubleshooting

| You see | It means | Do |
|---|---|---|
| `readyz` 503 `unreachable` | the collector cannot reach PostgreSQL | check the URL, the role, `pg_isready` |
| hosts get 503 `overloaded` | in-flight cap or body budget reached; the SDK retries | look at `GET /v1/collector/stats` (`shed`, `locks`); raise bounds only with a measurement |
| hosts get 429 | the key's rate limit | the SDK waits and retries; a key that is always limited needs a higher `AIOPS_INGEST_EVENTS_PER_SEC` |
| hosts get 401 | wrong or revoked key | mint a new key (revocation takes up to 30 s, D-015) |
| the console says "signed out" | token expired or revoked | mint a token |
| the console says "could not be reached" | the collector or proxy is down | `systemctl status aiops-server`, the proxy's log |
| operations show "no finish observed" | the host stopped or its events were lost | check the host; the gap rows say which instance, and how many are missing if known |
| the runner card says "not configured" / "unauthorized" | `AIOPS_AUTHORITY_DURABLE_RUNNER*` unset, or a wrong read key | set them; use the runner's read key, never its API key |
| the runner card says "link unverified" | the events name a run this operation is not derived from | the events were sent by something other than the adapter; check the ingest key that sent them (Sources) |

Logs are JSON on stdout (journald). They never contain keys, tokens or event payloads.
