import { describe, expect, it } from 'vitest';
import { fmtAgo, fmtDuration, runDuration, runElapsed, toMs, MICROS_PER_SEC } from './format';

/**
 * The backend contract is unix **microseconds**, everywhere, with no exceptions. These
 * tests exist because the first version inferred the unit from magnitude: a microsecond
 * value read as milliseconds landed in the year 58,000, and because the date format shows
 * no year it rendered as a plausible-looking "Dec 02" and shipped.
 */
const NOW_MS = Date.UTC(2026, 6, 26, 12, 0, 0);
const NOW_MICROS = NOW_MS * 1000;

describe('timestamps are microseconds', () => {
	it('converts to milliseconds by dividing, never by guessing', () => {
		expect(toMs(NOW_MICROS)).toBe(NOW_MS);
	});

	it('lands in the year it was recorded in', () => {
		expect(new Date(toMs(NOW_MICROS)).getUTCFullYear()).toBe(2026);
	});

	it('does not silently accept a millisecond value as if it were fine', () => {
		// Passing ms where micros are expected must be visibly wrong, not quietly wrong.
		expect(new Date(toMs(NOW_MS)).getUTCFullYear()).not.toBe(2026);
	});

	it('reports recent runs as recent', () => {
		expect(fmtAgo(NOW_MICROS - 40 * MICROS_PER_SEC, NOW_MS)).toBe('40s ago');
		expect(fmtAgo(NOW_MICROS - 3600 * MICROS_PER_SEC, NOW_MS)).toBe('1h ago');
		expect(fmtAgo(NOW_MICROS, NOW_MS)).toBe('just now');
	});
});

describe('durations', () => {
	it('turns a microsecond span into seconds for fmtDuration', () => {
		const run = { started_at: NOW_MICROS, ended_at: NOW_MICROS + 5_430_000 };
		expect(runDuration(run)).toBeCloseTo(5.43, 2);
		expect(fmtDuration(runDuration(run))).toBe('5.4s');
	});

	it('renders a six-millisecond run as milliseconds, not as 6000 seconds', () => {
		const run = { started_at: NOW_MICROS, ended_at: NOW_MICROS + 6_000 };
		expect(fmtDuration(runDuration(run))).toBe('6ms');
	});

	it('has no duration for a run that is still going', () => {
		expect(runDuration({ started_at: NOW_MICROS, ended_at: null })).toBeNull();
		expect(fmtDuration(null)).toBe('—');
	});

	it('measures elapsed time of a live run in seconds', () => {
		expect(runElapsed({ started_at: NOW_MICROS - 90 * MICROS_PER_SEC }, NOW_MS)).toBeCloseTo(90, 1);
	});
});
