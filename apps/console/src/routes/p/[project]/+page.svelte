<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { api, type Result } from '#lib/api.js';
	import { money, short, time } from '#lib/format.js';
	import type { OperationList } from '#lib/types.js';
	import StatusBadge from '#lib/components/StatusBadge.svelte';

	const project = $derived(page.params['project'] ?? '');
	let requestId = $state(page.url.searchParams.get('request_id') ?? '');
	let result: Result<OperationList> | null = $state(null);

	async function load(cursor?: [string, string]) {
		const q = new URLSearchParams({ limit: '50' });
		if (requestId.trim()) q.set('request_id', requestId.trim());
		if (cursor) {
			q.set('before_time', cursor[0]);
			q.set('before_id', cursor[1]);
		}
		result = await api<OperationList>(`/v1/projects/${encodeURIComponent(project)}/operations?${q}`);
	}

	$effect(() => {
		void load();
	});

	function search(event: SubmitEvent) {
		event.preventDefault();
		void load();
	}
</script>

<p class="muted"><a href={resolve('/')}>Projects</a> / {project}</p>
<h1>Operations</h1>
<form class="inline" onsubmit={search}>
	<input bind:value={requestId} placeholder="Find by request ID (e.g. from a support ticket)" />
	<button type="submit">Find</button>
</form>

{#if result === null}
	<p class="muted">Loading…</p>
{:else if result.state === 'ok'}
	{#if result.data.operations.length === 0}
		<p class="panel">No operations recorded{requestId ? ` for request ${requestId}` : ''}. That means no events
			arrived — not that nothing ran.</p>
	{:else}
		<table>
			<thead>
				<tr>
					<th>Started (event time)</th>
					<th>Operation</th>
					<th>Request</th>
					<th>Status</th>
					<th>Events</th>
					<th>Attempts</th>
					<th>Est. cost</th>
				</tr>
			</thead>
			<tbody>
				{#each result.data.operations as op (op.operation_id)}
					<tr>
						<td class="mono">{time(op.first_event_time)}</td>
						<td>
							<a href={resolve('/p/[project]/op/[id]', { project, id: op.operation_id })}>{op.name ?? 'name not observed'}</a>
							<div class="mono muted">{short(op.operation_id, 12)}{op.release ? ` · ${op.release}` : ''}</div>
						</td>
						<td class="mono">{op.request_id ?? '—'}</td>
						<td><StatusBadge status={op.status} /></td>
						<td>
							{op.events_received}{#if op.events_emitted !== null}/{op.events_emitted}{/if}
							{#if op.events_missing}<span class="flag">{op.events_missing} missing</span>{/if}
							{#if op.events_emitted === null}<span class="flag">total unknown</span>{/if}
							{#if op.reported_dropped > 0}<span class="flag">{op.reported_dropped} dropped by SDK</span>{/if}
							{#if op.detail_expired}<span class="flag">detail expired</span>{/if}
						</td>
						<td>
							{#if op.failed_attempts}<span class="flag">{op.failed_attempts} failed</span>{/if}
							{#if op.uncertain_attempts}<span class="flag">{op.uncertain_attempts} uncertain</span>{/if}
						</td>
						<td class="mono">{op.cost_estimate_micros ? money(op.cost_estimate_micros, op.cost_currency) : '—'}</td>
					</tr>
				{/each}
			</tbody>
		</table>
		{#if result.data.next}
			{@const next = result.data.next}
			<p><button onclick={() => load(next)}>Older operations</button></p>
		{/if}
	{/if}
{:else if result.state === 'forbidden'}
	<p class="notice fail">Denied: {result.message}. The denial was recorded in the audit log.</p>
{:else if result.state === 'rateLimited'}
	<p class="notice uncertain">Rate limited; retry in {result.retryAfter} s.</p>
{:else if result.state === 'needsAuth'}
	<p class="notice fail">The token is missing, expired or revoked.</p>
{:else if result.state === 'unreachable'}
	<p class="notice fail">The collector could not be reached: {result.message}</p>
{:else}
	<p class="notice fail">Not found.</p>
{/if}
