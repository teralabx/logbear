<script lang="ts">
	/**
	 * The Delta mark. Two runs side by side; one bar is recoloured to flare and
	 * shifted 3px right.
	 *
	 * The mark drops rows as it shrinks — 4 rows at 32px and up, 3 rows between
	 * 25 and 31, 2 rows at 24 and below, with bar weight roughly doubling. These
	 * are separate geometries, not one asset scaled: shrinking the 4-row version
	 * closes the gaps and it turns to mush. The same three geometries ship as
	 * files in static/brand; they are inlined here so the neutral bars can take
	 * currentColor and work on both the ink and bone grounds.
	 */
	let {
		size = 24,
		class: klass = '',
		title = 'logbear'
	}: { size?: number; class?: string; title?: string } = $props();

	type Bar = { x: number; y: number; w: number; h: number };

	const geometry = $derived.by(() => {
		if (size <= 24) {
			// 2 rows, heaviest bars.
			const h = 10;
			const bars: Bar[] = [
				{ x: 4, y: 16, w: 24, h },
				{ x: 4, y: 38, w: 24, h },
				{ x: 36, y: 38, w: 24, h }
			];
			return { bars, delta: { x: 36, y: 16, w: 24, h } };
		}
		if (size < 32) {
			// 3 rows.
			const h = 7;
			const bars: Bar[] = [
				{ x: 6, y: 13, w: 22, h },
				{ x: 6, y: 28, w: 22, h },
				{ x: 6, y: 43, w: 15, h },
				{ x: 36, y: 13, w: 22, h },
				{ x: 36, y: 43, w: 15, h }
			];
			return { bars, delta: { x: 39, y: 28, w: 22, h } };
		}
		// 4 rows — the full mark.
		const h = 5;
		const bars: Bar[] = [
			{ x: 6, y: 13, w: 22, h },
			{ x: 6, y: 24, w: 22, h },
			{ x: 6, y: 35, w: 15, h },
			{ x: 6, y: 46, w: 22, h },
			{ x: 36, y: 13, w: 22, h },
			{ x: 36, y: 35, w: 15, h },
			{ x: 36, y: 46, w: 22, h }
		];
		return { bars, delta: { x: 39, y: 24, w: 22, h } };
	});
</script>

<svg
	viewBox="0 0 64 64"
	width={size}
	height={size}
	role="img"
	aria-label={title}
	class="block {klass}"
>
	<g fill="currentColor">
		{#each geometry.bars as b (b.x + ':' + b.y)}
			<rect x={b.x} y={b.y} width={b.w} height={b.h} rx={b.h / 2} />
		{/each}
	</g>
	<rect
		x={geometry.delta.x}
		y={geometry.delta.y}
		width={geometry.delta.w}
		height={geometry.delta.h}
		rx={geometry.delta.h / 2}
		fill="var(--color-flare)"
	/>
</svg>
