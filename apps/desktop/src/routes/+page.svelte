<script lang="ts">
	import PageHeader from '$lib/components/PageHeader.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import ExitBadge from '$lib/components/ExitBadge.svelte';
	import ErrorCount from '$lib/components/ErrorCount.svelte';
	import { app } from '$lib/stores/app.svelte';
	import {
		fmtAgo,
		fmtBytes,
		fmtCommand,
		fmtDuration,
		fmtStamp,
		runDuration,
		runElapsed,
		shortSha,
		tildePath
	} from '$lib/format';
	import type { ProjectStatus } from '$lib/api/types';

	// Elapsed time on a running command has to tick on its own — no event
	// arrives just because a second passed.
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});

	const ordered = $derived(
		app.projects
			.map((p) => ({ p, s: app.statuses[p.id] as ProjectStatus | undefined }))
			.sort((a, b) => {
				const ra = (a.s?.active_runs.length ?? 0) > 0 ? 1 : 0;
				const rb = (b.s?.active_runs.length ?? 0) > 0 ? 1 : 0;
				if (ra !== rb) return rb - ra;
				return b.p.last_seen_at - a.p.last_seen_at;
			})
	);

	const totalActive = $derived(app.activeRuns.length);
</script>

<PageHeader
	title="Projects"
	sub={app.projects.length
		? `${app.projects.length} tracked · ${totalActive} running · ${fmtBytes(app.daemon?.total_db_bytes ?? 0)} stored`
		: ''}
/>

<div class="pane flex-1 p-4">
	{#if app.daemonError}
		<ErrorNote
			what="The runlog daemon is not answering, so this list may be stale."
			detail={app.daemonError}
			retry={() => void app.refresh()}
		/>
	{/if}

	{#if !app.loaded}
		<p class="meta text-[12px]">Reading the store…</p>
	{:else if app.projects.length === 0}
		<EmptyState
			title="Nothing recorded yet"
			body="runlog records a project the first time you run something through it — start your dev command with runlog and this list fills itself in."
			command="runlog npm run dev"
		>
			<p class="meta m-0 text-[11.5px] leading-[1.6]">
				Once it is capturing, install the PATH shims from Settings → Install and you can drop the
				prefix entirely.
			</p>
		</EmptyState>
	{:else}
		<div class="grid grid-cols-[repeat(auto-fill,minmax(360px,1fr))] gap-3">
			{#each ordered as { p, s } (p.id)}
				{@const running = s?.active_runs ?? []}
				{@const last = s?.last_run ?? null}
				<!-- A project running something reads as running from across the room: a lit
				     left edge, a tinted surface, and a breathing pip. -->
				<article
					class="card relative min-w-0 gap-2.5 border border-hair border-l-2 px-[14px] pt-3 pb-2.5 hover:border-hair-strong {running.length
						? 'border-l-success bg-[color-mix(in_oklch,var(--color-success)_5%,var(--color-base-200))] hover:border-l-success'
						: 'border-l-hair-strong bg-base-200'}"
				>
					<a class="absolute inset-0 z-0" href="/project/{p.id}/live" aria-label="Open {p.name}"></a>

					<header class="pointer-events-none relative z-[1] flex items-start justify-between gap-3">
						<div class="min-w-0">
							<h3 class="m-0 text-[14px] font-semibold">{p.name}</h3>
							<span class="meta block truncate text-[11px]" title={p.root}>{tildePath(p.root)}</span>
						</div>
						{#if running.length}
							<span
								class="inline-flex flex-none items-center gap-1.5 text-[10px] tracking-[0.16em] text-success uppercase"
							>
								<span class="is-running size-[7px] rounded-full bg-success"></span>
								running
							</span>
						{:else}
							<span
								class="inline-flex flex-none items-center gap-1.5 text-[10px] tracking-[0.16em] text-meta uppercase"
								>idle</span
							>
						{/if}
					</header>

					{#if running.length}
						<div class="pointer-events-none relative z-[1]">
							{#each running as r (r.id)}
								<div class="flex items-center justify-between gap-2.5">
									<code class="min-w-0 truncate text-[12px]">{fmtCommand(r.command)}</code>
									<span class="tnum flex-none text-[12px] text-success"
										>{fmtDuration(runElapsed(r, now))}</span
									>
								</div>
								<div class="meta tnum mt-[3px] truncate text-[11px]">
									{(app.counters[r.id] ?? r.counters).n_entries} entries ·
									{(app.counters[r.id] ?? r.counters).n_error} errors ·
									{r.git_branch ?? 'no branch'}@{shortSha(r.git_sha)}
								</div>
							{/each}
						</div>
					{:else if last}
						<div class="pointer-events-none relative z-[1]">
							<div class="flex items-center justify-between gap-2.5">
								<code class="meta min-w-0 truncate text-[12px]">{fmtCommand(last.command)}</code>
								<ExitBadge run={last} showLabel={false} />
							</div>
							<div class="meta tnum mt-[3px] truncate text-[11px]">
								{fmtAgo(last.started_at, now)} · {fmtDuration(runDuration(last))} ·
								{last.git_branch ?? 'no branch'}@{shortSha(last.git_sha)}{last.git_dirty_hash
									? ' +uncommitted'
									: ''}
							</div>
						</div>
					{:else}
						<div class="pointer-events-none relative z-[1]">
							<span class="meta">No completed runs stored.</span>
						</div>
					{/if}

					<footer
						class="rule-t pointer-events-none relative z-[1] flex items-end gap-[18px] pt-[9px]"
					>
						<span class="flex flex-col gap-[3px]">
							<span class="label-xs">last hour</span>
							<ErrorCount
								total={s?.errors_last_hour ?? 0}
								neu={s?.unacked_errors ?? 0}
								label="error"
								size="sm"
							/>
						</span>
						<span class="flex flex-col gap-[3px]">
							<span class="label-xs">stored</span>
							<span class="tnum text-[11.5px]">{fmtBytes(s?.db_bytes ?? 0)}</span>
						</span>
						<span class="flex flex-col gap-[3px]">
							<span class="label-xs">last seen</span>
							<span class="tnum text-[11.5px]" title={fmtStamp(p.last_seen_at)}
								>{fmtAgo(p.last_seen_at, now)}</span
							>
						</span>
						<nav class="pointer-events-auto ml-auto flex gap-2.5">
							<a
								class="border-b border-b-transparent text-[11px] text-meta no-underline hover:border-b-hair-strong hover:text-base-content"
								href="/project/{p.id}/runs">runs</a
							>
							<a
								class="border-b border-b-transparent text-[11px] text-meta no-underline hover:border-b-hair-strong hover:text-base-content"
								href="/project/{p.id}/errors">errors</a
							>
							<a
								class="border-b border-b-transparent text-[11px] text-meta no-underline hover:border-b-hair-strong hover:text-base-content"
								href="/project/{p.id}/diff">diff</a
							>
						</nav>
					</footer>
				</article>
			{/each}
		</div>
	{/if}
</div>
