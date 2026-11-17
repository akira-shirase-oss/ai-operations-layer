<script lang="ts">
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLSelectAttributes } from "svelte/elements";

	// The platform select, styled like Input: keyboard, screen readers and mobile pickers
	// work as the platform makes them.
	let {
		ref = $bindable(null),
		value = $bindable(),
		class: className,
		children,
		...restProps
	}: WithElementRef<HTMLSelectAttributes> = $props();
</script>

<select
	bind:this={ref}
	bind:value
	data-slot="native-select"
	class={cn(
		"h-11 w-full min-w-0 rounded-lg border border-input bg-card px-3 text-base outline-none transition-[color,box-shadow,border-color] md:text-sm focus-visible:border-ring focus-visible:ring-4 focus-visible:ring-ring/15 disabled:cursor-not-allowed disabled:opacity-50",
		className
	)}
	{...restProps}
>
	{@render children?.()}
</select>
