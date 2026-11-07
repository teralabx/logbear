<script lang="ts">
	import { tick } from 'svelte';
	import { page } from '$app/state';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import TerminalPane from '#lib/components/TerminalPane.svelte';
	import EntryRow from '#lib/components/EntryRow.svelte';
	import LevelChip from '#lib/components/LevelChip.svelte';
	import EmptyState from '#lib/components/EmptyState.svelte';
	import ErrorNote from '#lib/components/ErrorNote.svelte';
	import ExitBadge from '#lib/components/ExitBadge.svelte';
	import { apiError, entries, listRuns, projectRef, rawStream } from '#lib/api';
	import { entryToAnsi } from '#lib/ansi';
	import { app } from '#lib/stores/app.svelte';
	import {
		fmtCommand,
		fmtDuration,
		fmtStamp,
		runDuration,
		runElapsed,
		shortSha,
		tildePath
	} from '#lib/format';
	import { LEVELS, type EntryView, type Level, type Run } from '#lib/api/types';
	import { INLINE } from '#lib/icons';
	import { ArrowRight, ChevronDown, ChevronUp } from '@lucide/svelte';

	/**
	 * The screen people leave open.
	 *
	 * Two views of the same bytes, side by side vertically: the terminal replay
	 * (what you actually saw, ANSI intact) and the structured entry list (what
	 * logbear made of it). Keeping both visible is the point — when the parse is
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
	let newOnly = $state(false);
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

	/** Entries kept in view. The live tail is for now; history is on the run page. */
	const MAX_BUFFER = 2000;

	const counters = $derived(run ? (app.counters[run.id] ?? run.counters) : null);

	const filtered = $derived.by(() => {
		const q = search.trim().toLowerCase();
		return list.filter((e) => {
			if (levelFilter.size && !levelFilter.has(e.level)) return false;
			if (newOnly && !e.is_new_signature) return false;
			if (q && !e.text.toLowerCase().includes(q)) return false;
			return true;
		});
	});

	const newCount = $derived(list.filter((e) => e.is_new_signature).length);

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
				list = [];
				if (!target) {
					loading = false;
					return;
				}
				// The newest entries, not the oldest: on a server that has been up all
				// day, the first few thousand are hours stale.
				const es = (
					await entries(ref, { run_id: target.id, limit: MAX_BUFFER, ascending: false })
				).reverse();
				if (cancelled) return;
				// Anything the live tail appended while that was in flight is newer.
				const newest = es.at(-1)?.id ?? 0;
				list = es.concat(list.filter((e) => e.id > newest)).slice(-MAX_BUFFER);
				// The terminal only exists once `loading` is false, so the pane has
				// to be mounted before the stored bytes can be written into it.
				loading = false;
				await tick();
				if (cancelled) return;
				scrollListToBottom();
				termRef?.clear();
				await rawStream(ref, target.id, (bytes) => {
					if (!cancelled) termRef?.write(bytes);
				}).catch(() => false);
				if (cancelled) return;
				termRef?.scrollToBottom();
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
		<div
			class="flex overflow-hidden rounded-[2px] border border-hair"
			role="group"
			aria-label="Pane layout"
		>
			<button
				type="button"
				class={[
					'cursor-pointer px-[9px] py-[4px] text-[12px] tracking-[0.1em] uppercase',
					view === 'split' ? 'bg-base-content text-base-100' : 'text-meta'
				]}
				onclick={() => (view = 'split')}>split</button
			>
			<button
				type="button"
				class={[
					'rule-l cursor-pointer px-[9px] py-[4px] text-[12px] tracking-[0.1em] uppercase',
					view === 'raw' ? 'bg-base-content text-base-100' : 'text-meta'
				]}
				onclick={() => (view = 'raw')}>terminal</button
			>
			<button
				type="button"
				class={[
					'rule-l cursor-pointer px-[9px] py-[4px] text-[12px] tracking-[0.1em] uppercase',
					view === 'entries' ? 'bg-base-content text-base-100' : 'text-meta'
				]}
				onclick={() => (view = 'entries')}>entries</button
			>
		</div>
	{/snippet}
</PageHeader>

