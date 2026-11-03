<script lang="ts">
	/**
	 * The LogBear mark. A bear silhouette with three logged rows knocked out of
	 * the face; the middle row carries the delta bar, which travels the length of
	 * its slot when animated and parks at the right end when not.
	 *
	 * The rows are holes punched through the silhouette rather than shapes drawn
	 * on top. That is what keeps the delta bar legible across the whole travel,
	 * and it means the mark works unchanged on the ink and bone grounds, since
	 * the holes show whatever is behind them.
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

	/* `delta` is the bar at the left end of its travel and `travel` is how far it
	   slides to reach the right end. Both slots keep a padding of 2 on every side
	   the bar can touch, so the two geometries travel different distances: the
	   heavy row is wider but carries a wider bar. */
	const geometry = $derived.by(() => {
		if (size < 20) {
			// One heavy row: at this size ragged edges read as noise.
			return { rows: [rect(14, 31, 36, 10)], delta: rect(16, 33, 24, 6), travel: 8 };
		}
		// Ragged right edges, so the rows read as logged lines rather than as a
		// menu icon.
		return {
			rows: [rect(16, 23, 30, 7), rect(16, 32, 34, 8), rect(16, 43, 21, 7)],
			delta: rect(18, 34, 21, 4),
			travel: 9
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

	<!-- A still mark parks at the right end, which is where the icon files in
	     static/brand sit. -->
	<rect
		class:delta={animate}
		class:parked={!animate}
		style="--delta-travel: {geometry.travel}px"
		x={geometry.delta.x}
		y={geometry.delta.y}
		width={geometry.delta.w}
		height={geometry.delta.h}
		rx={geometry.delta.h / 2}
		fill="var(--color-flare)"
	/>
</svg>

<style>
	.parked {
		transform: translateX(var(--delta-travel));
	}

	.delta {
		animation: delta-shift 2600ms ease-in-out infinite alternate;
	}

	/* Travel is in SVG user units, so it holds against the 64 viewBox at any
	   rendered size. */
	@keyframes delta-shift {
		from {
			transform: translateX(0);
		}
		to {
			transform: translateX(var(--delta-travel));
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.delta {
			animation: none;
			transform: translateX(var(--delta-travel));
		}
	}
</style>
