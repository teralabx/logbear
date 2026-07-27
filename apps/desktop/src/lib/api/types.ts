// Wire types. These mirror the Rust serde output exactly — snake_case stays
// snake_case. Do not "fix" the casing here; it is the contract.

export type Level = 'error' | 'warn' | 'info' | 'debug' | 'trace' | 'unknown';

/**
 * How an entry's level was determined.
 *
 * `declared` — the process emitted a severity token and it was read verbatim.
 * `structural` — no token, but a runtime printed an exception banner.
 * `unknown` — the process said nothing. Never an error, and never notified on.
 */
export type Evidence = 'declared' | 'structural' | 'unknown';
export type Boundaries = 'exact' | 'inferred';
export type DelimiterMode = 'json' | 'colour_bracket' | 'prefix' | 'fallback';
export type ProjectRef = { by: 'path'; value: string } | { by: 'id'; value: number };
export type RunRef =
	| { kind: 'id'; value: number }
	| { kind: 'latest' }
	| { kind: 'last_good' }
	| { kind: 'last_bad' };

export interface Counters {
	n_entries: number;
	n_error: number;
	n_warn: number;
	n_info: number;
	n_debug: number;
	n_trace: number;
	n_unknown: number;
}

export interface Project {
	id: number;
	name: string;
	root: string;
	db_path: string;
	created_at: number;
	last_seen_at: number;
	retention_bytes: number | null;
	push_enabled: boolean;
	notify_enabled: boolean;
}

export interface Run {
	id: number;
	project_id: number;
	command: string[];
	cwd: string;
	session_group: number | null;
	started_at: number;
	ended_at: number | null;
	exit_code: number | null;
	signal: string | null;
	git_sha: string | null;
	git_branch: string | null;
	git_dirty_hash: string | null;
	lockfile_hashes: Record<string, string>;
	env_keys: string[];
	term_cols: number;
	delimiter_mode: DelimiterMode;
	chunks_evicted: boolean;
	counters: Counters;
}

export interface Entry {
	id: number;
	run_id: number;
	start_line: number;
	end_line: number;
	captured_at: number;
	level: Level;
	/** How `level` was arrived at. `unknown` means the process declared nothing. */
	evidence: Evidence;
	is_error: boolean;
	/** `rung:rule`, e.g. `declared:level_token`. Null when the severity is unknown. */
	error_reason: string | null;
	fingerprint_id: number | null;
	boundaries: Boundaries;
}

export interface EntryView extends Entry {
	text: string;
	template: string | null;
	is_new_signature: boolean;
}

export interface Fingerprint {
	id: number;
	template: string;
	hash: string;
	level: Level;
	first_seen: number;
	last_seen: number;
	total_count: number;
	run_count: number;
}

export interface ProjectStatus {
	project: Project;
	active_runs: Run[];
	last_run: Run | null;
	db_bytes: number;
	errors_last_hour: number;
	unacked_errors: number;
}

export interface DaemonStatus {
	version: string;
	pid: number;
	started_at: number;
	socket: string;
	projects: number;
	active_runs: number;
	total_db_bytes: number;
	spool_pending: number;
}

export interface EntryFilter {
	run_id?: number | null;
	level?: Level | null;
	errors_only?: boolean;
	/** Signatures being seen for the first time in this project, at any severity. */
	novel_only?: boolean;
	since?: number | null;
	after_entry?: number | null;
	search?: string | null;
	limit?: number;
	offset?: number;
	ascending?: boolean;
}

export interface DiffSignature {
	fingerprint_id: number;
	template: string;
	level: Level;
	count_a: number;
	count_b: number;
	sample: string | null;
}

export interface GitCommit {
	sha: string;
	subject: string;
	author: string;
	at: number | null;
}

export interface GitDiff {
	sha_a: string | null;
	sha_b: string | null;
	branch_a: string | null;
	branch_b: string | null;
	dirty_changed: boolean;
	commits: GitCommit[];
	files_changed: string[];
}

export interface LockfileChange {
	file: string;
	hash_a: string | null;
	hash_b: string | null;
}

export interface RunDiff {
	a: Run | null;
	b: Run | null;
	new_signatures: DiffSignature[];
	resolved_signatures: DiffSignature[];
	changed_counts: DiffSignature[];
	git: GitDiff;
	lockfiles_changed: LockfileChange[];
	env_added: string[];
	env_removed: string[];
	command_changed: boolean;
}

export interface RetentionState {
	project_id: number;
	bytes_used: number;
	bytes_cap: number;
	runs_total: number;
	runs_evicted: number;
	oldest_run_at: number | null;
}

export interface QuietHours {
	start_min: number;
	end_min: number;
}

export interface Settings {
	retention_bytes: number;
	notifications_enabled: boolean;
	quiet_hours: QuietHours | null;
	block_budget: number;
	push_enabled: boolean;
	denylist: string[];
	redaction_enabled: boolean;
	extra_redaction_patterns: string[];
}

export interface StepState {
	installed: boolean;
	available: boolean;
	detail: string;
	backup: string | null;
}

export interface InstallStatus {
	path: StepState;
	daemon: StepState;
	mcp_claude: StepState;
	mcp_codex: StepState;
	hook_claude: StepState;
	hook_codex: StepState;
}

export type InstallStep = keyof InstallStatus;

export interface Notice {
	project_id: number;
	project_name: string;
	title: string;
	body: string;
	level: Level;
	at: number;
	replaces_key: string | null;
	count: number;
}

export interface FingerprintHistoryPoint {
	run_id: number;
	count: number;
	started_at: number;
}

export type RunlogEvent =
	| { event: 'run_started'; run: Run }
	| { event: 'run_finished'; run: Run }
	| { event: 'entries'; run_id: number; entries: EntryView[] }
	| { event: 'counters'; run_id: number; counters: Counters }
	| { event: 'notice'; notice: Notice }
	| { event: 'projects_changed' };

export const LEVELS: Level[] = ['error', 'warn', 'info', 'debug', 'trace', 'unknown'];

export const INSTALL_STEPS: InstallStep[] = [
	'path',
	'daemon',
	'mcp_claude',
	'mcp_codex',
	'hook_claude',
	'hook_codex'
];
