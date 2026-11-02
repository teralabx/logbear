// Squashing a list of entries down to the unique ones.
//
// A dev server that logs the same request line or the same warning a thousand times
// buries the one line that is different. Squashed, each distinct entry is listed
// once, where it first appeared, with how many times it repeated and when it last did.
//
// "Same" ignores the parts that change on every repeat: timestamps, ids, durations,
// counters. An entry the daemon fingerprinted is keyed by that signature's template,
// which already has them stripped; anything else is normalised here the same way.

import type { EntryView } from './api/types';

export interface Squashed {
	/** The first occurrence, which is the one shown. */
	entry: EntryView;
	count: number;
	/** `captured_at` of the most recent occurrence. */
	lastAt: number;
}

const ANSI = /\x1b\[[0-9;?]*[ -/]*[@-~]/g;
const VOLATILE = [
	// ISO-8601 dates and times, with or without a zone.
	/\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?/g,
	/\b\d{1,2}:\d{2}:\d{2}(\.\d+)?\b/g,
	/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi,
	/\b0x[0-9a-f]+\b/gi,
	// Bare hex runs long enough to be hashes or ids, not words like "cafe".
	/\b(?=[0-9a-f]*\d)[0-9a-f]{8,}\b/gi,
	/\d+(\.\d+)?/g
];

/** Text with every volatile token replaced, so two repeats compare equal. */
export function normalise(text: string): string {
	let t = text.replace(ANSI, '');
	for (const re of VOLATILE) t = t.replace(re, '#');
	return t.replace(/\s+/g, ' ').trim();
}

export function squashKey(e: EntryView): string {
	return `${e.level}\u0000${e.template ?? normalise(e.text)}`;
}

export function squash(list: readonly EntryView[]): Squashed[] {
	const out: Squashed[] = [];
	const byKey = new Map<string, Squashed>();
	for (const e of list) {
		const key = squashKey(e);
		const seen = byKey.get(key);
		if (seen) {
			seen.count += 1;
			if (e.captured_at > seen.lastAt) seen.lastAt = e.captured_at;
		} else {
			const s = { entry: e, count: 1, lastAt: e.captured_at };
			byKey.set(key, s);
			out.push(s);
		}
	}
	return out;
}
