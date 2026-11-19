// Control-path load (A3, D-029..D-033): reconcile calls and control-record reads
// against random ingested operations, on the dedicated control bucket (5 rps / burst
// 10 per token) and the isolated control pool. Run after an ingest run so the
// operation ids resolve; the project's adapter must be configured for the host the
// run targets (e.g. AIOPS_CONTROL_REFERENCE_HOST=perf=http://127.0.0.1:53331).
//
// Reconcile is replay-safe: a repeated call returns the existing record — the load
// is therefore honest about the audit-write cost (one decision transaction per
// first call, replays after that).
//
//   k6 run -e BASE_URL=… -e TOKEN=<an operate+read token> -e RUN_ID=<same as ingest> \
//          -e OPERATIONS=<ingested> perf/k6/control.js
import http from 'k6/http';
import { check } from 'k6';

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:57410';
const TOKEN = __ENV.TOKEN;
const PROJECT = __ENV.PROJECT || 'perf';
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const OPERATIONS = Number(__ENV.OPERATIONS || 1);

export const options = {
	scenarios: {
		control: {
			executor: 'constant-arrival-rate',
			// Under the per-token control bucket (CONTROL_RATE = 5/s): a green run is
			// what the surface sustains inside its own limit — honest capacity, not an
			// unlimited one. The rate limiter itself is covered by the API tests.
			rate: Number(__ENV.RATE || 4),
			timeUnit: '1s',
			duration: __ENV.DURATION || '30s',
			preAllocatedVUs: 8,
			maxVUs: 32,
		},
	},
	thresholds: {
		'http_req_duration{name:reconcile}': ['p(95)<250'],
		'http_req_duration{name:controls}': ['p(95)<150'],
		http_req_failed: ['rate==0'],
		checks: ['rate==1'],
	},
	summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

function hex(n, width) {
	return n.toString(16).padStart(width, '0').slice(-width);
}

export default function () {
	const headers = {
		authorization: `Bearer ${TOKEN}`,
		'content-type': 'application/json',
	};
	const n = Math.floor(Math.random() * OPERATIONS);
	const op = RUN_ID + hex(n, 24);
	const key = `article:load-${RUN_ID}-${n}`;
	const base = `${BASE_URL}/v1/projects/${PROJECT}/operations/${op}`;

	// A reconcile under a business key that matches the adapter's shape; whatever the
	// authority answers is recorded — reconciled (any resolution) or denied are the
	// honest outcomes, never a guess.
	const rec = http.post(
		`${base}/reconcile`,
		JSON.stringify({ business_key: key, expected_state: { published: true } }),
		{ headers, tags: { name: 'reconcile' } },
	);
	check(rec, {
		'reconcile answered': (r) => r.status === 200,
		'reconcile resolved or replayed': (r) =>
			['reconciled', 'denied', 'still_uncertain'].includes(r.json().status) ||
			r.json().resolution !== undefined,
	});

	// The operation's control history — the read side of the same surface.
	const list = http.get(`${base}/controls`, { headers, tags: { name: 'controls' } });
	check(list, { 'controls 200': (r) => r.status === 200 && Array.isArray(r.json().controls) });
}
