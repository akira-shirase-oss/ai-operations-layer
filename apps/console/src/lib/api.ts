// The console holds no authority: it is a client of the same permissioned API as
// everything else (AGENTS.md, frontend rules). The token lives in this tab's session
// storage and travels as a bearer header — no cookies, so no CSRF surface (D-015).

const TOKEN_KEY = 'aiops.token';

export function token(): string | null {
	try {
		return sessionStorage.getItem(TOKEN_KEY);
	} catch {
		return null;
	}
}

export function setToken(value: string | null): void {
	try {
		if (value) sessionStorage.setItem(TOKEN_KEY, value.trim());
		else sessionStorage.removeItem(TOKEN_KEY);
	} catch {
		// storage unavailable: the user signs in again next time
	}
}

/** Every failure state is explicit; none is rendered as an empty success. */
export type Result<T> =
	| { state: 'ok'; data: T }
	| { state: 'needsAuth' }
	| { state: 'forbidden'; message: string }
	| { state: 'missing' }
	| { state: 'rateLimited'; retryAfter: number }
	| { state: 'unreachable'; message: string };

export async function api<T>(path: string, fetcher: typeof fetch = fetch): Promise<Result<T>> {
	const t = token();
	if (!t) return { state: 'needsAuth' };
	let response: Response;
	try {
		response = await fetcher(path, { headers: { authorization: `Bearer ${t}` } });
	} catch (e) {
		return { state: 'unreachable', message: e instanceof Error ? e.message : 'network error' };
	}
	if (response.ok) return { state: 'ok', data: (await response.json()) as T };
	let message = `HTTP ${response.status}`;
	try {
		const body = (await response.json()) as { error?: { message?: string } };
		if (body?.error?.message) message = body.error.message;
	} catch {
		// keep the status line
	}
	switch (response.status) {
		case 401:
			return { state: 'needsAuth' };
		case 403:
			return { state: 'forbidden', message };
		case 404:
			return { state: 'missing' };
		case 429:
			return { state: 'rateLimited', retryAfter: Number(response.headers.get('retry-after') ?? '1') };
		default:
			return { state: 'unreachable', message };
	}
}

/** A control request's answer: the record, or a refusal the person can read (A3). */
export type Sent<T> = Result<T> | { state: 'refused'; code: string; message: string };

/** POSTs JSON with the tab's token. A 409 or 503 is a refusal with the collector's words. */
export async function send<T>(path: string, body: unknown, fetcher: typeof fetch = fetch): Promise<Sent<T>> {
	const t = token();
	if (!t) return { state: 'needsAuth' };
	let response: Response;
	try {
		response = await fetcher(path, {
			method: 'POST',
			headers: { authorization: `Bearer ${t}`, 'content-type': 'application/json' },
			body: JSON.stringify(body)
		});
	} catch (e) {
		return { state: 'unreachable', message: e instanceof Error ? e.message : 'network error' };
	}
	if (response.ok) return { state: 'ok', data: (await response.json()) as T };
	let code = `http_${response.status}`;
	let message = `HTTP ${response.status}`;
	try {
		const b = (await response.json()) as { error?: { code?: string; message?: string } };
		if (b?.error?.code) code = b.error.code;
		if (b?.error?.message) message = b.error.message;
	} catch {
		// keep the status line
	}
	if (response.status === 409 || response.status === 503) return { state: 'refused', code, message };
	if (response.status === 401) return { state: 'needsAuth' };
	if (response.status === 403) return { state: 'forbidden', message };
	if (response.status === 404) return { state: 'missing' };
	return { state: 'unreachable', message };
}
