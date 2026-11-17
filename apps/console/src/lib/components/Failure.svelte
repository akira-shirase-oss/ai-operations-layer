<script lang="ts">
	import { resolve } from '$app/paths';
	import { Button } from '@aiops/ui/ui/button';
	import Notice from '#lib/components/Notice.svelte';
	import type { Result } from '#lib/api.js';

	// Every way a read can fail, in plain words, each with a way forward.
	let { result, retry }: { result: Exclude<Result<unknown>, { state: 'ok' }>; retry: () => void } = $props();
</script>

{#if result.state === 'needsAuth'}
	<Notice kind="fail" title="You are signed out.">
		Your token is missing, expired or revoked. <a href={resolve('/')}>Sign in again</a> with a current token.
	</Notice>
{:else if result.state === 'forbidden'}
	<Notice kind="fail" title="This token cannot read this project.">
		The denial was recorded in the audit log. Ask an operator for a token that includes the project, or go back to
		<a href={resolve('/')}>your projects</a>.
		<details class="mt-1 text-xs"><summary>Details</summary><code>{result.message}</code></details>
	</Notice>
{:else if result.state === 'rateLimited'}
	<Notice kind="uncertain" title="Too many requests for now.">
		Wait {result.retryAfter} s, then <Button variant="outline" size="sm" onclick={retry}>Try again</Button>
	</Notice>
{:else if result.state === 'missing'}
	<Notice kind="unknown" title="Nothing was found here.">
		No events have been received for it. That is not proof that it did not run.
	</Notice>
{:else}
	<Notice kind="fail" title="The collector could not be reached.">
		Check that it is running, then <Button variant="outline" size="sm" onclick={retry}>Try again</Button>
		<details class="mt-1 text-xs"><summary>Details</summary><code>{result.message}</code></details>
	</Notice>
{/if}
