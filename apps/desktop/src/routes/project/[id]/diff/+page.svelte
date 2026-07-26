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
		<div class="seg" role="group" aria-label="Baseline">
			<button
				type="button"
				class:on={!aParam && baselineKind === 'last_good'}
				onclick={() => setBaseline('last_good')}>vs last good</button
			>
			<button
				type="button"
				class:on={!aParam && baselineKind === 'last_bad'}
				onclick={() => setBaseline('last_bad')}>vs last failure</button
			>
		</div>
		{#if aParam || bParam}
			<button type="button" class="pill" onclick={clearPins}>reset to default</button>
		{/if}
		<a class="back" href="/project/{projectId}/runs">pick runs →</a>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="pad"><ErrorNote what="Could not build the diff." detail={loadError} /></div>
{:else if loading}
	<p class="pad meta">Comparing runs…</p>
{:else if !diff}
	<div class="pad">
		<EmptyState
			title="Nothing to compare yet"
			body="A diff needs two runs. Run your command once, change something, run it again."
			command="runlog npm test"
		/>
	</div>
{:else}
	<div class="pane body">
		{#if verdict}
			<section class="verdict" data-tone={verdict.tone}>
				<span class="stripe" aria-hidden="true"></span>
				<div>
					<h1>{verdict.headline}</h1>
					<p class="detail">{verdict.detail}</p>
				</div>
			</section>
		{/if}

		<section class="pair" aria-label="Runs being compared">
			{#each [{ side: 'A', run: diff.a, note: aParam ? 'pinned' : baselineKind === 'last_bad' ? 'last failure' : 'last known good' }, { side: 'B', run: diff.b, note: bParam ? 'pinned' : 'latest' }] as col (col.side)}
				<div class="col">
					<div class="colhead">
						<span class="side">{col.side}</span>
						<span class="label-xs">{col.note}</span>
					</div>
					{#if col.run}
						<a class="runline" href="/project/{projectId}/runs/{col.run.id}">
							<code>{fmtCommand(col.run.command)}</code>
						</a>
						<div class="runmeta tnum meta">
							<span>{fmtStamp(col.run.started_at)}</span>
							<span>{fmtDuration(runDuration(col.run))}</span>
							<span>{shortSha(col.run.git_sha)} {col.run.git_branch ?? ''}</span>
							{#if col.run.git_dirty_hash}<span class="dirty">dirty</span>{/if}
							<ExitBadge run={col.run} />
						</div>
					{:else}
						<p class="norun meta">No run matched.</p>
					{/if}
				</div>
			{/each}
			<div class="arrow" aria-hidden="true">→</div>
		</section>

		<section class="block">
			<h2 class="blockhead flare-head">
				New signatures
				<span class="n tnum">{diff.new_signatures.length}</span>
				<span class="sub meta">present in B, absent from A</span>
			</h2>
			{#if diff.new_signatures.length === 0}
				<p class="empty-line meta">
					Nothing appeared that the baseline did not already produce. Whatever is failing now was
					failing then.
				</p>
			{:else}
				<ul class="sigs">
					{#each diff.new_signatures as s (s.fingerprint_id)}
						<li class="sig new" data-level={s.level}>
							<span class="stripe" aria-hidden="true"></span>
							<div class="sigbody">
								<div class="sigtop">
									<LevelChip level={s.level} />
									<code class="tpl">{s.template}</code>
									<span class="delta tnum">+{s.count_b}</span>
								</div>
								{#if s.sample}
									<pre class="sample mono-block">{s.sample}</pre>
								{/if}
							</div>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<section class="block">
			<h2 class="blockhead clear-head">
				Resolved signatures
				<span class="n tnum">{diff.resolved_signatures.length}</span>
				<span class="sub meta">present in A, absent from B</span>
			</h2>
			{#if diff.resolved_signatures.length === 0}
				<p class="empty-line meta">Everything the baseline produced still occurs.</p>
			{:else}
				<ul class="sigs">
					{#each diff.resolved_signatures as s (s.fingerprint_id)}
						<li class="sig resolved" data-level={s.level}>
							<span class="stripe" aria-hidden="true"></span>
							<div class="sigbody">
								<div class="sigtop">
									<LevelChip level={s.level} />
									<code class="tpl">{s.template}</code>
									<span class="delta gone tnum">−{s.count_a}</span>
								</div>
							</div>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		{#if diff.changed_counts.length}
			<section class="block">
				<h2 class="blockhead">
					Count changes
					<span class="n tnum">{diff.changed_counts.length}</span>
					<span class="sub meta">same signature, different volume</span>
				</h2>
				<ul class="counts">
					{#each diff.changed_counts as s (s.fingerprint_id)}
						<li data-level={s.level}>
							<LevelChip level={s.level} />
							<code class="tpl">{s.template}</code>
							<span class="tnum ab">
								<span class="meta">{s.count_a}</span>
								<span class="meta">→</span>
								<span class:up={s.count_b > s.count_a} class:down={s.count_b < s.count_a}
									>{s.count_b}</span
								>
							</span>
						</li>
					{/each}
				</ul>
			</section>
		{/if}

		<section class="block">
			<h2 class="blockhead">What moved between them</h2>

			<div class="evidence">
				<div class="ev">
					<span class="label-xs"
						>commits {diff.git.sha_a && diff.git.sha_b
							? `${shortSha(diff.git.sha_a)}..${shortSha(diff.git.sha_b)}`
							: ''}</span
					>
					{#if diff.git.commits.length}
						<ul class="commits">
							{#each diff.git.commits as c (c.sha)}
								<li>
									<span class="csha tnum meta">{shortSha(c.sha)}</span>
									<span class="csub">{c.subject}</span>
									<span class="cauth meta">{c.author}</span>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="ev-none meta">
							Same commit{diff.git.dirty_changed
								? ', but the working tree dirty state changed'
								: ''}.
						</p>
					{/if}
					{#if diff.git.branch_a !== diff.git.branch_b}
						<p class="ev-note">
							Branch changed: <b>{diff.git.branch_a ?? '—'}</b> → <b>{diff.git.branch_b ?? '—'}</b>
						</p>
					{/if}
				</div>

				<div class="ev">
					<span class="label-xs">files changed ({diff.git.files_changed.length})</span>
					{#if diff.git.files_changed.length}
						<ul class="files">
							{#each diff.git.files_changed as f (f)}
								<li><code>{f}</code></li>
							{/each}
						</ul>
					{:else}
						<p class="ev-none meta">No tracked files differ.</p>
					{/if}
				</div>

				<div class="ev">
					<span class="label-xs">lockfiles</span>
					{#if diff.lockfiles_changed.length}
						<ul class="locks">
							{#each diff.lockfiles_changed as l (l.file)}
								<li>
									<code>{l.file}</code>
									<span class="tnum meta">{shortHash(l.hash_a)} → {shortHash(l.hash_b)}</span>
									<span class="moved">moved</span>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="ev-none meta">Unchanged. Dependencies are identical in both runs.</p>
					{/if}
				</div>

				<div class="ev">
					<span class="label-xs">environment variable names</span>
					{#if diff.env_added.length || diff.env_removed.length}
						<div class="envs">
							{#each diff.env_added as k (k)}
								<code class="env added">+{k}</code>
							{/each}
							{#each diff.env_removed as k (k)}
								<code class="env removed">−{k}</code>
							{/each}
						</div>
						<p class="ev-note meta">Names only. runlog never records environment values.</p>
					{:else}
						<p class="ev-none meta">Same set of variables in both runs.</p>
					{/if}
				</div>

				{#if diff.command_changed}
					<div class="ev span2">
						<span class="label-xs">command</span>
						<p class="ev-note">
							The command is not the same in both runs, so some of the difference above may simply
							be a different program.
						</p>
						<div class="cmdpair tnum">
							<code>A · {diff.a ? fmtCommand(diff.a.command) : '—'}</code>
							<code>B · {diff.b ? fmtCommand(diff.b.command) : '—'}</code>
						</div>
					</div>
				{/if}
			</div>
		</section>
	</div>
{/if}

<style>
	.pad {
		padding: 16px;
	}
	.body {
		flex: 1;
		padding: 0 0 40px;
	}
	.back {
		font-size: 11px;
		color: var(--rl-meta);
		text-decoration: none;
	}
	.back:hover {
		color: var(--color-base-content);
	}
	.seg {
		display: flex;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		overflow: hidden;
	}
	.seg button {
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
	.seg button + button {
		border-left: 1px solid var(--rl-hair);
	}
	.seg button.on {
		background: var(--color-base-content);
		color: var(--color-base-100);
	}
	.pill {
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: transparent;
		color: var(--rl-meta);
		font: inherit;
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		padding: 4px 8px;
		cursor: pointer;
	}

	/* ---- verdict ---- */
	.verdict {
		display: flex;
		gap: 14px;
		padding: 18px 16px 18px 0;
		border-bottom: 1px solid var(--rl-hair);
	}
	.verdict .stripe {
		width: 3px;
		flex: none;
		background: var(--rl-hair-strong);
	}
	.verdict[data-tone='bad'] .stripe {
		background: var(--color-flare);
	}
	.verdict[data-tone='good'] .stripe {
		background: var(--color-success);
	}
	.verdict[data-tone='warn'] .stripe {
		background: var(--color-warning);
	}
	h1 {
		font-size: 19px;
		font-weight: 600;
		letter-spacing: normal;
		margin: 0;
		max-width: 72ch;
	}
	.verdict .detail {
		margin: 7px 0 0;
		font-size: 12.5px;
		color: var(--rl-meta);
		line-height: 1.6;
		max-width: 80ch;
	}

	/* ---- pair ---- */
	.pair {
		position: relative;
		display: grid;
		grid-template-columns: 1fr 1fr;
		border-bottom: 1px solid var(--rl-hair);
		background: var(--color-base-200);
	}
	.col {
		padding: 10px 16px;
		min-width: 0;
	}
	.col + .col {
		border-left: 1px solid var(--rl-hair);
	}
	.colhead {
		display: flex;
		align-items: baseline;
		gap: 8px;
	}
	.side {
		font-size: 11px;
		font-weight: 600;
		border: 1px solid var(--rl-hair-strong);
		border-radius: 2px;
		padding: 0 5px;
	}
	.runline {
		display: block;
		margin-top: 5px;
		text-decoration: none;
		color: var(--color-base-content);
	}
	.runline code {
		font-size: 12.5px;
	}
	.runline:hover code {
		border-bottom: 1px solid var(--rl-hair-strong);
	}
	.runmeta {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: center;
		margin-top: 5px;
		font-size: 11px;
	}
	.dirty {
		color: var(--color-warning);
		font-size: 9px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
	}
	.arrow {
		position: absolute;
		left: 50%;
		top: 50%;
		transform: translate(-50%, -50%);
		background: var(--color-base-200);
		border: 1px solid var(--rl-hair);
		border-radius: 50%;
		width: 22px;
		height: 22px;
		display: grid;
		place-items: center;
		font-size: 12px;
		color: var(--rl-meta);
	}
	.norun {
		margin: 6px 0 0;
		font-size: 12px;
	}

	/* ---- blocks ---- */
	.block {
		padding: 16px 16px 4px;
		border-bottom: 1px solid var(--rl-hair);
	}
	.blockhead {
		display: flex;
		align-items: baseline;
		gap: 9px;
		font-size: 13px;
		font-weight: 600;
		margin: 0 0 10px;
	}
	.blockhead .n {
		font-size: 12px;
		border: 1px solid var(--rl-hair-strong);
		border-radius: 2px;
		padding: 0 6px;
		font-weight: 600;
	}
	.flare-head .n {
		background: var(--color-flare);
		border-color: var(--color-flare);
		color: #180703;
	}
	.clear-head .n {
		background: var(--color-success);
		border-color: var(--color-success);
		color: var(--color-success-content);
	}
	.blockhead .sub {
		font-size: 11px;
		font-weight: 400;
	}
	.empty-line {
		margin: 0 0 12px;
		font-size: 12px;
		max-width: 80ch;
		line-height: 1.6;
	}

	.sigs {
		list-style: none;
		margin: 0 0 8px;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.sig {
		display: flex;
		border: 1px solid var(--rl-hair);
		border-radius: 3px;
		overflow: hidden;
		background: var(--color-base-200);
	}
	.sig .stripe {
		width: 3px;
		flex: none;
	}
	.sig.new .stripe {
		background: var(--color-flare);
	}
	.sig.resolved .stripe {
		background: var(--color-success);
	}
	.sig.new {
		border-color: color-mix(in oklch, var(--color-flare) 35%, transparent);
	}
	.sigbody {
		padding: 8px 12px;
		min-width: 0;
		flex: 1;
	}
	.sigtop {
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 0;
	}
	.tpl {
		flex: 1;
		font-size: 12px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.delta {
		flex: none;
		font-size: 12px;
		font-weight: 600;
		color: var(--color-flare);
	}
	.delta.gone {
		color: var(--color-success);
	}
	.sample {
		margin: 7px 0 0;
		padding: 7px 9px;
		background: var(--rl-code-bg);
		border-left: 1px solid var(--rl-hair-strong);
		color: var(--rl-meta);
		overflow-x: auto;
		max-width: 100%;
		font-size: 11px;
	}

	.counts {
		list-style: none;
		margin: 0 0 10px;
		padding: 0;
	}
	.counts li {
		display: grid;
		grid-template-columns: 76px 1fr 110px;
		gap: 12px;
		align-items: center;
		padding: 4px 0;
		border-bottom: 1px solid var(--rl-hair);
	}
	.ab {
		display: flex;
		gap: 6px;
		justify-content: flex-end;
		font-size: 12px;
	}
	.ab .up {
		color: var(--color-flare);
		font-weight: 600;
	}
	.ab .down {
		color: var(--color-success);
		font-weight: 600;
	}

	/* ---- evidence ---- */
	.evidence {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
		gap: 14px 24px;
		padding-bottom: 12px;
	}
	.ev.span2 {
		grid-column: 1 / -1;
	}
	.ev-none,
	.ev-note {
		margin: 6px 0 0;
		font-size: 11.5px;
		line-height: 1.6;
		max-width: 70ch;
	}
	.commits,
	.files,
	.locks {
		list-style: none;
		margin: 6px 0 0;
		padding: 0;
		font-size: 11.5px;
	}
	.commits li {
		display: grid;
		grid-template-columns: 62px 1fr auto;
		gap: 10px;
		padding: 2px 0;
		align-items: baseline;
	}
	.csub {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.cauth {
		font-size: 10.5px;
	}
	.files li code {
		font-size: 11.5px;
		color: var(--rl-meta);
	}
	.locks li {
		display: flex;
		gap: 10px;
		align-items: baseline;
		padding: 2px 0;
		flex-wrap: wrap;
	}
	.moved {
		font-size: 9px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--color-warning);
	}
	.envs {
		display: flex;
		flex-wrap: wrap;
		gap: 5px;
		margin-top: 6px;
	}
	.env {
		font-size: 11px;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		padding: 1px 6px;
	}
	.env.added {
		color: var(--color-flare);
		border-color: color-mix(in oklch, var(--color-flare) 40%, transparent);
	}
	.env.removed {
		color: var(--rl-meta);
	}
	.cmdpair {
		display: flex;
		flex-direction: column;
		gap: 3px;
		margin-top: 7px;
		font-size: 11.5px;
	}
</style>
