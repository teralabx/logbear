<script lang="ts">
	import { onMount } from 'svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toggle from '$lib/components/Toggle.svelte';
	import Meter from '$lib/components/Meter.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import {
		apiError,
		getSettings,
		installStatus,
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
		type InstallStatus,
		type InstallStep,
		type RetentionState,
		type Settings
	} from '$lib/api/types';

	let settings = $state<Settings | null>(null);
	let saved = $state<Settings | null>(null);
	let install = $state<InstallStatus | null>(null);
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

	const MB = 1024 * 1024;

	const dirty = $derived(
		settings !== null && saved !== null && JSON.stringify(settings) !== JSON.stringify(saved)
	);

	/**
	 * Global settings are read exactly once. The app store re-polls the daemon
	 * every ten seconds; if that poll re-read settings it would silently throw
	 * away whatever the reader was halfway through typing.
	 */
	async function loadAll() {
		loadError = null;
		try {
			const [s, i] = await Promise.all([getSettings(), installStatus()]);
			settings = s;
			saved = structuredClone(s);
			install = i;
		} catch (err) {
			loadError = apiError(err);
		}
	}

	onMount(() => {
		void loadAll();
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
			if (install) install = { ...install, [step]: next };
		} catch (err) {
			saveError = apiError(err);
		} finally {
			stepBusy = null;
		}
	}

	/** Steps that could be installed right now: available, and not already done. */
	const pending = $derived(
		install ? INSTALL_STEPS.filter((s) => install![s].available && !install![s].installed) : []
	);

	/**
	 * Install every outstanding step in one go.
	 *
	 * Sequential rather than parallel: several steps write to the same shell profiles and
	 * harness configs, and two of them racing would interleave their edits. One failure does
	 * not stop the rest — the steps are independent, and a machine missing Codex should still
	 * get its Claude Code integration.
	 */
	async function installAll() {
		const steps = pending;
		if (steps.length === 0) return;

		installingAll = true;
		saveError = null;
		allFailures = [];
		allProgress = { done: 0, total: steps.length };

		for (const step of steps) {
			stepBusy = step;
			try {
				const next = await installStep(step);
				if (install) install = { ...install, [step]: next };
			} catch (err) {
				allFailures = [...allFailures, `${STEP_COPY[step].title}: ${apiError(err)}`];
			}
			allProgress = { done: allProgress.done + 1, total: steps.length };
		}

		stepBusy = null;
		installingAll = false;
		allProgress = null;
		// Re-read rather than trust the per-step replies: installing the PATH shims changes
		// what the later steps can see about themselves.
		try {
			install = await installStatus();
		} catch {
			// The individual replies already updated the rows; a failed refresh is cosmetic.
		}
	}

	const STEP_COPY: Record<InstallStep, { title: string; what: string }> = {
		path: {
			title: 'PATH shims',
			what: 'Puts ~/.logbear/bin first in your shell PATH. Commands like npm, mix and cargo then run through logbear with no prefix — including the ones your Makefiles and your coding agent invoke.'
		},
		daemon: {
			title: 'Background daemon',
			what: 'Registers a login agent so capture keeps working after this window is closed. Without it, only runs started while the app is open are recorded.'
		},
		mcp_claude: {
			title: 'MCP server — Claude Code',
			what: 'Registers logbear as an MCP server for Claude Code, giving it three read-only tools: recent runs, diff between two runs, and errors for a run.'
		},
		mcp_codex: {
			title: 'MCP server — Codex',
			what: 'The same three read-only tools, registered in the Codex CLI config.'
		},
		hook_claude: {
			title: 'Session hook — Claude Code',
			what: 'Adds a SessionStart hook that hands Claude Code a short summary of recent runs, so it starts a session already knowing what failed.'
		},
		hook_codex: {
			title: 'Session hook — Codex',
			what: 'The same session summary, wired into the Codex CLI.'
		},
		mcp_pi: {
			title: 'MCP server — pi',
			what: 'Registers logbear in ~/.pi/agent/mcp.json with direct tool exposure, so pi sees the tools without searching for them. Needs pi 1.0 or later.'
		},
		mcp_hermes: {
			title: 'MCP server — Hermes',
			what: 'Adds logbear under mcp_servers in ~/.hermes/config.yaml, between marker comments so removing it leaves the rest of the file untouched.'
		},
		hook_pi: {
			title: 'Extension — pi',
			what: 'pi has no command hooks, so this writes a small extension to ~/.pi/agent/extensions that runs the logbear hook: it states the rule once per session, notes new output after edits, and keeps pi working while errors it has not seen are unread.'
		},
		hook_hermes: {
			title: 'Shell hooks — Hermes',
			what: 'Adds pre_llm_call and pre_verify hooks to ~/.hermes/config.yaml. Hermes holds back from finishing a turn that edited code while new errors are unread, and hears about errors from other turns when your next message arrives. Approve both when Hermes asks.'
		}
	};

	const quietOn = $derived(settings?.quiet_hours != null);
