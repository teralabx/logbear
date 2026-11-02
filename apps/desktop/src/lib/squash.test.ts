import { describe, expect, it } from 'vitest';
import { normalise, squash } from './squash';
import type { EntryView } from './api/types';

let nextId = 1;
function entry(text: string, over: Partial<EntryView> = {}): EntryView {
	const id = nextId++;
	return {
		id,
		run_id: 1,
		start_line: id,
		end_line: id,
		captured_at: id * 1_000_000,
		level: 'info',
		evidence: 'declared',
		is_error: false,
		error_reason: null,
		fingerprint_id: null,
		boundaries: 'exact',
		text,
		template: null,
		is_new_signature: false,
		...over
	} as EntryView;
}

describe('normalise', () => {
	it('treats repeats that differ only in volatile tokens as equal', () => {
		expect(normalise('[info] GET /api/users/42 200 in 13ms')).toBe(
			normalise('[info] GET /api/users/7 200 in 210ms')
		);
		expect(normalise('2026-11-02T11:17:20.120Z req 3f2a9c1e-0b5d-4c1e-9f3a-1b2c3d4e5f60 done')).toBe(
			normalise('2026-11-02T11:18:01.004Z req 00000000-1111-2222-3333-444444444444 done')
		);
	});

	it('ignores colour codes', () => {
		expect(normalise('\x1b[32mcompiled\x1b[0m')).toBe('compiled');
	});

	it('keeps words that merely look like hex', () => {
		expect(normalise('deadbeef added to cafe')).toBe('deadbeef added to cafe');
	});
});

describe('squash', () => {
	it('lists each distinct entry once, at its first position, with a count', () => {
		const list = [
			entry('GET /a 200 in 3ms'),
			entry('compiled'),
			entry('GET /a 200 in 9ms'),
			entry('GET /a 200 in 1ms')
		];
		const out = squash(list);
		expect(out.map((s) => [s.entry.id, s.count])).toEqual([
			[list[0].id, 3],
			[list[1].id, 1]
		]);
		expect(out[0].lastAt).toBe(list[3].captured_at);
	});

	it('keeps the same text at different levels apart', () => {
		expect(squash([entry('boom'), entry('boom', { level: 'error' })])).toHaveLength(2);
	});

	it('groups by signature template when the daemon fingerprinted the entry', () => {
		const out = squash([
			entry('Timeout after 5000ms talking to db-1', { level: 'error', template: 'Timeout after <*> talking to <*>' }),
			entry('Timeout after 30000ms talking to cache', { level: 'error', template: 'Timeout after <*> talking to <*>' })
		]);
		expect(out).toHaveLength(1);
		expect(out[0].count).toBe(2);
	});
});
