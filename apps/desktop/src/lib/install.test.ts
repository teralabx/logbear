import { afterEach, describe, expect, it, vi } from 'vitest';
import {
	STEP_COPY,
	chosenSteps,
	installInOrder,
	installable,
	readInstall,
	undetectedAgents
} from './install';
import { INSTALL_STEPS, type InstallStatus, type StepState } from './api/types';

const step = (over: Partial<StepState> = {}): StepState => ({
	installed: false,
	available: true,
	detail: '',
	backup: null,
	...over
});

function status(over: Partial<InstallStatus> = {}): InstallStatus {
	const all = Object.fromEntries(INSTALL_STEPS.map((s) => [s, step()])) as unknown as InstallStatus;
	return { ...all, ...over };
}

afterEach(() => vi.useRealTimers());

describe('reading the install state', () => {
	it('reports a failure instead of staying on the loading text', async () => {
		const read = await readInstall(() => Promise.reject('logbear-setup panicked'));
		expect(read).toEqual({ state: 'failed', error: 'logbear-setup panicked' });
	});

	it('gives up on a read that never returns', async () => {
		vi.useFakeTimers();
		const read = readInstall(() => new Promise<InstallStatus>(() => {}), 1000);
		await vi.advanceTimersByTimeAsync(1000);
		expect((await read).state).toBe('failed');
	});

	it('returns the status when the read succeeds', async () => {
		const s = status();
		expect(await readInstall(() => Promise.resolve(s))).toEqual({ state: 'ready', status: s });
	});
});

describe('installing several steps', () => {
	it('runs them one at a time and carries on past a failure', async () => {
		let running = 0;
		const order: string[] = [];
		const failures = await installInOrder(['path', 'daemon', 'hook_claude'], async (s) => {
			expect(++running).toBe(1);
			order.push(s);
			await Promise.resolve();
			running--;
			if (s === 'daemon') throw 'launchctl refused';
			return step({ installed: true });
		});
		expect(order).toEqual(['path', 'daemon', 'hook_claude']);
		expect(failures).toEqual(['Background service: launchctl refused']);
	});

	it('reports progress for every step, with no state for the one that failed', async () => {
		const seen: [string, number, boolean][] = [];
		await installInOrder(
			['path', 'daemon'],
			async (s) => {
				if (s === 'path') throw 'no';
				return step({ installed: true });
			},
			(s, done, state) => seen.push([s, done, state !== null])
		);
		expect(seen).toEqual([
			['path', 1, false],
			['daemon', 2, true]
		]);
	});
});

describe('what the first-run screen offers', () => {
	const s = status({
		path: step({ installed: true }),
		hook_codex: step({ available: false }),
		mcp_codex: step({ available: false })
	});

	it('leaves out what is installed and what cannot be', () => {
		expect(installable(s)).not.toContain('path');
		expect(installable(s)).not.toContain('hook_codex');
		expect(installable(s)).toContain('hook_claude');
	});

	it('names the agents that were not found', () => {
		expect(undetectedAgents(s)).toEqual(['Codex']);
	});

	it('installs nothing that was not chosen', () => {
		expect(chosenSteps(s, new Set())).toEqual([]);
	});

	it('installs the chosen steps in install order, not the order they were picked', () => {
		expect(chosenSteps(s, new Set(['hook_claude', 'daemon']))).toEqual(['daemon', 'hook_claude']);
	});

	it('cannot be made to install something unavailable', () => {
		expect(chosenSteps(s, new Set(['hook_codex']))).toEqual([]);
	});
});

describe('install copy', () => {
	it('describes this product', () => {
		const all = Object.values(STEP_COPY)
			.map((c) => c.what)
			.join(' ');
		expect(all).not.toMatch(/\.logbear\/bin|shim|three read-only/i);
	});

	it('names all five MCP tools', () => {
		for (const tool of ['schema', 'query', 'wait', 'diff', 'ack']) {
			expect(STEP_COPY.mcp_claude.what).toContain(`logbear_${tool}`);
		}
	});

	it('says that every agent hook can block', () => {
		for (const hook of ['hook_claude', 'hook_codex', 'hook_pi', 'hook_hermes'] as const) {
			expect(STEP_COPY[hook].what).toMatch(/\bblock/i);
		}
	});
});
