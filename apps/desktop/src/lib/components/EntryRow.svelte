<script lang="ts">
	import { fmtClock } from '$lib/format';
	import { INLINE } from '$lib/icons';
	import { ChevronRight } from '@lucide/svelte';
	import type { EntryView } from '$lib/api/types';

	/**
	 * One structured entry.
	 *
	 * Multi-line entries collapse to their first line; the rest is one click
	 * away. Stack traces are the common case and an uncollapsed list of them is
	 * unscannable.
	 *
	 * `boundaries: inferred` gets a visible marker. The daemon groups lines into
	 * entries by delimiter when it can and by heuristic when it cannot, and the
	 * difference matters: an inferred grouping may have swallowed the start of
	 * the next entry. Showing it makes the confidence legible instead of assumed.
	 */
	let {
		entry,
		expanded = false,
		highlight = '',
		onToggle = undefined,
		fresh = false
	}: {
		entry: EntryView;
		expanded?: boolean;
		highlight?: string;
		onToggle?: ((e: EntryView) => void) | undefined;
		fresh?: boolean;
	} = $props();

	const lines = $derived(entry.text.split('\n'));
	const first = $derived(lines[0]);
	const rest = $derived(lines.slice(1));
	const multiline = $derived(rest.length > 0);

	/** Split a line around the search term so matches can be marked. */
	function segments(line: string, q: string): { t: string; hit: boolean }[] {
		if (!q) return [{ t: line, hit: false }];
		const out: { t: string; hit: boolean }[] = [];
		const lower = line.toLowerCase();
		const needle = q.toLowerCase();
		let i = 0;
		for (;;) {
			const j = lower.indexOf(needle, i);
			if (j < 0) {
				out.push({ t: line.slice(i), hit: false });
				break;
			}
			if (j > i) out.push({ t: line.slice(i, j), hit: false });
			out.push({ t: line.slice(j, j + needle.length), hit: true });
			i = j + needle.length;
		}
		return out;
	}
</script>

<div
	class="entry-row group grid grid-cols-[2px_16px_62px_1fr] items-start gap-x-1.5 gap-y-0 border-b border-transparent py-px pr-2 hover:bg-[color-mix(in_oklch,var(--color-base-content)_4%,transparent)]"
	class:entry-enter={fresh}
	data-level={entry.level}
	class:is-new={entry.is_new_signature}
>
	<span
		class="min-h-[17px] self-stretch bg-[var(--lvl)] opacity-85 group-data-[level=debug]:opacity-[0.28] group-data-[level=info]:opacity-[0.28] group-data-[level=trace]:opacity-[0.28] group-data-[level=unknown]:opacity-[0.28]"
		aria-hidden="true"
	></span>

	<button
		type="button"
		class="flex h-[18px] cursor-pointer items-center justify-center bg-transparent p-0 text-meta disabled:cursor-default"
		disabled={!multiline}
		aria-expanded={multiline ? expanded : undefined}
		aria-label={multiline ? (expanded ? 'Collapse entry' : 'Expand entry') : 'Single line entry'}
		onclick={() => onToggle?.(entry)}
	>
		{#if multiline}<span
				class="inline-grid transition-transform duration-[120ms]"
				class:rotate-90={expanded}><ChevronRight {...INLINE} aria-hidden="true" /></span
			>{/if}
	</button>

	<time class="tnum meta text-right text-[12px] leading-[18px]" datetime={String(entry.captured_at)}
		>{fmtClock(entry.captured_at)}</time
	>

	<span class="min-w-0 text-[12px] leading-[18px]">
		<span class="whitespace-pre-wrap text-[var(--lvl-text)] [overflow-wrap:anywhere]">
			{#each segments(first, highlight) as s, i (i)}{#if s.hit}<mark
						class="rounded-[1px] bg-[color-mix(in_oklch,var(--color-warning)_40%,transparent)] text-inherit"
						>{s.t}</mark
					>{:else}{s.t}{/if}{/each}
		</span>

		{#if entry.is_new_signature}
			<span
				class="ml-1.5 inline-block rounded-[2px] bg-flare px-1 align-[1px] text-[12px] font-semibold tracking-[0.08em] text-flare-content uppercase"
				title="This signature has never appeared in this project before">new</span
			>
		{/if}

		{#if entry.boundaries === 'inferred'}
			<span
				class="ml-1.5 cursor-help border-b border-dotted border-current text-[12px] text-meta"
				title="Entry boundaries inferred. The daemon could not find an explicit delimiter here, so this grouping is a best guess."
				aria-label="Boundaries inferred">≈</span
			>
		{/if}

		{#if multiline && !expanded}
			<span class="meta ml-1 rounded-[2px] border border-hair px-1 text-[12px] whitespace-nowrap"
				>+{rest.length}</span
			>
		{/if}

		{#if multiline && expanded}
			<pre
				class="mono-block mt-[3px] mb-1.5 block max-w-full overflow-x-auto border-l border-hair-strong bg-code-bg px-2 py-1.5 text-meta">{#each rest as l, i (i)}{#each segments(l, highlight) as s, j (j)}{#if s.hit}<mark
								class="rounded-[1px] bg-[color-mix(in_oklch,var(--color-warning)_40%,transparent)] text-inherit"
								>{s.t}</mark
							>{:else}{s.t}{/if}{/each}{'\n'}{/each}</pre>
			{#if entry.template}
				<div class="mb-1.5 flex items-baseline gap-2">
					<span class="label-xs">signature</span>
					<code class="text-[12px] text-base-content [overflow-wrap:anywhere]">{entry.template}</code>
				</div>
			{/if}
			{#if entry.error_reason}
				<div class="meta mb-1.5 flex items-baseline gap-2 text-[12px]">
					<span class="label-xs">flagged</span>
					{entry.error_reason} · lines {entry.start_line}–{entry.end_line}
				</div>
			{/if}
		{/if}
	</span>
</div>

<style>
	/* Off-screen rows skip layout and paint entirely: a long run is thousands of them,
	   and laying all of them out was most of the cost of opening one. */
	.entry-row {
		content-visibility: auto;
		contain-intrinsic-size: auto 19px;
	}

	/* Attribute-driven custom property mapping: not expressible as a utility. */
	[data-level='error'] {
		--lvl: var(--color-lvl-error);
		--lvl-text: var(--color-lvl-error);
	}
	[data-level='warn'] {
		--lvl: var(--color-lvl-warn);
		--lvl-text: var(--color-lvl-warn);
	}
	[data-level='info'] {
		--lvl: var(--color-lvl-info);
		--lvl-text: var(--color-base-content);
	}
	[data-level='debug'] {
		--lvl: var(--color-lvl-debug);
		--lvl-text: var(--color-meta);
	}
	[data-level='trace'],
	[data-level='unknown'] {
		--lvl: var(--color-lvl-trace);
		--lvl-text: var(--color-meta);
	}
</style>
