<script lang="ts">
	import { resolve } from '$app/paths';
	import { Card, CardContent, CardHeader, CardTitle } from '@aiops/ui/ui/card';
	import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@aiops/ui/ui/table';
	import { api, apiPost, type Result } from '#lib/api.js';
	import { time } from '#lib/format.js';
	import type { CollectorStats, Credentials } from '#lib/types.js';
	import Flag from '#lib/components/Flag.svelte';
	import Notice from '#lib/components/Notice.svelte';
	import Loading from '#lib/components/Loading.svelte';

	let result = $state<Result<Credentials> | null>(null);
	let stats = $state<Result<CollectorStats> | null>(null);
	// The credential pending a confirmed revoke ({kind:id}); the click that arms it and
	// the click that fires it are two different gestures (D-034's confirm step).
	let arming = $state('');
	let note = $state('');
	let busy = $state(false);

	function load() {
		api<Credentials>('/v1/admin/credentials').then((r) => (result = r));
		api<CollectorStats>('/v1/collector/stats').then((r) => (stats = r));
	}
	$effect(load);

	async function revoke(kind: 'tokens' | 'ingest-keys', id: string, name: string) {
		const handle = `${kind}:${id}`;
		if (arming !== handle) {
			arming = handle;
			note = `Confirm: revoke ${name}? This cannot be undone — the credential stops working immediately.`;
			return;
		}
		busy = true;
		const r = await apiPost<{ revoked: boolean }>(`/v1/admin/credentials/${kind}/${id}/revoke`, {});
		busy = false;
		arming = '';
		if (r.state === 'ok') {
			note = `${name} revoked — audited.`;
			load();
		} else if (r.state === 'missing') {
			note = `${name} was already revoked or is unknown — nothing changed.`;
			load();
		} else {
			note =
				r.state === 'forbidden'
					? `Refused: ${r.message}`
					: r.state === 'rateLimited'
						? `Rate limited — try again in ${r.retryAfter}s.`
						: `Could not revoke ${name}: ${'message' in r ? r.message : r.state}`;
		}
	}
</script>

<p class="text-sm text-muted-foreground"><a href={resolve('/')}>Projects</a> / credentials</p>
<h1 class="mt-1 mb-4 text-2xl font-semibold tracking-tight">Credentials</h1>
<p class="mb-4 max-w-3xl text-sm text-muted-foreground">
	Every credential the collector holds, with metadata — never the secret (D-034). Minting stays in
	the terminal (<span class="font-mono">aiops-server admin …</span>): a secret shown once there is a
	smaller surface than a browser. Revoking is an incident action: it is audited, and it takes effect
	the next time the credential is presented (the resolver cache may hold it a few minutes).
