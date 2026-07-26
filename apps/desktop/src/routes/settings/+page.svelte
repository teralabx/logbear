<script lang="ts">
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
	let loadError = $state<string | null>(null);
	let saveError = $state<string | null>(null);
	let saving = $state(false);
	let stepBusy = $state<InstallStep | null>(null);
	let justSaved = $state(false);

	const MB = 1024 * 1024;

	const dirty = $derived(
		settings !== null && saved !== null && JSON.stringify(settings) !== JSON.stringify(saved)
	);

	async function loadAll() {
		loadError = null;
		try {
			const [s, i] = await Promise.all([getSettings(), installStatus()]);
			settings = s;
			saved = structuredClone(s);
			install = i;
			const rs = await Promise.all(
				app.projects.map(async (p) => [p.id, await retention(projectRef(p.id))] as const)
			);
			retentions = Object.fromEntries(rs);
		} catch (err) {
			loadError = apiError(err);
		}
	}

	$effect(() => {
		// Re-read once the project list is known so the per-project meters have
		// something to measure.
		void app.projects.length;
		void loadAll();
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

	const STEP_COPY: Record<InstallStep, { title: string; what: string }> = {
		path: {
			title: 'PATH shims',
			what: 'Puts ~/.runlog/bin first in your shell PATH. Commands like npm, mix and cargo then run through runlog with no prefix — including the ones your Makefiles and your coding agent invoke.'
		},
		daemon: {
			title: 'Background daemon',
			what: 'Registers a login agent so capture keeps working after this window is closed. Without it, only runs started while the app is open are recorded.'
		},
		mcp_claude: {
			title: 'MCP server — Claude Code',
			what: 'Registers runlog as an MCP server for Claude Code, giving it three read-only tools: recent runs, diff between two runs, and errors for a run.'
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
		}
	};

	const quietOn = $derived(settings?.quiet_hours != null);
</script>

<PageHeader
	title="Settings"
	sub={app.daemon ? `runlog ${app.daemon.version} · pid ${app.daemon.pid}` : 'daemon not reachable'}
/>

<div class="pane body">
	{#if loadError}
		<ErrorNote what="Could not read settings." detail={loadError} retry={() => void loadAll()} />
	{/if}
	{#if saveError}
		<ErrorNote what="That change did not stick." detail={saveError} />
	{/if}

	{#if settings}
		<!-- ---------------------------------------------------------------- -->
		<section class="sec">
			<div class="sechead">
				<h3>Storage</h3>
				<p class="lede meta">
					runlog stores raw output on disk. When a project passes its cap the oldest runs are
					evicted — their counts and fingerprints survive, their bytes do not.
				</p>
			</div>

			<div class="secbody">
				<div class="field">
					<label for="global-cap">Default cap per project</label>
					<div class="inputrow">
						<input
							id="global-cap"
							type="number"
							min="64"
							step="64"
							class="tnum"
							value={Math.round(settings.retention_bytes / MB)}
							oninput={(e) => {
								const v = Number((e.currentTarget as HTMLInputElement).value);
								if (settings && Number.isFinite(v)) settings.retention_bytes = Math.max(1, v) * MB;
							}}
						/>
						<span class="unit meta">MB</span>
						<span class="hint meta">Applies to any project without its own cap.</span>
					</div>
				</div>

				<div class="projects">
					{#each app.projects as p (p.id)}
						{@const r = retentions[p.id]}
						<div class="prow">
							<div class="pident">
								<span class="pname">{p.name}</span>
								<button
									type="button"
									class="ppath meta"
									onclick={() => void revealPath(p.root)}
									title="Reveal {p.root} in Finder">{tildePath(p.root)}</button
								>
							</div>

							<div class="pmeter">
								{#if r}
									<Meter
										used={r.bytes_used}
										cap={r.bytes_cap}
										label="{r.runs_total} runs stored"
										note={r.runs_evicted > 0
											? `${pluralise(r.runs_evicted, 'run')} already evicted. Oldest surviving run: ${r.oldest_run_at ? fmtAgo(r.oldest_run_at) : 'unknown'}.`
											: `Nothing evicted yet. Oldest run: ${r.oldest_run_at ? fmtAgo(r.oldest_run_at) : 'unknown'}.`}
									/>
								{:else}
									<p class="meta small">Reading store size…</p>
								{/if}
							</div>

							<div class="pcap">
								<label class="label-xs" for="cap-{p.id}">cap</label>
								<div class="inputrow tight">
									<input
										id="cap-{p.id}"
										type="number"
										min="0"
										step="64"
										class="tnum narrow"
										placeholder={String(Math.round(settings.retention_bytes / MB))}
										value={p.retention_bytes === null ? '' : Math.round(p.retention_bytes / MB)}
										onchange={(e) => {
											const raw = (e.currentTarget as HTMLInputElement).value.trim();
											void setProject(p.id, {
												retentionBytes: raw === '' ? null : Math.max(1, Number(raw)) * MB
											});
										}}
									/>
									<span class="unit meta">MB</span>
								</div>
								{#if p.retention_bytes === null}
									<span class="meta small">using default</span>
								{:else}
									<button
										type="button"
										class="linkish"
										onclick={() => void setProject(p.id, { retentionBytes: null })}
										>use default</button
									>
								{/if}
							</div>
						</div>
					{:else}
						<p class="meta small">No projects tracked yet.</p>
					{/each}
				</div>
			</div>
		</section>

		<!-- ---------------------------------------------------------------- -->
		<section class="sec">
			<div class="sechead">
				<h3>Notifications</h3>
				<p class="lede meta">
					A notification fires when a run produces an error signature this project has not seen
					before. Repeats of a known signature stay silent.
				</p>
			</div>

			<div class="secbody">
				<Toggle
					bind:checked={settings.notifications_enabled}
					label="Notify on new error signatures"
					hint="Off means runlog still records everything — it just never interrupts you."
				/>

				<div class="field indent" class:off={!settings.notifications_enabled}>
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
						<div class="inputrow indent2">
							<input
								type="time"
								class="tnum"
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
								class="tnum"
								aria-label="Quiet hours end"
								value={minutesToHhmm(settings.quiet_hours.end_min)}
								onchange={(e) => {
									if (settings?.quiet_hours)
										settings.quiet_hours.end_min = hhmmToMinutes(
											(e.currentTarget as HTMLInputElement).value
										);
								}}
							/>
							<span class="hint meta">
								{minutesToHhmm(settings.quiet_hours.start_min)} →
								{minutesToHhmm(settings.quiet_hours.end_min)}
								{settings.quiet_hours.end_min < settings.quiet_hours.start_min
									? '(crosses midnight)'
									: ''}
							</span>
						</div>
					{/if}
				</div>

				<div class="mutes">
					<span class="label-xs">per project</span>
					{#each app.projects as p (p.id)}
						<div class="muterow">
							<span class="pname">{p.name}</span>
							<button
								type="button"
								class="mutebtn"
								class:muted={!p.notify_enabled}
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
		<section class="sec">
			<div class="sechead">
				<h3>Agent push</h3>
				<p class="lede meta">
					When a run fails, runlog can push a short block of context — the command, the exit code,
					the new signatures — into the coding agent's next turn instead of waiting to be asked.
				</p>
			</div>

			<div class="secbody">
				<Toggle
					bind:checked={settings.push_enabled}
					label="Push run context to agents"
					hint="Requires the session hook below to be installed for at least one agent."
				/>

				<div class="field">
					<label for="budget">Block budget per session</label>
					<div class="inputrow">
						<input
							id="budget"
							type="number"
							min="0"
							max="200"
							class="tnum narrow"
							bind:value={settings.block_budget}
							disabled={!settings.push_enabled}
						/>
						<span class="hint meta">
							The most blocks runlog will push into one agent session. Reached budget means later
							failures are still recorded and still queryable — they just stop being volunteered.
						</span>
					</div>
				</div>

				<div class="mutes">
					<span class="label-xs">per project</span>
					{#each app.projects as p (p.id)}
						<div class="muterow">
							<span class="pname">{p.name}</span>
							<button
								type="button"
								class="mutebtn"
								class:muted={!p.push_enabled}
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
		<section class="sec">
			<div class="sechead">
				<h3>Capture and redaction</h3>
				<p class="lede meta">
					Redaction happens on the way in. Data that never lands cannot leak. Environment variable
					values are never recorded at all — only their names.
				</p>
			</div>

			<div class="secbody">
				<Toggle
					bind:checked={settings.redaction_enabled}
					label="Redact secrets before writing"
					hint="Matches AWS keys, bearer tokens, connection strings and private key blocks. Turning this off is not recommended and does not retroactively expose anything already stored."
				/>

				<div class="field">
					<label for="denylist">Never capture these commands</label>
					<textarea
						id="denylist"
						rows="3"
						class="mono"
						value={settings.denylist.join('\n')}
						oninput={(e) => {
							if (settings)
								settings.denylist = (e.currentTarget as HTMLTextAreaElement).value
									.split('\n')
									.map((s) => s.trim())
									.filter(Boolean);
						}}
					></textarea>
					<p class="hint meta">
						One per line. Sessions rather than runs — a shell, an editor, a database client — are
						passed through untouched and nothing is stored.
					</p>
				</div>

				<div class="field">
					<label for="patterns">Extra redaction patterns</label>
					<textarea
						id="patterns"
						rows="3"
						class="mono"
						value={settings.extra_redaction_patterns.join('\n')}
						oninput={(e) => {
							if (settings)
								settings.extra_redaction_patterns = (e.currentTarget as HTMLTextAreaElement).value
									.split('\n')
									.map((s) => s.trim())
									.filter(Boolean);
						}}
					></textarea>
					<p class="hint meta">
						One regular expression per line, applied in addition to the built-in set. Matches are
						replaced with <code>[redacted]</code> before anything is written to disk.
					</p>
				</div>
			</div>
		</section>
	{/if}

	<!-- ------------------------------------------------------------------ -->
	<section class="sec">
		<div class="sechead">
			<h3>Install</h3>
			<p class="lede meta">
				Each step is independent and reversible. Anything runlog edits is backed up first, and the
				backup path is shown here.
			</p>
		</div>

		<div class="secbody">
			{#if install}
				<div class="steps">
					{#each INSTALL_STEPS as step (step)}
						{@const s = install[step]}
						<div class="step" class:installed={s.installed} class:unavailable={!s.available}>
							<span class="stepdot" aria-hidden="true"></span>
							<div class="steptext">
								<div class="steptop">
									<span class="steptitle">{STEP_COPY[step].title}</span>
									<span class="stepstate label-xs">
										{s.installed ? 'installed' : s.available ? 'not installed' : 'unavailable'}
									</span>
								</div>
								<p class="what meta">{STEP_COPY[step].what}</p>
								<p class="detail meta">{s.detail}</p>
								{#if s.backup}
									<button
										type="button"
										class="linkish"
										onclick={() => void revealPath(s.backup!)}
										title="Reveal the backup in Finder">backup: {s.backup}</button
									>
								{/if}
							</div>
							<div class="stepact">
								{#if s.installed}
									<button
										type="button"
										class="btn"
										disabled={stepBusy !== null}
										onclick={() => void runStep(step, 'uninstall')}
									>
										{stepBusy === step ? 'removing…' : 'Uninstall'}
									</button>
								{:else}
									<button
										type="button"
										class="btn primary"
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
				<p class="meta small">Reading install state…</p>
			{/if}
		</div>
	</section>

	<!-- ------------------------------------------------------------------ -->
	{#if app.daemon}
		<section class="sec last">
			<div class="sechead">
				<h3>Daemon</h3>
			</div>
			<div class="secbody">
				<dl class="kv">
					<div><dt class="label-xs">version</dt><dd class="tnum">{app.daemon.version}</dd></div>
					<div><dt class="label-xs">pid</dt><dd class="tnum">{app.daemon.pid}</dd></div>
					<div>
						<dt class="label-xs">up since</dt>
						<dd class="tnum">{fmtAgo(app.daemon.started_at)}</dd>
					</div>
					<div>
						<dt class="label-xs">socket</dt>
						<dd>
							<button type="button" class="linkish" onclick={() => void revealPath(app.daemon!.socket)}
								>{tildePath(app.daemon.socket)}</button
							>
						</dd>
					</div>
					<div><dt class="label-xs">projects</dt><dd class="tnum">{app.daemon.projects}</dd></div>
					<div>
						<dt class="label-xs">active runs</dt><dd class="tnum">{app.daemon.active_runs}</dd>
					</div>
					<div>
						<dt class="label-xs">total on disk</dt>
						<dd class="tnum">{fmtBytes(app.daemon.total_db_bytes)}</dd>
					</div>
					<div>
						<dt class="label-xs">spool pending</dt>
						<dd class="tnum">{app.daemon.spool_pending}</dd>
					</div>
				</dl>
			</div>
		</section>
	{/if}
</div>

{#if dirty || justSaved}
	<div class="savebar" class:done={!dirty && justSaved}>
		<span class="stext">
			{#if dirty}Unsaved changes to global settings.{:else}Saved.{/if}
		</span>
		{#if dirty}
			<button type="button" class="btn" onclick={revert} disabled={saving}>Discard</button>
			<button type="button" class="btn primary" onclick={save} disabled={saving}>
				{saving ? 'saving…' : 'Save changes'}
			</button>
		{/if}
	</div>
{/if}

<style>
	.body {
		flex: 1;
		padding: 0 0 90px;
	}
	.sec {
		display: grid;
		grid-template-columns: minmax(220px, 300px) minmax(0, 1fr);
		gap: 32px;
		padding: 20px 16px;
		border-bottom: 1px solid var(--rl-hair);
		max-width: 1180px;
	}
	.sec.last {
		border-bottom: 0;
	}
	.sechead h3 {
		margin: 0;
		font-size: 13px;
		font-weight: 600;
	}
	.lede {
		margin: 6px 0 0;
		font-size: 11.5px;
		line-height: 1.65;
	}
	.secbody {
		min-width: 0;
	}
	.field {
		margin: 12px 0;
	}
	.field.off {
		opacity: 0.55;
	}
	.field.indent {
		margin-left: 0;
	}
	label {
		display: block;
		font-size: 12px;
		margin-bottom: 5px;
	}
	.inputrow {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
	}
	.inputrow.tight {
		gap: 4px;
	}
	.indent2 {
		margin: 6px 0 0 40px;
	}
	input[type='number'],
	input[type='time'] {
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: transparent;
		color: var(--color-base-content);
		font: inherit;
		font-size: 12px;
		padding: 3px 7px;
		width: 110px;
		outline: none;
	}
	input.narrow {
		width: 78px;
	}
	input:focus {
		border-color: var(--rl-hair-strong);
	}
	input:disabled {
		opacity: 0.5;
	}
	textarea {
		width: 100%;
		max-width: 640px;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: var(--rl-code-bg);
		color: var(--color-base-content);
		font-family: var(--font-mono);
		font-size: 11.5px;
		line-height: 1.6;
		padding: 7px 9px;
		outline: none;
		resize: vertical;
	}
	textarea:focus {
		border-color: var(--rl-hair-strong);
	}
	.hint {
		font-size: 11px;
		line-height: 1.6;
		max-width: 62ch;
	}
	p.hint {
		margin: 5px 0 0;
	}
	.unit {
		font-size: 11px;
	}
	.small {
		font-size: 11px;
	}

	.projects {
		margin-top: 16px;
		border-top: 1px solid var(--rl-hair);
	}
	.prow {
		display: grid;
		grid-template-columns: minmax(140px, 200px) minmax(0, 1fr) 130px;
		gap: 18px;
		align-items: center;
		padding: 12px 0;
		border-bottom: 1px solid var(--rl-hair);
	}
	.pident {
		min-width: 0;
	}
	.pname {
		display: block;
		font-size: 12.5px;
		font-weight: 500;
	}
	.ppath {
		display: block;
		background: none;
		border: 0;
		padding: 0;
		font: inherit;
		font-size: 11px;
		color: var(--rl-meta);
		cursor: pointer;
		text-align: left;
		max-width: 100%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ppath:hover {
		color: var(--color-base-content);
	}
	.pcap {
		display: flex;
		flex-direction: column;
		gap: 3px;
		align-items: flex-start;
	}
	.linkish {
		background: none;
		border: 0;
		padding: 0;
		font: inherit;
		font-size: 10.5px;
		color: var(--rl-meta);
		cursor: pointer;
		text-align: left;
		border-bottom: 1px dotted var(--rl-hair-strong);
		max-width: 100%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.linkish:hover {
		color: var(--color-base-content);
	}

	.mutes {
		margin-top: 14px;
		padding-top: 10px;
		border-top: 1px solid var(--rl-hair);
	}
	.muterow {
		display: flex;
		align-items: center;
		justify-content: space-between;
		max-width: 340px;
		padding: 4px 0;
	}
	.mutebtn {
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: transparent;
		color: var(--color-base-content);
		font: inherit;
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		padding: 2px 8px;
		cursor: pointer;
	}
	.mutebtn.muted {
		color: var(--rl-meta);
		border-style: dashed;
	}
	.mutebtn:hover {
		border-color: var(--rl-hair-strong);
	}

	.steps {
		display: flex;
		flex-direction: column;
	}
	.step {
		display: grid;
		grid-template-columns: 10px minmax(0, 1fr) 110px;
		gap: 12px;
		align-items: start;
		padding: 12px 0;
		border-bottom: 1px solid var(--rl-hair);
	}
	.step:last-child {
		border-bottom: 0;
	}
	.stepdot {
		width: 8px;
		height: 8px;
		margin-top: 5px;
		border-radius: 50%;
		border: 1px solid var(--rl-hair-strong);
	}
	.step.installed .stepdot {
		background: var(--color-success);
		border-color: var(--color-success);
	}
	.step.unavailable .stepdot {
		border-style: dashed;
	}
	.steptop {
		display: flex;
		align-items: baseline;
		gap: 10px;
	}
	.steptitle {
		font-size: 12.5px;
		font-weight: 500;
	}
	.step.installed .stepstate {
		color: var(--color-success);
	}
	.step.unavailable .stepstate {
		color: var(--color-warning);
	}
	.what,
	.detail {
		margin: 4px 0 0;
		font-size: 11px;
		line-height: 1.6;
		max-width: 74ch;
	}
	.detail {
		color: var(--color-base-content);
		opacity: 0.75;
		font-size: 10.5px;
	}
	.stepact {
		display: flex;
		justify-content: flex-end;
	}

	.btn {
		border: 1px solid var(--rl-hair-strong);
		border-radius: 2px;
		background: transparent;
		color: var(--color-base-content);
		font: inherit;
		font-size: 11.5px;
		padding: 4px 11px;
		cursor: pointer;
		white-space: nowrap;
	}
	.btn:hover:not(:disabled) {
		border-color: var(--color-base-content);
	}
	.btn:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}
	.btn.primary {
		background: var(--color-base-content);
		color: var(--color-base-100);
		border-color: var(--color-base-content);
	}

	.kv {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
		gap: 12px 20px;
		margin: 0;
	}
	dt {
		margin-bottom: 3px;
	}
	dd {
		margin: 0;
		font-size: 12px;
	}

	.savebar {
		position: fixed;
		bottom: 16px;
		left: 50%;
		transform: translateX(-50%);
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 8px 12px;
		background: var(--color-base-200);
		border: 1px solid var(--rl-hair-strong);
		border-radius: 3px;
		box-shadow: 0 8px 24px rgb(0 0 0 / 0.3);
		z-index: 30;
	}
	.savebar.done {
		border-color: color-mix(in oklch, var(--color-success) 50%, transparent);
	}
	.stext {
		font-size: 12px;
	}
</style>
