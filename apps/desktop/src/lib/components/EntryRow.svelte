<script lang="ts">
	import { fmtClock } from '$lib/format';
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
	class="row"
	class:entry-enter={fresh}
	data-level={entry.level}
	class:is-new={entry.is_new_signature}
>
	<span class="stripe" aria-hidden="true"></span>

	<button
		type="button"
		class="disclose"
		disabled={!multiline}
		aria-expanded={multiline ? expanded : undefined}
		aria-label={multiline ? (expanded ? 'Collapse entry' : 'Expand entry') : 'Single line entry'}
		onclick={() => onToggle?.(entry)}
	>
		{#if multiline}<span class="caret" class:open={expanded}>›</span>{/if}
	</button>

	<time class="ts tnum meta" datetime={String(entry.captured_at)}>{fmtClock(entry.captured_at)}</time>

	<span class="body">
		<span class="first">
			{#each segments(first, highlight) as s, i (i)}{#if s.hit}<mark>{s.t}</mark>{:else}{s.t}{/if}{/each}
		</span>

		{#if entry.is_new_signature}
			<span class="new-tag" title="This signature has never appeared in this project before"
				>new</span
			>
		{/if}

		{#if entry.boundaries === 'inferred'}
			<span
				class="inferred"
				title="Entry boundaries inferred. The daemon could not find an explicit delimiter here, so this grouping is a best guess."
				aria-label="Boundaries inferred">≈</span
			>
		{/if}

		{#if multiline && !expanded}
			<span class="more meta">+{rest.length}</span>
		{/if}

		{#if multiline && expanded}
			<pre class="rest mono-block">{#each rest as l, i (i)}{#each segments(l, highlight) as s, j (j)}{#if s.hit}<mark
								>{s.t}</mark
							>{:else}{s.t}{/if}{/each}{'\n'}{/each}</pre>
			{#if entry.template}
				<div class="template">
					<span class="label-xs">signature</span>
					<code>{entry.template}</code>
				</div>
			{/if}
			{#if entry.error_reason}
				<div class="reason meta">
					<span class="label-xs">flagged</span>
					{entry.error_reason} · lines {entry.start_line}–{entry.end_line}
				</div>
			{/if}
		{/if}
	</span>
</div>

<style>
	.row {
		display: grid;
		grid-template-columns: 2px 16px 62px 1fr;
		align-items: start;
		gap: 0 6px;
		padding: 1px 8px 1px 0;
		border-bottom: 1px solid transparent;
	}
	.row:hover {
		background: color-mix(in oklch, var(--color-base-content) 4%, transparent);
	}
	.stripe {
		align-self: stretch;
		background: var(--lvl);
		opacity: 0.85;
		min-height: 17px;
	}
	[data-level='info'] .stripe,
	[data-level='debug'] .stripe,
	[data-level='trace'] .stripe,
	[data-level='unknown'] .stripe {
		opacity: 0.28;
	}
	.disclose {
		background: none;
		border: 0;
		padding: 0;
		height: 18px;
		color: var(--rl-meta);
		cursor: pointer;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.disclose:disabled {
		cursor: default;
	}
	.caret {
		display: inline-block;
		transition: transform 120ms;
		font-size: 13px;
		line-height: 1;
	}
	.caret.open {
		transform: rotate(90deg);
	}
	.ts {
		font-size: 11px;
		line-height: 18px;
		text-align: right;
	}
	.body {
		min-width: 0;
		font-size: 12px;
		line-height: 18px;
	}
	.first {
		color: var(--lvl-text);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.rest {
		display: block;
		margin: 3px 0 6px;
		padding: 6px 8px;
		background: var(--rl-code-bg);
		border-left: 1px solid var(--rl-hair-strong);
		color: var(--rl-meta);
		overflow-x: auto;
		max-width: 100%;
	}
	.template {
		margin: 0 0 6px;
		display: flex;
		gap: 8px;
		align-items: baseline;
	}
	.template code {
		font-size: 11px;
		color: var(--color-base-content);
		overflow-wrap: anywhere;
	}
	.reason {
		margin: 0 0 6px;
		font-size: 11px;
		display: flex;
		gap: 8px;
		align-items: baseline;
	}
	.more {
		font-size: 10px;
		padding: 0 4px;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		margin-left: 4px;
		white-space: nowrap;
	}
	.new-tag {
		display: inline-block;
		margin-left: 6px;
		padding: 0 4px;
		font-size: 10px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		background: var(--color-flare);
		color: #180703;
		border-radius: 2px;
		vertical-align: 1px;
	}
	.inferred {
		margin-left: 6px;
		color: var(--rl-meta);
		font-size: 12px;
		cursor: help;
		border-bottom: 1px dotted currentColor;
	}
	mark {
		background: color-mix(in oklch, var(--color-warning) 40%, transparent);
		color: inherit;
		border-radius: 1px;
	}

	[data-level='error'] {
		--lvl: var(--rl-lvl-error);
		--lvl-text: var(--rl-lvl-error);
	}
	[data-level='warn'] {
		--lvl: var(--rl-lvl-warn);
		--lvl-text: var(--rl-lvl-warn);
	}
	[data-level='info'] {
		--lvl: var(--rl-lvl-info);
		--lvl-text: var(--color-base-content);
	}
	[data-level='debug'] {
		--lvl: var(--rl-lvl-debug);
		--lvl-text: var(--rl-meta);
	}
	[data-level='trace'],
	[data-level='unknown'] {
		--lvl: var(--rl-lvl-trace);
		--lvl-text: var(--rl-meta);
	}
</style>
