<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { Button } from '@aiops/ui/ui/button';
	import { Input } from '@aiops/ui/ui/input';
	import { NativeSelect } from '@aiops/ui/ui/native-select';
	import Failure from '#lib/components/Failure.svelte';
	import Loading from '#lib/components/Loading.svelte';
	import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@aiops/ui/ui/table';
	import { api, type Result } from '#lib/api.js';
	import { money, short, time } from '#lib/format.js';
	import type { OperationList, OperationSummary } from '#lib/types.js';
	import { viewport } from '#lib/viewport.svelte.js';
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

	const filtered = $derived(Boolean(requestId || status || name || release || from || to));
	function clearFilters() {
		requestId = status = name = release = from = to = '';
		void load();
	}

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
			<NativeSelect bind:value={status} aria-label="Status">
				<option value="">any</option>
				{#each STATUS_CHOICES as s (s)}<option value={s}>{s.replaceAll('_', ' ')}</option>{/each}
			</NativeSelect>
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
	<Loading label="Loading operations" />
{:else if result.state === 'ok'}
	{#if result.data.operations.length === 0 && filtered}
		<Notice kind="unknown" title="No operations match these filters.">
			Nothing arrived that matches — which is not proof that nothing ran.
			<Button variant="outline" size="sm" onclick={clearFilters}>Clear filters</Button>
		</Notice>
	{:else if result.data.operations.length === 0}
		<Notice kind="unknown" title="No operations yet.">
			<p>No host has sent events to this project. To connect a first host:</p>
			<ol class="mt-1 list-decimal pl-5">
				<li>Mint an ingest key: <code>aiops-server admin create-ingest-key --project {project} --name my-host</code></li>
				<li>
					Give it to the host's SDK (<code>AIOPS_ENDPOINT</code>, <code>AIOPS_INGEST_KEY</code>; see
					<code>crates/aiops-sdk/examples</code>), or run the Durable Runner adapter in front of its handlers.
				</li>
				<li>Run one operation; it appears here within a second.</li>
			</ol>
		</Notice>
	{:else}
		{#snippet opLink(op: OperationSummary)}
			<a class="font-medium underline-offset-4 hover:underline" href={resolve('/p/[project]/op/[id]', { project, id: op.operation_id })}
				>{op.name ?? 'name not observed'}</a
			>
		{/snippet}
		{#snippet eventsFacts(op: OperationSummary)}
			{op.events_received}{#if op.events_emitted !== null}/{op.events_emitted}{/if}
			{#if op.events_missing}<Flag>{op.events_missing} missing</Flag>{/if}
			{#if op.multi_source}<Flag>several sources: see timeline</Flag>
			{:else if op.events_emitted === null}<Flag>total unknown</Flag>{/if}
			{#if op.reported_dropped > 0}<Flag>{op.reported_dropped} dropped by SDK</Flag>{/if}
			{#if op.detail_expired}<Flag>detail expired</Flag>{/if}
		{/snippet}
		{#snippet attemptFacts(op: OperationSummary)}
			{#if op.failed_attempts}<Flag>{op.failed_attempts} failed</Flag>{/if}
			{#if op.uncertain_attempts}<Flag>{op.uncertain_attempts} uncertain</Flag>{/if}
		{/snippet}
		{#if viewport.narrow}
			<!-- Below 640 px each operation is a card with its status first (review U1). -->
			<ul class="grid gap-2" aria-label="Operations">
				{#each result.data.operations as op (op.operation_id)}
					<li data-testid="operation" class="grid gap-1.5 rounded-xl border bg-card p-3 text-sm">
						<StatusBadge status={op.status} />
						<div class="min-w-0 break-words">{@render opLink(op)}</div>
						<div class="font-mono text-xs break-all text-muted-foreground">
							{time(op.first_event_time)} · {op.request_id ? `request ${op.request_id}` : 'no request ID'}{op.release
								? ` · ${op.release}`
								: ''}
						</div>
						<div class="text-xs">
							<span class="text-muted-foreground">Events</span> {@render eventsFacts(op)}
							{@render attemptFacts(op)}
							{#if op.cost_estimate_micros}<span class="font-mono text-muted-foreground"
									>· est. {money(op.cost_estimate_micros, op.cost_currency)}</span
								>{/if}
						</div>
					</li>
				{/each}
			</ul>
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
						<TableRow data-testid="operation">
							<TableCell class="font-mono text-xs">{time(op.first_event_time)}</TableCell>
							<TableCell>
								{@render opLink(op)}
								<div class="font-mono text-xs text-muted-foreground">
									{short(op.operation_id, 12)}{op.release ? ` · ${op.release}` : ''}
								</div>
							</TableCell>
							<TableCell class="font-mono text-xs">{op.request_id ?? '—'}</TableCell>
							<TableCell><StatusBadge status={op.status} /></TableCell>
							<TableCell>{@render eventsFacts(op)}</TableCell>
							<TableCell>{@render attemptFacts(op)}</TableCell>
							<TableCell class="font-mono text-xs"
								>{op.cost_estimate_micros ? money(op.cost_estimate_micros, op.cost_currency) : '—'}</TableCell
							>
						</TableRow>
					{/each}
				</TableBody>
			</Table>
		{/if}
		{#if result.data.next}
			{@const next = result.data.next}
			<Button class="mt-4" variant="outline" onclick={() => load(next)}>Older operations</Button>
		{/if}
	{/if}
{:else}
	<Failure {result} retry={() => void load()} />
{/if}
