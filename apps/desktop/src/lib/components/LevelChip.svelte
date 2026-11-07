<script lang="ts">
	import type { Level } from '#lib/api/types';

	let {
		level,
		count = null,
		active = null,
		onclick = undefined,
		title = undefined
	}: {
		level: Level;
		count?: number | null;
		/** When non-null the chip renders as a filter toggle. */
		active?: boolean | null;
		onclick?: (() => void) | undefined;
		title?: string | undefined;
	} = $props();

	const shortLabel: Record<Level, string> = {
		error: 'error',
		warn: 'warn',
		info: 'info',
		debug: 'debug',
		trace: 'trace',
		unknown: 'other'
	};
</script>

{#if onclick}
	<button
		type="button"
		{onclick}
		{title}
		class="inline-flex h-5 cursor-pointer items-center gap-[5px] rounded-[2px] border px-[7px] text-[12px] leading-none whitespace-nowrap transition-[border-color,color,background-color] duration-[120ms] hover:border-hair-strong hover:text-base-content {active
			? 'border-[color-mix(in_oklch,var(--lvl)_55%,transparent)] bg-[color-mix(in_oklch,var(--lvl)_12%,transparent)] text-base-content'
			: 'border-hair bg-transparent text-meta'}"
		data-level={level}
		aria-pressed={active ?? undefined}
	>
		<span class="h-1.5 w-1.5 flex-none rounded-[1px] bg-[var(--lvl)]"></span>
		<span>{shortLabel[level]}</span>
		{#if count !== null}<span
				class="tnum font-medium {active ? 'text-base-content' : 'text-meta'}">{count}</span
			>{/if}
	</button>
{:else}
	<span
		class="inline-flex h-5 items-center gap-[5px] rounded-[2px] border border-[color-mix(in_oklch,var(--lvl)_38%,transparent)] bg-transparent px-[7px] text-[12px] leading-none whitespace-nowrap text-[var(--lvl)]"
		data-level={level}
		{title}
	>
		<span class="h-1.5 w-1.5 flex-none rounded-[1px] bg-[var(--lvl)]"></span>
		<span>{shortLabel[level]}</span>
		{#if count !== null}<span class="tnum font-medium text-base-content">{count}</span>{/if}
	</span>
{/if}

<style>
	/* Attribute-driven custom property mapping: not expressible as a utility. */
	[data-level='error'] {
		--lvl: var(--color-lvl-error);
	}
	[data-level='warn'] {
		--lvl: var(--color-lvl-warn);
	}
	[data-level='info'] {
		--lvl: var(--color-lvl-info);
	}
	[data-level='debug'] {
		--lvl: var(--color-lvl-debug);
	}
	[data-level='trace'] {
		--lvl: var(--color-lvl-trace);
	}
	[data-level='unknown'] {
		--lvl: var(--color-lvl-unknown);
	}
</style>
