import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { pollWhileVisible } from './poll';

function fakeDocument(visibilityState: string) {
	const doc = new EventTarget() as EventTarget & { visibilityState: string };
	doc.visibilityState = visibilityState;
	return doc;
}
const asDoc = (d: ReturnType<typeof fakeDocument>) => d as unknown as Document;

beforeEach(() => void vi.useFakeTimers());
afterEach(() => void vi.useRealTimers());

describe('polling', () => {
	it('ticks while visible', () => {
		const tick = vi.fn();
		const stop = pollWhileVisible(tick, 1000, asDoc(fakeDocument('visible')));
		vi.advanceTimersByTime(3000);
		expect(tick).toHaveBeenCalledTimes(3);
		stop();
	});

	it('does nothing while hidden, and catches up on return', () => {
		const tick = vi.fn();
		const doc = fakeDocument('hidden');
		const stop = pollWhileVisible(tick, 1000, asDoc(doc));
		vi.advanceTimersByTime(5000);
		expect(tick).not.toHaveBeenCalled();

		doc.visibilityState = 'visible';
		doc.dispatchEvent(new Event('visibilitychange'));
		expect(tick).toHaveBeenCalledTimes(1);
		stop();
	});

	it('does not tick on the way to hidden', () => {
		const tick = vi.fn();
		const doc = fakeDocument('visible');
		const stop = pollWhileVisible(tick, 1000, asDoc(doc));
		doc.visibilityState = 'hidden';
		doc.dispatchEvent(new Event('visibilitychange'));
		expect(tick).not.toHaveBeenCalled();
		stop();
	});

	it('stops completely when disposed', () => {
		const tick = vi.fn();
		const doc = fakeDocument('visible');
		pollWhileVisible(tick, 1000, asDoc(doc))();
		vi.advanceTimersByTime(3000);
		doc.dispatchEvent(new Event('visibilitychange'));
		expect(tick).not.toHaveBeenCalled();
	});
});
