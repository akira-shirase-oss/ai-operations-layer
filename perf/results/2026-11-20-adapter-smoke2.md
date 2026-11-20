# Durable Runner adapter + authority run: smoke2 (2026-11-20)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `3d618e3-dirty` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Stack | release builds; PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL); runner binary `.scratch/dr-src/target/release/durable-server` |
| Load average (1/5/15 min) before | 12.32 27.34 34.77 |
| … after | 9.59 25.51 33.89 |
| Load generator | k6 v2.3.0 on the same machine (constant arrival rate, two scenarios) |
| Offered load | 5 job submissions/s for 20s + 10 authority reads/s |
| Chain | k6 -> durable-server -> aiops-dr-adapter -> dr-article-worker; adapter -> collector |

## The run (`perf/k6/adapter.js`, exit 0)

Real `POST /jobs` submissions (4-step publish_article) polled to a terminal state
on the runner's own journal; concurrent read-token ops-list -> /authority walks.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| POST /jobs (runner admission) | 6.46 | 20.0 | n/a | 25.78 |
| run completion (submit -> journal done, s) | — | 165 | n/a | 167 |
| ops list | 2.13 | 4.44 | n/a | 19.05 |
| authority read (proxied to the runner) | 1.58 | 19.03 | n/a | 34.98 |

| Metric | Value |
|---|---|
| Jobs submitted | 100 |
| Runs reaching `done` | 100 |
| Runs failed / timed out | 0 |
| Ops lists that found ops | 201 |
| Authority reads answered | 201 |
| … truthful unavailable/not-linked answers | 0 |
| Checks passed (rate) | 1 |
| HTTP failures (rate) | 0 |
| Dropped iterations | 0 |

## What the stores hold afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 1010 |
| Operations stored (jobs + warm-up) | 101 |
| Operation links to the runner | 101 |
| Collector RSS after the run | unavailable (process tools denied here) |
| Adapter counters | `{"calls":404,"probes":0,"calls_unidentified":0,"runs_untracked_capacity":0,"calls_shed":0,"runs_tracked":0,"runs_finished":101,"runs_unknown_to_runner":0,"polls":101,"poll_errors":0}` |
| Collector counters | `{"batches":79,"events_accepted":1010,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":80,"wait_ns_total":7918,"wait_ns_max":333,"hold_ns_total":36170,"hold_ns_max":3250},"names_shared":{"acquisitions":79,"wait_ns_total":3999,"wait_ns_max":208,"hold_ns_total":111042,"hold_ns_max":2417},"names_exclusive":{"acquisitions":1,"wait_ns_total":167,"wait_ns_max":167,"hold_ns_total":6083,"hold_ns_max":6083},"ingest_key_cache":{"acquisitions":82,"wait_ns_total":18753,"wait_ns_max":750,"hold_ns_total":76708,"hold_ns_max":3250},"ingest_rate_limit":{"acquisitions":79,"wait_ns_total":12084,"wait_ns_max":458,"hold_ns_total":38459,"hold_ns_max":2250},"token_cache":{"acquisitions":413,"wait_ns_total":84128,"wait_ns_max":667,"hold_ns_total":505502,"hold_ns_max":4125},"query_rate_limit":{"acquisitions":406,"wait_ns_total":49915,"wait_ns_max":375,"hold_ns_total":148000,"hold_ns_max":1584},"project_cache":{"acquisitions":406,"wait_ns_total":42461,"wait_ns_max":375,"hold_ns_total":210595,"hold_ns_max":1459}}}` |

## First failures (verbatim — status 0 means the request never left k6)

```

```

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'p(95)<500' p(95)=19.02ms
✓ 'p(99)<2200' p(99)=25.98ms
✓ 'p(95)<300' p(95)=4.43ms
✓ 'p(99)<1000' p(99)=8.83ms
✓ 'p(95)<500' p(95)=20ms
✓ 'p(99)<2000' p(99)=25.3ms
✓ 'rate==0' rate=0.00%
✓ 'count>0' count=100
✓ 'p(99)<5000' p(99)=167ms
✓ 'count==0' count=0
✓ job accepted
✓ run reached done
✓ ops listed
✓ authority answered
```
