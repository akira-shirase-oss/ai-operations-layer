<script lang="ts">
	import { resolve } from '$app/paths';
	import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@aiops/ui/ui/table';
	import { api, type Result } from '#lib/api.js';
	import { time } from '#lib/format.js';
	import type { AuditEntry, AuditPage } from '#lib/types.js';
	import Flag from '#lib/components/Flag.svelte';
	import Notice from '#lib/components/Notice.svelte';
	import Loading from '#lib/components/Loading.svelte';

	let result = $state<Result<AuditPage> | null>(null);
	// Rows accumulated across "Older" pages (keyset — stable while rows arrive).
	let rows = $state<AuditEntry[]>([]);
	let next = $state<[string, string] | null>(null);
	let moreError = $state<Result<AuditPage> | null>(null);
	let action = $state('');
	let actor = $state('');
	let token = $state('');
	let from = $state('');
	let to = $state('');
	let appliedParams = $state(new URLSearchParams());

	// The params travel as an argument, never through $state: the effect reads only
	// the five filter fields — reading appliedParams back here once made the effect
	// feed its own write (effect_update_depth_exceeded + a 403 request storm).
	function query(params: URLSearchParams, cursor: [string, string] | null): string {
		const p = new URLSearchParams(params);
		p.set('limit', '100');
		if (cursor) {
			p.set('before_time', cursor[0]);
			p.set('before_id', cursor[1]);
		}
		return `/v1/admin/audit?${p}`;
	}

	function apply() {
		const p = new URLSearchParams();
		if (action.trim()) p.set('action', action.trim());
		if (actor.trim()) p.set('actor', actor.trim());
		if (token.trim()) p.set('token', token.trim());
		if (from) p.set('from', new Date(from).toISOString());
		if (to) p.set('to', new Date(to).toISOString());
		appliedParams = p;
		result = null;
		rows = [];
		next = null;
		moreError = null;
		api<AuditPage>(query(p, null)).then((r) => {
			result = r;
			if (r.state === 'ok') {
				rows = r.data.audit;
				next = r.data.next;
			}
		});
	}
	$effect(apply);

	function older() {
		if (!next) return;
		api<AuditPage>(query(appliedParams, next)).then((r) => {
			if (r.state === 'ok') {
				rows = [...rows, ...r.data.audit];
				next = r.data.next;
				moreError = null;
			} else {
				moreError = r;
			}
		});
	}
</script>

<p class="text-sm text-muted-foreground"><a href={resolve('/')}>Projects</a> / audit</p>
<h1 class="mt-1 mb-4 text-2xl font-semibold tracking-tight">Audit</h1>
<p class="mb-4 max-w-3xl text-sm text-muted-foreground">
	Every authorization decision the collector audited, newest first — metadata only, never a secret.
	Reading the log does not write to it; denials are decisions and are recorded.
</p>

<form
	class="mb-4 grid grid-cols-2 items-end gap-2 text-sm sm:grid-cols-3 lg:grid-cols-6"
	onsubmit={(ev) => (ev.preventDefault(), apply())}
>
	<label class="flex flex-col gap-1">
		<span class="text-muted-foreground">Action</span>
		<input class="rounded-md border bg-background px-2 py-1" bind:value={action} placeholder="audit.list" />
	</label>
	<label class="flex flex-col gap-1">
		<span class="text-muted-foreground">Actor</span>
		<input class="rounded-md border bg-background px-2 py-1" bind:value={actor} placeholder="ops" />
	</label>
	<label class="flex flex-col gap-1">
		<span class="text-muted-foreground">Token id</span>
		<input class="rounded-md border bg-background px-2 py-1" bind:value={token} placeholder="uuid" />
	</label>
	<label class="flex flex-col gap-1">
		<span class="text-muted-foreground">From</span>
		<input class="rounded-md border bg-background px-2 py-1" type="datetime-local" bind:value={from} />
	</label>
	<label class="flex flex-col gap-1">
		<span class="text-muted-foreground">To</span>
		<input class="rounded-md border bg-background px-2 py-1" type="datetime-local" bind:value={to} />
	</label>
	<div class="flex gap-2">
		<button class="rounded-md border px-3 py-1.5" type="submit">Apply</button>
		{#if appliedParams.size > 0}
			<button
				class="rounded-md border px-3 py-1.5 text-muted-foreground"
				type="button"
				onclick={() => ((action = ''), (actor = ''), (token = ''), (from = ''), (to = ''), apply())}
				>Clear</button
			>
		{/if}
	</div>
</form>

{#if result === null}
	<Loading rows={6} label="Loading the audit log" />
{:else if result.state === 'ok'}
	{#if rows.length === 0}
		<p class="rounded-md border border-dashed p-4 text-sm text-muted-foreground" data-testid="audit-empty">
			No audit entries match{appliedParams.size > 0 ? ' these filters' : ''}.
		</p>
	{:else}
		<Table>
			<TableHeader>
				<TableRow
					><TableHead>When</TableHead><TableHead>Actor</TableHead><TableHead>Action</TableHead><TableHead
						>Project</TableHead
					><TableHead>Target</TableHead><TableHead>Decision</TableHead><TableHead>Reason</TableHead></TableRow
				>
			</TableHeader>
			<TableBody>
				{#each rows as r (r.id)}
					<TableRow data-testid="audit-row">
						<TableCell class="whitespace-nowrap font-mono text-xs">{time(r.at)}</TableCell>
						<TableCell class="font-mono text-xs"><bdi>{r.actor}</bdi></TableCell>
						<TableCell class="font-mono text-xs">{r.action}</TableCell>
						<TableCell class="font-mono text-xs">{r.project_slug ?? '—'}</TableCell>
						<TableCell class="font-mono text-xs"><bdi>{r.target ?? '—'}</bdi></TableCell>
						<TableCell>
							{#if r.decision === 'denied'}<Flag>denied</Flag>{:else}allowed{/if}
						</TableCell>
						<TableCell class="text-xs"><bdi>{r.reason}</bdi></TableCell>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
		{#if moreError}
			<Notice kind="fail">The next page could not be read ({'message' in moreError ? moreError.message : moreError.state}).</Notice>
		{/if}
		{#if next}
			<button class="mt-3 rounded-md border px-3 py-1.5 text-sm" onclick={older}>Older</button>
		{/if}
	{/if}
{:else if result.state === 'forbidden'}
	<Notice kind="fail"
		>Your token cannot read the audit log — this surface needs the <span class="font-mono">admin</span> scope
		({result.message}).</Notice
	>
{:else if result.state === 'needsAuth'}
	<Notice kind="fail">The token is missing, expired or revoked. Sign out and sign in again.</Notice>
{:else if result.state === 'unreachable'}
	<Notice kind="fail">The collector could not be reached: {result.message}</Notice>
{:else}
	<Notice kind="fail">The collector refused the request ({result.state}).</Notice>
{/if}
