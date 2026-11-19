// The reference operation's event shape, shared by the load scenarios so a contract
// change lands in one place (docs/INGEST_CONTRACT.md, schema_version 1). Module
// state is per VU — k6 gives each VU its own JS runtime — so `seq` is one source
// instance's sequence, as the SDK keeps it.
export const EVENTS_PER_OP = 10;

// The first failures, printed verbatim so a dead harness can never read as a
// capacity result again (CI 44544883621: 1.36M requests, all refused — and the
// summary held no reason). A status of 0 means k6 never got an answer: `error`
// then carries the dial error ("connection refused"), which a real HTTP status
// or body can't give. Module state is per-VU, so each VU logs at most three —
// bounded noise even on a total outage — and the runner lifts the lines into
// the result file.
let noted = 0;

export function note(tag, r, expected) {
	if (expected.indexOf(r.status) !== -1 || noted >= 3) return;
	noted += 1;
	console.log(`AIOPS_FIRST_FAILURE ${JSON.stringify({
		op: tag,
		status: r.status,
		error: r.error || '',
		body: String(r.body || '').slice(0, 240),
	})}`);
}

let seq = 0;

export function hex(n, width) {
	return n.toString(16).padStart(width, '0').slice(-width);
}

function iso(ms) {
	// Fixed six fractional digits, like the SDK.
	return new Date(ms).toISOString().replace('Z', '000Z');
}

// One operation = ten events: operation.started, four steps started+finished,
// operation.finished. `op` is deterministic in (runId, n): control reads can name
// the same operation ingest creates.
export function operation(n, now, runId) {
	const op = runId + hex(n, 24);
	const step = (k) => hex(n * 8 + k + 1, 16);
	const base = (k, type) => {
		seq += 1;
		return {
			event_id: runId + hex(n * EVENTS_PER_OP + k, 24),
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
			request_id: `load-${runId}-${n}`,
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
	return { operation_id: op, events };
}