</script>

<PageHeader
	title="Settings"
	sub={app.daemon ? `logbear ${app.daemon.version} · pid ${app.daemon.pid}` : 'daemon not reachable'}
/>

<div class="pane flex-1 pb-[90px]">
	{#if loadError}
		<ErrorNote what="Could not read settings." detail={loadError} retry={() => void loadAll()} />
	{/if}
	{#if saveError}
		<ErrorNote what="That change did not stick." detail={saveError} />
	{/if}

	{#if settings}
		<!-- ---------------------------------------------------------------- -->
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Storage</h3>
				<p class="meta mt-1.5 text-[11.5px] leading-[1.65]">
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
						<span class="meta text-[11px]">MB</span>
						<span class="meta max-w-[62ch] text-[11px] leading-[1.6]"
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
									class="meta block max-w-full cursor-pointer overflow-hidden text-left text-[11px] text-ellipsis whitespace-nowrap hover:text-base-content"
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
											class="cursor-pointer rounded-[2px] border border-hair px-2 py-[3px] text-[10px] tracking-[0.16em] text-meta uppercase enabled:hover:border-hair-strong enabled:hover:text-base-content disabled:cursor-default disabled:opacity-50"
											disabled={squashing[p.id] === true}
											onclick={() => void squashProject(p.id)}
											title="Shrink every finished run to the first occurrence of each distinct entry, now rather than when the cap is reached. Repeats are deleted for good; counts and signatures are kept."
											>{squashing[p.id] ? 'squashing…' : 'squash finished runs'}</button
										>
										{#if squashed[p.id]}<span class="meta text-[11px]">{squashed[p.id]}</span>{/if}
									</div>
								{:else}
									<p class="meta text-[11px]">Reading store size…</p>
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
									<span class="meta text-[11px]">MB</span>
								</div>
								{#if p.retention_bytes === null}
									<span class="meta text-[11px]">using default</span>
								{:else}
									<button
										type="button"
										class="meta max-w-full cursor-pointer overflow-hidden border-b border-dotted border-hair-strong text-left text-[10.5px] text-ellipsis whitespace-nowrap hover:text-base-content"
										onclick={() => void setProject(p.id, { retentionBytes: null })}
										>use default</button
									>
								{/if}
							</div>
						</div>
					{:else}
						<p class="meta text-[11px]">No projects tracked yet.</p>
					{/each}
				</div>
			</div>
		</section>

		<!-- ---------------------------------------------------------------- -->
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Notifications</h3>
				<p class="meta mt-1.5 text-[11.5px] leading-[1.65]">
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
							<span class="meta max-w-[62ch] text-[11px] leading-[1.6]">
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
								class="badge badge-xs h-auto cursor-pointer border-hair px-[8px] py-[2px] tracking-[0.16em] uppercase hover:border-hair-strong {p.notify_enabled
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
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Agent push</h3>
				<p class="meta mt-1.5 text-[11.5px] leading-[1.65]">
					When a run fails, logbear can push a short block of context — the command, the exit code,
					the new signatures — into the coding agent's next turn instead of waiting to be asked.
				</p>
			</div>

			<div class="min-w-0">
				<Toggle
					bind:checked={settings.push_enabled}
					label="Push run context to agents"
					hint="Requires the session hook below to be installed for at least one agent."
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
						<span class="meta max-w-[62ch] text-[11px] leading-[1.6]">
							The most blocks logbear will push into one agent session. Reached budget means later
							failures are still recorded and still queryable — they just stop being volunteered.
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
								class="badge badge-xs h-auto cursor-pointer border-hair px-[8px] py-[2px] tracking-[0.16em] uppercase hover:border-hair-strong {p.push_enabled
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
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 rule-b"
		>
			<div>
				<h3 class="text-[13px] font-semibold">Capture and redaction</h3>
				<p class="meta mt-1.5 text-[11.5px] leading-[1.65]">
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
						class="w-full max-w-[640px] resize-y rounded-[2px] border border-hair bg-code-bg px-[9px] py-[7px] font-mono text-[11.5px] leading-[1.6] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
						value={settings.denylist.join('\n')}
						oninput={(e) => {
							if (settings)
								settings.denylist = (e.currentTarget as HTMLTextAreaElement).value
									.split('\n')
									.map((s) => s.trim())
									.filter(Boolean);
						}}
					></textarea>
					<p class="meta mt-[5px] max-w-[62ch] text-[11px] leading-[1.6]">
						One per line. Sessions rather than runs — a shell, an editor, a database client — are
						passed through untouched and nothing is stored.
					</p>
				</div>

				<div class="my-3">
					<label class="mb-[5px] block text-[12px]" for="patterns">Extra redaction patterns</label>
					<textarea
						id="patterns"
						rows="3"
						class="w-full max-w-[640px] resize-y rounded-[2px] border border-hair bg-code-bg px-[9px] py-[7px] font-mono text-[11.5px] leading-[1.6] outline-none focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-base-content focus:border-hair-strong"
						value={settings.extra_redaction_patterns.join('\n')}
						oninput={(e) => {
							if (settings)
								settings.extra_redaction_patterns = (e.currentTarget as HTMLTextAreaElement).value
									.split('\n')
									.map((s) => s.trim())
									.filter(Boolean);
						}}
					></textarea>
					<p class="meta mt-[5px] max-w-[62ch] text-[11px] leading-[1.6]">
						One regular expression per line, applied in addition to the built-in set. Matches are
						replaced with <code>[redacted]</code> before anything is written to disk.
					</p>
				</div>
			</div>
		</section>
	{/if}

	<!-- ------------------------------------------------------------------ -->
	<section
		class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5 rule-b"
	>
		<div>
			<h3 class="text-[13px] font-semibold">Install</h3>
			<p class="meta mt-1.5 text-[11.5px] leading-[1.65]">
				Each step is independent and reversible. Anything logbear edits is backed up first, and the
				backup path is shown here.
			</p>
			{#if install && pending.length > 0}
				<button
					type="button"
					class="btn btn-primary btn-sm mt-3 h-auto px-[11px] py-[4px] text-[11.5px] font-normal disabled:border-primary disabled:bg-primary disabled:text-primary-content disabled:opacity-45"
					disabled={stepBusy !== null || installingAll}
					onclick={() => void installAll()}
				>
					{#if allProgress}
						Installing… {allProgress.done}/{allProgress.total}
					{:else}
						Install all {pending.length} remaining
					{/if}
				</button>
				<p class="meta mt-1.5 text-[11px] leading-[1.6]">
					Sets up everything below in one go, including the Claude Code and Codex integrations.
				</p>
			{/if}
		</div>

		<div class="min-w-0">
			{#if allFailures.length > 0}
				<div class="mb-3 rounded border border-warning/40 px-3 py-2">
					<p class="text-[11.5px] font-medium text-warning">
						{pluralise(allFailures.length, 'step')} could not be installed
					</p>
					<ul class="mt-1 flex flex-col gap-0.5">
						{#each allFailures as failure (failure)}
							<li class="meta text-[10.5px] leading-[1.6]">{failure}</li>
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
								<p class="meta mt-1 max-w-[74ch] text-[11px] leading-[1.6]">
									{STEP_COPY[step].what}
								</p>
								<p class="mt-1 max-w-[74ch] text-[10.5px] leading-[1.6] text-base-content opacity-75">
									{s.detail}
								</p>
								{#if s.backup}
									<button
										type="button"
										class="meta max-w-full cursor-pointer overflow-hidden border-b border-dotted border-hair-strong text-left text-[10.5px] text-ellipsis whitespace-nowrap hover:text-base-content"
										onclick={() => void revealPath(s.backup!)}
										title="Reveal the backup in Finder">backup: {s.backup}</button
									>
								{/if}
							</div>
							<div class="flex justify-end">
								{#if s.installed}
									<button
										type="button"
										class="btn btn-outline btn-sm h-auto border-hair-strong px-[11px] py-[4px] text-[11.5px] font-normal hover:border-base-content hover:bg-transparent disabled:border-hair-strong disabled:bg-transparent disabled:text-base-content disabled:opacity-45"
										disabled={stepBusy !== null}
										onclick={() => void runStep(step, 'uninstall')}
									>
										{stepBusy === step ? 'removing…' : 'Uninstall'}
									</button>
								{:else}
									<button
										type="button"
										class="btn btn-primary btn-sm h-auto px-[11px] py-[4px] text-[11.5px] font-normal disabled:border-primary disabled:bg-primary disabled:text-primary-content disabled:opacity-45"
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
			{:else}
				<p class="meta text-[11px]">Reading install state…</p>
			{/if}
		</div>
	</section>

	<!-- ------------------------------------------------------------------ -->
	{#if app.daemon}
		<section
			class="grid max-w-[1180px] grid-cols-[minmax(220px,300px)_minmax(0,1fr)] gap-8 px-4 py-5"
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
								class="meta max-w-full cursor-pointer overflow-hidden border-b border-dotted border-hair-strong text-left text-[10.5px] text-ellipsis whitespace-nowrap hover:text-base-content"
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
				class="btn btn-outline btn-sm h-auto border-hair-strong px-[11px] py-[4px] text-[11.5px] font-normal hover:border-base-content hover:bg-transparent disabled:border-hair-strong disabled:bg-transparent disabled:text-base-content disabled:opacity-45"
				onclick={revert}
				disabled={saving}>Discard</button
			>
			<button
				type="button"
				class="btn btn-primary btn-sm h-auto px-[11px] py-[4px] text-[11.5px] font-normal disabled:border-primary disabled:bg-primary disabled:text-primary-content disabled:opacity-45"
				onclick={save}
				disabled={saving}
			>
				{saving ? 'saving…' : 'Save changes'}
			</button>
		{/if}
	</div>
{/if}
