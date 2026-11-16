// Ingestion load: open-model (constant arrival rate) batches against POST /v1/ingest.
//
// One iteration = one batch = OPS_PER_BATCH operations of the reference shape (ten events
// each: operation.started, four steps started+finished, operation.finished), so the
// guide's reference load (100 requests/s × 10 events) is RATE=100 OPS_PER_BATCH=1.
// Every event ID is unique per RUN_ID, so nothing here is a duplicate unless DUPLICATE=1.
//
// Loss is never hidden behind latency: every event the collector did not store is
// counted in `events_lost`, and the run fails if any was.
//
//   k6 run -e BASE_URL=… -e INGEST_KEY=… -e RUN_ID=<8 hex> -e RATE=100 -e DURATION=60s perf/k6/ingest.js
import http from 'k6/http';
import exec from 'k6/execution';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:57410';
const KEY = __ENV.INGEST_KEY;
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const RATE = Number(__ENV.RATE || 100);
const DURATION = __ENV.DURATION || '30s';
const OPS_PER_BATCH = Number(__ENV.OPS_PER_BATCH || 1);
const EVENTS_PER_OP = 10;

const sent = new Counter('events_sent');
const accepted = new Counter('events_accepted');
const lost = new Counter('events_lost');
const limited = new Counter('batches_rate_limited');
const shed = new Counter('batches_shed');

export const options = {
	discardResponseBodies: false,
	scenarios: {
		ingest: {
			executor: 'constant-arrival-rate',
			rate: RATE,
			timeUnit: '1s',
			duration: DURATION,
			preAllocatedVUs: Number(__ENV.PRE_VUS || 50),
			maxVUs: Number(__ENV.MAX_VUS || 400),
		},
	},
	thresholds: {
		// Development targets on a laptop (docs/END_GOAL.md evidence plan); a miss fails the run.
		'http_req_duration{scenario:ingest}': ['p(95)<100', 'p(99)<250'],
		http_req_failed: ['rate==0'],
		checks: ['rate==1'],
		events_lost: ['count==0'],
		dropped_iterations: ['count==0'],
	},
	summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

function hex(n, width) {
	return n.toString(16).padStart(width, '0').slice(-width);
}

// Per VU (each VU is its own JS runtime): one source instance with its own sequence.
let seq = 0;

function iso(ms) {
	// Fixed six fractional digits, like the SDK.
	return new Date(ms).toISOString().replace('Z', '000Z');
}

function operation(n, now) {
	const op = RUN_ID + hex(n, 24);
	const step = (k) => hex(n * 8 + k + 1, 16);
	const base = (k, type) => {
		seq += 1;
		return {
			event_id: RUN_ID + hex(n * EVENTS_PER_OP + k, 24),
			type,
			operation_id: op,
			seq,
			op_seq: k + 1,
			event_time: iso(now - 50 + k),
		};
	};
	const steps = [
		['model', 'generate'],
		['validation', 'validate'],
		['tool', 'fact_check'],
		['tool', 'publish'],
	];
	const events = [
		Object.assign(base(0, 'operation.started'), {
			name: 'publish_article',
			request_id: `load-${RUN_ID}-${n}`,
			release: 'load-r1',
		}),
	];
	steps.forEach(([kind, name], i) => {
		events.push(
			Object.assign(base(1 + i * 2, 'step.started'), {
				step_id: step(i),
				kind,
				name,
				attempt: 1,
				external_effect: name === 'publish',
			}),
		);
		const finished = Object.assign(base(2 + i * 2, 'step.finished'), {
			step_id: step(i),
			attempt: 1,
			outcome: 'succeeded',
		});
		if (kind === 'model') {
			finished.usage = { input_tokens: 132, output_tokens: 480 };
			finished.cost_estimate = { micros: 2052, currency: 'USD', basis: 'fake-list-price-2026-11' };
		}
		events.push(finished);
	});
	events.push(
		Object.assign(base(9, 'operation.finished'), { outcome: 'succeeded', events_emitted: EVENTS_PER_OP }),
	);
	return events;
}

export default function () {
	const i = exec.scenario.iterationInTest;
	const now = Date.now();
	let events = [];
	for (let j = 0; j < OPS_PER_BATCH; j++) {
		events = events.concat(operation(i * OPS_PER_BATCH + j, now));
	}
	const body = JSON.stringify({
		schema_version: 1,
		source: {
			instance_id: RUN_ID + hex(exec.vu.idInTest, 24),
			service: 'k6-load',
			sdk: 'k6/ingest.js',
		},
		events,
	});
	const res = http.post(`${BASE_URL}/v1/ingest`, body, {
		headers: { authorization: `Bearer ${KEY}`, 'content-type': 'application/json' },
		tags: { name: 'ingest' },
	});
	sent.add(events.length);
	let stored = 0;
	if (res.status === 200) {
		const answer = res.json();
		stored = answer.accepted + answer.duplicates;
		accepted.add(answer.accepted);
	} else if (res.status === 429) {
		limited.add(1);
	} else if (res.status === 503) {
		shed.add(1);
	}
	lost.add(events.length - stored);
	check(res, {
		'200': (r) => r.status === 200,
		'every event stored': () => stored === events.length,
	});
}