{#if run}
	<div
		class="rule-b flex flex-none items-center gap-[10px] overflow-x-auto bg-base-200 px-4 py-[6px] text-[12px] whitespace-nowrap"
	>
		<span
			class={[
				'h-[7px] w-[7px] flex-none rounded-full',
				run.ended_at === null ? 'is-running bg-success' : 'bg-hair-strong'
			]}
			aria-hidden="true"
		></span>
		<code class="text-[12px] font-medium">{fmtCommand(run.command)}</code>
		<ExitBadge {run} />
		<span class="meta tnum rule-l pl-[10px]">
			{run.ended_at === null
				? `${fmtDuration(runElapsed(run, now))} elapsed`
				: fmtDuration(runDuration(run))}
		</span>
		<span class="meta tnum rule-l pl-[10px]"
			>{run.git_branch ?? 'no branch'}@{shortSha(run.git_sha)}</span
		>
		{#if run.git_dirty_hash}<span
				class="text-[12px] tracking-[0.1em] text-warning uppercase"
				title="This run started with uncommitted changes, so the commit above does not fully describe it"
				>uncommitted</span
			>{/if}
		<span class="meta tnum rule-l pl-[10px]">started {fmtStamp(run.started_at)}</span>
		<span class="meta rule-l pl-[10px]" title="How the daemon split this stream into entries"
			>delimiter: {run.delimiter_mode}</span
		>
		<a
			class="ml-auto inline-flex items-center gap-1 pl-[12px] text-meta no-underline hover:text-base-content"
			href="/project/{projectId}/runs/{run.id}">run detail <ArrowRight {...INLINE} aria-hidden="true" /></a
		>
	</div>
{/if}

{#if loadError}
	<div class="p-4"><ErrorNote what="Could not read this run." detail={loadError} /></div>
{:else if loading}
	<p class="meta p-4">Loading stored output…</p>
{:else if !run}
	<div class="p-4">
		<EmptyState
			title="No runs recorded for this project yet"
			body="Start a command through logbear from {project ? tildePath(project.root) : 'the project root'} and its output will stream here."
			command="logbear mix phx.server"
		/>
	</div>
{:else}
	<div
		class="grid min-h-0 flex-1 grid-rows-[var(--term-h)_auto_1fr] data-[view=entries]:grid-rows-[1fr] data-[view=raw]:grid-rows-[1fr]"
		bind:this={splitEl}
		style="--term-h: {view === 'split' ? termHeightPct : view === 'raw' ? 100 : 0}%"
		data-view={view}
	>
		<section
			class={['min-h-0 overflow-hidden bg-term-bg', view === 'entries' && 'hidden']}
			aria-label="Terminal replay"
		>
			<TerminalPane bind:this={termRef} cols={run.term_cols} />
		</section>

		{#if view === 'split'}
			<button
				type="button"
				class="rule-t rule-b group flex h-[7px] w-full cursor-row-resize items-center justify-center bg-base-200"
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
				<span
					class="h-[2px] w-[34px] rounded-[1px] bg-hair-strong group-hover:bg-base-content group-focus-visible:bg-base-content"
				></span>
			</button>
		{/if}

		<section
			class={['relative flex min-h-0 flex-col', view === 'raw' && 'hidden']}
			aria-label="Structured entries"
		>
			<div
				class="rule-b flex flex-none flex-wrap items-center justify-between gap-[12px] px-[12px] py-[7px]"
			>
				<div class="flex flex-wrap items-center gap-[5px]">
					{#each LEVELS as l (l)}
						<LevelChip
							level={l}
							count={countFor(l)}
							active={levelFilter.has(l)}
							onclick={() => toggleLevel(l)}
							title="Show only {l} entries"
						/>
					{/each}
					<button
						type="button"
						class={[
							'inline-flex h-[20px] cursor-pointer items-center gap-[5px] rounded-[2px] border px-[7px] text-[12px] leading-none whitespace-nowrap transition-[border-color,color,background-color] duration-[120ms]',
							newOnly
								? 'border-[color-mix(in_oklch,var(--color-flare)_55%,transparent)] bg-[color-mix(in_oklch,var(--color-flare)_12%,transparent)] text-base-content'
								: 'border-hair bg-transparent text-meta hover:border-hair-strong hover:text-base-content'
						]}
						aria-pressed={newOnly}
						onclick={() => (newOnly = !newOnly)}
						title="Output whose signature has never appeared in this project before. Debug prints you just added show up here, since they declare no level to filter by."
					>
						new{#if newCount}<span
								class={['tnum font-medium', newOnly ? 'text-base-content' : 'text-meta']}
								>{newCount}</span
							>{/if}
					</button>

					{#if levelFilter.size || newOnly}
						<button
							type="button"
							class="cursor-pointer px-[4px] text-[12px] tracking-[0.1em] text-meta uppercase hover:text-base-content"
							onclick={() => {
								levelFilter = new Set();
								newOnly = false;
							}}>clear filter</button
						>
					{/if}
				</div>

				<div class="flex items-center gap-[8px]">
					<div
						class="flex h-[22px] items-center rounded-[2px] border border-hair focus-within:border-hair-strong"
					>
						<input
							type="search"
							class="w-[190px] bg-transparent px-[8px] text-[12px] text-base-content outline-none"
							placeholder="Filter entries…"
							bind:value={search}
							aria-label="Filter entries by text"
							onkeydown={(e) => {
								if (e.key === 'Enter') searchTerminal(e.shiftKey ? -1 : 1);
							}}
						/>
						{#if search.trim()}
							<span class="tnum pr-[6px] text-[12px] text-meta">{filtered.length}</span>
							<button
								type="button"
								class="rule-l grid h-full w-[20px] cursor-pointer place-items-center text-meta hover:text-base-content"
								onclick={() => searchTerminal(-1)}
								aria-label="Find previous match in the terminal pane"
								title="Find previous match in the terminal pane"
								><ChevronUp {...INLINE} aria-hidden="true" /></button
							>
							<button
								type="button"
								class="rule-l grid h-full w-[20px] cursor-pointer place-items-center text-meta hover:text-base-content"
								onclick={() => searchTerminal(1)}
								aria-label="Find next match in the terminal pane"
								title="Find next match in the terminal pane"
								><ChevronDown {...INLINE} aria-hidden="true" /></button
							>
						{/if}
					</div>

					<button
						type="button"
						class={[
							'inline-flex h-[22px] cursor-pointer items-center gap-[6px] rounded-[2px] border bg-transparent px-[8px] text-[12px] whitespace-nowrap',
							follow ? 'border-hair-strong text-base-content' : 'border-hair text-meta'
						]}
						aria-pressed={follow}
						onclick={() => {
							follow = !follow;
							if (follow) scrollListToBottom();
						}}
						title={follow
							? 'Following the tail — new entries scroll into view'
							: 'Not following — new entries arrive below without scrolling'}
					>
						<span
							class={[
								'h-[6px] w-[6px] rounded-full',
								follow && run.ended_at === null ? 'is-running bg-success' : 'bg-hair-strong'
							]}
						></span>
						follow tail
					</button>
				</div>
			</div>

			{#if inferredCount > 0}
				<p
					class="meta rule-b m-0 flex-none bg-[color-mix(in_oklch,var(--color-warning)_7%,transparent)] px-[12px] py-[5px] text-[12px]"
				>
					<span class="mr-[4px]">≈</span>
					{inferredCount} of {filtered.length} entries had their boundaries inferred — no explicit delimiter
					was found, so the grouping is a best guess.
				</p>
			{/if}

			<div class="pane flex-1 pt-[4px] pb-[40px]" bind:this={listEl} onscroll={onListScroll}>
				{#if filtered.length === 0}
					<p class="meta px-4 py-[14px] text-[12px]">
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
				<button
					type="button"
					class="absolute bottom-[14px] left-1/2 inline-flex -translate-x-1/2 cursor-pointer items-center gap-[7px] rounded-[2px] bg-base-content px-[11px] py-[5px] text-[12px] font-medium text-base-100 shadow-[0_4px_14px_rgba(0,0,0,0.3)]"
					onclick={scrollListToBottom}
				>
					jump to newest
					{#if missedWhileScrolled > 0}
						<span class="tnum rounded-[2px] bg-flare px-[4px] font-semibold text-flare-content"
							>+{missedWhileScrolled}</span
						>
					{/if}
				</button>
			{/if}
		</section>
	</div>
{/if}
