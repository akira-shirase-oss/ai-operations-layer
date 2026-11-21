// Collector loss at million scale (the evidence plan's collector-loss run).
// The offered load is ingest.js's reference shape; the runner kills the
// collector mid-run (SIGKILL — no drain) and restarts it on the same database.
// Every sent event must land in exactly one counted bucket:
//
//   stored       — 200 answers (accepted + duplicates)
//   refused      — per-event refusals inside 200 answers
//   limited      — 429 token-bucket answers
//   shed         — 503 in-flight-budget answers
//   unreachable  — status 0 after the one retry: the batch never got an answer
//   server_error — any other 5xx (a failure class, never expected)
//
// The gates are the accounting identity and the absence of silent classes:
//
//   events_unaccounted == 0    (sent equals the sum of its buckets)
//   batches_server_error == 0
//
// Loss inside the outage window is real and expected — it is counted as
// `unreachable`, disclosed in the result file, and the runner reconciles the
// database's row count against `stored` exactly.
//
//   k6 run -e BASE_URL=… -e INGEST_KEY=… -e RUN_ID=<8 hex> \
//          -e RATE=500 -e DURATION=200s perf/k6/loss.js
import http from 'k6/http';
import exec from 'k6/execution';
import { check } from 'k6';
import { Counter } from 'k6/metrics';
import { EVENTS_PER_OP, hex, note, operation } from './ops.js';

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:57410';
const KEY = __ENV.INGEST_KEY;
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const RATE = Number(__ENV.RATE || 500);
const DURATION = __ENV.DURATION || '200s';
const OPS_PER_BATCH = Number(__ENV.OPS_PER_BATCH || 1);

const sent = new Counter('events_sent');
const stored = new Counter('events_stored');
const refused = new Counter('events_refused');
const limited = new Counter('events_limited');
const shed = new Counter('events_shed');
const unreachable = new Counter('events_unreachable');
const serverErrorEvents = new Counter('events_server_error');
const unaccounted = new Counter('events_unaccounted');
const serverErrors = new Counter('batches_server_error');
const retried = new Counter('batches_retried');

export const options = {
	// A VU's connection must not idle past the collector's 10 s header
	// bound, whose idle close races a reuse (D-035): connections retire at
	// each iteration, and the intra-iteration gaps are the poll sleeps.
	noVUConnectionReuse: true,
	scenarios: {
		loss: {
			executor: 'constant-arrival-rate',
			rate: RATE,
			timeUnit: '1s',
			duration: DURATION,
			preAllocatedVUs: Number(__ENV.PRE_VUS || 50),
			maxVUs: Number(__ENV.MAX_VUS || 400),
		},
	},
	thresholds: {
		events_unaccounted: ['count==0'],
		batches_server_error: ['count==0'],
		checks: ['rate==1'],
	},
	summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

export default function () {
	const i = exec.scenario.iterationInTest;
	const now = Date.now();
	let events = [];
	for (let j = 0; j < OPS_PER_BATCH; j++) {
		events = events.concat(operation(i * OPS_PER_BATCH + j, now, RUN_ID).events);
	}
	const body = JSON.stringify({
		schema_version: 1,
		source: {
			instance_id: RUN_ID + hex(exec.vu.idInTest, 24),
			service: 'k6-load',
			sdk: 'k6/loss.js',
		},
		events,
	});
	const params = {
		headers: { authorization: `Bearer ${KEY}`, 'content-type': 'application/json' },
		// 429/503 are bounded answers; status 0 is the outage's signature —
		// never marked an HTTP failure, always counted as unreachable.
		expectedStatuses: [200, 429, 503],
		tags: { name: 'ingest' },
	};
	let res = http.post(`${BASE_URL}/v1/ingest`, body, params);
	if (res.status === 0) {
		note('loss', res, [200, 429, 503]);
		retried.add(1);
		res = http.post(`${BASE_URL}/v1/ingest`, body, params);
	}
	const n = events.length;
	sent.add(n);
	let bucket = 0;
	if (res.status === 200) {
		const answer = res.json();
		const storedN = answer.accepted + answer.duplicates;
		stored.add(storedN);
		refused.add((answer.refused || []).length);
		bucket += storedN + (answer.refused || []).length;
	} else if (res.status === 429) {
		limited.add(n);
		bucket += n;
	} else if (res.status === 503) {
		shed.add(n);
		bucket += n;
	} else if (res.status === 0) {
		unreachable.add(n);
		bucket += n;
	} else {
		serverErrors.add(1);
		serverErrorEvents.add(n);
		bucket += n;
	}
	if (bucket !== n) unaccounted.add(n - bucket);
	note('loss', res, [200, 429, 503, 0]);
	check(res, {
		'answered or a counted outage': (r) =>
			r.status === 200 || r.status === 429 || r.status === 503 || r.status === 0,
	});
}
