# Control-surface run: freeport (2026-11-20)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `7e2ba48-dirty` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL), 16 ingest connections |
| Load average (1/5/15 min) before | 13.23 26.27 36.28 |
| … after | 11.18 24.94 35.57 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 5 control req/s for 15s + 2 ingest batches/s concurrently |
| Credentials | 5 admin tokens, 2 read tokens, a 3-id revoke pool |

## The mix (`perf/k6/control.js`, exit 0)

~40% audit page walk · ~30% recovery reads · ~15% credentials · ~10% audited denials · ~5% revokes.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| audit (page walk, filters) | 2.34 | 4.35 | 6.48 | 7.15 |
| controls (recovery reads) | 2.2 | 4.78 | 6.62 | 7.09 |
| credentials | 3.46 | 6.99 | 8.42 | 8.78 |
| denied (audited 403) | 2.03 | 2.51 | 2.53 | 2.54 |
| revoke (allowed→audited 404) | 4.01 | 7.39 | 8.14 | 8.32 |

| Metric | Value |
|---|---|
| Requests (total) | 186 |
| Control requests (rate achieved) | 3.88 |
| Rate-limited by the control bucket (429) | 1 |
| Audit pages walked | 27 |
| Checks passed (rate) | 1 |
| HTTP failures (rate, real errors only — expected 4xx incl. 429 excluded) | 0 |
| Dropped iterations | 0 |

## The invariant — ingest concurrent, never starved (scenario `ingest`)

| Metric | Value |
|---|---|
| Events sent | 310 |
| **Events lost** | 0 |
| Ingest p50 / p95 / p99 (ms) | 5.82 / 9.03 / 17.67 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 310 |
| Operations stored | 31 |
| Audit rows (the log the walk read) | 72 |
| … denials among them | 70 |
| Tokens revoked by the run | 2 |
| Collector RSS after the run | unavailable (process tools denied here) |
| Collector counters | `{"batches":31,"events_accepted":310,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":19,"denied":66,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":32,"wait_ns_total":2545,"wait_ns_max":209,"hold_ns_total":11417,"hold_ns_max":1125},"names_shared":{"acquisitions":31,"wait_ns_total":919,"wait_ns_max":83,"hold_ns_total":29536,"hold_ns_max":1625},"names_exclusive":{"acquisitions":1,"wait_ns_total":41,"wait_ns_max":41,"hold_ns_total":2625,"hold_ns_max":2625},"ingest_key_cache":{"acquisitions":34,"wait_ns_total":7130,"wait_ns_max":417,"hold_ns_total":23413,"hold_ns_max":1708},"ingest_rate_limit":{"acquisitions":31,"wait_ns_total":2247,"wait_ns_max":292,"hold_ns_total":8376,"hold_ns_max":500},"token_cache":{"acquisitions":182,"wait_ns_total":39047,"wait_ns_max":500,"hold_ns_total":138167,"hold_ns_max":2583},"query_rate_limit":{"acquisitions":1,"wait_ns_total":6917,"wait_ns_max":6917,"hold_ns_total":6000,"hold_ns_max":6000},"project_cache":{"acquisitions":27,"wait_ns_total":2912,"wait_ns_max":292,"hold_ns_total":25086,"hold_ns_max":10416}}}` |

## First failures (verbatim — status 0 means the request never left k6)

```

```

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'p(95)<150' p(95)=4.34ms
✓ 'p(99)<300' p(99)=6.47ms
✓ 'p(95)<150' p(95)=4.77ms
✓ 'p(99)<300' p(99)=6.62ms
✓ 'p(95)<200' p(95)=6.99ms
✓ 'p(99)<400' p(99)=8.42ms
✓ 'p(95)<200' p(95)=2.5ms
✓ 'p(99)<400' p(99)=2.53ms
✓ 'p(95)<250' p(95)=7.39ms
✓ 'p(99)<500' p(99)=8.13ms
✓ 'p(95)<100' p(95)=9.03ms
✓ 'p(99)<250' p(99)=17.67ms
✓ 'rate==0' rate=0.00%
✓ controls answered
✓ ingest 200
✓ every event stored
✓ audit page answered
✓ audit page is a page
✓ credentials answered
✓ denied answered
✓ revoke answered
```
