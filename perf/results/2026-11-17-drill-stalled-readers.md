# Drill: clients that never read (review N3) — 2026-11-17

- Commit: `fbba0de`
- Machine: Apple M2 Pro, 12 CPUs
- Load average before / after: 13.74 19.03 24.90 / 13.74 19.03 24.90
- Shape: release collector on 127.0.0.1:57413 with `AIOPS_MAX_CONNECTIONS=8`; 8 clients with 4 KiB receive buffers each pipeline 4,000 `GET /healthz` and never read; then a fresh `GET /healthz`

```
fresh_healthz_answered_after_s=0.0
```

**PASS** — pass means the fresh `/healthz` is answered within 20 s (the 10 s write-stall
deadline cuts the non-reading clients and frees their slots).

Here the request cap freed the slots: each stalled client's first 1,000 small responses fit in the
socket buffers, the 1,000th carried `Connection: close`, and the rest of its pipeline was dropped.
The write-stall deadline (a response too large for the buffers) is covered by
`a_client_that_never_reads_is_cut_and_frees_its_slot` in `crates/aiops-server/tests/serve.rs`.

## Before the fix

The same drill on `86f301f` (the commit before), load 13 / 14:

```
fresh_healthz_answered_after_s=none in 60
```

**FAIL** — the eight non-reading clients held all eight slots; the fresh client waited in the
backlog for the whole minute.
