<script lang="ts">
	import { onMount } from 'svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toggle from '$lib/components/Toggle.svelte';
	import Meter from '$lib/components/Meter.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import ConfirmDialog from '$lib/components/ConfirmDialog.svelte';
	import {
		apiError,
		getSettings,
		installStep,
		projectRef,
		retention,
		squash,
		revealPath,
		uninstallStep,
		updateProject,
		updateSettings
	} from '$lib/api';
	import { app } from '$lib/stores/app.svelte';
	import { theme, type ThemeName } from '$lib/stores/theme.svelte';
	import { confirmSquashProject, type ConfirmCopy } from '$lib/confirm';
	import {
		STEP_COPY,
		installInOrder,
		installable,
		readInstall,
		type InstallRead
	} from '$lib/install';
	import {
		fmtAgo,
		fmtBytes,
		hhmmToMinutes,
		minutesToHhmm,
		pluralise,
		tildePath
	} from '$lib/format';
	import {
		INSTALL_STEPS,
		type InstallStep,
		type RetentionState,
		type Settings,
		type StepState
	} from '$lib/api/types';

	let settings = $state<Settings | null>(null);
	let saved = $state<Settings | null>(null);
	let installRead = $state<InstallRead>({ state: 'loading' });
	const install = $derived(installRead.state === 'ready' ? installRead.status : null);
	let confirming = $state<{ copy: ConfirmCopy; run: () => void } | null>(null);
	let retentions = $state<Record<number, RetentionState>>({});
	let squashing = $state<Record<number, boolean>>({});
	let squashed = $state<Record<number, string>>({});

	async function squashProject(id: number) {
		squashing[id] = true;
		try {
			const report = await squash(projectRef(id), null);
			squashed[id] =
				report.runs === 0
					? 'Nothing to squash — every finished run already is.'
					: `${pluralise(report.runs, 'run')} squashed, ${report.entries_dropped.toLocaleString()} ${report.entries_dropped === 1 ? 'repeat' : 'repeats'} dropped · ${fmtBytes(report.bytes_before)} → ${fmtBytes(report.bytes_after)}.`;
			retentions[id] = await retention(projectRef(id));
		} catch (err) {
			squashed[id] = apiError(err);
		} finally {
			squashing[id] = false;
		}
	}
	let loadError = $state<string | null>(null);
	let saveError = $state<string | null>(null);
	let saving = $state(false);
	let stepBusy = $state<InstallStep | null>(null);
	let justSaved = $state(false);
	let installingAll = $state(false);
	let allProgress = $state<{ done: number; total: number } | null>(null);
	let allFailures = $state<string[]>([]);
	let refreshError = $state<string | null>(null);

	const MB = 1024 * 1024;

	const dirty = $derived(
		settings !== null && saved !== null && JSON.stringify(settings) !== JSON.stringify(saved)
	);

	/**
	 * Global settings are read exactly once. The app store re-polls the daemon
	 * every ten seconds; if that poll re-read settings it would silently throw
	 * away whatever the reader was halfway through typing.
	 */
	async function loadSettings() {
		loadError = null;
		try {
			const s = await getSettings();
			settings = s;
			saved = structuredClone(s);
		} catch (err) {
			loadError = apiError(err);
		}
	}

	/**
	 * Separate from settings, which need the daemon. This is how someone whose daemon is
	 * down gets it running, so a settings failure must not leave it unread.
	 */
	async function loadInstall() {
		refreshError = null;
		const next = await readInstall();
		// A failed re-read should not take away rows that are already on screen.
		if (next.state === 'failed' && install) refreshError = next.error;
		else installRead = next;
	}

	async function retryInstall() {
		installRead = { state: 'loading' };
		await loadInstall();
	}

	function replaceStep(step: InstallStep, state: StepState) {
		if (installRead.state === 'ready') {
			installRead = { state: 'ready', status: { ...installRead.status, [step]: state } };
		}
	}

	/** The daemon may have just come up, and with it everything it held back. */
	async function afterDaemonStep() {
		await app.refresh();
		if (!settings) await loadSettings();
	}

	onMount(() => {
		void loadSettings();
		void loadInstall();
	});

	/** Retention meters do follow the project list — they are read-only. */
	$effect(() => {
		const ids = app.projects.map((p) => p.id);
		let cancelled = false;
		(async () => {
			try {
				const rs = await Promise.all(
					ids.map(async (id) => [id, await retention(projectRef(id))] as const)
				);
				if (!cancelled) retentions = Object.fromEntries(rs);
			} catch (err) {
				if (!cancelled) loadError = apiError(err);
			}
		})();
		return () => {
			cancelled = true;
		};
	});

	async function save() {
		if (!settings) return;
		saving = true;
		saveError = null;
		try {
			await updateSettings($state.snapshot(settings));
			saved = structuredClone($state.snapshot(settings));
			justSaved = true;
			setTimeout(() => (justSaved = false), 2000);
		} catch (err) {
			saveError = apiError(err);
		} finally {
			saving = false;
		}
	}

	function revert() {
		if (saved) settings = structuredClone(saved);
		saveError = null;
	}

	async function setProject(
		id: number,
		patch: { retentionBytes?: number | null; pushEnabled?: boolean; notifyEnabled?: boolean }
	) {
		const p = app.project(id);
		if (!p) return;
		const next = {
			retentionBytes: patch.retentionBytes !== undefined ? patch.retentionBytes : p.retention_bytes,
			pushEnabled: patch.pushEnabled ?? p.push_enabled,
			notifyEnabled: patch.notifyEnabled ?? p.notify_enabled
		};
		try {
			await updateProject(projectRef(id), next.retentionBytes, next.pushEnabled, next.notifyEnabled);
			await app.refresh();
			retentions[id] = await retention(projectRef(id));
		} catch (err) {
			saveError = apiError(err);
		}
	}

	async function runStep(step: InstallStep, direction: 'install' | 'uninstall') {
		stepBusy = step;
		saveError = null;
		try {
			const next = direction === 'install' ? await installStep(step) : await uninstallStep(step);
			replaceStep(step, next);
			if (step === 'daemon') void afterDaemonStep();
		} catch (err) {
			saveError = apiError(err);
		} finally {
			stepBusy = null;
		}
	}

	const pending = $derived(install ? installable(install) : []);

	/** Install every outstanding step in one go. */
	async function installAll() {
		const steps = pending;
		if (steps.length === 0) return;

		installingAll = true;
		saveError = null;
		allFailures = [];
		allProgress = { done: 0, total: steps.length };
		stepBusy = steps[0];

		allFailures = await installInOrder(steps, installStep, (step, done, state) => {
			if (state) replaceStep(step, state);
			allProgress = { done, total: steps.length };
			stepBusy = steps[done] ?? null;
		});

		stepBusy = null;
		installingAll = false;
		allProgress = null;
		// Re-read rather than trust the per-step replies: installing `logbear` changes what
		// the later steps can see about themselves.
		await loadInstall();
		void afterDaemonStep();
	}

	const THEMES: { value: ThemeName | 'system'; label: string }[] = [
		{ value: 'system', label: 'system' },
		{ value: 'logbear-light', label: 'light' },
		{ value: 'logbear-dark', label: 'dark' }
	];

	const quietOn = $derived(settings?.quiet_hours != null);
