<script lang="ts">
	import type { Level } from '$lib/api/types';

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
		class="chip"
		class:on={active}
		data-level={level}
		aria-pressed={active ?? undefined}
	>
		<span class="dot"></span>
		<span>{shortLabel[level]}</span>
		{#if count !== null}<span class="tnum n">{count}</span>{/if}
	</button>
{:else}
	<span class="chip static" data-level={level} {title}>
		<span class="dot"></span>
		<span>{shortLabel[level]}</span>
		{#if count !== null}<span class="tnum n">{count}</span>{/if}
	</span>
{/if}

<style>
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		height: 20px;
		padding: 0 7px;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		font-size: 11px;
		line-height: 1;
		color: var(--rl-meta);
		background: transparent;
		white-space: nowrap;
		transition:
			border-color 120ms,
			color 120ms,
			background-color 120ms;
	}
	button.chip {
		cursor: pointer;
	}
	button.chip:hover {
		border-color: var(--rl-hair-strong);
		color: var(--color-base-content);
	}
	.chip.on {
		border-color: color-mix(in oklch, var(--lvl) 55%, transparent);
		background: color-mix(in oklch, var(--lvl) 12%, transparent);
		color: var(--color-base-content);
	}
	.chip.static {
		color: var(--lvl);
		border-color: color-mix(in oklch, var(--lvl) 38%, transparent);
	}
	.dot {
		width: 6px;
		height: 6px;
		border-radius: 1px;
		background: var(--lvl);
		flex: none;
	}
	.n {
		color: var(--color-base-content);
		font-weight: 500;
	}
	.chip:not(.on):not(.static) .n {
		color: var(--rl-meta);
	}

	[data-level='error'] {
		--lvl: var(--rl-lvl-error);
	}
	[data-level='warn'] {
		--lvl: var(--rl-lvl-warn);
	}
	[data-level='info'] {
		--lvl: var(--rl-lvl-info);
	}
	[data-level='debug'] {
		--lvl: var(--rl-lvl-debug);
	}
	[data-level='trace'] {
		--lvl: var(--rl-lvl-trace);
	}
	[data-level='unknown'] {
		--lvl: var(--rl-lvl-unknown);
	}
</style>
