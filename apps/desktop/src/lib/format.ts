// Formatting helpers. Every number that lands on screen goes through one of
// these, so units and rounding stay consistent across screens.

/**
 * The store records unix time. Accept seconds or milliseconds and normalise —
 * anything past ~2001 in milliseconds is above 1e12.
 */
/**
 * Every timestamp the backend produces is **unix microseconds**. There is nothing to
 * guess about, and guessing is what broke this: the previous version inferred the unit
 * from magnitude, which read a microsecond value as milliseconds and rendered every run
 * as a date in the year 58,000 — invisible because the format shows no year.
 */
export const MICROS_PER_MS = 1000;
export const MICROS_PER_SEC = 1_000_000;

export function toMs(microseconds: number): number {
	return microseconds / MICROS_PER_MS;
}

const clock = new Intl.DateTimeFormat(undefined, {
	hour: '2-digit',
	minute: '2-digit',
	second: '2-digit',
	hour12: false
});

const dayClock = new Intl.DateTimeFormat(undefined, {
	month: 'short',
	day: '2-digit',
	hour: '2-digit',
	minute: '2-digit',
	hour12: false
});

const fullDate = new Intl.DateTimeFormat(undefined, {
	year: 'numeric',
	month: 'short',
	day: '2-digit',
	hour: '2-digit',
	minute: '2-digit',
	second: '2-digit',
	hour12: false
});

/** 14:22:07 — for entries inside a single run. */
export const fmtClock = (t: number): string => clock.format(toMs(t));

/** Jul 24 14:22 — for run lists spanning days. */
export const fmtStamp = (t: number): string => dayClock.format(toMs(t));

/** Full timestamp, for tooltips. */
export const fmtFull = (t: number): string => fullDate.format(toMs(t));

/** 4s ago · 12m ago · 3h ago · 6d ago. Deliberately coarse. */
export function fmtAgo(t: number, now = Date.now()): string {
	const s = Math.max(0, Math.round((now - toMs(t)) / 1000));
	if (s < 5) return 'just now';
	if (s < 60) return `${s}s ago`;
	const m = Math.round(s / 60);
	if (m < 60) return `${m}m ago`;
	const h = Math.round(m / 60);
	if (h < 48) return `${h}h ago`;
	return `${Math.round(h / 24)}d ago`;
}

/** Wall-clock length of a run: 840ms · 12.4s · 3m 12s · 1h 04m. */
export function fmtDuration(seconds: number | null): string {
	if (seconds === null || Number.isNaN(seconds)) return '—';
	if (seconds < 1) return `${Math.round(seconds * 1000)}ms`;
	if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
	const m = Math.floor(seconds / 60);
	const s = Math.round(seconds % 60);
	if (m < 60) return `${m}m ${String(s).padStart(2, '0')}s`;
	const h = Math.floor(m / 60);
	return `${h}h ${String(m % 60).padStart(2, '0')}m`;
}

/** Seconds, which is what fmtDuration takes. The stored values are microseconds. */
export function runDuration(run: { started_at: number; ended_at: number | null }): number | null {
	if (run.ended_at === null) return null;
	return Math.max(0, (run.ended_at - run.started_at) / MICROS_PER_SEC);
}

/** Elapsed seconds for a run that has not finished. */
export function runElapsed(run: { started_at: number }, now = Date.now()): number {
	return Math.max(0, (now - toMs(run.started_at)) / 1000);
}

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];

export function fmtBytes(n: number): string {
	if (!n) return '0 B';
	let i = 0;
	let v = n;
	while (v >= 1024 && i < UNITS.length - 1) {
		v /= 1024;
		i++;
	}
	return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${UNITS[i]}`;
}

export const shortSha = (sha: string | null): string => (sha ? sha.slice(0, 7) : '—');

export const shortHash = (h: string | null): string => (h ? h.slice(0, 8) : '—');

/** The command as you would type it. Quotes any argument containing spaces. */
export function fmtCommand(cmd: string[]): string {
	return cmd.map((a) => (/\s/.test(a) ? JSON.stringify(a) : a)).join(' ');
}

/** ~/code/checkout-api — home-relative, since every path here is local. */
export function tildePath(p: string): string {
	const m = p.match(/^\/(?:Users|home)\/[^/]+(\/.*)?$/);
	return m ? `~${m[1] ?? ''}` : p;
}

export function pluralise(n: number, one: string, many = one + 's'): string {
	return `${n} ${n === 1 ? one : many}`;
}

/** Minutes-past-midnight to HH:MM, for quiet hours. */
export function minutesToHhmm(mins: number): string {
	const m = ((mins % 1440) + 1440) % 1440;
	return `${String(Math.floor(m / 60)).padStart(2, '0')}:${String(m % 60).padStart(2, '0')}`;
}

export function hhmmToMinutes(v: string): number {
	const [h, m] = v.split(':').map((x) => parseInt(x, 10));
	if (Number.isNaN(h) || Number.isNaN(m)) return 0;
	return h * 60 + m;
}

/** Exit code rendered the way a shell would report it. */
export function fmtExit(run: {
	exit_code: number | null;
	signal: string | null;
	ended_at: number | null;
}): string {
	if (run.signal) return run.signal;
	if (run.ended_at === null) return 'running';
	if (run.exit_code === null) return '—';
	return String(run.exit_code);
}