</script>

<PageHeader
	title="Settings"
	sub={app.daemon ? `logbear ${app.daemon.version} · pid ${app.daemon.pid}` : 'daemon not reachable'}
/>

<div class="pane @container flex-1 pb-[90px]">
	{#if loadError}
		<ErrorNote
			what="Could not read settings."
			detail={loadError}
			retry={() => void loadSettings()}
		/>
	{/if}
	{#if saveError}
		<ErrorNote what="That change did not stick." detail={saveError} />
	{/if}

	{#if settings}
		<!-- ---------------------------------------------------------------- -->
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 @max-[900px]:grid-cols-1 @max-[900px]:gap-3 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Storage</h3>
				<p class="meta mt-1.5 text-[12px] leading-[1.65]">
					logbear stores raw output on disk. When a project passes its cap the oldest finished runs
					are squashed first: each keeps one copy of every distinct entry and drops its repeats.
					Only if that is not enough are whole runs evicted — their counts and fingerprints
					survive, their bytes do not.
				</p>
			</div>

			<div class="min-w-0">
				<div class="my-3">
					<label class="mb-[5px] block text-[12px]" for="global-cap">Default cap per project</label>
					<div class="flex flex-wrap items-center gap-2">
						<input
							id="global-cap"
							type="number"
							min="64"
							step="64"
							class="tnum input input-xs w-[110px] border-hair bg-transparent px-[7px] text-[12px] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
							value={Math.round(settings.retention_bytes / MB)}
							oninput={(e) => {
								const v = Number((e.currentTarget as HTMLInputElement).value);
								if (settings && Number.isFinite(v)) settings.retention_bytes = Math.max(1, v) * MB;
							}}
						/>
						<span class="meta text-[12px]">MB</span>
						<span class="meta max-w-[62ch] text-[12px] leading-[1.6]"
							>Applies to any project without its own cap.</span
						>
					</div>
				</div>

				<div class="mt-4 rule-t">
					{#each app.projects as p (p.id)}
						{@const r = retentions[p.id]}
						<div
							class="grid grid-cols-[minmax(140px,200px)_minmax(0,1fr)_130px] items-center gap-[18px] py-3 rule-b"
						>
							<div class="min-w-0">
								<span class="block text-[12.5px] font-medium">{p.name}</span>
								<button
									type="button"
									class="meta block max-w-full cursor-pointer overflow-hidden text-left text-[12px] text-ellipsis whitespace-nowrap hover:text-base-content"
									onclick={() => void revealPath(p.root)}
									title="Reveal {p.root} in Finder">{tildePath(p.root)}</button
								>
							</div>

							<div>
								{#if r}
									<Meter
										used={r.bytes_used}
										cap={r.bytes_cap}
										label="{r.runs_total} runs stored"
										note={[
											r.runs_squashed > 0 ? `${pluralise(r.runs_squashed, 'run')} squashed.` : '',
											r.runs_evicted > 0
												? `${pluralise(r.runs_evicted, 'run')} already evicted. Oldest surviving run: ${r.oldest_run_at ? fmtAgo(r.oldest_run_at) : 'unknown'}.`
												: `Nothing evicted yet. Oldest run: ${r.oldest_run_at ? fmtAgo(r.oldest_run_at) : 'unknown'}.`
										]
											.filter(Boolean)
											.join(' ')}
									/>
									<div class="mt-1.5 flex flex-wrap items-baseline gap-2">
										<button
											type="button"
											class="cursor-pointer rounded-[2px] border border-hair px-2 py-[3px] text-[12px] tracking-[0.1em] text-meta uppercase enabled:hover:border-hair-strong enabled:hover:text-base-content disabled:cursor-default disabled:opacity-50"
											disabled={squashing[p.id] === true}
											onclick={() =>
												(confirming = {
													copy: confirmSquashProject(p.name, r.runs_total),
													run: () => void squashProject(p.id)
												})}
											title="Shrink every finished run to the first occurrence of each distinct entry, now rather than when the cap is reached. Repeats are deleted for good; counts and signatures are kept."
											>{squashing[p.id] ? 'squashing…' : 'squash finished runs'}</button
										>
										{#if squashed[p.id]}<span class="meta text-[12px]">{squashed[p.id]}</span>{/if}
									</div>
								{:else}
									<p class="meta text-[12px]">Reading store size…</p>
								{/if}
							</div>

							<div class="flex flex-col items-start gap-[3px]">
								<label class="label-xs mb-[5px] block text-[12px]" for="cap-{p.id}">cap</label>
								<div class="flex flex-wrap items-center gap-1">
									<input
										id="cap-{p.id}"
										type="number"
										min="0"
										step="64"
										class="tnum input input-xs w-[78px] border-hair bg-transparent px-[7px] text-[12px] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
										placeholder={String(Math.round(settings.retention_bytes / MB))}
										value={p.retention_bytes === null ? '' : Math.round(p.retention_bytes / MB)}
										onchange={(e) => {
											const raw = (e.currentTarget as HTMLInputElement).value.trim();
											void setProject(p.id, {
												retentionBytes: raw === '' ? null : Math.max(1, Number(raw)) * MB
											});
										}}
									/>
									<span class="meta text-[12px]">MB</span>
								</div>
								{#if p.retention_bytes === null}
									<span class="meta text-[12px]">using default</span>
								{:else}
									<button
										type="button"
										class="meta max-w-full cursor-pointer overflow-hidden border-b border-dotted border-hair-strong text-left text-[12px] text-ellipsis whitespace-nowrap hover:text-base-content"
										onclick={() => void setProject(p.id, { retentionBytes: null })}
										>use default</button
									>
								{/if}
							</div>
						</div>
					{:else}
						<p class="meta text-[12px]">No projects tracked yet.</p>
					{/each}
				</div>
			</div>
		</section>

		<!-- ---------------------------------------------------------------- -->
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 @max-[900px]:grid-cols-1 @max-[900px]:gap-3 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Notifications</h3>
				<p class="meta mt-1.5 text-[12px] leading-[1.65]">
					A notification fires when a run produces an error signature this project has not seen
					before. Repeats of a known signature stay silent.
				</p>
			</div>

			<div class="min-w-0">
				<Toggle
					bind:checked={settings.notifications_enabled}
					label="Notify on new error signatures"
					hint="Off means logbear still records everything — it just never interrupts you."
				/>

				<div class="my-3 {settings.notifications_enabled ? '' : 'opacity-55'}">
					<Toggle
						checked={quietOn}
						label="Quiet hours"
						disabled={!settings.notifications_enabled}
						hint="Notifications inside this window are recorded and shown in the app, never surfaced by the OS."
						onchange={(v) => {
							if (!settings) return;
							settings.quiet_hours = v ? { start_min: 22 * 60, end_min: 8 * 60 } : null;
						}}
					/>
					{#if settings.quiet_hours}
						<div class="mt-1.5 ml-10 flex flex-wrap items-center gap-2">
							<input
								type="time"
								class="tnum input input-xs w-[110px] border-hair bg-transparent px-[7px] text-[12px] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
								aria-label="Quiet hours start"
								value={minutesToHhmm(settings.quiet_hours.start_min)}
								onchange={(e) => {
									if (settings?.quiet_hours)
										settings.quiet_hours.start_min = hhmmToMinutes(
											(e.currentTarget as HTMLInputElement).value
										);
								}}
							/>
							<span class="meta">to</span>
							<input
								type="time"
								class="tnum input input-xs w-[110px] border-hair bg-transparent px-[7px] text-[12px] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
								aria-label="Quiet hours end"
								value={minutesToHhmm(settings.quiet_hours.end_min)}
								onchange={(e) => {
									if (settings?.quiet_hours)
										settings.quiet_hours.end_min = hhmmToMinutes(
											(e.currentTarget as HTMLInputElement).value
										);
								}}
							/>
							<span class="meta max-w-[62ch] text-[12px] leading-[1.6]">
								{minutesToHhmm(settings.quiet_hours.start_min)} →
								{minutesToHhmm(settings.quiet_hours.end_min)}
								{settings.quiet_hours.end_min < settings.quiet_hours.start_min
									? '(crosses midnight)'
									: ''}
							</span>
						</div>
					{/if}
				</div>

				<div class="mt-[14px] pt-2.5 rule-t">
					<span class="label-xs">per project</span>
					{#each app.projects as p (p.id)}
						<div class="flex max-w-[340px] items-center justify-between py-1">
							<span class="block text-[12.5px] font-medium">{p.name}</span>
							<button
								type="button"
								class="badge badge-xs h-auto text-[12px] cursor-pointer border-hair px-[8px] py-[2px] tracking-[0.1em] uppercase hover:border-hair-strong {p.notify_enabled
									? 'badge-outline'
									: 'badge-dash text-meta'}"
								onclick={() => void setProject(p.id, { notifyEnabled: !p.notify_enabled })}
							>
								{p.notify_enabled ? 'notifying' : 'muted'}
							</button>
						</div>
					{/each}
				</div>
			</div>
		</section>

		<!-- ---------------------------------------------------------------- -->
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 @max-[900px]:grid-cols-1 @max-[900px]:gap-3 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Agent push</h3>
				<p class="meta mt-1.5 text-[12px] leading-[1.65]">
					With an agent hook installed, LogBear tells the coding agent about new errors as they
					happen, and can stop it finishing a turn until it has dealt with them.
				</p>
			</div>

			<div class="min-w-0">
				<Toggle
					bind:checked={settings.push_enabled}
					label="Push errors to agents"
					hint="Needs an agent hook from Install below. Off, the hooks stay in place but never block or add anything."
				/>

				<div class="my-3">
					<label class="mb-[5px] block text-[12px]" for="budget">Block budget per session</label>
					<div class="flex flex-wrap items-center gap-2">
						<input
							id="budget"
							type="number"
							min="0"
							max="200"
							class="tnum input input-xs w-[78px] border-hair bg-transparent px-[7px] text-[12px] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong disabled:border-hair disabled:bg-transparent disabled:text-base-content disabled:opacity-50"
							bind:value={settings.block_budget}
							disabled={!settings.push_enabled}
						/>
						<span class="meta max-w-[62ch] text-[12px] leading-[1.6]">
							How many times one agent session can be stopped from finishing. After that, new
							errors reach the agent as advice and are still recorded and queryable.
						</span>
					</div>
				</div>

				<div class="mt-[14px] pt-2.5 rule-t">
					<span class="label-xs">per project</span>
					{#each app.projects as p (p.id)}
						<div class="flex max-w-[340px] items-center justify-between py-1">
							<span class="block text-[12.5px] font-medium">{p.name}</span>
							<button
								type="button"
								class="badge badge-xs h-auto text-[12px] cursor-pointer border-hair px-[8px] py-[2px] tracking-[0.1em] uppercase hover:border-hair-strong {p.push_enabled
									? 'badge-outline'
									: 'badge-dash text-meta'}"
								onclick={() => void setProject(p.id, { pushEnabled: !p.push_enabled })}
							>
								{p.push_enabled ? 'pushing' : 'off'}
							</button>
						</div>
					{/each}
				</div>
			</div>
		</section>

		<!-- ---------------------------------------------------------------- -->
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 @max-[900px]:grid-cols-1 @max-[900px]:gap-3 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Capture and redaction</h3>
				<p class="meta mt-1.5 text-[12px] leading-[1.65]">
					Redaction happens on the way in. Data that never lands cannot leak. Environment variable
					values are never recorded at all — only their names.
				</p>
			</div>

			<div class="min-w-0">
				<Toggle
					bind:checked={settings.redaction_enabled}
					label="Redact secrets before writing"
					hint="Matches AWS keys, bearer tokens, connection strings and private key blocks. Turning this off is not recommended and does not retroactively expose anything already stored."
				/>

				<div class="my-3">
					<label class="mb-[5px] block text-[12px]" for="denylist"
						>Never capture these commands</label
					>
					<textarea
						id="denylist"
						rows="3"
						class="w-full max-w-[640px] resize-y rounded-[2px] border border-hair bg-code-bg px-[9px] py-[7px] font-mono text-[12px] leading-[1.6] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
						value={settings.denylist.join('\n')}
						oninput={(e) => {
							if (settings)
								settings.denylist = (e.currentTarget as HTMLTextAreaElement).value
									.split('\n')
									.map((s) => s.trim())
									.filter(Boolean);
						}}
					></textarea>
					<p class="meta mt-[5px] max-w-[62ch] text-[12px] leading-[1.6]">
						One per line. Sessions rather than runs — a shell, an editor, a database client — are
						passed through untouched and nothing is stored.
					</p>
				</div>

				<div class="my-3">
					<label class="mb-[5px] block text-[12px]" for="patterns">Extra redaction patterns</label>
					<textarea
						id="patterns"
						rows="3"
						class="w-full max-w-[640px] resize-y rounded-[2px] border border-hair bg-code-bg px-[9px] py-[7px] font-mono text-[12px] leading-[1.6] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
						value={settings.extra_redaction_patterns.join('\n')}
						oninput={(e) => {
							if (settings)
								settings.extra_redaction_patterns = (e.currentTarget as HTMLTextAreaElement).value
									.split('\n')
									.map((s) => s.trim())
									.filter(Boolean);
						}}
					></textarea>
					<p class="meta mt-[5px] max-w-[62ch] text-[12px] leading-[1.6]">
						One regular expression per line, applied in addition to the built-in set. Matches are
						replaced with <code>[redacted]</code> before anything is written to disk.
					</p>
				</div>
			</div>
		</section>
	{/if}

	<!-- ------------------------------------------------------------------ -->
	<section
		class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 @max-[900px]:grid-cols-1 @max-[900px]:gap-3 rule-b"
	>
		<div>
			<h3 class="text-[13px] font-semibold">Appearance</h3>
			<p class="meta mt-1.5 text-[12px] leading-[1.65]">
				System follows the light or dark setting of your OS, including when it changes.
			</p>
		</div>
		<div class="min-w-0">
			<div
				class="inline-flex overflow-hidden rounded-[2px] border border-hair"
				role="group"
				aria-label="Theme"
			>
				{#each THEMES as t, i (t.value)}
					<button
						type="button"
						class="cursor-pointer px-[9px] py-[4px] text-[12px] tracking-[0.1em] uppercase {i > 0
							? 'border-l border-hair'
							: ''} {theme.choice === t.value ? 'bg-base-content text-base-100' : 'text-meta'}"
						aria-pressed={theme.choice === t.value}
						onclick={() => (t.value === 'system' ? theme.followSystem() : theme.set(t.value))}
						>{t.label}</button
					>
				{/each}
			</div>
		</div>
	</section>

	<!-- ------------------------------------------------------------------ -->
	<section
		class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 @max-[900px]:grid-cols-1 @max-[900px]:gap-3 rule-b"
	>
		<div>
			<h3 class="text-[13px] font-semibold">Install</h3>
			<p class="meta mt-1.5 text-[12px] leading-[1.65]">
				Each step is independent and can be removed here. Installing backs up a config file before
				changing it and shows the backup path. The agent hooks can block your agent; read them
				before installing.
			</p>
			{#if install && pending.length > 0}
				<button
					type="button"
					class="btn btn-primary btn-sm mt-3 h-auto px-[11px] py-[4px] text-[12px] font-normal disabled:border-primary disabled:bg-primary disabled:text-primary-content disabled:opacity-45"
					disabled={stepBusy !== null || installingAll}
					onclick={() => void installAll()}
				>
					{#if allProgress}
						Installing… {allProgress.done}/{allProgress.total}
					{:else}
						Install all {pending.length} remaining
					{/if}
				</button>
				<p class="meta mt-1.5 text-[12px] leading-[1.6]">
					Sets up every step not yet installed, including the agent hooks.
				</p>
			{/if}
		</div>

		<div class="min-w-0">
			{#if refreshError}
				<ErrorNote
					what="Could not refresh the install state."
					detail={refreshError}
					retry={() => void loadInstall()}
				/>
			{/if}
			{#if allFailures.length > 0}
				<div class="mb-3 rounded border border-warning/40 px-3 py-2">
					<p class="text-[12px] font-medium text-warning">
						{pluralise(allFailures.length, 'step')} could not be installed
					</p>
					<ul class="mt-1 flex flex-col gap-0.5">
						{#each allFailures as failure (failure)}
							<li class="meta text-[12px] leading-[1.6]">{failure}</li>
						{/each}
					</ul>
				</div>
			{/if}
			{#if install}
				<div class="flex flex-col">
					{#each INSTALL_STEPS as step (step)}
						{@const s = install[step]}
						<div
							class="grid grid-cols-[10px_minmax(0,1fr)_110px] items-start gap-3 py-3 rule-b last:border-b-0"
						>
							<span
								class="mt-[5px] h-2 w-2 rounded-full border {s.installed
									? 'border-success bg-success'
									: 'border-hair-strong'} {s.available ? '' : 'border-dashed'}"
								aria-hidden="true"
							></span>
							<div>
								<div class="flex items-baseline gap-2.5">
									<span class="text-[12.5px] font-medium">{STEP_COPY[step].title}</span>
									<span
										class="label-xs {s.installed
											? 'text-success'
											: s.available
												? ''
												: 'text-warning'}"
									>
										{s.installed ? 'installed' : s.available ? 'not installed' : 'unavailable'}
									</span>
								</div>
								<p class="meta mt-1 max-w-[74ch] text-[12px] leading-[1.6]">
									{STEP_COPY[step].what}
								</p>
								<p class="mt-1 max-w-[74ch] text-[12px] leading-[1.6] text-base-content opacity-75">
									{s.detail}
								</p>
								{#if s.backup}
									<button
										type="button"
										class="meta max-w-full cursor-pointer overflow-hidden border-b border-dotted border-hair-strong text-left text-[12px] text-ellipsis whitespace-nowrap hover:text-base-content"
										onclick={() => void revealPath(s.backup!)}
										title="Reveal the backup in Finder">backup: {s.backup}</button
									>
								{/if}
							</div>
							<div class="flex justify-end">
								{#if s.installed}
									<button
										type="button"
										class="btn btn-outline btn-sm h-auto border-hair-strong px-[11px] py-[4px] text-[12px] font-normal hover:border-base-content hover:bg-transparent disabled:border-hair-strong disabled:bg-transparent disabled:text-base-content disabled:opacity-45"
										disabled={stepBusy !== null}
										onclick={() => void runStep(step, 'uninstall')}
									>
										{stepBusy === step ? 'removing…' : 'Uninstall'}
									</button>
								{:else}
									<button
										type="button"
										class="btn btn-primary btn-sm h-auto px-[11px] py-[4px] text-[12px] font-normal disabled:border-primary disabled:bg-primary disabled:text-primary-content disabled:opacity-45"
										disabled={stepBusy !== null || !s.available}
										onclick={() => void runStep(step, 'install')}
									>
										{stepBusy === step ? 'installing…' : 'Install'}
									</button>
								{/if}
							</div>
						</div>
					{/each}
				</div>
			{:else if installRead.state === 'failed'}
				<ErrorNote
					what="Could not read the install state."
					detail={installRead.error}
					retry={() => void retryInstall()}
				/>
			{:else}
				<p class="meta text-[12px]">Reading install state…</p>
			{/if}
		</div>
	</section>

	<!-- ------------------------------------------------------------------ -->
	{#if app.daemon}
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 @max-[900px]:grid-cols-1 @max-[900px]:gap-3"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Daemon</h3>
			</div>
			<div class="min-w-0">
				<dl class="grid grid-cols-[repeat(auto-fill,minmax(160px,1fr))] gap-x-5 gap-y-3">
					<div>
						<dt class="label-xs mb-[3px]">version</dt>
						<dd class="tnum text-[12px]">{app.daemon.version}</dd>
					</div>
					<div>
						<dt class="label-xs mb-[3px]">pid</dt>
						<dd class="tnum text-[12px]">{app.daemon.pid}</dd>
					</div>
					<div>
						<dt class="label-xs mb-[3px]">up since</dt>
						<dd class="tnum text-[12px]">{fmtAgo(app.daemon.started_at)}</dd>
					</div>
					<div>
						<dt class="label-xs mb-[3px]">socket</dt>
						<dd class="text-[12px]">
							<button
								type="button"
								class="meta max-w-full cursor-pointer overflow-hidden border-b border-dotted border-hair-strong text-left text-[12px] text-ellipsis whitespace-nowrap hover:text-base-content"
								onclick={() => void revealPath(app.daemon!.socket)}
								>{tildePath(app.daemon.socket)}</button
							>
						</dd>
					</div>
					<div>
						<dt class="label-xs mb-[3px]">projects</dt>
						<dd class="tnum text-[12px]">{app.daemon.projects}</dd>
					</div>
					<div>
						<dt class="label-xs mb-[3px]">active runs</dt>
						<dd class="tnum text-[12px]">{app.daemon.active_runs}</dd>
					</div>
					<div>
						<dt class="label-xs mb-[3px]">total on disk</dt>
						<dd class="tnum text-[12px]">{fmtBytes(app.daemon.total_db_bytes)}</dd>
					</div>
					<div>
						<dt class="label-xs mb-[3px]">spool pending</dt>
						<dd class="tnum text-[12px]">{app.daemon.spool_pending}</dd>
					</div>
				</dl>
			</div>
		</section>
	{/if}
</div>

{#if dirty || justSaved}
	<div
		class="fixed bottom-4 left-1/2 z-30 flex -translate-x-1/2 items-center gap-3 rounded-[3px] border bg-base-200 px-3 py-2 shadow-[0_8px_24px_rgb(0_0_0/0.3)] {!dirty &&
		justSaved
			? 'border-[color-mix(in_oklch,var(--color-success)_50%,transparent)]'
			: 'border-hair-strong'}"
	>
		<span class="text-[12px]">
			{#if dirty}Unsaved changes to global settings.{:else}Saved.{/if}
		</span>
		{#if dirty}
			<button
				type="button"
				class="btn btn-outline btn-sm h-auto border-hair-strong px-[11px] py-[4px] text-[12px] font-normal hover:border-base-content hover:bg-transparent disabled:border-hair-strong disabled:bg-transparent disabled:text-base-content disabled:opacity-45"
				onclick={revert}
				disabled={saving}>Discard</button
			>
			<button
				type="button"
				class="btn btn-primary btn-sm h-auto px-[11px] py-[4px] text-[12px] font-normal disabled:border-primary disabled:bg-primary disabled:text-primary-content disabled:opacity-45"
				onclick={save}
				disabled={saving}
			>
				{saving ? 'saving…' : 'Save changes'}
			</button>
		{/if}
	</div>
{/if}

<ConfirmDialog
	request={confirming?.copy ?? null}
	oncancel={() => (confirming = null)}
	onconfirm={() => {
		confirming?.run();
		confirming = null;
	}}
/>
