<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { Button } from '@aiops/ui/ui/button';
	import { Input } from '@aiops/ui/ui/input';
	import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@aiops/ui/ui/table';
	import { api, type Result } from '#lib/api.js';
	import { money, time } from '#lib/format.js';
	import type { Comparison } from '#lib/types.js';
	import Notice from '#lib/components/Notice.svelte';

	// Releases side by side, from the same summaries the operations list filters (A2): every
	// count links to the list that shows exactly those operations.
	const project = $derived(page.params['project'] ?? '');
	let releases = $state(page.url.searchParams.get('releases') ?? '');
	let hours = $state('24');
	let result = $state<Result<Comparison> | null>(null);
	const STATUS_COLUMNS = ['succeeded', 'failed', 'uncertain', 'conflicting', 'no_finish_observed'] as const;

	async function compare(event?: SubmitEvent) {
		event?.preventDefault();
		if (!releases.trim()) return;
		const to = new Date();
		const from = new Date(to.getTime() - Number(hours) * 3_600_000);
		const q = new URLSearchParams({ releases: releases.trim(), from: from.toISOString(), to: to.toISOString() });
		result = await api<Comparison>(`/v1/projects/${encodeURIComponent(project)}/releases/compare?${q}`);
	}

	$effect(() => {
		if (releases.trim()) void compare();
	});

	function listed(c: Comparison, release: string, status: string): string {
		const q = new URLSearchParams({ release, status, from: c.from, to: c.to });
		return `${resolve('/p/[project]', { project })}?${q}`;
	}
</script>

<p class="text-sm text-muted-foreground">
	<a href={resolve('/')}>Projects</a> / <a href={resolve('/p/[project]', { project })}>{project}</a> / releases
</p>
<h1 class="mb-4 text-2xl font-semibold tracking-tight">Compare releases</h1>
<form class="mb-4 flex max-w-2xl flex-wrap items-end gap-2" onsubmit={compare}>
	<label class="grid flex-1 gap-1 text-sm">
		<span class="text-muted-foreground">Releases (comma-separated, up to four)</span>
		<Input bind:value={releases} aria-label="Releases" placeholder="2026-11-17.1, 2026-11-17.2" />
	</label>
	<label class="grid gap-1 text-sm">
		<span class="text-muted-foreground">Window</span>
		<select bind:value={hours} aria-label="Window" class="h-9 rounded-md border bg-background px-2">
			<option value="1">last hour</option>
			<option value="24">last 24 hours</option>
			<option value="168">last 7 days</option>
		</select>
	</label>
	<Button type="submit">Compare</Button>
</form>

{#if result === null}
	<p class="text-muted-foreground">Name the releases to compare.</p>
{:else if result.state === 'ok'}
	{@const c = result.data}
	<p class="mb-2 text-sm text-muted-foreground">
		Operations that started (event time) {time(c.from)} – {time(c.to)}. "No finish observed" and "missing events" are
		telemetry's gaps, not failures and not successes.
	</p>
	<Table>
		<TableHeader>
			<TableRow>
				<TableHead>Release</TableHead>
				<TableHead>Operations</TableHead>
				{#each STATUS_COLUMNS as s (s)}<TableHead>{s.replaceAll('_', ' ')}</TableHead>{/each}
				<TableHead>Missing events</TableHead>
				<TableHead>Dropped by SDK</TableHead>
				<TableHead>Failed / uncertain attempts</TableHead>
				<TableHead>Est. cost (not a bill)</TableHead>
			</TableRow>
		</TableHeader>
		<TableBody>
			{#each c.releases as r (r.release)}
				<TableRow data-release={r.release}>
					<TableCell class="font-mono text-xs"><bdi>{r.release}</bdi></TableCell>
					<TableCell>{r.operations}</TableCell>
					{#each STATUS_COLUMNS as s (s)}
						<TableCell data-status={s}>
							{#if r[s] > 0}<a href={listed(c, r.release, s)}>{r[s]}</a>{:else}0{/if}
						</TableCell>
					{/each}
					<TableCell>{r.missing_events}</TableCell>
					<TableCell>{r.reported_dropped}</TableCell>
					<TableCell>{r.failed_attempts} / {r.uncertain_attempts}</TableCell>
					<TableCell class="font-mono text-xs">
						{r.mixed_currencies ? 'mixed currencies' : r.cost_estimate_micros ? money(r.cost_estimate_micros, null) : '—'}
					</TableCell>
				</TableRow>
			{/each}
		</TableBody>
	</Table>
{:else if result.state === 'forbidden'}
	<Notice kind="fail">Denied: {result.message}. The denial was recorded in the audit log.</Notice>
{:else if result.state === 'rateLimited'}
	<Notice kind="uncertain">Rate limited; retry in {result.retryAfter} s.</Notice>
{:else if result.state === 'needsAuth'}
	<Notice kind="fail">The token is missing, expired or revoked.</Notice>
{:else}
	<Notice kind="fail">The comparison could not be read: {result.state === 'unreachable' ? result.message : 'not found'}</Notice>
{/if}
