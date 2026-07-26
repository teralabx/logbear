<script lang="ts">
	import { fmtStamp } from '$lib/format';
	import type { FingerprintHistoryPoint, Level } from '$lib/api/types';

	/**
	 * Occurrences of one signature across the last ~30 runs, oldest to newest.
	 *
	 * Runs where the signature did not occur still get a tick on the baseline —
	 * an absent bar and a zero bar mean different things, and collapsing them
	 * would hide the most interesting shape of all: the gap where it stopped
	 * happening. Bars are quantised to whole pixels so they stay crisp.
	 */
	let {
		points,
		level = 'error',
		width = 132,
		height = 22
	}: {
		points: FingerprintHistoryPoint[];
		level?: Level;
		width?: number;
		height?: number;
	} = $props();

	const gap = 1;
	const max = $derived(Math.max(1, ...points.map((p) => p.count)));
	const barW = $derived(
		points.length ? Math.max(2, Math.floor((width - gap * (points.length - 1)) / points.length)) : 2
	);
	const baseline = $derived(height - 1);
</script>

<svg
	{width}
	{height}
	viewBox="0 0 {width} {height}"
	role="img"
	aria-label="{points.reduce((a, p) => a + p.count, 0)} occurrences across the last {points.length} runs"
	data-level={level}
	class="spark"
>
	<line x1="0" y1={baseline + 0.5} x2={width} y2={baseline + 0.5} class="axis" />
	{#each points as p, i (p.run_id)}
		{@const x = i * (barW + gap)}
		{@const h = p.count === 0 ? 1 : Math.max(2, Math.round((p.count / max) * (height - 3)))}
		<rect
			{x}
			y={baseline - h}
			width={barW}
			height={h}
			class:zero={p.count === 0}
			class:last={i === points.length - 1}
		>
			<title>run #{p.run_id} · {p.count} × · {fmtStamp(p.started_at)}</title>
		</rect>
	{/each}
</svg>

<style>
	.spark {
		display: block;
		overflow: visible;
	}
	.axis {
		stroke: var(--rl-hair);
		stroke-width: 1;
	}
	rect {
		fill: var(--lvl);
	}
	rect.zero {
		fill: var(--rl-hair-strong);
	}
	rect.last {
		fill: var(--lvl);
	}

	[data-level='error'] {
		--lvl: var(--rl-lvl-error);
	}
	[data-level='warn'] {
		--lvl: var(--rl-lvl-warn);
	}
	[data-level='info'],
	[data-level='debug'],
	[data-level='trace'],
	[data-level='unknown'] {
		--lvl: var(--rl-lvl-info);
	}
</style>
