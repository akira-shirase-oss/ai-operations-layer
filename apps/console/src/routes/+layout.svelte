<script lang="ts">
	import '../app.css';
	import { resolve } from '$app/paths';
	import { ModeWatcher, toggleMode } from 'mode-watcher';
	import { Button } from '@aiops/ui/ui/button';
	import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@aiops/ui/ui/card';
	import { Input } from '@aiops/ui/ui/input';
	import SunMoon from '@lucide/svelte/icons/sun-moon';
	import { api, setToken, token } from '#lib/api.js';
	import type { ProjectsView } from '#lib/types.js';

	let { children } = $props();
	let signedIn = $state(token() !== null);
	let draft = $state('');
	let signError = $state('');
	let checking = $state(false);
	let inputRef: HTMLInputElement | null = $state(null);

	async function signIn(event: SubmitEvent) {
		event.preventDefault();
		signError = '';
		const t = draft.trim();
		if (!t.startsWith('aiops_tk_')) {
			signError = 'That does not look like an API token — tokens start aiops_tk_.';
			inputRef?.focus();
			return;
		}
		// The token's truth is the collector's answer, not its shape: check it for real,
		// and roll the session back if it refuses or never answers.
		checking = true;
		setToken(t);
		const r = await api<ProjectsView>('/v1/projects');
		checking = false;
		if (r.state === 'ok') {
			draft = '';
			signedIn = true;
			location.reload();
			return;
		}
		setToken(null);
		signError =
			r.state === 'needsAuth'
				? 'The collector did not accept that token — it is expired, revoked or wrong.'
				: r.state === 'unreachable'
					? `The collector could not be reached — the token was not checked. (${r.message})`
					: `The token could not be verified (${r.state}).`;
		inputRef?.focus();
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
				{#if signedIn}
					<a class="text-sm text-muted-foreground underline-offset-4 hover:underline" href={resolve('/audit')}
						>Audit</a
					>
					<a class="text-sm text-muted-foreground underline-offset-4 hover:underline" href={resolve('/credentials')}
						>Credentials</a
					>
				{/if}
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
					<form class="flex flex-col gap-2" onsubmit={signIn}>
						<div class="flex gap-2">
							<Input
								type="password"
								autocomplete="off"
								bind:value={draft}
								bind:ref={inputRef}
								placeholder="aiops_tk_…"
								aria-label="API token"
								aria-invalid={signError !== ''}
								aria-describedby={signError ? 'signin-error' : undefined}
							/>
							<Button type="submit" disabled={checking}>{checking ? 'Checking…' : 'Sign in'}</Button>
						</div>
						{#if signError}<p id="signin-error" role="alert" class="text-sm text-state-fail-ink">{signError}</p>{/if}
					</form>
				</CardContent>
			</Card>
		{/if}
	</main>
</div>
