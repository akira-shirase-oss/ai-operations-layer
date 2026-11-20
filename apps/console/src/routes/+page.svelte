<script lang="ts">
	import { resolve } from '$app/paths';
	import { Card, CardContent } from '@aiops/ui/ui/card';
	import { api, type Result } from '#lib/api.js';
	import type { ProjectsView } from '#lib/types.js';
	import Loading from '#lib/components/Loading.svelte';
	import Failure from '#lib/components/Failure.svelte';
	import Notice from '#lib/components/Notice.svelte';

	let result: Result<ProjectsView> | null = $state(null);
	async function load() {
		result = await api<ProjectsView>('/v1/projects');
	}
	$effect(() => {
		load();
	});
</script>

<h1 class="mb-4 text-2xl font-semibold tracking-tight">Projects</h1>
{#if result === null}
	<Loading rows={4} label="Loading your projects" />
{:else if result.state === 'ok'}
	<p class="mb-4 text-muted-foreground">Signed in as {result.data.actor} ({result.data.scopes.join(', ')}).</p>
	{#if result.data.projects.length === 0}
		<Notice kind="unknown" title="No projects this token can read.">
			Ask an operator for a token scoped to a project — reads outside a token's scope are denied and recorded.
		</Notice>
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
{:else}
	<Failure {result} retry={load} />
{/if}
