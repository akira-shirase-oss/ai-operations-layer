// Query load after an ingestion run: timeline reads of random ingested operations and
// list pages, over whatever the database holds (≥ 1,000,000 events for a full run).
//
//   k6 run -e BASE_URL=… -e TOKEN=… -e RUN_ID=<same as ingest> -e OPERATIONS=<ingested> perf/k6/query.js
import http from 'k6/http';
import { check } from 'k6';

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:57410';
const TOKEN = __ENV.TOKEN;
const PROJECT = __ENV.PROJECT || 'perf';
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const OPERATIONS = Number(__ENV.OPERATIONS || 1);

export const options = {
	scenarios: {
		timeline: {
			executor: 'constant-arrival-rate',
			rate: Number(__ENV.RATE || 20),
			timeUnit: '1s',
			duration: __ENV.DURATION || '30s',
			preAllocatedVUs: 20,
			maxVUs: 100,
		},
	},
	thresholds: {
		'http_req_duration{name:timeline}': ['p(95)<150'],
		'http_req_duration{name:list}': ['p(95)<150'],
		// A2: the list filtered by status and release, and the release comparison (an aggregate
		// over the last 24 h of summaries, so a tenth of iterations ask for it).
		'http_req_duration{name:list_filtered}': ['p(95)<150'],
		'http_req_duration{name:compare}': ['p(95)<500'],
		http_req_failed: ['rate==0'],
		checks: ['rate==1'],
	},
	summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

function hex(n, width) {
	return n.toString(16).padStart(width, '0').slice(-width);
}

export default function () {
	const headers = { authorization: `Bearer ${TOKEN}` };
	const n = Math.floor(Math.random() * OPERATIONS);
	const op = RUN_ID + hex(n, 24);
	const t = http.get(`${BASE_URL}/v1/projects/${PROJECT}/operations/${op}`, { headers, tags: { name: 'timeline' } });
	check(t, {
		'timeline 200': (r) => r.status === 200,
		'ten events, nothing missing': (r) => {
			const b = r.json();
			return b.coverage.events_received === 10 && b.coverage.missing_known === 0;
		},
	});
	const l = http.get(`${BASE_URL}/v1/projects/${PROJECT}/operations?limit=50&request_id=load-${RUN_ID}-${n}`, {
		headers,
		tags: { name: 'list' },
	});
	check(l, { 'list 200': (r) => r.status === 200 && r.json().operations.length === 1 });
	// The ingest scenario's operations are release load-r1, all succeeded.
	const f = http.get(`${BASE_URL}/v1/projects/${PROJECT}/operations?limit=50&status=succeeded&release=load-r1`, {
		headers,
		tags: { name: 'list_filtered' },
	});
	check(f, { 'filtered 200': (r) => r.status === 200 && r.json().operations.length > 0 });
	if (Math.random() < 0.1) {
		const c = http.get(`${BASE_URL}/v1/projects/${PROJECT}/releases/compare?releases=load-r1`, {
			headers,
			tags: { name: 'compare' },
		});
		check(c, { 'compare 200': (r) => r.status === 200 && r.json().releases[0].operations > 0 });
	}
}
