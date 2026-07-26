<script lang="ts">
	import type { Snippet } from 'svelte';

	let {
		title,
		sub = '',
		actions,
		children
	}: { title: string; sub?: string; actions?: Snippet; children?: Snippet } = $props();
</script>

<header class="head">
	<div class="titles">
		<h2>{title}</h2>
		{#if sub}<span class="sub meta">{sub}</span>{/if}
		{#if children}{@render children()}{/if}
	</div>
	{#if actions}
		<div class="actions">{@render actions()}</div>
	{/if}
</header>

<style>
	.head {
		flex: none;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
		height: 46px;
		padding: 0 16px;
		border-bottom: 1px solid var(--rl-hair);
		background: var(--color-base-100);
	}
	.titles {
		display: flex;
		align-items: baseline;
		gap: 10px;
		min-width: 0;
	}
	h2 {
		font-size: 15px;
		font-weight: 600;
		white-space: nowrap;
	}
	.sub {
		font-size: 11.5px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 8px;
		flex: none;
	}
</style>