</p>
{#if note}<p class="mb-4 rounded-md border border-dashed p-2 text-sm" data-testid="revoke-note">{note}</p>{/if}
{#if result === null}
	<Loading rows={6} label="Loading credentials" />
{:else if result.state === 'ok'}
	{@const d = result.data}
	{#if stats?.state === 'ok'}
		{@const s = stats.data}
		<Card class="mb-6" data-testid="collector-stats">
			<CardHeader>
				<CardTitle class="text-sm text-muted-foreground"
					>The collector's own counters — drops and refusals are data, shown, not smoothed over</CardTitle
				>
			</CardHeader>
			<CardContent>
				<dl class="grid grid-cols-2 gap-x-6 gap-y-1 text-sm sm:grid-cols-4">
					<div><dt class="text-muted-foreground">Batches</dt><dd class="font-mono">{s.batches}</dd></div>
					<div><dt class="text-muted-foreground">Events accepted</dt><dd class="font-mono">{s.events_accepted}</dd></div>
					<div><dt class="text-muted-foreground">Duplicates merged</dt><dd class="font-mono">{s.events_duplicate}</dd></div>
					<div><dt class="text-muted-foreground">Refused</dt><dd class="font-mono">{s.events_refused}</dd></div>
					<div><dt class="text-muted-foreground">Conflicting</dt><dd class="font-mono">{s.events_conflicting}</dd></div>
					<div><dt class="text-muted-foreground">Attributes withheld</dt><dd class="font-mono">{s.attributes_withheld}</dd></div>
					<div><dt class="text-muted-foreground">Redactions</dt><dd class="font-mono">{s.redactions}</dd></div>
					<div><dt class="text-muted-foreground">Unauthenticated</dt><dd class="font-mono">{s.unauthenticated}</dd></div>
					<div><dt class="text-muted-foreground">Shed</dt><dd class="font-mono">{s.shed}</dd></div>
					<div><dt class="text-muted-foreground">Rate limited</dt><dd class="font-mono">{s.rate_limited}</dd></div>
					<div><dt class="text-muted-foreground">Denied</dt><dd class="font-mono">{s.denied}</dd></div>
					<div><dt class="text-muted-foreground">Store unavailable</dt><dd class="font-mono">{s.store_unavailable}</dd></div>
					<div><dt class="text-muted-foreground">Auth shed</dt><dd class="font-mono">{s.auth_shed}</dd></div>
				</dl>
			</CardContent>
		</Card>
	{/if}
	<h2 class="mt-6 mb-2 text-lg font-semibold">Projects</h2>
	<Table>
		<TableHeader>
			<TableRow
				><TableHead>Slug</TableHead><TableHead>Opted-in attribute keys</TableHead><TableHead>Created</TableHead></TableRow
			>
		</TableHeader>
		<TableBody>
			{#each d.projects as p (p.id)}
				<TableRow>
					<TableCell class="font-mono text-xs">{p.slug}</TableCell>
					<TableCell class="font-mono text-xs">{p.opt_in_attribute_keys.join(', ') || '—'}</TableCell>
					<TableCell class="font-mono text-xs">{time(p.created_at)}</TableCell>
				</TableRow>
			{/each}
		</TableBody>
	</Table>

	<h2 class="mt-6 mb-2 text-lg font-semibold">Ingest keys</h2>
	<Table>
		<TableHeader>
			<TableRow
				><TableHead>Name</TableHead><TableHead>Project</TableHead><TableHead>Created</TableHead><TableHead
					>State</TableHead
				><TableHead></TableHead></TableRow
			>
		</TableHeader>
		<TableBody>
			{#each d.ingest_keys as k (k.id)}
				<TableRow data-testid="ingest-key">
					<TableCell class="font-mono text-xs"><bdi>{k.name}</bdi></TableCell>
					<TableCell class="font-mono text-xs">{k.project}</TableCell>
					<TableCell class="font-mono text-xs">{time(k.created_at)}</TableCell>
					<TableCell>
						{#if k.revoked_at}<Flag>revoked {time(k.revoked_at)}</Flag>{:else}active{/if}
					</TableCell>
					<TableCell>
						{#if !k.revoked_at}
							<button
								class="rounded-md border px-2 py-1 text-xs disabled:opacity-50 {arming === `ingest-keys:${k.id}`
									? 'border-destructive text-destructive'
									: ''}"
								disabled={busy}
								onclick={() => revoke('ingest-keys', k.id, `ingest key ${k.name}`)}
								>{arming === `ingest-keys:${k.id}` ? 'Confirm revoke' : 'Revoke'}</button
							>
						{/if}
					</TableCell>
				</TableRow>
			{/each}
		</TableBody>
	</Table>

	<h2 class="mt-6 mb-2 text-lg font-semibold">API tokens</h2>
	<Table>
		<TableHeader>
			<TableRow
				><TableHead>Actor</TableHead><TableHead>Scopes</TableHead><TableHead>Projects</TableHead><TableHead
					>Expires</TableHead
				><TableHead>State</TableHead><TableHead></TableHead></TableRow
			>
		</TableHeader>
		<TableBody>
			{#each d.tokens as t (t.id)}
				<TableRow data-testid="token">
					<TableCell class="font-mono text-xs"><bdi>{t.actor}</bdi></TableCell>
					<TableCell class="font-mono text-xs">{t.scopes.join(', ')}</TableCell>
					<TableCell class="font-mono text-xs">{t.projects.join(', ') || '—'}</TableCell>
					<TableCell class="font-mono text-xs">{time(t.expires_at)}</TableCell>
					<TableCell>
						{#if t.revoked_at}<Flag>revoked {time(t.revoked_at)}</Flag>{:else if new Date(t.expires_at) < new Date()}<Flag
								>expired</Flag
							>{:else}active{/if}
					</TableCell>
					<TableCell>
						{#if !t.revoked_at}
							<button
								class="rounded-md border px-2 py-1 text-xs disabled:opacity-50 {arming === `tokens:${t.id}`
									? 'border-destructive text-destructive'
									: ''}"
								disabled={busy}
								onclick={() => revoke('tokens', t.id, `token for ${t.actor}`)}
								>{arming === `tokens:${t.id}` ? 'Confirm revoke' : 'Revoke'}</button
							>
						{/if}
					</TableCell>
				</TableRow>
			{/each}
		</TableBody>
	</Table>
{:else if result.state === 'forbidden'}
	<Notice kind="fail"
		>Your token cannot administer credentials — this surface needs the <span class="font-mono">admin</span> scope
		({result.message}).</Notice
	>
{:else if result.state === 'needsAuth'}
	<Notice kind="fail">The token is missing, expired or revoked. Sign out and sign in again.</Notice>
{:else if result.state === 'unreachable'}
	<Notice kind="fail">The collector could not be reached: {result.message}</Notice>
{:else}
	<Notice kind="fail">The collector refused the request ({result.state}).</Notice>
{/if}
