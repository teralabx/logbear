import { beforeEach, describe, expect, it, vi } from 'vitest';
import { theme } from './theme.svelte';

let osIsLight = false;
let store: Record<string, string> = {};
let onOsChange: (() => void) | undefined;

beforeEach(() => {
	osIsLight = false;
	store = {};
	vi.stubGlobal('localStorage', {
		getItem: (k: string) => store[k] ?? null,
		setItem: (k: string, v: string) => void (store[k] = v),
		removeItem: (k: string) => void delete store[k]
	});
	vi.stubGlobal('document', { documentElement: { setAttribute: vi.fn() } });
	vi.stubGlobal('window', {
		matchMedia: () => ({
			get matches() {
				return osIsLight;
			},
			addEventListener: (_: string, fn: () => void) => (onOsChange = fn),
			removeEventListener: () => {}
		})
	});
	theme.init();
});

describe('theme', () => {
	it('starts by following the OS', () => {
		expect(theme.choice).toBe('system');
		expect(theme.current).toBe('logbear-dark');
	});

	it('remembers a fixed choice and ignores the OS while it stands', () => {
		theme.set('logbear-dark');
		osIsLight = true;
		onOsChange?.();
		expect(theme.choice).toBe('logbear-dark');
		expect(theme.current).toBe('logbear-dark');
	});

	it('can go back to following the OS, live', () => {
		theme.set('logbear-dark');
		theme.followSystem();
		expect(theme.choice).toBe('system');
		expect(store).toEqual({});

		osIsLight = true;
		onOsChange?.();
		expect(theme.current).toBe('logbear-light');
	});
});
