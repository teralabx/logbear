import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { fingerprints, projectRef, updateProject } from './index';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn(async () => null), Channel: class {} }));

beforeEach(() => {
	vi.stubGlobal('window', { __TAURI_INTERNALS__: {} });
	vi.mocked(invoke).mockClear();
});

describe('the Tauri boundary', () => {
	it('sends null to put a project back on the default cap', async () => {
		await updateProject(projectRef(7), null, true, false);
		expect(invoke).toHaveBeenCalledWith('update_project', {
			project: { by: 'id', value: 7 },
			retentionBytes: null,
			pushEnabled: true,
			notifyEnabled: false
		});
	});

	it('passes the errors-only filter through to the daemon', async () => {
		await fingerprints(projectRef(7), 200, true);
		expect(invoke).toHaveBeenCalledWith('fingerprints', {
			project: { by: 'id', value: 7 },
			limit: 200,
			errorsOnly: true
		});
	});
});
