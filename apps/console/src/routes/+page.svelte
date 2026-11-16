<script lang="ts">
	import { resolve } from '$app/paths';
	import { api, type Result } from '#lib/api.js';
	import type { ProjectsView } from '#lib/types.js';

	let result: Result<ProjectsView> | null = $state(null);
	$effect(() => {
		api<ProjectsView>('/v1/projects').then((r) => (result = r));
	});
</script>

<h1>Projects</h1>
{#if result === null}
	<p class="muted">Loading…</p>
{:else if result.state === 'ok'}
	<p class="muted">Signed in as {result.data.actor} ({result.data.scopes.join(', ')}).</p>
	{#if result.data.projects.length === 0}
		<p>This token can read no projects.</p>
	{:else}
		<ul>
			{#each result.data.projects as p (p.slug)}
				<li><a href={resolve('/p/[project]', { project: p.slug })}>{p.slug}</a></li>
			{/each}
		</ul>
	{/if}
{:else if result.state === 'needsAuth'}
	<p class="notice fail">The token is missing, expired or revoked. Sign out and sign in again.</p>
{:else if result.state === 'unreachable'}
	<p class="notice fail">The collector could not be reached: {result.message}</p>
{:else}
	<p class="notice fail">The collector refused the request ({result.state}).</p>
{/if}
