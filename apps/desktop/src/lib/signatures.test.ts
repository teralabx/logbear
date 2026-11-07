import { describe, expect, it, vi } from 'vitest';
import { SIGNATURE_LIMIT, readSignatures, sortSignatures } from './signatures';
import { fingerprints } from './api';
import type { Fingerprint } from './api/types';

vi.mock('./api', () => ({ fingerprints: vi.fn(async () => []) }));

const fp = (id: number, over: Partial<Fingerprint> = {}): Fingerprint => ({
	id,
	template: '',
	hash: '',
	level: 'error',
	first_seen: 0,
	last_seen: 0,
	total_count: 0,
	run_count: 0,
	...over
});

describe('reading signatures', () => {
	it('asks the server to filter, so newer warnings cannot push errors off the page', async () => {
		const ref = { by: 'id', value: 1 } as const;
		await readSignatures(ref, true);
		expect(fingerprints).toHaveBeenCalledWith(ref, SIGNATURE_LIMIT, true);
	});

	it('asks for every level when the filter is off', async () => {
		const ref = { by: 'id', value: 1 } as const;
		await readSignatures(ref, false);
		expect(fingerprints).toHaveBeenLastCalledWith(ref, SIGNATURE_LIMIT, false);
	});
});

describe('sorting signatures', () => {
	const list = [
		fp(1, { total_count: 5, last_seen: 10, run_count: 1 }),
		fp(2, { total_count: 9, last_seen: 5, run_count: 2 }),
		fp(3, { total_count: 1, last_seen: 20, run_count: 3 })
	];
	const ids = (l: Fingerprint[]) => l.map((f) => f.id);

	it('orders by frequency, recency or spread', () => {
		expect(ids(sortSignatures(list, 'frequency'))).toEqual([2, 1, 3]);
		expect(ids(sortSignatures(list, 'recency'))).toEqual([3, 1, 2]);
		expect(ids(sortSignatures(list, 'spread'))).toEqual([3, 2, 1]);
	});

	it('leaves the list it was given alone', () => {
		sortSignatures(list, 'recency');
		expect(ids(list)).toEqual([1, 2, 3]);
	});
});
