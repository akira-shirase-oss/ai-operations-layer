<script lang="ts">
	import '../app.css';
	import { resolve } from '$app/paths';
	import { setToken, token } from '#lib/api.js';

	let { children } = $props();
	let signedIn = $state(token() !== null);
	let draft = $state('');

	function signIn(event: SubmitEvent) {
		event.preventDefault();
		if (!draft.startsWith('aiops_tk_')) return;
		setToken(draft);
		draft = '';
		signedIn = true;
		location.reload();
	}

	function signOut() {
		setToken(null);
		signedIn = false;
		location.href = resolve('/');
	}
</script>

<header class="top">
	<a class="brand" href={resolve('/')}>AI Operations Layer</a>
	{#if signedIn}
		<button onclick={signOut}>Sign out</button>
	{/if}
</header>
<main>
	{#if signedIn}
		{@render children()}
	{:else}
		<h1>Sign in</h1>
		<p class="muted">
			Paste an API token (<span class="mono">aiops_tk_…</span>, minted with
			<span class="mono">aiops-server admin create-token</span>). It stays in this tab only.
		</p>
		<form class="inline" onsubmit={signIn}>
			<input type="password" autocomplete="off" bind:value={draft} placeholder="aiops_tk_…" />
			<button type="submit">Sign in</button>
		</form>
	{/if}
</main>
