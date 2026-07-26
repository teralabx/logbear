// In-memory backend used whenever the app is not running inside Tauri.
//
// This exists so the frontend can be built and reviewed before the Rust side
// lands, but it is also the app's storybook: two real-shaped projects (a
// Phoenix API and a Next.js web app), ~40 runs each, genuine-looking Elixir and
// Node stack traces, fingerprints with plausible templates, and a ticker that
// pushes entries into the active run every ~800ms so the Live screen actually
// moves.

import { SGR, entryToAnsi } from '../ansi';
import type {
	Counters,
	DaemonStatus,
	DiffSignature,
	EntryFilter,
	EntryView,
	Fingerprint,
	FingerprintHistoryPoint,
	GitCommit,
	InstallStatus,
	InstallStep,
	Level,
	Project,
	ProjectRef,
	ProjectStatus,
	RetentionState,
	Run,
	RunDiff,
	RunRef,
	RunlogEvent,
	Settings,
	StepState
} from './types';

/* -------------------------------------------------------------------------- */
/* deterministic randomness                                                    */
/* -------------------------------------------------------------------------- */

function mulberry32(seed: number) {
	let a = seed >>> 0;
	return () => {
		a |= 0;
		a = (a + 0x6d2b79f5) | 0;
		let t = Math.imul(a ^ (a >>> 15), 1 | a);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

const rand = mulberry32(20260726);
const pick = <T>(xs: readonly T[]): T => xs[Math.floor(rand() * xs.length)];
const between = (lo: number, hi: number) => lo + rand() * (hi - lo);
const intBetween = (lo: number, hi: number) => Math.floor(between(lo, hi + 1));

function sha(): string {
	let s = '';
	for (let i = 0; i < 40; i++) s += '0123456789abcdef'[Math.floor(rand() * 16)];
	return s;
}

function hexHash(n = 16): string {
	let s = '';
	for (let i = 0; i < n; i++) s += '0123456789abcdef'[Math.floor(rand() * 16)];
	return s;
}

const NOW = Math.floor(Date.now() / 1000);
const HOUR = 3600;
const DAY = 86400;

/* -------------------------------------------------------------------------- */
/* projects                                                                    */
/* -------------------------------------------------------------------------- */

const projects: Project[] = [
	{
		id: 1,
		name: 'checkout-api',
		root: '/Users/dev/code/checkout-api',
		db_path: '/Users/dev/.runlog/projects/checkout-api/runlog.db',
		created_at: NOW - 63 * DAY,
		last_seen_at: NOW - 40,
		retention_bytes: 512 * 1024 * 1024,
		push_enabled: true,
		notify_enabled: true
	},
	{
		id: 2,
		name: 'pearl-web',
		root: '/Users/dev/code/pearl-web',
		db_path: '/Users/dev/.runlog/projects/pearl-web/runlog.db',
		created_at: NOW - 48 * DAY,
		last_seen_at: NOW - 51 * 60,
		retention_bytes: null,
		push_enabled: false,
		notify_enabled: true
	}
];

const COMMANDS: Record<number, string[][]> = {
	1: [
		['mix', 'test'],
		['mix', 'test', 'test/checkout/payments_test.exs'],
		['mix', 'phx.server'],
		['mix', 'ecto.migrate'],
		['mix', 'deps.get'],
		['mix', 'credo', '--strict'],
		['iex', '-S', 'mix', 'phx.server']
	],
	2: [
		['npm', 'run', 'dev'],
		['npm', 'test'],
		['npm', 'run', 'build'],
		['pnpm', 'vitest', 'run'],
		['npx', 'tsc', '--noEmit'],
		['npm', 'run', 'lint']
	]
};

const BRANCHES: Record<number, string[]> = {
	1: ['main', 'main', 'main', 'fix/idempotency-keys', 'feat/stripe-webhooks'],
	2: ['main', 'main', 'chore/next-15', 'feat/checkout-redesign']
};

const LOCKFILES: Record<number, string[]> = {
	1: ['mix.lock'],
	2: ['package-lock.json', 'pnpm-lock.yaml']
};

const ENV_BASE: Record<number, string[]> = {
	1: [
		'DATABASE_URL',
		'MIX_ENV',
		'PHX_HOST',
		'PORT',
		'SECRET_KEY_BASE',
		'STRIPE_SECRET',
		'REDIS_URL',
		'POOL_SIZE'
	],
	2: [
		'NODE_ENV',
		'NEXT_PUBLIC_API_URL',
		'NEXT_PUBLIC_SENTRY_DSN',
		'SESSION_SECRET',
		'ANALYZE',
		'PORT'
	]
};

/* -------------------------------------------------------------------------- */
/* fingerprints — the recurring identities                                     */
/* -------------------------------------------------------------------------- */

interface FpSeed {
	project_id: number;
	template: string;
	level: Level;
	/** Renders a concrete instance of this signature, as stored output. */
	render: () => string;
	weight: number;
}

const fpSeeds: FpSeed[] = [
	{
		project_id: 1,
		level: 'error',
		template: '** (Postgrex.Error) FATAL 53300 (too_many_connections) sorry, too many clients already',
		weight: 8,
		render: () =>
			[
				'** (Postgrex.Error) FATAL 53300 (too_many_connections) sorry, too many clients already',
				'    (ecto_sql 3.11.3) lib/ecto/adapters/sql.ex:1054: Ecto.Adapters.SQL.raise_sql_call_error/1',
				'    (ecto 3.11.2) lib/ecto/repo/supervisor.ex:163: Ecto.Repo.Supervisor.tuplet/2',
				'    (elixir 1.16.2) lib/enum.ex:1700: Enum."-map/2-lists^map/1-1-"/2',
				'    (checkout 0.4.1) lib/checkout/payments/ledger.ex:88: Checkout.Payments.Ledger.post_entries/2'
			].join('\n')
	},
	{
		project_id: 1,
		level: 'error',
		template:
			'** (Ecto.ConstraintError) constraint error when attempting to insert struct: <*>_unique_index (unique_constraint)',
		weight: 6,
		render: () =>
			[
				'** (Ecto.ConstraintError) constraint error when attempting to insert struct:',
				'',
				'    * payments_idempotency_key_index (unique_constraint)',
				'',
				'If you would like to stop this constraint violation from raising an',
				'exception and instead add it as an error to your changeset, please',
				'call `unique_constraint/3` on your changeset with the constraint',
				'`:name` as an option.',
				'',
				'    (ecto 3.11.2) lib/ecto/repo/schema.ex:783: anonymous fn/4 in Ecto.Repo.Schema.surface_changes/3',
				'    (checkout 0.4.1) lib/checkout/payments.ex:142: Checkout.Payments.capture/2',
				'    test/checkout/payments_test.exs:96: (test)'
			].join('\n')
	},
	{
		project_id: 1,
		level: 'error',
		template: '** (MatchError) no match of right hand side value: {:error, <*>}',
		weight: 5,
		render: () =>
			[
				`** (MatchError) no match of right hand side value: {:error, ${pick([
					':timeout',
					':closed',
					'%Stripe.Error{code: :rate_limit}',
					':econnrefused'
				])}}`,
				'    (checkout 0.4.1) lib/checkout/gateway/stripe.ex:61: Checkout.Gateway.Stripe.charge/2',
				'    (checkout 0.4.1) lib/checkout/payments.ex:118: Checkout.Payments.capture/2',
				'    (elixir 1.16.2) lib/task/supervised.ex:101: Task.Supervised.invoke_mfa/2'
			].join('\n')
	},
	{
		project_id: 1,
		level: 'error',
		template:
			'** (DBConnection.ConnectionError) tcp recv: closed (the connection was closed by the pool, possibly due to a timeout)',
		weight: 4,
		render: () =>
			[
				'** (DBConnection.ConnectionError) tcp recv: closed (the connection was closed by the pool, possibly due to a timeout)',
				'    (ecto_sql 3.11.3) lib/ecto/adapters/sql.ex:1054: Ecto.Adapters.SQL.raise_sql_call_error/1',
				'    (checkout 0.4.1) lib/checkout/reconcile/worker.ex:37: Checkout.Reconcile.Worker.handle_info/2',
				'    (stdlib 5.2.3) gen_server.erl:1077: :gen_server.try_handle_info/3'
			].join('\n')
	},
	{
		project_id: 1,
		level: 'error',
		template: '[error] GenServer <*> terminating',
		weight: 4,
		render: () =>
			[
				`[error] GenServer #PID<0.${intBetween(600, 999)}.0> terminating`,
				'** (KeyError) key :idempotency_key not found in: %{amount_cents: 4200, currency: "usd"}',
				'    (checkout 0.4.1) lib/checkout/payments/intent.ex:24: Checkout.Payments.Intent.new/1',
				'Last message: {:capture, %{amount_cents: 4200, currency: "usd"}}',
				'State: %Checkout.Reconcile.Worker.State{pending: 3, cursor: nil}'
			].join('\n')
	},
	{
		project_id: 1,
		level: 'warn',
		template: 'warning: variable "<*>" is unused (if the variable is not meant to be used, prefix it with an underscore)',
		weight: 7,
		render: () => {
			const v = pick(['conn', 'opts', 'attrs', 'socket', 'params']);
			return [
				`warning: variable "${v}" is unused (if the variable is not meant to be used, prefix it with an underscore)`,
				`    │`,
				` ${intBetween(20, 180)} │   def handle(${v}, state) do`,
				`    │          ~`,
				`    │`,
				`    └─ lib/checkout/${pick(['payments', 'reconcile', 'gateway'])}/handler.ex:${intBetween(20, 180)}:14`
			].join('\n');
		}
	},
	{
		project_id: 1,
		level: 'warn',
		template: 'warning: <*> is deprecated. Use <*> instead',
		weight: 3,
		render: () =>
			[
				'warning: Logger.warn/1 is deprecated. Use Logger.warning/2 instead',
				'    (checkout 0.4.1) lib/checkout/reconcile/worker.ex:52: Checkout.Reconcile.Worker.flush/1'
			].join('\n')
	},
	{
		project_id: 2,
		level: 'error',
		template: "TypeError: Cannot read properties of undefined (reading '<*>')",
		weight: 9,
		render: () => {
			const prop = pick(['id', 'lineItems', 'currency', 'total', 'address']);
			return [
				`TypeError: Cannot read properties of undefined (reading '${prop}')`,
				`    at normalizeCart (/Users/dev/code/pearl-web/src/lib/cart/normalize.ts:${intBetween(
					12,
					90
				)}:23)`,
				'    at CheckoutSummary (/Users/dev/code/pearl-web/src/components/CheckoutSummary.tsx:41:19)',
				'    at renderWithHooks (/Users/dev/code/pearl-web/node_modules/react-dom/cjs/react-dom-server.development.js:5658:16)',
				'    at processChild (/Users/dev/code/pearl-web/node_modules/react-dom/cjs/react-dom-server.development.js:8420:14)',
				'    at resolve (/Users/dev/code/pearl-web/node_modules/react-dom/cjs/react-dom-server.development.js:8290:5)'
			].join('\n');
		}
	},
	{
		project_id: 2,
		level: 'error',
		template: 'Error: connect ECONNREFUSED 127.0.0.1:<*>',
		weight: 7,
		render: () => {
			const port = pick(['4000', '5432', '6379']);
			return [
				`Error: connect ECONNREFUSED 127.0.0.1:${port}`,
				'    at TCPConnectWrap.afterConnect [as oncomplete] (node:net:1607:16) {',
				"  errno: -61,",
				"  code: 'ECONNREFUSED',",
				"  syscall: 'connect',",
				"  address: '127.0.0.1',",
				`  port: ${port}`,
				'}'
			].join('\n');
		}
	},
	{
		project_id: 2,
		level: 'error',
		template:
			"TS2345: Argument of type '<*>' is not assignable to parameter of type '<*>'",
		weight: 6,
		render: () =>
			[
				`src/lib/cart/normalize.ts:${intBetween(
					12,
					90
				)}:18 - error TS2345: Argument of type 'CartLine | undefined' is not assignable to parameter of type 'CartLine'.`,
				"  Type 'undefined' is not assignable to type 'CartLine'.",
				'',
				'  47   return toMoney(line.unitPrice, line.quantity)',
				'                     ~~~~'
			].join('\n')
	},
	{
		project_id: 2,
		level: 'error',
		template: "Error: Cannot find module '<*>'",
		weight: 4,
		render: () =>
			[
				`Error: Cannot find module '${pick([
					'@pearl/design-tokens',
					'sharp',
					'./generated/schema',
					'@next/font/google'
				])}'`,
				'Require stack:',
				'- /Users/dev/code/pearl-web/next.config.mjs',
				'    at Module._resolveFilename (node:internal/modules/cjs/loader:1145:15)',
				'    at Module._load (node:internal/modules/cjs/loader:986:27)',
				'    at Module.require (node:internal/modules/cjs/loader:1233:19)'
			].join('\n')
	},
	{
		project_id: 2,
		level: 'warn',
		template: 'npm warn deprecated <*>: This package is no longer supported.',
		weight: 6,
		render: () =>
			`npm warn deprecated ${pick([
				'inflight@1.0.6',
				'rimraf@3.0.2',
				'glob@7.2.3',
				'@humanwhocodes/config-array@0.13.0'
			])}: This package is no longer supported.`
	},
	{
		project_id: 2,
		level: 'warn',
		template: 'Warning: <*> exceeds the recommended size limit (<*> kB)',
		weight: 3,
		render: () =>
			[
				`Warning: /_next/static/chunks/${hexHash(8)}.js exceeds the recommended size limit (244 kB)`,
				'  Consider splitting this route or deferring the import.'
			].join('\n')
	}
];

const fingerprints: Fingerprint[] = fpSeeds.map((s, i) => ({
	id: i + 1,
	template: s.template,
	hash: hexHash(16),
	level: s.level,
	first_seen: NOW - intBetween(14, 55) * DAY,
	last_seen: NOW - intBetween(0, 3) * HOUR,
	total_count: 0,
	run_count: 0
}));

function seedFor(fpId: number): FpSeed {
	return fpSeeds[fpId - 1];
}

/* -------------------------------------------------------------------------- */
/* benign chatter — what surrounds the errors                                  */
/* -------------------------------------------------------------------------- */

const CHATTER: Record<number, { level: Level; text: () => string }[]> = {
	1: [
		{ level: 'info', text: () => `[info] Running CheckoutWeb.Endpoint with Bandit 1.5.5 at 127.0.0.1:4000 (http)` },
		{ level: 'info', text: () => `[info] Access CheckoutWeb.Endpoint at http://localhost:4000` },
		{
			level: 'info',
			text: () =>
				`[info] GET /api/v1/payments/${hexHash(8)}\n[debug] Processing with CheckoutWeb.PaymentController.show/2\n[info] Sent 200 in ${intBetween(
					2,
					180
				)}ms`
		},
		{
			level: 'debug',
			text: () =>
				`[debug] QUERY OK source="payments" db=${between(0.4, 9.2).toFixed(1)}ms queue=${between(
					0.1,
					2.4
				).toFixed(1)}ms idle=${intBetween(10, 900)}ms\nSELECT p0."id", p0."amount_cents", p0."currency", p0."state" FROM "payments" AS p0 WHERE (p0."id" = $1) [<<${intBetween(
					1,
					9999
				)}>>]`
		},
		{ level: 'info', text: () => `Compiling ${intBetween(1, 34)} files (.ex)` },
		{ level: 'info', text: () => `Generated checkout app` },
		{
			level: 'info',
			text: () =>
				`Finished in ${between(0.4, 22).toFixed(1)} seconds (${between(0.1, 3).toFixed(
					1
				)}s async, ${between(0.3, 19).toFixed(1)}s sync)\n${intBetween(
					40,
					320
				)} tests, 0 failures`
		},
		{ level: 'debug', text: () => `[debug] Sent Oban job ${intBetween(1000, 9999)} to queue :reconcile` }
	],
	2: [
		{ level: 'info', text: () => `  ▲ Next.js 15.1.4\n  - Local:        http://localhost:3000\n  - Network:      http://192.168.1.24:3000` },
		{ level: 'info', text: () => ` ✓ Ready in ${between(0.8, 4.2).toFixed(1)}s` },
		{
			level: 'info',
			text: () => ` ✓ Compiled /${pick(['checkout', 'cart', 'account', 'api/session'])} in ${intBetween(
				120,
				2400
			)}ms (${intBetween(180, 2200)} modules)`
		},
		{
			level: 'debug',
			text: () =>
				`GET /${pick(['checkout', 'cart', '_next/data/build/index.json'])} 200 in ${intBetween(
					4,
					320
				)}ms`
		},
		{
			level: 'info',
			text: () =>
				` Test Files  ${intBetween(8, 24)} passed (${intBetween(8, 24)})\n      Tests  ${intBetween(
					60,
					240
				)} passed (${intBetween(60, 240)})\n   Duration  ${between(1.2, 18).toFixed(2)}s`
		},
		{ level: 'debug', text: () => `[webpack.cache.PackFileCacheStrategy] Restored pack in ${intBetween(20, 400)}ms` },
		{ level: 'info', text: () => `Route (app)                              Size     First Load JS\n┌ ○ /                                    1.94 kB         112 kB\n├ ○ /cart                                4.21 kB         126 kB\n└ ƒ /checkout                            8.77 kB         141 kB` }
	]
};

/* -------------------------------------------------------------------------- */
/* runs + entries                                                              */
/* -------------------------------------------------------------------------- */

interface RunData {
	run: Run;
	entries: EntryView[];
}

const runData: RunData[] = [];
let nextRunId = 1;
let nextEntryId = 1;

function emptyCounters(): Counters {
	return {
		n_entries: 0,
		n_error: 0,
		n_warn: 0,
		n_info: 0,
		n_debug: 0,
		n_trace: 0,
		n_unknown: 0
	};
}

function bump(c: Counters, level: Level) {
	c.n_entries++;
	if (level === 'error') c.n_error++;
	else if (level === 'warn') c.n_warn++;
	else if (level === 'info') c.n_info++;
	else if (level === 'debug') c.n_debug++;
	else if (level === 'trace') c.n_trace++;
	else c.n_unknown++;
}

/** Fingerprint ids already emitted, per project — drives is_new_signature. */
const seenFingerprints: Record<number, Set<number>> = { 1: new Set(), 2: new Set() };

function makeEntry(
	run: Run,
	level: Level,
	text: string,
	fpId: number | null,
	capturedAt: number,
	line: number
): EntryView {
	const lines = text.split('\n').length;
	const isNew = fpId !== null && !seenFingerprints[run.project_id].has(fpId);
	if (fpId !== null) {
		seenFingerprints[run.project_id].add(fpId);
		fingerprints[fpId - 1].total_count++;
	}
	return {
		id: nextEntryId++,
		run_id: run.id,
		start_line: line,
		end_line: line + lines - 1,
		captured_at: capturedAt,
		level,
		is_error: level === 'error',
		error_reason:
			level === 'error'
				? fpId !== null
					? 'matched known signature'
					: 'stderr + non-zero exit'
				: null,
		fingerprint_id: fpId,
		boundaries: lines > 1 ? 'inferred' : 'exact',
		text,
		template: fpId !== null ? fingerprints[fpId - 1].template : null,
		is_new_signature: isNew
	};
}

function buildRun(project: Project, startedAt: number, opts: { active?: boolean } = {}): RunData {
	const pid = project.id;
	const command = pick(COMMANDS[pid]);
	const branch = pick(BRANCHES[pid]);
	const dirty = rand() < 0.35;
	const duration = opts.active ? 0 : Math.round(between(1.2, 210));
	// Roughly one run in four fails, clustered a little by command.
	const isTest = command.includes('test') || command.includes('vitest') || command.includes('tsc');
	const failed = !opts.active && rand() < (isTest ? 0.34 : 0.16);

	const lockfiles: Record<string, string> = {};
	for (const f of LOCKFILES[pid]) lockfiles[f] = hexHash(64);

	const envKeys = [...ENV_BASE[pid]];
	if (rand() < 0.2) envKeys.push(pick(['DEBUG', 'LOG_LEVEL', 'OTEL_EXPORTER_OTLP_ENDPOINT']));

	const run: Run = {
		id: nextRunId++,
		project_id: pid,
		command,
		cwd: project.root,
		session_group: rand() < 0.3 ? intBetween(1, 8) : null,
		started_at: startedAt,
		ended_at: opts.active ? null : startedAt + duration,
		exit_code: opts.active ? null : failed ? pick([1, 1, 1, 2, 127]) : 0,
		signal: null,
		git_sha: sha(),
		git_branch: branch,
		git_dirty_hash: dirty ? hexHash(16) : null,
		lockfile_hashes: lockfiles,
		env_keys: envKeys.sort(),
		term_cols: pick([80, 120, 132, 164]),
		delimiter_mode: pid === 1 ? pick(['colour_bracket', 'prefix']) : pick(['json', 'prefix', 'fallback']),
		chunks_evicted: false,
		counters: emptyCounters()
	};

	const entries: EntryView[] = [];
	const chatter = CHATTER[pid];
	const seeds = fpSeeds
		.map((s, i) => ({ s, id: i + 1 }))
		.filter((x) => x.s.project_id === pid);

	const nChatter = intBetween(6, 22);
	const nErrors = failed ? intBetween(1, 6) : rand() < 0.4 ? intBetween(1, 3) : 0;
	const total = nChatter + nErrors;
	let line = 1;

	const slots: ('chatter' | 'signal')[] = [];
	for (let i = 0; i < nChatter; i++) slots.push('chatter');
	for (let i = 0; i < nErrors; i++) slots.push('signal');
	// Signals cluster toward the end of a run, the way real failures do.
	slots.sort(() => rand() - 0.62);

	for (let i = 0; i < total; i++) {
		const at = startedAt + Math.round((duration || 60) * (i / Math.max(total, 1)));
		if (slots[i] === 'chatter') {
			const c = pick(chatter);
			const text = c.text();
			const e = makeEntry(run, c.level, text, null, at, line);
			line = e.end_line + 2;
			entries.push(e);
			bump(run.counters, c.level);
		} else {
			// Warnings are more common than errors even in failing runs.
			const wantWarn = !failed || rand() < 0.45;
			const candidates = seeds.filter((x) =>
				wantWarn ? x.s.level === 'warn' : x.s.level === 'error'
			);
			const chosen = candidates.length ? weightedPick(candidates) : weightedPick(seeds);
			const e = makeEntry(run, chosen.s.level, chosen.s.render(), chosen.id, at, line);
			line = e.end_line + 2;
			entries.push(e);
			bump(run.counters, chosen.s.level);
		}
	}

	if (!opts.active) {
		const tail =
			run.exit_code === 0
				? pid === 1
					? 'Finished. exit status 0'
					: 'Done in ' + between(0.4, 30).toFixed(2) + 's.'
				: pid === 1
					? `** (exit) exited in: GenServer.call(Checkout.Repo, ...)\nmix test failed. exit status ${run.exit_code}`
					: `npm ERR! code ELIFECYCLE\nnpm ERR! errno ${run.exit_code}\nnpm ERR! ${project.name}@0.1.0 ${command.slice(1).join(' ')}: exited with ${run.exit_code}`;
		const e = makeEntry(
			run,
			run.exit_code === 0 ? 'info' : 'error',
			tail,
			null,
			run.ended_at ?? startedAt,
			line
		);
		entries.push(e);
		bump(run.counters, e.level);
	}

	return { run, entries };
}

function weightedPick<T extends { s: FpSeed }>(xs: T[]): T {
	const total = xs.reduce((a, x) => a + x.s.weight, 0);
	let r = rand() * total;
	for (const x of xs) {
		r -= x.s.weight;
		if (r <= 0) return x;
	}
	return xs[xs.length - 1];
}

// Build oldest → newest so `is_new_signature` and first_seen read correctly.
for (const project of projects) {
	const n = 40;
	let t = NOW - 26 * DAY;
	for (let i = 0; i < n; i++) {
		t += Math.round(between(0.15, 1.1) * DAY * (project.id === 1 ? 0.62 : 0.66));
		if (t > NOW - 20 * 60) t = NOW - intBetween(21, 400) * 60;
		runData.push(buildRun(project, t));
	}
}

runData.sort((a, b) => a.run.started_at - b.run.started_at);

// One live run on the Phoenix project: the dev server, currently streaming.
const liveStart = NOW - 18 * 60;
const live = buildRun(projects[0], liveStart, { active: true });
live.run.command = ['mix', 'phx.server'];
live.run.git_branch = 'fix/idempotency-keys';
live.run.git_dirty_hash = hexHash(16);
runData.push(live);

// run_count per fingerprint
for (const fp of fingerprints) {
	fp.run_count = runData.filter((r) => r.entries.some((e) => e.fingerprint_id === fp.id)).length;
	const last = runData
		.flatMap((r) => r.entries)
		.filter((e) => e.fingerprint_id === fp.id)
		.map((e) => e.captured_at)
		.sort((a, b) => b - a)[0];
	if (last) fp.last_seen = last;
	const first = runData
		.flatMap((r) => r.entries)
		.filter((e) => e.fingerprint_id === fp.id)
		.map((e) => e.captured_at)
		.sort((a, b) => a - b)[0];
	if (first) fp.first_seen = first;
}

/* -------------------------------------------------------------------------- */
/* lookup helpers                                                              */
/* -------------------------------------------------------------------------- */

function resolveProject(ref: ProjectRef): Project {
	const p =
		ref.by === 'id'
			? projects.find((x) => x.id === ref.value)
			: projects.find((x) => x.root === ref.value);
	if (!p) throw `No project matches ${ref.by} "${ref.value}". Run a command in that directory first.`;
	return p;
}

function runsOf(pid: number): RunData[] {
	return runData.filter((r) => r.run.project_id === pid).sort((a, b) => b.run.started_at - a.run.started_at);
}

function findRun(pid: number, runId: number): RunData {
	const r = runData.find((x) => x.run.project_id === pid && x.run.id === runId);
	if (!r) throw `Run ${runId} is not in this project's store. It may have been evicted by the retention cap.`;
	return r;
}

function resolveRunRef(pid: number, ref: RunRef | null | undefined): RunData | null {
	const rs = runsOf(pid);
	if (!ref) return null;
	switch (ref.kind) {
		case 'id':
			return findRun(pid, ref.value);
		case 'latest':
			return rs.find((r) => r.run.ended_at !== null) ?? null;
		case 'last_good':
			return rs.find((r) => r.run.exit_code === 0) ?? null;
		case 'last_bad':
			return rs.find((r) => r.run.exit_code !== null && r.run.exit_code !== 0) ?? null;
	}
}

/* -------------------------------------------------------------------------- */
/* mutable app state                                                           */
/* -------------------------------------------------------------------------- */

let settings: Settings = {
	retention_bytes: 2 * 1024 * 1024 * 1024,
	notifications_enabled: true,
	quiet_hours: { start_min: 22 * 60, end_min: 8 * 60 },
	block_budget: 24,
	push_enabled: true,
	denylist: ['psql', 'ssh', 'vim', 'nvim', 'less', 'top', 'htop'],
	redaction_enabled: true,
	extra_redaction_patterns: ['sk_live_[A-Za-z0-9]{24,}', 'ghp_[A-Za-z0-9]{36}']
};

const installStatus: InstallStatus = {
	path: {
		installed: true,
		available: true,
		detail: '~/.runlog/bin is first in PATH (added to ~/.zshrc)',
		backup: '~/.runlog/backups/zshrc.2026-07-11T09-14-02'
	},
	daemon: {
		installed: true,
		available: true,
		detail: 'launchd agent sh.runlog.daemon loaded, running as pid 41822',
		backup: null
	},
	mcp_claude: {
		installed: true,
		available: true,
		detail: 'Registered in ~/.claude.json as MCP server "runlog"',
		backup: '~/.runlog/backups/claude.json.2026-07-11T09-14-05'
	},
	mcp_codex: {
		installed: false,
		available: true,
		detail: 'Not registered. Adds "runlog" to ~/.codex/config.toml',
		backup: null
	},
	hook_claude: {
		installed: false,
		available: true,
		detail: 'Not installed. Adds a SessionStart hook that injects recent run history',
		backup: null
	},
	hook_codex: {
		installed: false,
		available: false,
		detail: 'Codex CLI not found on PATH — install it first, then re-check',
		backup: null
	}
};

/* -------------------------------------------------------------------------- */
/* commands                                                                    */
/* -------------------------------------------------------------------------- */

const delay = <T>(v: T, ms = 40): Promise<T> =>
	new Promise((res) => setTimeout(() => res(v), ms));

function dbBytes(pid: number): number {
	const rs = runsOf(pid);
	return rs.reduce((a, r) => a + r.entries.reduce((b, e) => b + e.text.length * 3 + 420, 0), 0);
}

export const mock = {
	daemon_status: () =>
		delay<DaemonStatus>({
			version: '0.4.1',
			pid: 41822,
			started_at: NOW - 3 * DAY - 4 * HOUR,
			socket: '/Users/dev/.runlog/daemon.sock',
			projects: projects.length,
			active_runs: runData.filter((r) => r.run.ended_at === null).length,
			total_db_bytes: projects.reduce((a, p) => a + dbBytes(p.id), 0),
			spool_pending: 0
		}),

	list_projects: () => delay(projects.slice()),

	project_status: (ref: ProjectRef) => {
		const p = resolveProject(ref);
		const rs = runsOf(p.id);
		const active = rs.filter((r) => r.run.ended_at === null).map((r) => r.run);
		const last = rs.find((r) => r.run.ended_at !== null)?.run ?? null;
		const errorsLastHour = rs
			.flatMap((r) => r.entries)
			.filter((e) => e.level === 'error' && e.captured_at > NOW - HOUR).length;
		return delay<ProjectStatus>({
			project: p,
			active_runs: active,
			last_run: last,
			db_bytes: dbBytes(p.id),
			errors_last_hour: errorsLastHour,
			unacked_errors: Math.min(errorsLastHour, 3)
		});
	},

	list_runs: (ref: ProjectRef, limit = 50, offset = 0) => {
		const p = resolveProject(ref);
		return delay(runsOf(p.id).slice(offset, offset + limit).map((r) => r.run));
	},

	get_run: (ref: ProjectRef, runId: number) => {
		const p = resolveProject(ref);
		return delay(findRun(p.id, runId).run);
	},

	entries: (ref: ProjectRef, filter: EntryFilter) => {
		const p = resolveProject(ref);
		let all: EntryView[] = filter.run_id
			? findRun(p.id, filter.run_id).entries.slice()
			: runsOf(p.id).flatMap((r) => r.entries);

		if (filter.level) all = all.filter((e) => e.level === filter.level);
		if (filter.errors_only) all = all.filter((e) => e.is_error);
		if (filter.since != null) all = all.filter((e) => e.captured_at >= filter.since!);
		if (filter.after_entry != null) all = all.filter((e) => e.id > filter.after_entry!);
		if (filter.search) {
			const q = filter.search.toLowerCase();
			all = all.filter((e) => e.text.toLowerCase().includes(q));
		}
		all.sort((a, b) => (filter.ascending === false ? b.id - a.id : a.id - b.id));
		const off = filter.offset ?? 0;
		return delay(all.slice(off, off + (filter.limit ?? 500)));
	},

	fingerprints: (ref: ProjectRef, limit = 100, errorsOnly = false) => {
		const p = resolveProject(ref);
		const ids = new Set(
			runsOf(p.id)
				.flatMap((r) => r.entries)
				.map((e) => e.fingerprint_id)
				.filter((x): x is number => x !== null)
		);
		let fps = fingerprints.filter((f) => ids.has(f.id));
		if (errorsOnly) fps = fps.filter((f) => f.level === 'error');
		fps = fps
			.slice()
			.sort((a, b) => b.total_count - a.total_count || b.last_seen - a.last_seen);
		return delay(fps.slice(0, limit));
	},

	fingerprint_history: (ref: ProjectRef, fingerprintId: number, limit = 30) => {
		const p = resolveProject(ref);
		const out: FingerprintHistoryPoint[] = runsOf(p.id)
			.slice(0, limit)
			.map((r) => ({
				run_id: r.run.id,
				started_at: r.run.started_at,
				count: r.entries.filter((e) => e.fingerprint_id === fingerprintId).length
			}))
			.reverse();
		return delay(out);
	},

	run_diff: (ref: ProjectRef, a: RunRef | null, b: RunRef | null) => {
		const p = resolveProject(ref);
		const ra = resolveRunRef(p.id, a ?? { kind: 'last_good' });
		const rb = resolveRunRef(p.id, b ?? { kind: 'latest' });
		return delay(buildDiff(p, ra, rb));
	},

	raw_stream: (ref: ProjectRef, runId: number) => {
		const p = resolveProject(ref);
		const r = findRun(p.id, runId);
		return delay(renderRaw(r), 60);
	},

	get_settings: () => delay({ ...settings }),
	update_settings: (s: Settings) => {
		settings = { ...s };
		return delay(null);
	},

	update_project: (
		ref: ProjectRef,
		retentionBytes: number | null,
		pushEnabled: boolean,
		notifyEnabled: boolean
	) => {
		const p = resolveProject(ref);
		p.retention_bytes = retentionBytes;
		p.push_enabled = pushEnabled;
		p.notify_enabled = notifyEnabled;
		return delay(null);
	},

	retention: (ref: ProjectRef) => {
		const p = resolveProject(ref);
		const rs = runsOf(p.id);
		return delay<RetentionState>({
			project_id: p.id,
			bytes_used: dbBytes(p.id),
			bytes_cap: p.retention_bytes ?? settings.retention_bytes,
			runs_total: rs.length,
			runs_evicted: p.id === 1 ? 14 : 0,
			oldest_run_at: rs.length ? rs[rs.length - 1].run.started_at : null
		});
	},

	reparse: () => delay(null, 400),

	delete_run: (ref: ProjectRef, runId: number) => {
		const p = resolveProject(ref);
		const i = runData.findIndex((x) => x.run.project_id === p.id && x.run.id === runId);
		if (i >= 0) runData.splice(i, 1);
		return delay(null);
	},

	install_status: () => delay({ ...installStatus }),

	install_step: (step: InstallStep) => {
		const s = installStatus[step];
		if (!s.available) throw `Cannot install "${step}": ${s.detail}`;
		s.installed = true;
		s.detail = s.detail.replace(/^Not (registered|installed)\.\s*/, '').replace(/^Adds/, 'Added');
		return delay<StepState>({ ...s });
	},

	uninstall_step: (step: InstallStep) => {
		const s = installStatus[step];
		s.installed = false;
		return delay<StepState>({ ...s });
	},

	reveal_path: (path: string) => {
		// eslint-disable-next-line no-console
		console.info('[mock] reveal_path', path);
		return delay(null);
	}
};

/* -------------------------------------------------------------------------- */
/* diff                                                                        */
/* -------------------------------------------------------------------------- */

const COMMIT_SUBJECTS: Record<number, string[]> = {
	1: [
		'Bump ecto_sql to 3.11.3',
		'Add idempotency key to payment intents',
		'Reduce Repo pool_size from 30 to 10',
		'Move reconcile worker off the request path',
		'Handle Stripe rate_limit as a retryable error',
		'Drop unused index on payments.created_at'
	],
	2: [
		'Upgrade Next.js to 15.1.4',
		'Split cart normalisation out of CheckoutSummary',
		'Make CartLine.unitPrice non-optional',
		'Pin sharp to 0.33.5',
		'Enable typedRoutes',
		'Remove @next/font in favour of next/font'
	]
};

const FILES: Record<number, string[]> = {
	1: [
		'lib/checkout/payments.ex',
		'lib/checkout/payments/ledger.ex',
		'lib/checkout/gateway/stripe.ex',
		'config/dev.exs',
		'config/runtime.exs',
		'mix.exs',
		'mix.lock',
		'test/checkout/payments_test.exs'
	],
	2: [
		'src/lib/cart/normalize.ts',
		'src/components/CheckoutSummary.tsx',
		'next.config.mjs',
		'package.json',
		'package-lock.json',
		'tsconfig.json',
		'src/app/checkout/page.tsx'
	]
};

function sigOf(fpId: number, countA: number, countB: number, sample: string | null): DiffSignature {
	const fp = fingerprints[fpId - 1];
	return {
		fingerprint_id: fpId,
		template: fp.template,
		level: fp.level,
		count_a: countA,
		count_b: countB,
		sample
	};
}

function buildDiff(p: Project, ra: RunData | null, rb: RunData | null): RunDiff {
	const countsIn = (r: RunData | null) => {
		const m = new Map<number, { n: number; sample: string }>();
		if (!r) return m;
		for (const e of r.entries) {
			if (e.fingerprint_id === null) continue;
			const cur = m.get(e.fingerprint_id);
			if (cur) cur.n++;
			else m.set(e.fingerprint_id, { n: 1, sample: e.text });
		}
		return m;
	};

	const ca = countsIn(ra);
	const cb = countsIn(rb);
	const newSigs: DiffSignature[] = [];
	const resolved: DiffSignature[] = [];
	const changed: DiffSignature[] = [];

	for (const [id, v] of cb) {
		const a = ca.get(id);
		if (!a) newSigs.push(sigOf(id, 0, v.n, v.sample));
		else if (a.n !== v.n) changed.push(sigOf(id, a.n, v.n, v.sample));
	}
	for (const [id, v] of ca) {
		if (!cb.has(id)) resolved.push(sigOf(id, v.n, 0, v.sample));
	}

	const order = (x: DiffSignature) => (x.level === 'error' ? 0 : x.level === 'warn' ? 1 : 2);
	newSigs.sort((x, y) => order(x) - order(y) || y.count_b - x.count_b);
	resolved.sort((x, y) => order(x) - order(y) || y.count_a - x.count_a);
	changed.sort((x, y) => Math.abs(y.count_b - y.count_a) - Math.abs(x.count_b - x.count_a));

	const nCommits = ra && rb ? intBetween(1, 6) : 0;
	const commits: GitCommit[] = [];
	const subjects = COMMIT_SUBJECTS[p.id].slice();
	for (let i = 0; i < nCommits && subjects.length; i++) {
		const s = subjects.splice(Math.floor(rand() * subjects.length), 1)[0];
		commits.push({
			sha: sha(),
			subject: s,
			author: pick(['Ana Ruiz', 'Sam Okonkwo', 'dev']),
			at: (rb?.run.started_at ?? NOW) - intBetween(1, 60) * HOUR
		});
	}

	const files = FILES[p.id].filter(() => rand() < 0.45);
	const lockfilesChanged =
		rand() < 0.55
			? [
					{
						file: LOCKFILES[p.id][0],
						hash_a: ra ? Object.values(ra.run.lockfile_hashes)[0] : null,
						hash_b: rb ? Object.values(rb.run.lockfile_hashes)[0] : null
					}
				]
			: [];

	const envA = new Set(ra?.run.env_keys ?? []);
	const envB = new Set(rb?.run.env_keys ?? []);

	return {
		a: ra?.run ?? null,
		b: rb?.run ?? null,
		new_signatures: newSigs,
		resolved_signatures: resolved,
		changed_counts: changed,
		git: {
			sha_a: ra?.run.git_sha ?? null,
			sha_b: rb?.run.git_sha ?? null,
			branch_a: ra?.run.git_branch ?? null,
			branch_b: rb?.run.git_branch ?? null,
			dirty_changed: !!ra && !!rb && !!ra.run.git_dirty_hash !== !!rb.run.git_dirty_hash,
			commits,
			files_changed: files.length ? files : FILES[p.id].slice(0, 2)
		},
		lockfiles_changed: lockfilesChanged,
		env_added: [...envB].filter((k) => !envA.has(k)),
		env_removed: [...envA].filter((k) => !envB.has(k)),
		command_changed: !!ra && !!rb && ra.run.command.join(' ') !== rb.run.command.join(' ')
	};
}

/* -------------------------------------------------------------------------- */
/* raw ANSI stream for xterm                                                   */
/* -------------------------------------------------------------------------- */

function renderRaw(r: RunData): string {
	const head =
		SGR.dim +
		`$ ${r.run.command.join(' ')}` +
		SGR.reset +
		'\r\n' +
		SGR.grey +
		`  ${r.run.cwd}  ${r.run.git_branch ?? '-'}@${(r.run.git_sha ?? '').slice(0, 7)}` +
		SGR.reset +
		'\r\n\r\n';
	return head + r.entries.map(entryToAnsi).join('');
}

/* -------------------------------------------------------------------------- */
/* event ticker                                                                */
/* -------------------------------------------------------------------------- */

type Handler = (e: RunlogEvent) => void;
const handlers = new Set<Handler>();
let timer: ReturnType<typeof setInterval> | null = null;
let tick = 0;

function emit(e: RunlogEvent) {
	for (const h of handlers) h(e);
}

function step() {
	tick++;
	const target = runData.find((r) => r.run.ended_at === null);
	if (!target) return;
	const project = projects.find((p) => p.id === target.run.project_id)!;
	const at = Math.floor(Date.now() / 1000);

	// One in eight ticks produces a signal; the rest is ordinary chatter.
	let e: EntryView;
	if (tick % 8 === 0) {
		const seeds = fpSeeds.map((s, i) => ({ s, id: i + 1 })).filter((x) => x.s.project_id === project.id);
		const chosen = weightedPick(seeds);
		e = makeEntry(target.run, chosen.s.level, chosen.s.render(), chosen.id, at, 1);
	} else {
		const c = pick(CHATTER[project.id]);
		e = makeEntry(target.run, c.level, c.text(), null, at, 1);
	}
	target.entries.push(e);
	bump(target.run.counters, e.level);

	emit({ event: 'entries', run_id: target.run.id, entries: [e] });
	emit({ event: 'counters', run_id: target.run.id, counters: { ...target.run.counters } });

	if (e.level === 'error' && e.is_new_signature) {
		emit({
			event: 'notice',
			notice: {
				project_id: project.id,
				project_name: project.name,
				title: `New error signature in ${project.name}`,
				body: e.template ?? e.text.split('\n')[0],
				level: 'error',
				at,
				replaces_key: `fp:${e.fingerprint_id}`,
				count: 1
			}
		});
	}
}

export function mockSubscribe(handler: Handler): () => void {
	handlers.add(handler);
	if (!timer) timer = setInterval(step, 800);
	return () => {
		handlers.delete(handler);
		if (handlers.size === 0 && timer) {
			clearInterval(timer);
			timer = null;
		}
	};
}
