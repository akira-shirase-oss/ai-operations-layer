# Durable Runner adapter + authority run: smoke (2026-11-20)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `3d618e3-dirty` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Stack | release builds; PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL); durable-server durable-server |
| Load average (1/5/15 min) before | 29.22 51.86 43.75 |
| … after | 18.93 46.45 42.14 |
| Load generator | k6 v2.3.0 on the same machine (constant arrival rate, two scenarios) |
| Offered load | 5 job submissions/s for 30s + 10 authority reads/s |
| Chain | k6 -> durable-server -> aiops-dr-adapter -> dr-article-worker; adapter -> collector |

## The run (`perf/k6/adapter.js`, exit 0)

Real `POST /jobs` submissions (4-step publish_article) polled to a terminal state
on the runner's own journal; concurrent read-token ops-list -> /authority walks.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| POST /jobs (runner admission) | 5.53 | 18.56 | n/a | 31.09 |
| run completion (submit -> journal done, s) | — | 167 | n/a | 185 |
| ops list | 3.0 | 6.03 | n/a | 9.0 |
| authority read (proxied to the runner) | 1.85 | 13.47 | n/a | 29.38 |

| Metric | Value |
|---|---|
| Jobs submitted | 150 |
| Runs reaching `done` | 150 |
| Runs failed / timed out | 0 |
| Ops lists that found ops | 301 |
| Authority reads answered | 301 |
| … truthful unavailable/not-linked answers | 0 |
| Checks passed (rate) | 1 |
| HTTP failures (rate) | 0 |
| Dropped iterations | 0 |

## What the stores hold afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 1510 |
| Operations stored (jobs + warm-up) | 151 |
| Operation links to the runner | 151 |
| Collector RSS after the run | unavailable (process tools denied here) |
| Adapter counters | `{"calls":604,"probes":0,"calls_unidentified":0,"runs_untracked_capacity":0,"calls_shed":0,"runs_tracked":0,"runs_finished":151,"runs_unknown_to_runner":0,"polls":151,"poll_errors":0}` |
| Collector counters | `{"batches":123,"events_accepted":1510,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":124,"wait_ns_total":10283,"wait_ns_max":292,"hold_ns_total":62003,"hold_ns_max":1125},"names_shared":{"acquisitions":123,"wait_ns_total":9751,"wait_ns_max":292,"hold_ns_total":196079,"hold_ns_max":4500},"names_exclusive":{"acquisitions":1,"wait_ns_total":0,"wait_ns_max":0,"hold_ns_total":3084,"hold_ns_max":3084},"ingest_key_cache":{"acquisitions":128,"wait_ns_total":28750,"wait_ns_max":4291,"hold_ns_total":135331,"hold_ns_max":5375},"ingest_rate_limit":{"acquisitions":123,"wait_ns_total":18208,"wait_ns_max":3583,"hold_ns_total":51627,"hold_ns_max":3709},"token_cache":{"acquisitions":616,"wait_ns_total":116793,"wait_ns_max":584,"hold_ns_total":716994,"hold_ns_max":8208},"query_rate_limit":{"acquisitions":605,"wait_ns_total":58414,"wait_ns_max":375,"hold_ns_total":240294,"hold_ns_max":16542},"project_cache":{"acquisitions":605,"wait_ns_total":43913,"wait_ns_max":1583,"hold_ns_total":310379,"hold_ns_max":1292}}}` |

## First failures (verbatim — status 0 means the request never left k6)

```

```

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'p(95)<500' p(95)=13.47ms
✓ 'p(99)<2200' p(99)=22.02ms
✓ 'p(95)<300' p(95)=6.03ms
✓ 'p(99)<1000' p(99)=7.13ms
✓ 'p(95)<500' p(95)=18.56ms
✓ 'p(99)<2000' p(99)=26.33ms
✓ 'rate==0' rate=0.00%
✓ 'count>0' count=150
✓ 'p(99)<5000' p(99)=170.51ms
✓ 'count==0' count=0
✓ job accepted
✓ run reached done
✓ ops listed
✓ authority answered
```
