<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import ExitBadge from '$lib/components/ExitBadge.svelte';
	import LevelChip from '$lib/components/LevelChip.svelte';
	import { apiError, projectRef, runDiff } from '$lib/api';
	import { app } from '$lib/stores/app.svelte';
	import {
		fmtCommand,
		fmtDuration,
		fmtStamp,
		pluralise,
		runDuration,
		shortHash,
		shortSha
	} from '$lib/format';
	import type { RunDiff, RunRef } from '$lib/api/types';

	/**
	 * "It worked two days ago. What changed?"
	 *
	 * This screen answers that question in the first paragraph and shows its
	 * working underneath. The order is deliberate: verdict, then what is newly
	 * broken, then what stopped being broken, then the ordinary evidence — the
	 * commits, the lockfiles, the environment. A reader who stops after the
	 * first two blocks should already know where to look.
	 */

	const projectId = $derived(Number(page.params.id));
	const project = $derived(app.project(projectId));

	const aParam = $derived(page.url.searchParams.get('a'));
	const bParam = $derived(page.url.searchParams.get('b'));
	const baselineKind = $derived(page.url.searchParams.get('base') ?? 'last_good');

	const refA = $derived<RunRef>(
		aParam
			? { kind: 'id', value: Number(aParam) }
			: baselineKind === 'last_bad'
				? { kind: 'last_bad' }
				: { kind: 'last_good' }
	);
	const refB = $derived<RunRef>(
		bParam ? { kind: 'id', value: Number(bParam) } : { kind: 'latest' }
	);

	let diff = $state<RunDiff | null>(null);
	let loading = $state(true);
	let loadError = $state<string | null>(null);

	$effect(() => {
		const id = projectId;
		const a = refA;
		const b = refB;
		if (!Number.isFinite(id)) return;
		let cancelled = false;
		loading = true;
		loadError = null;

		(async () => {
			try {
				const d = await runDiff(projectRef(id), a, b);
				if (!cancelled) {
					diff = d;
					loading = false;
				}
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

	const newErrors = $derived(diff?.new_signatures.filter((s) => s.level === 'error') ?? []);
	const newWarns = $derived(diff?.new_signatures.filter((s) => s.level !== 'error') ?? []);
	const resolvedErrors = $derived(
		diff?.resolved_signatures.filter((s) => s.level === 'error') ?? []
	);

	/** The one-sentence answer, plus the evidence that supports it. */
	const verdict = $derived.by(() => {
		if (!diff) return null;
		if (!diff.a || !diff.b) {
			return {
				tone: 'neutral' as const,
				headline: 'Not enough history to compare.',
				detail: !diff.a
					? 'There is no baseline run matching that selection yet — runlog needs at least one run that exited 0 before it can tell you what changed.'
					: 'There is no later run to compare the baseline against.'
			};
		}

		const causes: string[] = [];
		if (diff.command_changed) causes.push('the command itself changed');
		if (diff.lockfiles_changed.length)
			causes.push(
				`${pluralise(diff.lockfiles_changed.length, 'lockfile')} moved (${diff.lockfiles_changed.map((l) => l.file).join(', ')})`
			);
		if (diff.git.commits.length)
			causes.push(`${pluralise(diff.git.commits.length, 'commit')} landed`);
		if (diff.env_added.length)
			causes.push(`${pluralise(diff.env_added.length, 'environment variable')} appeared`);
		if (diff.env_removed.length)
			causes.push(`${pluralise(diff.env_removed.length, 'environment variable')} went missing`);
		if (diff.git.dirty_changed) causes.push('the working tree dirty state changed');

		const because = causes.length
			? `Between them, ${causes.slice(0, 3).join(', ')}.`
			: 'Nothing changed in git, the lockfiles, or the environment between these two runs — which points at something outside the repository.';

		if (newErrors.length) {
			return {
				tone: 'bad' as const,
				headline: `${pluralise(newErrors.length, 'error signature')} appeared that the baseline never produced.`,
				detail: because
			};
		}
		if (resolvedErrors.length && !newErrors.length) {
			return {
				tone: 'good' as const,
				headline: `Nothing new broke. ${pluralise(resolvedErrors.length, 'error signature')} stopped occurring.`,
				detail: because
			};
		}
		if (newWarns.length) {
			return {
				tone: 'warn' as const,
				headline: `No new errors, but ${pluralise(newWarns.length, 'new warning signature')}.`,
				detail: because
			};
		}
		return {
			tone: 'good' as const,
			headline: 'No new signatures. Both runs failed and succeeded in exactly the same ways.',
			detail: because
		};
	});

	function setBaseline(kind: 'last_good' | 'last_bad') {
		const u = new URL(page.url);
		u.searchParams.delete('a');
		u.searchParams.set('base', kind);
		void goto(u.pathname + u.search, { replaceState: true, noScroll: true });
	}

	function clearPins() {
		void goto(`/project/${projectId}/diff`, { replaceState: true, noScroll: true });
	}
</script>

<PageHeader title="Diff" sub={project ? project.name : ''}>
	{#snippet actions()}
		<div
			class="flex overflow-hidden rounded-[2px] border border-hair"
			role="group"
			aria-label="Baseline"
		>
			<button
				type="button"
				class="cursor-pointer px-[9px] py-[4px] text-[10px] tracking-[0.16em] uppercase {!aParam &&
				baselineKind === 'last_good'
					? 'bg-base-content text-base-100'
					: 'text-meta'}"
				onclick={() => setBaseline('last_good')}>vs last good</button
			>
			<button
				type="button"
				class="cursor-pointer border-l border-hair px-[9px] py-[4px] text-[10px] tracking-[0.16em] uppercase {!aParam &&
				baselineKind === 'last_bad'
					? 'bg-base-content text-base-100'
					: 'text-meta'}"
				onclick={() => setBaseline('last_bad')}>vs last failure</button
			>
		</div>
		{#if aParam || bParam}
			<button
				type="button"
				class="cursor-pointer rounded-[2px] border border-hair px-2 py-[4px] text-[10px] tracking-[0.16em] text-meta uppercase"
				onclick={clearPins}>reset to default</button
			>
		{/if}
		<a class="text-[11px] text-meta hover:text-base-content" href="/project/{projectId}/runs"
			>pick runs →</a
		>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="p-4"><ErrorNote what="Could not build the diff." detail={loadError} /></div>
{:else if loading}
	<p class="p-4 text-meta">Comparing runs…</p>
{:else if !diff}
	<div class="p-4">
		<EmptyState
			title="Nothing to compare yet"
			body="A diff needs two runs. Run your command once, change something, run it again."
			command="runlog npm test"
		/>
	</div>
{:else}
	<div class="pane flex-1 pb-10">
		{#if verdict}
			<section class="rule-b flex gap-[14px] py-[18px] pr-4" data-tone={verdict.tone}>
				<span
					class="w-[3px] flex-none {verdict.tone === 'bad'
						? 'bg-flare'
						: verdict.tone === 'good'
							? 'bg-clear'
							: verdict.tone === 'warn'
								? 'bg-warning'
								: 'bg-hair-strong'}"
					aria-hidden="true"
				></span>
				<div>
					<h1 class="max-w-[72ch] text-[19px] font-semibold tracking-normal">{verdict.headline}</h1>
					<p class="mt-[7px] max-w-[80ch] text-[12.5px] leading-[1.6] text-meta">{verdict.detail}</p>
				</div>
			</section>
		{/if}

		<section class="rule-b relative grid grid-cols-2 bg-base-200" aria-label="Runs being compared">
			{#each [{ side: 'A', run: diff.a, note: aParam ? 'pinned' : baselineKind === 'last_bad' ? 'last failure' : 'last known good' }, { side: 'B', run: diff.b, note: bParam ? 'pinned' : 'latest' }] as col (col.side)}
				<div class="min-w-0 px-4 py-[10px] {col.side === 'B' ? 'rule-l' : ''}">
					<div class="flex items-baseline gap-2">
						<span class="rounded-[2px] border border-hair-strong px-[5px] text-[11px] font-semibold"
							>{col.side}</span
						>
						<span class="label-xs">{col.note}</span>
					</div>
					{#if col.run}
						<a class="group mt-[5px] block" href="/project/{projectId}/runs/{col.run.id}">
							<code class="text-[12.5px] group-hover:border-b group-hover:border-hair-strong"
								>{fmtCommand(col.run.command)}</code
							>
						</a>
						<div class="tnum mt-[5px] flex flex-wrap items-center gap-[10px] text-[11px] text-meta">
							<span>{fmtStamp(col.run.started_at)}</span>
							<span>{fmtDuration(runDuration(col.run))}</span>
							<span>{shortSha(col.run.git_sha)} {col.run.git_branch ?? ''}</span>
							{#if col.run.git_dirty_hash}<span
									class="text-[9px] tracking-[0.16em] text-warning uppercase">dirty</span
								>{/if}
							<ExitBadge run={col.run} />
						</div>
					{:else}
						<p class="mt-[6px] text-[12px] text-meta">No run matched.</p>
					{/if}
				</div>
			{/each}
			<div
				class="absolute top-1/2 left-1/2 grid h-[22px] w-[22px] -translate-x-1/2 -translate-y-1/2 place-items-center rounded-full border border-hair bg-base-200 text-[12px] text-meta"
				aria-hidden="true"
			>
				→
			</div>
		</section>

		<section class="rule-b px-4 pt-4 pb-1">
			<h2 class="mb-[10px] flex items-baseline gap-[9px] text-[13px] font-semibold">
				New signatures
				<span
					class="tnum rounded-[2px] border border-flare bg-flare px-[6px] text-[12px] font-semibold text-flare-content"
					>{diff.new_signatures.length}</span
				>
				<span class="text-[11px] font-normal text-meta">present in B, absent from A</span>
			</h2>
			{#if diff.new_signatures.length === 0}
				<p class="mb-3 max-w-[80ch] text-[12px] leading-[1.6] text-meta">
					Nothing appeared that the baseline did not already produce. Whatever is failing now was
					failing then.
				</p>
			{:else}
				<ul class="mb-2 flex flex-col gap-[6px]">
					{#each diff.new_signatures as s (s.fingerprint_id)}
						<li
							class="flex overflow-hidden rounded-[3px] border border-[color-mix(in_oklch,var(--color-flare)_35%,transparent)] bg-base-200"
							data-level={s.level}
						>
							<span class="w-[3px] flex-none bg-flare" aria-hidden="true"></span>
							<div class="min-w-0 flex-1 px-3 py-2">
								<div class="flex min-w-0 items-center gap-[10px]">
									<LevelChip level={s.level} />
									<code class="min-w-0 flex-1 truncate text-[12px]">{s.template}</code>
									<span class="tnum flex-none text-[12px] font-semibold text-flare">+{s.count_b}</span
									>
								</div>
								{#if s.sample}
									<pre
										class="mono-block mt-[7px] max-w-full overflow-x-auto border-l border-hair-strong bg-code-bg px-[9px] py-[7px] text-[11px]! text-meta">{s.sample}</pre>
								{/if}
							</div>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<section class="rule-b px-4 pt-4 pb-1">
			<h2 class="mb-[10px] flex items-baseline gap-[9px] text-[13px] font-semibold">
				Resolved signatures
				<span
					class="tnum rounded-[2px] border border-clear bg-clear px-[6px] text-[12px] font-semibold text-flare-content"
					>{diff.resolved_signatures.length}</span
				>
				<span class="text-[11px] font-normal text-meta">present in A, absent from B</span>
			</h2>
			{#if diff.resolved_signatures.length === 0}
				<p class="mb-3 max-w-[80ch] text-[12px] leading-[1.6] text-meta">
					Everything the baseline produced still occurs.
				</p>
			{:else}
				<ul class="mb-2 flex flex-col gap-[6px]">
					{#each diff.resolved_signatures as s (s.fingerprint_id)}
						<li
							class="flex overflow-hidden rounded-[3px] border border-hair bg-base-200"
							data-level={s.level}
						>
							<span class="w-[3px] flex-none bg-clear" aria-hidden="true"></span>
							<div class="min-w-0 flex-1 px-3 py-2">
								<div class="flex min-w-0 items-center gap-[10px]">
									<LevelChip level={s.level} />
									<code class="min-w-0 flex-1 truncate text-[12px]">{s.template}</code>
									<span class="tnum flex-none text-[12px] font-semibold text-success"
										>−{s.count_a}</span
									>
								</div>
							</div>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		{#if diff.changed_counts.length}
			<section class="rule-b px-4 pt-4 pb-1">
				<h2 class="mb-[10px] flex items-baseline gap-[9px] text-[13px] font-semibold">
					Count changes
					<span
						class="tnum rounded-[2px] border border-hair-strong px-[6px] text-[12px] font-semibold"
						>{diff.changed_counts.length}</span
					>
					<span class="text-[11px] font-normal text-meta">same signature, different volume</span>
				</h2>
				<ul class="mb-[10px]">
					{#each diff.changed_counts as s (s.fingerprint_id)}
						<li
							class="rule-b grid grid-cols-[76px_1fr_110px] items-center gap-3 py-1"
							data-level={s.level}
						>
							<LevelChip level={s.level} />
							<code class="min-w-0 truncate text-[12px]">{s.template}</code>
							<span class="tnum flex justify-end gap-[6px] text-[12px]">
								<span class="text-meta">{s.count_a}</span>
								<span class="text-meta">→</span>
								<span
									class={s.count_b > s.count_a
										? 'font-semibold text-flare'
										: s.count_b < s.count_a
											? 'font-semibold text-success'
											: ''}>{s.count_b}</span
								>
							</span>
						</li>
					{/each}
				</ul>
			</section>
		{/if}

		<section class="rule-b px-4 pt-4 pb-1">
			<h2 class="mb-[10px] flex items-baseline gap-[9px] text-[13px] font-semibold">
				What moved between them
			</h2>

			<div class="grid grid-cols-[repeat(auto-fit,minmax(300px,1fr))] gap-x-6 gap-y-[14px] pb-3">
				<div>
					<span class="label-xs"
						>commits {diff.git.sha_a && diff.git.sha_b
							? `${shortSha(diff.git.sha_a)}..${shortSha(diff.git.sha_b)}`
							: ''}</span
					>
					{#if diff.git.commits.length}
						<ul class="mt-[6px] text-[11.5px]">
							{#each diff.git.commits as c (c.sha)}
								<li class="grid grid-cols-[62px_1fr_auto] items-baseline gap-[10px] py-[2px]">
									<span class="tnum text-meta">{shortSha(c.sha)}</span>
									<span class="truncate">{c.subject}</span>
									<span class="text-[10.5px] text-meta">{c.author}</span>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6] text-meta">
							Same commit{diff.git.dirty_changed
								? ', but the working tree dirty state changed'
								: ''}.
						</p>
					{/if}
					{#if diff.git.branch_a !== diff.git.branch_b}
						<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6]">
							Branch changed: <b>{diff.git.branch_a ?? '—'}</b> → <b>{diff.git.branch_b ?? '—'}</b>
						</p>
					{/if}
				</div>

				<div>
					<span class="label-xs">files changed ({diff.git.files_changed.length})</span>
					{#if diff.git.files_changed.length}
						<ul class="mt-[6px] text-[11.5px]">
							{#each diff.git.files_changed as f (f)}
								<li><code class="text-meta">{f}</code></li>
							{/each}
						</ul>
					{:else}
						<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6] text-meta">
							No tracked files differ.
						</p>
					{/if}
				</div>

				<div>
					<span class="label-xs">lockfiles</span>
					{#if diff.lockfiles_changed.length}
						<ul class="mt-[6px] text-[11.5px]">
							{#each diff.lockfiles_changed as l (l.file)}
								<li class="flex flex-wrap items-baseline gap-[10px] py-[2px]">
									<code>{l.file}</code>
									<span class="tnum text-meta">{shortHash(l.hash_a)} → {shortHash(l.hash_b)}</span>
									<span class="text-[9px] tracking-[0.16em] text-warning uppercase">moved</span>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6] text-meta">
							Unchanged. Dependencies are identical in both runs.
						</p>
					{/if}
				</div>

				<div>
					<span class="label-xs">environment variable names</span>
					{#if diff.env_added.length || diff.env_removed.length}
						<div class="mt-[6px] flex flex-wrap gap-[5px]">
							{#each diff.env_added as k (k)}
								<code
									class="rounded-[2px] border border-[color-mix(in_oklch,var(--color-flare)_40%,transparent)] px-[6px] py-px text-[11px] text-flare"
									>+{k}</code
								>
							{/each}
							{#each diff.env_removed as k (k)}
								<code class="rounded-[2px] border border-hair px-[6px] py-px text-[11px] text-meta"
									>−{k}</code
								>
							{/each}
						</div>
						<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6] text-meta">
							Names only. runlog never records environment values.
						</p>
					{:else}
						<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6] text-meta">
							Same set of variables in both runs.
						</p>
					{/if}
				</div>

				{#if diff.command_changed}
					<div class="col-span-full">
						<span class="label-xs">command</span>
						<p class="mt-[6px] max-w-[70ch] text-[11.5px] leading-[1.6]">
							The command is not the same in both runs, so some of the difference above may simply
							be a different program.
						</p>
						<div class="tnum mt-[7px] flex flex-col gap-[3px] text-[11.5px]">
							<code>A · {diff.a ? fmtCommand(diff.a.command) : '—'}</code>
							<code>B · {diff.b ? fmtCommand(diff.b.command) : '—'}</code>
						</div>
					</div>
				{/if}
			</div>
		</section>
	</div>
{/if}
