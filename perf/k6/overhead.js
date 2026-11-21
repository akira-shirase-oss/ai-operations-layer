// Instrumentation overhead (the evidence plan's proposed budget: under 5%
// median application latency — docs/END_GOAL.md). The same pilot workload —
// POST /articles: request -> generate -> validate -> fact-check -> publish —
// is driven concurrently at the same offered rate against two reference hosts
// that differ only in their SDK client:
//
//   bare         — Client::disabled(), the host's documented uninstrumented
//                  mode (no AIOPS_INGEST_KEY; identically otherwise)
//   instrumented — the live SDK emitting to the collector under test
//
// Concurrency puts both legs inside the same load window; whatever the
// collector's ingest work adds to the machine, both legs pay it — the delta
// stays honest. The per-leg medians leave this run inside the summary, and
// the cross-leg gate is stated here so the scenario carries its own
// threshold, enforced by perf/run-overhead.sh:
//
//   (med(instrumented) - med(bare)) / med(bare) < OVERHEAD_BUDGET_PCT   (5)
//
// The run also fails any leg that answers a non-201 or a dead connection —
// an overhead number taken against a broken harness is not a measurement.
// SDK drop counters are disclosed by the runner from GET /aiops/stats.
//
//   k6 run -e HOST_BARE=… -e HOST_ON=… -e RUN_ID=<8 hex> \
//          -e RATE=20 -e DURATION=300s perf/k6/overhead.js
import http from 'k6/http';
import exec from 'k6/execution';
import { check } from 'k6';
import { hex, note } from './ops.js';

const HOST_BARE = __ENV.HOST_BARE;
const HOST_ON = __ENV.HOST_ON;
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const RATE = Number(__ENV.RATE || 20);
const DURATION = __ENV.DURATION || '300s';

export const options = {
	// A VU's connection must not idle past the collector's 10 s header
	// bound, whose idle close races a reuse (D-035): connections retire at
	// each iteration.
	noVUConnectionReuse: true,
	scenarios: {
		bare: {
			executor: 'constant-arrival-rate',
			rate: RATE,
			timeUnit: '1s',
			duration: DURATION,
			preAllocatedVUs: Number(__ENV.PRE_VUS || 10),
			maxVUs: Number(__ENV.MAX_VUS || 60),
			exec: 'hitBare',
		},
		instrumented: {
			executor: 'constant-arrival-rate',
			rate: RATE,
			timeUnit: '1s',
			duration: DURATION,
			preAllocatedVUs: Number(__ENV.PRE_VUS || 10),
			maxVUs: Number(__ENV.MAX_VUS || 60),
			exec: 'hitInstrumented',
		},
	},
	thresholds: {
		checks: ['rate==1'],
		// A dead connection or a 5xx on either leg is a broken harness, not an
		// overhead result.
		http_req_failed: ['rate==0'],
		// Sanity band only — the cross-leg median gate lives in the runner.
		'http_req_duration{scenario:bare}': ['p(95)<1000'],
		'http_req_duration{scenario:instrumented}': ['p(95)<1000'],
	},
	summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

let n = 0;

function hit(url, leg) {
	n += 1;
	// A unique topic per request: every publish is a distinct effect, so no leg
	// gets a cheaper path through the publisher's dedup.
	const topic = `topic-${leg}-${RUN_ID}-${hex(exec.scenario.iterationInTest, 12)}-${hex(n, 8)}`;
	const res = http.post(`${url}/articles`, JSON.stringify({ topic }), {
		headers: { 'content-type': 'application/json' },
	});
	note(leg, res, [201]);
	check(res, { '201 published': (r) => r.status === 201 });
}

export function hitBare() {
	hit(HOST_BARE, 'bare');
}

export function hitInstrumented() {
	hit(HOST_ON, 'instrumented');
}
