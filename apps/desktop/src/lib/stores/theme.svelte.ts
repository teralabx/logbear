// Theme state.
//
// Wiring, end to end:
//   1. app.css declares logbear-light as daisyUI's `default` (emitted at
//      `:where(:root)`) and logbear-dark as `prefersdark` (emitted inside
//      `@media (prefers-color-scheme: dark)`), so the OS preference already
//      picks correctly with no JavaScript at all.
//   2. Both themes also emit a `[data-theme="..."]` selector, which outranks
//      the two above. That attribute is what this module writes.
//   3. The inline script in app.html sets the attribute before first paint.
//      This module keeps it in sync afterwards and persists the choice.
//
// With nothing stored we keep following the OS, including live changes.

export type ThemeName = 'logbear-dark' | 'logbear-light';

const KEY = 'logbear:theme';

function systemTheme(): ThemeName {
	if (typeof window === 'undefined') return 'logbear-dark';
	return window.matchMedia('(prefers-color-scheme: light)').matches
		? 'logbear-light'
		: 'logbear-dark';
}

function stored(): ThemeName | null {
	if (typeof localStorage === 'undefined') return null;
	const v = localStorage.getItem(KEY);
	return v === 'logbear-dark' || v === 'logbear-light' ? v : null;
}

class Theme {
	current = $state<ThemeName>('logbear-dark');
	/** True while the theme is inherited from the OS rather than chosen. */
	followingSystem = $state(true);

	init(): () => void {
		const s = stored();
		this.followingSystem = s === null;
		this.current = s ?? systemTheme();
		this.#apply();

		if (typeof window === 'undefined') return () => {};
		const mq = window.matchMedia('(prefers-color-scheme: light)');
		const onChange = () => {
			if (!this.followingSystem) return;
			this.current = systemTheme();
			this.#apply();
		};
		mq.addEventListener('change', onChange);
		return () => mq.removeEventListener('change', onChange);
	}

	set(name: ThemeName) {
		this.current = name;
		this.followingSystem = false;
		try {
			localStorage.setItem(KEY, name);
		} catch {
			/* private mode — the theme still applies for this session */
		}
		this.#apply();
	}

	toggle() {
		this.set(this.current === 'logbear-dark' ? 'logbear-light' : 'logbear-dark');
	}

	/** Drop the stored override and go back to tracking the OS. */
	followSystem() {
		try {
			localStorage.removeItem(KEY);
		} catch {
			/* ignore */
		}
		this.followingSystem = true;
		this.current = systemTheme();
		this.#apply();
	}

	/** What Settings shows as selected: the OS, or a fixed theme. */
	get choice(): ThemeName | 'system' {
		return this.followingSystem ? 'system' : this.current;
	}

	get isDark(): boolean {
		return this.current === 'logbear-dark';
	}

	#apply() {
		if (typeof document !== 'undefined') {
			document.documentElement.setAttribute('data-theme', this.current);
		}
	}
}

export const theme = new Theme();
