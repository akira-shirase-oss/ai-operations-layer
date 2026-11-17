<script lang="ts">
	import { resolve } from '$app/paths';
	import { Card, CardContent } from '@aiops/ui/ui/card';
	import { api, type Result } from '#lib/api.js';
	import type { ProjectsView } from '#lib/types.js';
	import Notice from '#lib/components/Notice.svelte';

	let result: Result<ProjectsView> | null = $state(null);
	$effect(() => {
		api<ProjectsView>('/v1/projects').then((r) => (result = r));
	});
</script>

<h1 class="mb-4 text-2xl font-semibold tracking-tight">Projects</h1>
{#if result === null}
	<p class="text-muted-foreground">Loading…</p>
{:else if result.state === 'ok'}
	<p class="mb-4 text-muted-foreground">Signed in as {result.data.actor} ({result.data.scopes.join(', ')}).</p>
	{#if result.data.projects.length === 0}
		<p>This token can read no projects.</p>
	{:else}
		<Card>
			<CardContent>
				<ul class="space-y-2">
					{#each result.data.projects as p (p.slug)}
						<li><a class="font-medium underline-offset-4 hover:underline" href={resolve('/p/[project]', { project: p.slug })}>{p.slug}</a></li>
					{/each}
				</ul>
			</CardContent>
		</Card>
	{/if}
{:else if result.state === 'needsAuth'}
	<Notice kind="fail">The token is missing, expired or revoked. Sign out and sign in again.</Notice>
{:else if result.state === 'unreachable'}
	<Notice kind="fail">The collector could not be reached: {result.message}</Notice>
{:else}
	<Notice kind="fail">The collector refused the request ({result.state}).</Notice>
{/if}
