<script lang="ts">
	import { page } from '$app/state';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import TerminalPane from '$lib/components/TerminalPane.svelte';
	import EntryRow from '$lib/components/EntryRow.svelte';
	import LevelChip from '$lib/components/LevelChip.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import ExitBadge from '$lib/components/ExitBadge.svelte';
	import { apiError, entries, listRuns, projectRef, rawStream } from '$lib/api';
	import { entryToAnsi } from '$lib/ansi';
	import { app } from '$lib/stores/app.svelte';
	import {
		fmtCommand,
		fmtDuration,
		fmtStamp,
		runDuration,
		runElapsed,
		shortSha,
		tildePath
	} from '$lib/format';
	import { LEVELS, type EntryView, type Level, type Run } from '$lib/api/types';

	/**
	 * The screen people leave open.
	 *
	 * Two views of the same bytes, side by side vertically: the terminal replay
	 * (what you actually saw, ANSI intact) and the structured entry list (what
	 * runlog made of it). Keeping both visible is the point — when the parse is
	 * wrong you can see that it is wrong.
	 */

	const projectId = $derived(Number(page.params.id));
	const project = $derived(app.project(projectId));
	const status = $derived(app.statuses[projectId]);

	// The run in view: whatever is running, else the most recent completed run.
	let run = $state<Run | null>(null);
	let list = $state<EntryView[]>([]);
	let loading = $state(true);
	let loadError = $state<string | null>(null);

	let follow = $state(true);
	let search = $state('');
	let levelFilter = $state<Set<Level>>(new Set());
	let expanded = $state<Set<number>>(new Set());
	let freshIds = $state<Set<number>>(new Set());
	let view = $state<'split' | 'raw' | 'entries'>('split');
	let scrolledUp = $state(false);
	let missedWhileScrolled = $state(0);

	let termRef = $state<ReturnType<typeof TerminalPane> | undefined>();
	let listEl = $state<HTMLDivElement | undefined>();
	let termHeightPct = $state(38);

	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});

	const MAX_BUFFER = 5000;

	const counters = $derived(run ? (app.counters[run.id] ?? run.counters) : null);

	const filtered = $derived.by(() => {
		const q = search.trim().toLowerCase();
		return list.filter((e) => {
			if (levelFilter.size && !levelFilter.has(e.level)) return false;
			if (q && !e.text.toLowerCase().includes(q)) return false;
			return true;
		});
	});

	const inferredCount = $derived(filtered.filter((e) => e.boundaries === 'inferred').length);

	/** Load the run in view and its stored output. Re-runs when the project changes. */
	$effect(() => {
		const id = projectId;
		if (!Number.isFinite(id)) return;
		let cancelled = false;
		loading = true;
		loadError = null;

		(async () => {
			try {
				const ref = projectRef(id);
				const runs = await listRuns(ref, 1, 0);
				const active = app.statuses[id]?.active_runs ?? [];
				const target = active[0] ?? runs[0] ?? null;
				if (cancelled) return;
				run = target;
				if (!target) {
					list = [];
					loading = false;
					return;
				}
				const [es, raw] = await Promise.all([
					entries(ref, { run_id: target.id, limit: MAX_BUFFER, ascending: true }),
					rawStream(ref, target.id).catch(() => '')
				]);
				if (cancelled) return;
				list = es;
				termRef?.replace(raw);
				termRef?.scrollToBottom();
				loading = false;
				queueMicrotask(() => scrollListToBottom());
			} catch (err) {
				if (!cancelled) {
					loadError = apiError(err);
					loading = false;
				}
			}
		})();

		return () => {
			cancelled = true;
		};
	});

	/** Live tail. Entries for the run in view append to both panes. */
	$effect(() => {
		const current = run;
		if (!current) return;
		return app.onEvent((e) => {
			if (e.event === 'entries' && e.run_id === current.id) {
				const next = list.concat(e.entries);
				list = next.length > MAX_BUFFER ? next.slice(next.length - MAX_BUFFER) : next;

				const ids = new Set(freshIds);
				for (const en of e.entries) {
					ids.add(en.id);
					termRef?.write(entryToAnsi(en));
				}
				freshIds = ids;
				setTimeout(() => {
					const drop = new Set(freshIds);
					for (const en of e.entries) drop.delete(en.id);
					freshIds = drop;
				}, 500);

				if (follow && !scrolledUp) {
					queueMicrotask(() => scrollListToBottom());
					termRef?.scrollToBottom();
				} else {
					missedWhileScrolled += e.entries.length;
				}
			}
			if (e.event === 'run_finished' && e.run.id === current.id) run = e.run;
			if (e.event === 'run_started' && e.run.project_id === current.project_id) {
				// A new run in this project takes over the view.
				run = e.run;
				list = [];
				termRef?.replace('');
			}
		});
	});

	function scrollListToBottom() {
		if (!listEl) return;
		listEl.scrollTop = listEl.scrollHeight;
		scrolledUp = false;
		missedWhileScrolled = 0;
	}

	function onListScroll() {
		if (!listEl) return;
		const gap = listEl.scrollHeight - listEl.scrollTop - listEl.clientHeight;
		const wasUp = scrolledUp;
		scrolledUp = gap > 40;
		if (wasUp && !scrolledUp) missedWhileScrolled = 0;
	}

	function toggleLevel(l: Level) {
		const next = new Set(levelFilter);
		if (next.has(l)) next.delete(l);
		else next.add(l);
		levelFilter = next;
	}

	function toggleEntry(e: EntryView) {
		const next = new Set(expanded);
		if (next.has(e.id)) next.delete(e.id);
		else next.add(e.id);
		expanded = next;
	}

	function countFor(l: Level): number {
		if (!counters) return 0;
		switch (l) {
			case 'error':
				return counters.n_error;
			case 'warn':
				return counters.n_warn;
			case 'info':
				return counters.n_info;
			case 'debug':
				return counters.n_debug;
			case 'trace':
				return counters.n_trace;
			default:
				return counters.n_unknown;
		}
	}

	function searchTerminal(dir: 1 | -1) {
		const q = search.trim();
		if (!q) return;
		if (dir === 1) termRef?.findNext(q);
		else termRef?.findPrevious(q);
	}

	/* --- draggable split ------------------------------------------------- */
	let dragging = $state(false);
	let splitEl = $state<HTMLDivElement | undefined>();

	function startDrag(ev: PointerEvent) {
		dragging = true;
		(ev.target as HTMLElement).setPointerCapture(ev.pointerId);
	}
	function onDrag(ev: PointerEvent) {
		if (!dragging || !splitEl) return;
		const r = splitEl.getBoundingClientRect();
		const pct = ((ev.clientY - r.top) / r.height) * 100;
		termHeightPct = Math.min(78, Math.max(12, pct));
		termRef?.refit();
	}
	function endDrag(ev: PointerEvent) {
		dragging = false;
		(ev.target as HTMLElement).releasePointerCapture(ev.pointerId);
		termRef?.refit();
	}
