<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		title,
		sub = '',
		actions,
		children
	}: { title: string; sub?: string; actions?: Snippet; children?: Snippet } = $props();
</script>

<header
	class="rule-b flex h-[46px] flex-none items-center justify-between gap-4 bg-base-100 px-4"
>
	<div class="flex min-w-0 items-baseline gap-2.5">
		<h2 class="text-[15px] font-semibold whitespace-nowrap">{title}</h2>
		{#if sub}<span class="meta truncate text-[12px]">{sub}</span>{/if}
		{#if children}{@render children()}{/if}
	</div>
	{#if actions}
		<div class="flex flex-none items-center gap-2">{@render actions()}</div>
	{/if}
</header>
