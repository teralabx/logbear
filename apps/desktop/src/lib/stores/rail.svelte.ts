// Whether the left rail is collapsed to its compact strip. Remembered per machine;
// storage can be unavailable (private mode, cleared site data), in which case the
// choice simply lasts for the session.

const KEY = 'logbear:rail-collapsed';

function stored(): boolean {
	try {
		return localStorage.getItem(KEY) === '1';
	} catch {
		return false;
	}
}

class RailState {
	collapsed = $state(false);

	init(): () => void {
		this.collapsed = stored();
		if (typeof window === 'undefined') return () => {};
		// ⌘\ / Ctrl+\ — the sidebar shortcut in most editors.
		const onKey = (e: KeyboardEvent) => {
			if (e.key === '\\' && (e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey) {
				e.preventDefault();
				this.toggle();
			}
		};
		window.addEventListener('keydown', onKey);
		return () => window.removeEventListener('keydown', onKey);
	}

	toggle() {
		this.collapsed = !this.collapsed;
		try {
			localStorage.setItem(KEY, this.collapsed ? '1' : '0');
		} catch {
			/* not persisted; still applies for this session */
		}
	}
}

export const rail = new RailState();
