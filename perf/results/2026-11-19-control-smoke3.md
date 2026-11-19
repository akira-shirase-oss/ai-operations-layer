# Control-surface run: smoke3 (2026-11-19)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `918a371-dirty` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL), 16 ingest connections |
| Load average (1/5/15 min) before | 11.61 11.22 10.38 |
| … after | 10.58 11.02 10.34 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 20 control req/s for 30s + 20 ingest batches/s concurrently |
| Credentials | 9 admin tokens, 3 read tokens, a 12-id revoke pool |

## The mix (`perf/k6/control.js`, exit 0)

~40% audit page walk · ~30% recovery reads · ~15% credentials · ~10% audited denials · ~5% revokes.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| audit (page walk, filters) | 2.39 | 5.37 | 7.61 | 29.12 |
| controls (recovery reads) | 1.96 | 4.47 | 5.42 | 6.0 |
| credentials | 5.19 | 10.02 | 11.6 | 13.03 |
| denied (audited 403) | 2.57 | 5.76 | 10.84 | 12.69 |
| revoke (allowed→audited 404) | 4.62 | 6.84 | 7.04 | 7.05 |

| Metric | Value |
|---|---|
| Requests (total) | 1281 |
| Control requests (rate achieved) | 17.53 |
| Rate-limited by the control bucket (429) | 0 |
| Audit pages walked | 240 |
| Checks passed (rate) | 1 |
| HTTP failures (rate, real errors only — expected 4xx incl. 429 excluded) | 0 |
| Dropped iterations | 0 |

## The invariant — ingest concurrent, never starved (scenario `ingest`)

| Metric | Value |
|---|---|
| Events sent | 6000 |
| **Events lost** | 0 |
| Ingest p50 / p95 / p99 (ms) | 5.17 / 9.03 / 13.22 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 6000 |
| Operations stored | 600 |
| Audit rows (the log the walk read) | 185 |
| … denials among them | 181 |
| Tokens revoked by the run | 4 |
| Collector RSS after the run | unavailable (process tools denied here) |
| Collector counters | `{"batches":600,"events_accepted":6000,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":151,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":601,"wait_ns_total":40616,"wait_ns_max":292,"hold_ns_total":258673,"hold_ns_max":9083},"names_shared":{"acquisitions":600,"wait_ns_total":23967,"wait_ns_max":208,"hold_ns_total":591742,"hold_ns_max":9208},"names_exclusive":{"acquisitions":1,"wait_ns_total":0,"wait_ns_max":0,"hold_ns_total":3542,"hold_ns_max":3542},"ingest_key_cache":{"acquisitions":603,"wait_ns_total":102079,"wait_ns_max":667,"hold_ns_total":407595,"hold_ns_max":11750},"ingest_rate_limit":{"acquisitions":600,"wait_ns_total":37793,"wait_ns_max":1959,"hold_ns_total":166658,"hold_ns_max":917},"token_cache":{"acquisitions":729,"wait_ns_total":168705,"wait_ns_max":709,"hold_ns_total":786137,"hold_ns_max":19792},"query_rate_limit":{"acquisitions":1,"wait_ns_total":166,"wait_ns_max":166,"hold_ns_total":1042,"hold_ns_max":1042},"project_cache":{"acquisitions":175,"wait_ns_total":12915,"wait_ns_max":541,"hold_ns_total":95084,"hold_ns_max":1417}}}` |

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'p(95)<150' p(95)=5.37ms
✓ 'p(99)<300' p(99)=7.61ms
✓ 'p(95)<150' p(95)=4.47ms
✓ 'p(99)<300' p(99)=5.41ms
✓ 'p(95)<200' p(95)=10.02ms
✓ 'p(99)<400' p(99)=11.6ms
✓ 'p(95)<200' p(95)=5.76ms
✓ 'p(99)<400' p(99)=10.84ms
✓ 'p(95)<250' p(95)=6.83ms
✓ 'p(99)<500' p(99)=7.03ms
✓ 'p(95)<100' p(95)=9.02ms
✓ 'p(99)<250' p(99)=13.22ms
✓ 'rate==0' rate=0.00%
✓ revoke answered
✓ ingest 200
✓ every event stored
✓ controls answered
✓ audit page answered
✓ audit page is a page
✓ denied answered
✓ credentials answered
```