</script>

<PageHeader
	title="Live"
	sub={project ? `${project.name} · ${tildePath(project.root)}` : ''}
>
	{#snippet actions()}
		<div class="viewswitch" role="group" aria-label="Pane layout">
			<button type="button" class:on={view === 'split'} onclick={() => (view = 'split')}
				>split</button
			>
			<button type="button" class:on={view === 'raw'} onclick={() => (view = 'raw')}>terminal</button
			>
			<button type="button" class:on={view === 'entries'} onclick={() => (view = 'entries')}
				>entries</button
			>
		</div>
	{/snippet}
</PageHeader>

{#if run}
	<div class="runbar">
		<span class="pip" class:live={run.ended_at === null} aria-hidden="true"></span>
		<code class="cmd">{fmtCommand(run.command)}</code>
		<ExitBadge {run} />
		<span class="meta tnum sep">
			{run.ended_at === null
				? `${fmtDuration(runElapsed(run, now))} elapsed`
				: fmtDuration(runDuration(run))}
		</span>
		<span class="meta tnum sep">{run.git_branch ?? 'no branch'}@{shortSha(run.git_sha)}</span>
		{#if run.git_dirty_hash}<span class="dirty">dirty</span>{/if}
		<span class="meta tnum sep">started {fmtStamp(run.started_at)}</span>
		<span class="meta sep" title="How the daemon split this stream into entries"
			>delimiter: {run.delimiter_mode}</span
		>
		<a class="detail" href="/project/{projectId}/runs/{run.id}">run detail →</a>
	</div>
{/if}

{#if loadError}
	<div class="pad"><ErrorNote what="Could not read this run." detail={loadError} /></div>
{:else if loading}
	<p class="pad meta">Loading stored output…</p>
{:else if !run}
	<div class="pad">
		<EmptyState
			title="No runs recorded for this project yet"
			body="Start a command through runlog from {project ? tildePath(project.root) : 'the project root'} and its output will stream here."
			command="runlog mix phx.server"
		/>
	</div>
{:else}
	<div
		class="split"
		bind:this={splitEl}
		style="--term-h: {view === 'split' ? termHeightPct : view === 'raw' ? 100 : 0}%"
		data-view={view}
	>
		<section class="termwrap" aria-label="Terminal replay">
			<TerminalPane bind:this={termRef} cols={run.term_cols} />
		</section>

		{#if view === 'split'}
			<button
				type="button"
				class="divider"
				aria-label="Resize terminal pane — drag, or use the up and down arrow keys"
				onpointerdown={startDrag}
				onpointermove={onDrag}
				onpointerup={endDrag}
				onkeydown={(e) => {
					if (e.key === 'ArrowUp') termHeightPct = Math.max(12, termHeightPct - 4);
					if (e.key === 'ArrowDown') termHeightPct = Math.min(78, termHeightPct + 4);
					termRef?.refit();
				}}
			>
				<span class="grip"></span>
			</button>
		{/if}

		<section class="entrywrap" aria-label="Structured entries">
			<div class="controls">
				<div class="chips">
					{#each LEVELS as l (l)}
						<LevelChip
							level={l}
							count={countFor(l)}
							active={levelFilter.has(l)}
							onclick={() => toggleLevel(l)}
							title="Show only {l} entries"
						/>
					{/each}
					{#if levelFilter.size}
						<button type="button" class="clear" onclick={() => (levelFilter = new Set())}
							>clear filter</button
						>
					{/if}
				</div>

				<div class="right">
					<div class="searchbox">
						<input
							type="search"
							placeholder="Filter entries…"
							bind:value={search}
							aria-label="Filter entries by text"
							onkeydown={(e) => {
								if (e.key === 'Enter') searchTerminal(e.shiftKey ? -1 : 1);
							}}
						/>
						{#if search.trim()}
							<span class="hits tnum">{filtered.length}</span>
							<button
								type="button"
								class="tbtn"
								onclick={() => searchTerminal(-1)}
								title="Find previous match in the terminal pane">↑</button
							>
							<button
								type="button"
								class="tbtn"
								onclick={() => searchTerminal(1)}
								title="Find next match in the terminal pane">↓</button
							>
						{/if}
					</div>

					<button
						type="button"
						class="follow"
						class:on={follow}
						aria-pressed={follow}
						onclick={() => {
							follow = !follow;
							if (follow) scrollListToBottom();
						}}
						title={follow
							? 'Following the tail — new entries scroll into view'
							: 'Not following — new entries arrive below without scrolling'}
					>
						<span class="fdot" class:live={follow && run.ended_at === null}></span>
						follow tail
					</button>
				</div>
			</div>

			{#if inferredCount > 0}
				<p class="inferred-note meta">
					<span class="glyph">≈</span>
					{inferredCount} of {filtered.length} entries had their boundaries inferred — no explicit delimiter
					was found, so the grouping is a best guess.
				</p>
			{/if}

			<div class="entries pane" bind:this={listEl} onscroll={onListScroll}>
				{#if filtered.length === 0}
					<p class="none meta">
						{list.length === 0
							? 'No entries stored for this run yet.'
							: 'No entries match this filter.'}
					</p>
				{:else}
					{#each filtered as e (e.id)}
						<EntryRow
							entry={e}
							expanded={expanded.has(e.id)}
							highlight={search.trim()}
							onToggle={toggleEntry}
							fresh={freshIds.has(e.id)}
						/>
					{/each}
				{/if}
			</div>

			{#if scrolledUp}
				<button type="button" class="jump" onclick={scrollListToBottom}>
					jump to newest
					{#if missedWhileScrolled > 0}
						<span class="badge tnum">+{missedWhileScrolled}</span>
					{/if}
				</button>
			{/if}
		</section>
	</div>
{/if}

<style>
	.pad {
		padding: 16px;
	}
	.runbar {
		flex: none;
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 6px 16px;
		border-bottom: 1px solid var(--rl-hair);
		background: var(--color-base-200);
		font-size: 11.5px;
		overflow-x: auto;
		white-space: nowrap;
	}
	.pip {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--rl-hair-strong);
		flex: none;
	}
	.pip.live {
		background: var(--color-success);
		animation: rl-breathe 2.4s ease-in-out infinite;
	}
	.cmd {
		font-size: 12px;
		font-weight: 500;
	}
	.sep {
		border-left: 1px solid var(--rl-hair);
		padding-left: 10px;
	}
	.dirty {
		color: var(--color-warning);
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.16em;
	}
	.detail {
		margin-left: auto;
		color: var(--rl-meta);
		text-decoration: none;
		padding-left: 12px;
	}
	.detail:hover {
		color: var(--color-base-content);
	}

	.viewswitch {
		display: flex;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		overflow: hidden;
	}
	.viewswitch button {
		border: 0;
		background: transparent;
		color: var(--rl-meta);
		font: inherit;
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		padding: 4px 9px;
		cursor: pointer;
	}
	.viewswitch button + button {
		border-left: 1px solid var(--rl-hair);
	}
	.viewswitch button.on {
		background: var(--color-base-content);
		color: var(--color-base-100);
	}

	.split {
		flex: 1;
		min-height: 0;
		display: grid;
		grid-template-rows: var(--term-h) auto 1fr;
	}
	.split[data-view='raw'] {
		grid-template-rows: 1fr;
	}
	.split[data-view='entries'] {
		grid-template-rows: 0 1fr;
	}
	.termwrap {
		min-height: 0;
		overflow: hidden;
		background: var(--rl-term-bg);
	}
	.split[data-view='entries'] .termwrap {
		display: none;
	}
	.split[data-view='raw'] .entrywrap {
		display: none;
	}

	.divider {
		height: 7px;
		width: 100%;
		padding: 0;
		background: var(--color-base-200);
		border-top: 1px solid var(--rl-hair);
		border-bottom: 1px solid var(--rl-hair);
		cursor: row-resize;
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.divider:hover .grip,
	.divider:focus-visible .grip {
		background: var(--color-base-content);
	}
	.grip {
		width: 34px;
		height: 2px;
		border-radius: 1px;
		background: var(--rl-hair-strong);
	}

	.entrywrap {
		position: relative;
		min-height: 0;
		display: flex;
		flex-direction: column;
	}
	.controls {
		flex: none;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 7px 12px;
		border-bottom: 1px solid var(--rl-hair);
		flex-wrap: wrap;
	}
	.chips {
		display: flex;
		gap: 5px;
		flex-wrap: wrap;
		align-items: center;
	}
	.clear {
		border: 0;
		background: none;
		color: var(--rl-meta);
		font: inherit;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.16em;
		cursor: pointer;
		padding: 0 4px;
	}
	.clear:hover {
		color: var(--color-base-content);
	}
	.right {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.searchbox {
		display: flex;
		align-items: center;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		height: 22px;
	}
	.searchbox:focus-within {
		border-color: var(--rl-hair-strong);
	}
	input[type='search'] {
		border: 0;
		background: transparent;
		color: var(--color-base-content);
		font: inherit;
		font-size: 11.5px;
		padding: 0 8px;
		width: 190px;
		outline: none;
	}
	.hits {
		font-size: 10px;
		color: var(--rl-meta);
		padding-right: 6px;
	}
	.tbtn {
		border: 0;
		border-left: 1px solid var(--rl-hair);
		background: none;
		color: var(--rl-meta);
		cursor: pointer;
		width: 20px;
		height: 100%;
		font-size: 11px;
	}
	.tbtn:hover {
		color: var(--color-base-content);
	}
	.follow {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		height: 22px;
		padding: 0 8px;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: transparent;
		color: var(--rl-meta);
		font: inherit;
		font-size: 11px;
		cursor: pointer;
		white-space: nowrap;
	}
	.follow.on {
		color: var(--color-base-content);
		border-color: var(--rl-hair-strong);
	}
	.fdot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--rl-hair-strong);
	}
	.fdot.live {
		background: var(--color-success);
		animation: rl-breathe 2.4s ease-in-out infinite;
	}
	.inferred-note {
		flex: none;
		margin: 0;
		padding: 5px 12px;
		font-size: 11px;
		border-bottom: 1px solid var(--rl-hair);
		background: color-mix(in oklch, var(--color-warning) 7%, transparent);
	}
	.glyph {
		margin-right: 4px;
	}
	.entries {
		flex: 1;
		padding: 4px 0 40px;
	}
	.none {
		padding: 14px 16px;
		font-size: 12px;
	}
	.jump {
		position: absolute;
		bottom: 14px;
		left: 50%;
		transform: translateX(-50%);
		display: inline-flex;
		align-items: center;
		gap: 7px;
		background: var(--color-base-content);
		color: var(--color-base-100);
		border: 0;
		border-radius: 2px;
		font: inherit;
		font-size: 11px;
		font-weight: 500;
		padding: 5px 11px;
		cursor: pointer;
		box-shadow: 0 4px 14px rgb(0 0 0 / 0.3);
	}
	.badge {
		background: var(--color-flare);
		color: #180703;
		border-radius: 2px;
		padding: 0 4px;
		font-weight: 600;
	}
</style>
