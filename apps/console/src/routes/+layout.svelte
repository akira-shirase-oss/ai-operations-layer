<script lang="ts">
	import '../app.css';
	import { resolve } from '$app/paths';
	import { ModeWatcher, toggleMode } from 'mode-watcher';
	import { Button } from '@aiops/ui/ui/button';
	import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@aiops/ui/ui/card';
	import { Input } from '@aiops/ui/ui/input';
	import SunMoon from '@lucide/svelte/icons/sun-moon';
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

<!-- The theme script is static in app.html (hashed by the collector's CSP), not injected. -->
<ModeWatcher defaultMode="system" disableHeadScriptInjection />

<div class="flex min-h-dvh flex-col bg-background text-foreground">
	<header class="border-b">
		<div class="mx-auto flex h-14 w-full max-w-6xl items-center justify-between px-4 sm:px-6">
			<a class="font-semibold tracking-tight" href={resolve('/')}>AI Operations Layer</a>
			<div class="flex items-center gap-2">
				<Button variant="ghost" size="icon" aria-label="Toggle dark mode" onclick={toggleMode}>
					<SunMoon aria-hidden="true" />
				</Button>
				{#if signedIn}
					<Button variant="outline" size="sm" onclick={signOut}>Sign out</Button>
				{/if}
			</div>
		</div>
	</header>
	<main class="mx-auto w-full max-w-6xl flex-1 px-4 py-6 sm:px-6 sm:py-8">
		{#if signedIn}
			{@render children()}
		{:else}
			<Card class="mx-auto max-w-lg">
				<CardHeader>
					<CardTitle><h1 class="text-xl">Sign in</h1></CardTitle>
					<CardDescription>
						Paste an API token (<span class="font-mono">aiops_tk_…</span>, minted with
						<span class="font-mono">aiops-server admin create-token</span>). It stays in this tab only.
					</CardDescription>
				</CardHeader>
				<CardContent>
					<form class="flex gap-2" onsubmit={signIn}>
						<Input type="password" autocomplete="off" bind:value={draft} placeholder="aiops_tk_…" />
						<Button type="submit">Sign in</Button>
					</form>
				</CardContent>
			</Card>
		{/if}
	</main>
</div>
