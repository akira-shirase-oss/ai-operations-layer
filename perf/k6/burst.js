// Burst ingestion at scale (the evidence plan's burst run). Staged offered
// load: the reference baseline -> a spike past the collector's configured
// limits -> baseline again. The collector's answers are the measurement:
//
//   200 the batch landed       429 the key's token bucket said wait
//   503 the in-flight budget shed it   status 0 the connection itself failed
//
// Every sent event lands in exactly one counted bucket — stored, refused
// (inside a 200), limited, shed, unreachable or server_error — and the gate is
// the accounting identity, not a latency chart:
//
//   events_unaccounted == 0        (sent equals the sum of its buckets)
//   batches_server_error == 0      (a non-503 5xx is never an answer)
//   batches_bounded > 0            (the spike provably crossed the configured
//                                   limits — no shed means no burst)
//   recovered_failed == 0          (the post-spike window answers only 200 —
//                                   shedding under overload is the contract,
//                                   staying broken is not)
//
// The runner configures the collector's limits so the spike provably exceeds
// them; those limits are printed in the result file.
//
//   k6 run -e BASE_URL=… -e INGEST_KEY=… -e RUN_ID=<8 hex> perf/k6/burst.js
import http from 'k6/http';
import exec from 'k6/execution';
import { check } from 'k6';
import { Counter } from 'k6/metrics';
import { EVENTS_PER_OP, hex, note, operation } from './ops.js';

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:57410';
const KEY = __ENV.INGEST_KEY;
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const BASELINE_RATE = Number(__ENV.BASELINE_RATE || 100);
const SPIKE_RATE = Number(__ENV.SPIKE_RATE || 1500);
const WARM_S = Number(__ENV.WARM_S || 30);
const HOLD_S = Number(__ENV.HOLD_S || 60);
const SPIKE_HOLD_S = Number(__ENV.SPIKE_HOLD_S || 30);
const RECOVER_S = Number(__ENV.RECOVER_S || 60);
const OPS_PER_BATCH = Number(__ENV.OPS_PER_BATCH || 1);

// Seconds elapsed before the recovery window starts — the same stage math the
// executor uses, so the recovery check tracks the shape, not the clock alone.
const RECOVERY_AT_S = WARM_S + HOLD_S + 15 + SPIKE_HOLD_S + 15;

const sent = new Counter('events_sent');
const stored = new Counter('events_stored');
const refused = new Counter('events_refused');
const limited = new Counter('events_limited');
const shed = new Counter('events_shed');
const unreachable = new Counter('events_unreachable');
const serverErrorEvents = new Counter('events_server_error');
const unaccounted = new Counter('events_unaccounted');
const bounded = new Counter('batches_bounded');
const serverErrors = new Counter('batches_server_error');
const retried = new Counter('batches_retried');
const recoveredFailed = new Counter('recovered_failed');

const t0 = Date.now();

export const options = {
	// A VU's connection must not idle past the collector's 10 s header
	// bound, whose idle close races a reuse (D-035): connections retire at
	// each iteration, and the intra-iteration gaps are the poll sleeps.
	noVUConnectionReuse: true,
	scenarios: {
		burst: {
			executor: 'ramping-arrival-rate',
			startRate: BASELINE_RATE,
			timeUnit: '1s',
			preAllocatedVUs: Number(__ENV.PRE_VUS || 50),
			maxVUs: Number(__ENV.MAX_VUS || 600),
			stages: [
				{ target: BASELINE_RATE, duration: `${WARM_S}s` },
				{ target: BASELINE_RATE, duration: `${HOLD_S}s` },
				{ target: SPIKE_RATE, duration: '15s' },
				{ target: SPIKE_RATE, duration: `${SPIKE_HOLD_S}s` },
				{ target: BASELINE_RATE, duration: '15s' },
				{ target: BASELINE_RATE, duration: `${RECOVER_S}s` },
			],
		},
	},
	thresholds: {
		events_unaccounted: ['count==0'],
		batches_server_error: ['count==0'],
		batches_bounded: ['count>0'],
		recovered_failed: ['count==0'],
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
			sdk: 'k6/burst.js',
		},
		events,
	});
	const params = {
		headers: { authorization: `Bearer ${KEY}`, 'content-type': 'application/json' },
		expectedStatuses: [200, 429, 503],
		tags: { name: 'ingest' },
	};
	let res = http.post(`${BASE_URL}/v1/ingest`, body, params);
	if (res.status === 0) {
		note('burst', res, [200, 429, 503]);
		retried.add(1);
		res = http.post(`${BASE_URL}/v1/ingest`, body, params);
	}
	const n = events.length;
	sent.add(n);
	let storedN = 0;
	let bucket = 0;
	if (res.status === 200) {
		const answer = res.json();
		storedN = answer.accepted + answer.duplicates;
		refused.add((answer.refused || []).length);
		bucket += (answer.refused || []).length;
	} else if (res.status === 429) {
		limited.add(n);
		bounded.add(1);
		bucket += n;
	} else if (res.status === 503) {
		shed.add(n);
		bounded.add(1);
		bucket += n;
	} else if (res.status === 0) {
		unreachable.add(n);
		bucket += n;
	} else {
		serverErrors.add(1);
		serverErrorEvents.add(n);
		bucket += n;
	}
	stored.add(storedN);
	bucket += storedN;
	if (bucket !== n) unaccounted.add(n - bucket);
	// Recovery: inside the post-spike window nothing but a stored batch is an
	// acceptable answer — a collector still shedding then is broken, not bounded.
	if (now - t0 >= RECOVERY_AT_S * 1000 && res.status !== 200) {
		recoveredFailed.add(1);
	}
	note('burst', res, [200, 429, 503, 0]);
	check(res, {
		'bounded answer (200/429/503 or a clean outage)': (r) =>
			r.status === 200 || r.status === 429 || r.status === 503 || r.status === 0,
	});
}
