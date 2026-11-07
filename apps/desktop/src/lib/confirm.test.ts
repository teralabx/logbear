import { describe, expect, it } from 'vitest';
import { confirmDeleteRun, confirmSquashProject, confirmSquashRun } from './confirm';

describe('confirmation copy', () => {
	it('names what is being deleted and says it is permanent', () => {
		const copy = confirmDeleteRun({ id: 42, command: ['mix', 'test'] });
		expect(copy.title).toContain('#42');
		expect(copy.body).toContain('mix test');
		expect(copy.body).toMatch(/cannot be undone/i);
	});

	it('says what a squash keeps and what it destroys', () => {
		for (const copy of [confirmSquashRun({ id: 42 }), confirmSquashProject('api', 12)]) {
			expect(copy.body).toMatch(/deleted for good/);
			expect(copy.body).toMatch(/counts and signatures are kept/);
			expect(copy.body).toMatch(/cannot be undone/i);
		}
	});

	it('names the project a project-wide squash applies to', () => {
		const copy = confirmSquashProject('api', 12);
		expect(copy.title).toContain('api');
		expect(copy.body).toContain('12 stored runs');
	});
});
