<script lang="ts">
	/**
	 * The LogBear mark. A bear silhouette with three logged rows knocked out of
	 * the face; the middle row carries the delta bar, shifted 3 units right and
	 * recoloured to flare.
	 *
	 * The rows are holes punched through the silhouette rather than shapes drawn
	 * on top. That is what keeps the delta bar legible in both states — neutral
	 * inside its slot while it animates, flare once it settles — and it means the
	 * mark works unchanged on the ink and bone grounds, since the holes show
	 * whatever is behind them.
	 *
	 * The middle slot is both wider and taller than the bar it holds. That gap is
	 * load-bearing: at equal height, or with the bar long enough to reach the end
	 * of the slot, the bar's cap merges into the slot wall and the flare reads as
	 * breaking out of the head rather than sitting inside a row.
	 *
	 * Below 20px the ragged rows close up, so the small cut carries a single
	 * heavier one. These are separate geometries, not one asset scaled. Both ship
	 * as files in static/brand (mark.svg and mark-small.svg) and are inlined here
	 * so the silhouette can take currentColor.
	 */
	let {
		size = 24,
		class: klass = '',
		title = 'LogBear',
		animate = false
	}: { size?: number; class?: string; title?: string; animate?: boolean } = $props();

	type Rect = { x: number; y: number; w: number; h: number };

	const rect = (x: number, y: number, w: number, h: number): Rect => ({ x, y, w, h });

	/** Ears and head are the same at every size; only the rows change. */
	const silhouette: Rect[] = [rect(9, 11, 14, 14), rect(41, 11, 14, 14), rect(8, 17, 48, 39)];
	const radii = [7, 7, 15];

	const geometry = $derived.by(() => {
		if (size < 20) {
			// One heavy row: at this size ragged edges read as noise.
			return { rows: [rect(14, 31, 36, 10)], delta: rect(22, 33, 24, 6) };
		}
		// Ragged right edges, so the rows read as logged lines rather than as a
		// menu icon.
		return {
			rows: [rect(16, 23, 30, 7), rect(16, 32, 34, 8), rect(16, 43, 21, 7)],
			delta: rect(24, 34, 21, 4)
		};
	});

	/* Several marks can be on screen at once, so the mask needs its own id. */
	const maskId = $props.id();
</script>

<svg viewBox="0 0 64 64" width={size} height={size} role="img" aria-label={title} class="block {klass}">
	<mask id={maskId}>
		<rect width="64" height="64" fill="#000" />
		{#each silhouette as s, i (s.x + ':' + s.y)}
			<rect x={s.x} y={s.y} width={s.w} height={s.h} rx={radii[i]} fill="#fff" />
		{/each}
		{#each geometry.rows as r (r.x + ':' + r.y)}
			<rect x={r.x} y={r.y} width={r.w} height={r.h} rx={r.h / 2} fill="#000" />
		{/each}
	</mask>

	<rect width="64" height="64" fill="currentColor" mask="url(#{maskId})" />

	<!-- At rest the bar is already offset and flared: the settled state is the
	     correct one, and the animation ends exactly there. -->
	<rect
		class:delta={animate}
		x={geometry.delta.x}
		y={geometry.delta.y}
		width={geometry.delta.w}
		height={geometry.delta.h}
		rx={geometry.delta.h / 2}
		fill="var(--color-flare)"
	/>
</svg>

<style>
	.delta {
		animation: delta-shift 620ms cubic-bezier(0.16, 1, 0.3, 1) 260ms both;
	}

	/* Starts aligned and neutral, so the shift is what carries the meaning. */
	@keyframes delta-shift {
		from {
			transform: translateX(-3px);
			fill: currentColor;
		}
		60% {
			fill: currentColor;
		}
		to {
			transform: translateX(0);
			fill: var(--color-flare);
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.delta {
			animation: none;
		}
	}
</style>
