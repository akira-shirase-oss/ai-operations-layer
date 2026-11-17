<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { Button } from '@aiops/ui/ui/button';
	import { Input } from '@aiops/ui/ui/input';
	import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@aiops/ui/ui/table';
	import { api, type Result } from '#lib/api.js';
	import { money, short, time } from '#lib/format.js';
	import type { OperationList } from '#lib/types.js';
	import StatusBadge from '#lib/components/StatusBadge.svelte';
	import Notice from '#lib/components/Notice.svelte';
	import Flag from '#lib/components/Flag.svelte';

	const project = $derived(page.params['project'] ?? '');
	const params = page.url.searchParams;
	let requestId = $state(params.get('request_id') ?? '');
	let status = $state(params.get('status') ?? '');
	let name = $state(params.get('name') ?? '');
	let release = $state(params.get('release') ?? '');
	// Times travel as RFC 3339 (UTC); the inputs show them as UTC too, to the minute.
	const toInput = (iso: string | null) => (iso ? new Date(iso).toISOString().slice(0, 16) : '');
	let from = $state(toInput(params.get('from')));
	let to = $state(toInput(params.get('to')));
	// A window passed in from the comparison is kept exact (the inputs show it to the minute).
	const exactFrom = params.get('from');
	const exactTo = params.get('to');
	let result = $state<Result<OperationList> | null>(null);
	const STATUS_CHOICES = ['succeeded', 'failed', 'uncertain', 'conflicting', 'no_finish_observed'];
	const utc = (input: string, exact: string | null) =>
		exact && toInput(exact) === input ? exact : `${input}:00Z`;

	async function load(cursor?: [string, string]) {
		const q = new URLSearchParams({ limit: '50' });
		if (requestId.trim()) q.set('request_id', requestId.trim());
		if (status) q.set('status', status);
		if (name.trim()) q.set('name', name.trim());
		if (release.trim()) q.set('release', release.trim());
		if (from) q.set('from', utc(from, exactFrom));
		if (to) q.set('to', utc(to, exactTo));
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

<p class="text-sm text-muted-foreground"><a href={resolve('/')}>Projects</a> / {project}</p>
<h1 class="mb-4 text-2xl font-semibold tracking-tight">Operations</h1>
<p class="mb-2 text-sm"><a href={resolve('/p/[project]/releases', { project })}>Compare releases</a></p>
<form class="mb-4 grid max-w-4xl gap-2" onsubmit={search}>
	<div class="flex gap-2">
		<Input bind:value={requestId} placeholder="Find by request ID (e.g. from a support ticket)" />
		<Button type="submit">Find</Button>
	</div>
	<div class="grid grid-cols-2 gap-2 text-sm md:grid-cols-5">
		<label class="grid gap-1">
			<span class="text-muted-foreground">Status</span>
			<select bind:value={status} aria-label="Status" class="h-9 rounded-md border bg-background px-2">
				<option value="">any</option>
				{#each STATUS_CHOICES as s (s)}<option value={s}>{s.replaceAll('_', ' ')}</option>{/each}
			</select>
		</label>
		<label class="grid gap-1"
			><span class="text-muted-foreground">Operation name</span><Input bind:value={name} aria-label="Operation name" /></label
		>
		<label class="grid gap-1"
			><span class="text-muted-foreground">Release</span><Input bind:value={release} aria-label="Release" /></label
		>
		<label class="grid gap-1"
			><span class="text-muted-foreground">From (UTC, event time)</span><Input
				type="datetime-local"
				bind:value={from}
				aria-label="From"
			/></label
		>
		<label class="grid gap-1"
			><span class="text-muted-foreground">To (UTC)</span><Input type="datetime-local" bind:value={to} aria-label="To" /></label
		>
	</div>
</form>

{#if result === null}
	<p class="text-muted-foreground">Loading…</p>
{:else if result.state === 'ok'}
	{#if result.data.operations.length === 0}
		<Notice kind="unknown">
			No operations recorded{requestId || status || name || release || from || to ? ' that match' : ''}. That means no
			events arrived — not that nothing ran.
		</Notice>
	{:else}
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>Started (event time)</TableHead>
					<TableHead>Operation</TableHead>
					<TableHead>Request</TableHead>
					<TableHead>Status</TableHead>
					<TableHead>Events</TableHead>
					<TableHead>Attempts</TableHead>
					<TableHead>Est. cost</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each result.data.operations as op (op.operation_id)}
					<TableRow>
						<TableCell class="font-mono text-xs">{time(op.first_event_time)}</TableCell>
						<TableCell>
							<a class="font-medium underline-offset-4 hover:underline" href={resolve('/p/[project]/op/[id]', { project, id: op.operation_id })}
								>{op.name ?? 'name not observed'}</a
							>
							<div class="font-mono text-xs text-muted-foreground">
								{short(op.operation_id, 12)}{op.release ? ` · ${op.release}` : ''}
							</div>
						</TableCell>
						<TableCell class="font-mono text-xs">{op.request_id ?? '—'}</TableCell>
						<TableCell><StatusBadge status={op.status} /></TableCell>
						<TableCell>
							{op.events_received}{#if op.events_emitted !== null}/{op.events_emitted}{/if}
							{#if op.events_missing}<Flag>{op.events_missing} missing</Flag>{/if}
							{#if op.multi_source}<Flag>several sources: see timeline</Flag>
							{:else if op.events_emitted === null}<Flag>total unknown</Flag>{/if}
							{#if op.reported_dropped > 0}<Flag>{op.reported_dropped} dropped by SDK</Flag>{/if}
							{#if op.detail_expired}<Flag>detail expired</Flag>{/if}
						</TableCell>
						<TableCell>
							{#if op.failed_attempts}<Flag>{op.failed_attempts} failed</Flag>{/if}
							{#if op.uncertain_attempts}<Flag>{op.uncertain_attempts} uncertain</Flag>{/if}
						</TableCell>
						<TableCell class="font-mono text-xs"
							>{op.cost_estimate_micros ? money(op.cost_estimate_micros, op.cost_currency) : '—'}</TableCell
						>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
		{#if result.data.next}
			{@const next = result.data.next}
			<Button class="mt-4" variant="outline" onclick={() => load(next)}>Older operations</Button>
		{/if}
	{/if}
{:else if result.state === 'forbidden'}
	<Notice kind="fail">Denied: {result.message}. The denial was recorded in the audit log.</Notice>
{:else if result.state === 'rateLimited'}
	<Notice kind="uncertain">Rate limited; retry in {result.retryAfter} s.</Notice>
{:else if result.state === 'needsAuth'}
	<Notice kind="fail">The token is missing, expired or revoked.</Notice>
{:else if result.state === 'unreachable'}
	<Notice kind="fail">The collector could not be reached: {result.message}</Notice>
{:else}
	<Notice kind="fail">Not found.</Notice>
{/if}
