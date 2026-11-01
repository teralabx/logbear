// Global app state: daemon health, the project list and their statuses, and the
// single subscription to the daemon's event channel.
//
// The root layout calls `app.init()` once. Screens read from here and, when they
// need the raw stream (the Live view), attach their own listener via
// `app.onEvent`. Entry bodies are deliberately not buffered here — a dev server
// left running all day would grow this store without bound.

import {
	apiError,
	daemonStatus,
	listProjects,
	projectRef,
	projectStatus,
	subscribeEvents,
	usingMockData
} from '$lib/api';
import type {
	Counters,
	DaemonStatus,
	Notice,
	Project,
	ProjectStatus,
	Run,
	LogbearEvent
} from '$lib/api/types';

const POLL_MS = 10_000;

class AppState {
	daemon = $state<DaemonStatus | null>(null);
	/** Set when the daemon cannot be reached. Drives the red rail dot. */
	daemonError = $state<string | null>(null);
	projects = $state<Project[]>([]);
	statuses = $state<Record<number, ProjectStatus>>({});
	/** Live counters keyed by run id, patched from the event channel. */
	counters = $state<Record<number, Counters>>({});
	notices = $state<Notice[]>([]);
	loaded = $state(false);
	readonly mock = usingMockData();

	#listeners = new Set<(e: LogbearEvent) => void>();
	#dispose: (() => void) | null = null;
	#poll: ReturnType<typeof setInterval> | null = null;
	#refreshing: Promise<void> | null = null;
	#again = false;

	get connected(): boolean {
		return this.daemon !== null && this.daemonError === null;
	}

	get activeRuns(): Run[] {
		return Object.values(this.statuses).flatMap((s) => s.active_runs);
	}

	project(id: number): Project | undefined {
		return this.projects.find((p) => p.id === id);
	}

	status(id: number): ProjectStatus | undefined {
		return this.statuses[id];
	}

	/** Unacknowledged errors across every project — the rail badge. */
	get unacked(): number {
		return Object.values(this.statuses).reduce((a, s) => a + s.unacked_errors, 0);
	}

	init(): () => void {
		void this.refresh();
		this.#dispose = subscribeEvents((e) => this.#handle(e));
		this.#poll = setInterval(() => void this.refresh(), POLL_MS);
		return () => {
			this.#dispose?.();
			this.#dispose = null;
			if (this.#poll) clearInterval(this.#poll);
			this.#poll = null;
		};
	}

	/**
	 * Re-read daemon health and every project's status. Calls that arrive while one is in
	 * flight fold into a single follow-up, so a burst of run events — a test watcher
	 * restarting, a script launching several servers — costs two reads, not one per event.
	 */
	refresh(): Promise<void> {
		if (this.#refreshing) {
			this.#again = true;
			return this.#refreshing;
		}
		this.#refreshing = (async () => {
			do {
				this.#again = false;
				await this.#read();
			} while (this.#again);
			this.#refreshing = null;
		})();
		return this.#refreshing;
	}

	async #read(): Promise<void> {
		try {
			this.daemon = await daemonStatus();
			this.daemonError = null;
		} catch (err) {
			this.daemon = null;
			this.daemonError = apiError(err);
		}
		try {
			this.projects = await listProjects();
			const next: Record<number, ProjectStatus> = {};
			await Promise.all(
				this.projects.map(async (p) => {
					try {
						next[p.id] = await projectStatus(projectRef(p.id));
					} catch {
						/* a project whose store is locked simply has no status this tick */
					}
				})
			);
			this.statuses = next;
			for (const s of Object.values(next)) {
				for (const r of s.active_runs) this.counters[r.id] = r.counters;
			}
		} catch (err) {
			this.daemonError ??= apiError(err);
		}
		this.loaded = true;
	}

	/** Attach a listener to the raw event stream. Returns a disposer. */
	onEvent(fn: (e: LogbearEvent) => void): () => void {
		this.#listeners.add(fn);
		return () => this.#listeners.delete(fn);
	}

	dismissNotice(at: number) {
		this.notices = this.notices.filter((n) => n.at !== at);
	}

	clearNotices() {
		this.notices = [];
	}

	#handle(e: LogbearEvent) {
		switch (e.event) {
			case 'counters':
				this.counters[e.run_id] = e.counters;
				break;
			case 'run_started':
			case 'run_finished':
			case 'projects_changed':
				void this.refresh();
				break;
			case 'notice': {
				// `replaces_key` collapses repeats of the same signature into one
				// row with a count, instead of stacking near-identical toasts.
				const key = e.notice.replaces_key;
				if (key) {
					const i = this.notices.findIndex((n) => n.replaces_key === key);
					if (i >= 0) {
						const prev = this.notices[i];
						this.notices = [
							{ ...e.notice, count: prev.count + e.notice.count },
							...this.notices.filter((_, j) => j !== i)
						].slice(0, 6);
						break;
					}
				}
				this.notices = [e.notice, ...this.notices].slice(0, 6);
				break;
			}
			case 'entries':
				break;
		}
		for (const fn of this.#listeners) fn(e);
	}
}

export const app = new AppState();
