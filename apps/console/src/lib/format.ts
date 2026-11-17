// Formatting only. No value is ever invented: missing data stays visibly missing.

export function time(iso: string | null | undefined): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return iso;
	const pad = (n: number, w = 2) => String(n).padStart(w, '0');
	return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(
		d.getUTCHours()
	)}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}.${pad(d.getUTCMilliseconds(), 3)}Z`;
}

/** Microseconds as a short duration. */
export function micros(us: number | null | undefined): string {
	if (us === null || us === undefined) return '—';
	const sign = us < 0 ? '−' : '';
	const a = Math.abs(us);
	if (a < 1_000) return `${sign}${a} µs`;
	if (a < 1_000_000) return `${sign}${(a / 1_000).toFixed(1)} ms`;
	return `${sign}${(a / 1_000_000).toFixed(2)} s`;
}

/** An estimate in a currency, from millionths of a unit. */
export function money(microsAmount: number, currency: string | null): string {
	if (currency === 'MIXED') return `${microsAmount} µ (mixed currencies)`;
	const amount = microsAmount / 1_000_000;
	return `${amount.toFixed(amount < 0.01 ? 6 : 4)} ${currency ?? ''}`.trim();
}

export function short(id: string, n = 8): string {
	return id.length > n ? `${id.slice(0, n)}…` : id;
}

export function value(v: unknown): string {
	return typeof v === 'string' ? v : JSON.stringify(v);
}

const STATE_TEXT: Record<string, string> = {
	succeeded: 'succeeded',
	failed: 'failed',
	uncertain: 'outcome unknown',
	conflicting: 'conflicting outcomes',
	no_finish_observed: 'no finish observed'
};

export function stateText(state: string): string {
	return STATE_TEXT[state] ?? state.replaceAll('_', ' ');
}

// The host's declared classes (D-020) in plain words; the codes stay one click away.
const CLASS_TEXT: Record<string, string> = {
	transport: 'never delivered',
	rejected: 'refused',
	validation: 'invalid output',
	internal: 'host error'
};
const REASON_TEXT: Record<string, string> = {
	timeout_after_send: 'timed out after sending',
	connection_lost_after_send: 'connection lost after sending',
	response_unreadable: 'reply unreadable'
};

/** A declared outcome in plain words: "failed: invalid output", "outcome unknown: timed out
 * after sending", "cancelled". A failed finish whose host error code is `cancelled` is a run
 * stopped on request (the code is the host's declaration; nothing is inferred). */
export function statusText(s: { state: string; class?: string; reason?: string; code?: string }): string {
	if (s.state === 'failed' && s.code === 'cancelled') return 'cancelled';
	const detail = s.class
		? (CLASS_TEXT[s.class] ?? s.class.replaceAll('_', ' '))
		: s.reason
			? (REASON_TEXT[s.reason] ?? s.reason.replaceAll('_', ' '))
			: '';
	return detail ? `${stateText(s.state)}: ${detail}` : stateText(s.state);
}

/** The words after the colon in `statusText` ("invalid output"), or "" when there are none. */
export function outcomeDetail(s: { state: string; class?: string; reason?: string; code?: string }): string {
	const text = statusText(s);
	const colon = text.indexOf(': ');
	return colon < 0 ? '' : text.slice(colon + 2);
}

/** "1 step", "2 steps". */
export function count(n: number, one: string, many: string): string {
	return `${n} ${n === 1 ? one : many}`;
}

/** The codes behind a status, for a details toggle or a tooltip. */
export function statusCodes(s: { state: string; class?: string; reason?: string; code?: string }): string {
	return [
		`outcome ${s.state}`,
		s.class ? `failure_class ${s.class}` : '',
		s.reason ? `uncertain_reason ${s.reason}` : '',
		s.code ? `error_code ${s.code}` : ''
	]
		.filter(Boolean)
		.join(' · ');
}

const EVENT_TEXT: Record<string, string> = {
	'operation.started': 'operation started',
	'operation.finished': 'operation finished',
	'step.started': 'step started',
	'step.finished': 'step finished',
	'gap.reported': 'gap reported'
};

export function eventTypeText(type: string): string {
	return EVENT_TEXT[type] ?? type;
}
