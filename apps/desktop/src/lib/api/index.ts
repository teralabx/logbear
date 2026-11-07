// The one place the frontend talks to the outside world.
//
// Inside Tauri every call goes through `invoke`. Under plain `vite dev` the same
// calls are served from the in-memory mock, so every screen can be exercised
// without a daemon. The mock is development-only: `import.meta.env.DEV` is false
// in `vite build`, which drops the dynamic import and with it the whole module.
//
// Argument names are camelCase on the way out — Tauri maps them to the Rust
// snake_case params. Return values stay snake_case and are never renamed.

import { Channel, invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type {
	DaemonStatus,
	EntryFilter,
	EntryView,
	Fingerprint,
	FingerprintHistory,
	InstallStatus,
	InstallStep,
	Project,
	ProjectRef,
	ProjectStatus,
	RetentionState,
	Run,
	RunDiff,
	RunErrorCount,
	RunRef,
	LogbearEvent,
	Settings,
	SquashReport,
	StepState
} from './types';

export const isTauri = (): boolean =>
	typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/** True when the screens are showing generated data rather than a real daemon. */
export const usingMockData = (): boolean => !isTauri() && import.meta.env.DEV;

const loadMock = import.meta.env.DEV ? () => import('./mock') : null;

/** Serve a call from the mock, or fail the way a missing daemon does in a production build. */
async function viaMock<T>(call: (m: typeof import('./mock').mock) => Promise<T>): Promise<T> {
	if (!loadMock) throw 'The LogBear backend is only available inside the desktop app.';
	return call((await loadMock()).mock);
}

export const projectRef = (id: number): ProjectRef => ({ by: 'id', value: id });
export const projectPath = (path: string): ProjectRef => ({ by: 'path', value: path });

/** Normalises whatever the backend throws into a string we can render. */
export function apiError(err: unknown): string {
	if (typeof err === 'string') return err;
	if (err instanceof Error) return err.message;
	if (err && typeof err === 'object' && 'message' in err) return String(err.message);
	return String(err);
}

/* -------------------------------------------------------------------------- */

export const daemonStatus = (): Promise<DaemonStatus> =>
	isTauri() ? invoke('daemon_status') : viaMock((m) => m.daemon_status());

export const listProjects = (): Promise<Project[]> =>
	isTauri() ? invoke('list_projects') : viaMock((m) => m.list_projects());

export const projectStatus = (project: ProjectRef): Promise<ProjectStatus> =>
	isTauri() ? invoke('project_status', { project }) : viaMock((m) => m.project_status(project));

export const listRuns = (project: ProjectRef, limit = 50, offset = 0): Promise<Run[]> =>
	isTauri()
		? invoke('list_runs', { project, limit, offset })
		: viaMock((m) => m.list_runs(project, limit, offset));

export const getRun = (project: ProjectRef, runId: number): Promise<Run> =>
	isTauri() ? invoke('get_run', { project, runId }) : viaMock((m) => m.get_run(project, runId));

export const entries = (project: ProjectRef, filter: EntryFilter): Promise<EntryView[]> =>
	isTauri() ? invoke('entries', { project, filter }) : viaMock((m) => m.entries(project, filter));

export const fingerprints = (
	project: ProjectRef,
	limit = 100,
	errorsOnly = false
): Promise<Fingerprint[]> =>
	isTauri()
		? invoke('fingerprints', { project, limit, errorsOnly })
		: viaMock((m) => m.fingerprints(project, limit, errorsOnly));

/** Sparkline data for a screen of signatures, in one round trip. */
export const fingerprintHistories = (
	project: ProjectRef,
	fingerprintIds: number[],
	limit = 30
): Promise<FingerprintHistory[]> =>
	isTauri()
		? invoke('fingerprint_histories', { project, fingerprintIds, limit })
		: viaMock((m) => m.fingerprint_histories(project, fingerprintIds, limit));

/** Error totals per run. Runs with none are absent. */
export const runErrorCounts = (project: ProjectRef, runIds: number[]): Promise<RunErrorCount[]> =>
	isTauri()
		? invoke('run_error_counts', { project, runIds })
		: viaMock((m) => m.run_error_counts(project, runIds));

export const runDiff = (
	project: ProjectRef,
	a: RunRef | null,
	b: RunRef | null
): Promise<RunDiff> =>
	isTauri() ? invoke('run_diff', { project, a, b }) : viaMock((m) => m.run_diff(project, a, b));

/**
 * Streams the tail of a run's stored output into `onChunk` as it arrives, so the
 * terminal starts painting before the read finishes and the whole log is never held
 * as one string. Resolves to whether older output was left out of the replay.
 */
export async function rawStream(
	project: ProjectRef,
	runId: number,
	onChunk: (bytes: Uint8Array) => void
): Promise<boolean> {
	if (!isTauri()) {
		onChunk(new TextEncoder().encode(await viaMock((m) => m.raw_stream(project, runId))));
		return false;
	}
	const channel = new Channel<ArrayBuffer>();
	channel.onmessage = (buf) => onChunk(new Uint8Array(buf));
	return invoke('raw_stream', { project, runId, onChunk: channel });
}

export const getSettings = (): Promise<Settings> =>
	isTauri() ? invoke('get_settings') : viaMock((m) => m.get_settings());

export const updateSettings = (settings: Settings): Promise<null> =>
	isTauri() ? invoke('update_settings', { settings }) : viaMock((m) => m.update_settings(settings));

export const updateProject = (
	project: ProjectRef,
	retentionBytes: number | null,
	pushEnabled: boolean,
	notifyEnabled: boolean
): Promise<null> =>
	isTauri()
		? invoke('update_project', { project, retentionBytes, pushEnabled, notifyEnabled })
		: viaMock((m) => m.update_project(project, retentionBytes, pushEnabled, notifyEnabled));

export const retention = (project: ProjectRef): Promise<RetentionState> =>
	isTauri() ? invoke('retention', { project }) : viaMock((m) => m.retention(project));

export const reparse = (project: ProjectRef, runId: number): Promise<null> =>
	isTauri() ? invoke('reparse', { project, runId }) : viaMock((m) => m.reparse());

/** Squash one finished run, or every finished run in the project when `runId` is null. */
export const squash = (project: ProjectRef, runId: number | null): Promise<SquashReport> =>
	isTauri() ? invoke('squash', { project, runId }) : viaMock((m) => m.squash(project, runId));

export const deleteRun = (project: ProjectRef, runId: number): Promise<null> =>
	isTauri() ? invoke('delete_run', { project, runId }) : viaMock((m) => m.delete_run(project, runId));

export const installStatus = (): Promise<InstallStatus> =>
	isTauri() ? invoke('install_status') : viaMock((m) => m.install_status());

export const installStep = (step: InstallStep): Promise<StepState> =>
	isTauri() ? invoke('install_step', { step }) : viaMock((m) => m.install_step(step));

export const uninstallStep = (step: InstallStep): Promise<StepState> =>
	isTauri() ? invoke('uninstall_step', { step }) : viaMock((m) => m.uninstall_step(step));

/** True until the first-run screen has been answered. Nothing is installed before that. */
export const firstRunPending = (): Promise<boolean> =>
	isTauri() ? invoke('first_run_pending') : viaMock((m) => m.first_run_pending());

export const finishFirstRun = (): Promise<null> =>
	isTauri() ? invoke('finish_first_run') : viaMock((m) => m.finish_first_run());

export const revealPath = (path: string): Promise<null> =>
	isTauri() ? invoke('reveal_path', { path }) : viaMock((m) => m.reveal_path(path));

/* -------------------------------------------------------------------------- */
/* live events                                                                 */
/* -------------------------------------------------------------------------- */

/**
 * Subscribe to the daemon's event channel. Returns a disposer.
 * Called once, from the root layout — everything else reads the store.
 */
export function subscribeEvents(handler: (e: LogbearEvent) => void): () => void {
	let dispose: (() => void) | null = null;
	let cancelled = false;
	const subscribed: Promise<() => void> = isTauri()
		? listen<LogbearEvent>('logbear://event', (msg) => handler(msg.payload))
		: loadMock
			? loadMock().then((m) => m.mockSubscribe(handler))
			: Promise.resolve(() => {});
	void subscribed.then((un) => {
		if (cancelled) un();
		else dispose = un;
	});
	return () => {
		cancelled = true;
		dispose?.();
	};
}
