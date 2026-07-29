<script lang="ts">
	import { page } from '$app/state';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import LevelChip from '$lib/components/LevelChip.svelte';
	import Sparkline from '$lib/components/Sparkline.svelte';
	import { apiError, entries, fingerprintHistory, fingerprints, projectRef } from '$lib/api';
	import { app } from '$lib/stores/app.svelte';
	import { fmtAgo, fmtStamp, pluralise } from '$lib/format';
	import type { EntryView, Fingerprint, FingerprintHistoryPoint } from '$lib/api/types';

	/**
	 * Error signatures, not error lines.
	 *
	 * A signature that fired 40 times in one run is one problem; a signature
	 * that fired once in each of the last 12 runs is a different, worse problem.
	 * The sparkline is what separates them, so it is a first-class column rather
	 * than a decoration.
	 */

	const projectId = $derived(Number(page.params.id));
	const project = $derived(app.project(projectId));

	let fps = $state<Fingerprint[]>([]);
	let history = $state<Record<number, FingerprintHistoryPoint[]>>({});
	let instances = $state<Record<number, EntryView[]>>({});
	let loading = $state(true);
	let loadError = $state<string | null>(null);
	let sort = $state<'frequency' | 'recency' | 'spread'>('frequency');
	let errorsOnly = $state(true);
	let open = $state<number | null>(null);

	$effect(() => {
		const id = projectId;
		if (!Number.isFinite(id)) return;
		let cancelled = false;
		loading = true;
		loadError = null;
		open = null;

		(async () => {
			try {
				const ref = projectRef(id);
				const list = await fingerprints(ref, 200, false);
				if (cancelled) return;
				fps = list;

				const all = await entries(ref, { limit: 8000, ascending: false });
				if (cancelled) return;
				const byFp: Record<number, EntryView[]> = {};
				for (const e of all) {
					if (e.fingerprint_id === null) continue;
					(byFp[e.fingerprint_id] ??= []).push(e);
				}
				instances = byFp;

				const hist = await Promise.all(
					list.map(async (f) => [f.id, await fingerprintHistory(ref, f.id, 30)] as const)
				);
				if (cancelled) return;
				history = Object.fromEntries(hist);
				loading = false;
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

	const shown = $derived.by(() => {
		let out = fps.filter((f) => (errorsOnly ? f.level === 'error' : true));
		out = [...out].sort((a, b) => {
			if (sort === 'recency') return b.last_seen - a.last_seen;
			if (sort === 'spread') return b.run_count - a.run_count || b.total_count - a.total_count;
			return b.total_count - a.total_count || b.last_seen - a.last_seen;
		});
		return out;
	});

	/** Occurrences in the most recent run that carried this signature. */
	function latestBurst(id: number): number {
		const h = history[id];
		if (!h) return 0;
		for (let i = h.length - 1; i >= 0; i--) if (h[i].count > 0) return h[i].count;
		return 0;
	}

	/** Consecutive most-recent runs with zero occurrences. */
	function quietRuns(id: number): number {
		const h = history[id];
		if (!h) return 0;
		let n = 0;
		for (let i = h.length - 1; i >= 0; i--) {
			if (h[i].count > 0) break;
			n++;
		}
		return n;
	}
</script>

<PageHeader
	title="Errors"
	sub={project ? `${project.name} · ${shown.length} signatures` : ''}
>
	{#snippet actions()}
		<div
			class="flex overflow-hidden rounded-[2px] border border-hair"
			role="group"
			aria-label="Sort signatures"
		>
			<button
				type="button"
				class="cursor-pointer px-[9px] py-[4px] text-[10px] tracking-[0.16em] uppercase {sort ===
				'frequency'
					? 'bg-base-content text-base-100'
					: 'text-meta'}"
				onclick={() => (sort = 'frequency')}>frequency</button
			>
			<button
				type="button"
				class="cursor-pointer border-l border-hair px-[9px] py-[4px] text-[10px] tracking-[0.16em] uppercase {sort ===
				'recency'
					? 'bg-base-content text-base-100'
					: 'text-meta'}"
				onclick={() => (sort = 'recency')}>recency</button
			>
			<button
				type="button"
				class="cursor-pointer border-l border-hair px-[9px] py-[4px] text-[10px] tracking-[0.16em] uppercase {sort ===
				'spread'
					? 'bg-base-content text-base-100'
					: 'text-meta'}"
				onclick={() => (sort = 'spread')}
				title="Signatures that appear across the most distinct runs">spread</button
			>
		</div>
		<button
			type="button"
			class="cursor-pointer rounded-[2px] border px-2 py-[4px] text-[10px] tracking-[0.16em] uppercase {errorsOnly
				? 'border-base-content bg-base-content text-base-100'
				: 'border-hair text-meta'}"
			aria-pressed={errorsOnly}
			onclick={() => (errorsOnly = !errorsOnly)}>errors only</button
		>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="p-4"><ErrorNote what="Could not read error signatures." detail={loadError} /></div>
{:else if loading}
	<p class="p-4 text-meta">Ranking signatures…</p>
{:else if shown.length === 0}
	<div class="p-4">
		<EmptyState
			title={errorsOnly ? 'No error signatures recorded' : 'No signatures recorded'}
			body="logbear fingerprints failures as it captures them. Once something fails, its signature appears here with a count and a first-seen date."
			command="logbear mix test"
		/>
	</div>
{:else}
	<div class="pane min-h-0 flex-1">
		<div
			class="sticky top-0 z-[2] grid grid-cols-[76px_minmax(280px,1fr)_140px_92px_54px_128px] items-center gap-3 border-b border-hair-strong bg-base-100 px-4 py-[7px]"
		>
			<span class="label-xs">level</span>
			<span class="label-xs">signature</span>
			<span class="label-xs">last 30 runs</span>
			<span class="label-xs">total</span>
			<span class="label-xs">runs</span>
			<span class="label-xs">last seen</span>
		</div>

		{#each shown as f (f.id)}
			{@const h = history[f.id] ?? []}
			{@const quiet = quietRuns(f.id)}
			<div
				class="rule-b border-l-2 {f.level === 'error'
					? 'border-l-[color-mix(in_oklch,var(--color-lvl-error)_60%,transparent)]'
					: f.level === 'warn'
						? 'border-l-[color-mix(in_oklch,var(--color-lvl-warn)_50%,transparent)]'
						: 'border-l-transparent'} {open === f.id ? 'bg-base-200' : ''}"
				data-level={f.level}
			>
				<button
					type="button"
					class="grid w-full cursor-pointer grid-cols-[76px_minmax(280px,1fr)_140px_92px_54px_128px] items-center gap-3 px-4 py-2 text-left hover:bg-[color-mix(in_oklch,var(--color-base-content)_4%,transparent)]"
					aria-expanded={open === f.id}
					onclick={() => (open = open === f.id ? null : f.id)}
				>
					<span><LevelChip level={f.level} /></span>
					<span class="min-w-0">
						<code class="block truncate text-[12px]">{f.template}</code>
						<span class="mt-[2px] block text-[10.5px] text-meta"
							>first seen {fmtStamp(f.first_seen)}</span
						>
					</span>
					<span><Sparkline points={h} level={f.level} /></span>
					<span class="tnum">
						<b class="text-[13px] font-semibold">{f.total_count}</b>
						{#if latestBurst(f.id) > 1}<span class="ml-1 text-[10.5px] text-meta"
								>·{latestBurst(f.id)}× last</span
							>{/if}
					</span>
					<span class="tnum text-right text-[12px] text-meta">{f.run_count}</span>
					<span class="tnum text-[11.5px] text-meta" title={fmtStamp(f.last_seen)}>
						{fmtAgo(f.last_seen)}
						{#if quiet >= 3}<span
								class="ml-[6px] inline-block text-[9px] tracking-[0.16em] text-success uppercase"
								title="Not seen in the last {quiet} runs">quiet {quiet}</span
							>{/if}
					</span>
				</button>

				{#if open === f.id}
					{@const rows = (instances[f.id] ?? []).slice(0, 40)}
					<div class="border-t border-dashed border-hair-strong px-4 pt-1 pb-[14px]">
						<div class="flex items-baseline justify-between pt-2 pb-[6px]">
							<span class="label-xs"
								>{pluralise(instances[f.id]?.length ?? 0, 'stored instance')}, newest first</span
							>
							<span class="tnum text-[10.5px] text-meta">fingerprint {f.hash.slice(0, 12)}</span>
						</div>
						{#if rows.length === 0}
							<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6] text-meta">
								The instances for this signature have been evicted by the retention cap. The
								signature's counts survive eviction; the text does not.
							</p>
						{:else}
							<ul class="pane max-h-[300px] rounded-[2px] border border-hair">
								{#each rows as e (e.id)}
									<li class="[&+li]:border-t [&+li]:border-hair">
										<a
											class="tnum grid grid-cols-[108px_72px_auto_1fr] items-baseline gap-[10px] px-[10px] py-1 text-[11.5px] hover:bg-[color-mix(in_oklch,var(--color-base-content)_6%,transparent)]"
											href="/project/{projectId}/runs/{e.run_id}"
										>
											<span class="text-[11px] text-meta">{fmtStamp(e.captured_at)}</span>
											<span class="text-meta">run #{e.run_id}</span>
											{#if e.is_new_signature}<span
													class="rounded-[2px] bg-flare px-1 text-[9px] font-semibold tracking-[0.14em] text-ink uppercase"
													>first</span
												>{/if}
											<span class="min-w-0 truncate">{e.text.split('\n')[0]}</span>
										</a>
									</li>
								{/each}
							</ul>
							{#if (instances[f.id]?.length ?? 0) > rows.length}
								<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6] text-meta">
									Showing the {rows.length} newest of {instances[f.id].length} stored instances.
								</p>
							{/if}
						{/if}
					</div>
				{/if}
			</div>
		{/each}
	</div>
{/if}
