<script lang="ts">
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLAttributes } from "svelte/elements";

	// A placeholder with the size of what is loading, so nothing jumps when it arrives.
	let {
		ref = $bindable(null),
		class: className,
		...restProps
	}: WithElementRef<HTMLAttributes<HTMLDivElement>> = $props();
</script>

<div
	bind:this={ref}
	data-slot="skeleton"
	aria-hidden="true"
	class={cn("animate-pulse rounded-md bg-muted motion-reduce:animate-none", className)}
	{...restProps}
></div>
