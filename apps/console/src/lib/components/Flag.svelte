<script lang="ts">
	import type { Snippet } from 'svelte';
	import { Badge } from '@aiops/ui/ui/badge';

	// A small fact about a row (a loss, a lateness, a policy mark): quiet, never a state.
	let { children }: { children: Snippet } = $props();
</script>

<Badge variant="secondary" class="flag mr-1 font-normal">{@render children()}</Badge>
