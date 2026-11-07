// What the install steps do, and the logic shared by Settings → Install and the
// first-run screen. The copy has to match `crates/setup`, `crates/hook` and
// `crates/mcp`: it is what people consent to.

import { apiError, installStatus } from '#lib/api';
import {
	INSTALL_STEPS,
	type InstallStatus,
	type InstallStep,
	type StepState
} from '#lib/api/types';

export const STEP_COPY: Record<InstallStep, { title: string; what: string }> = {
	path: {
		title: 'logbear on your PATH',
		what: 'Symlinks the logbear command into a directory already on your PATH. If none is usable it links into ~/.local/bin and adds that directory to .zshrc, .bashrc and .profile, each backed up first, or to a fish conf.d file. Commands are captured only when you put logbear in front of them.'
	},
	daemon: {
		title: 'Background service',
		what: 'Registers logbeard as a launch agent, ~/Library/LaunchAgents/sh.logbear.daemon.plist, so it starts at login and restarts after a crash. Without it the daemon is started on demand and does not come back after a reboot.'
	},
	mcp_claude: {
		title: 'MCP server — Claude Code',
		what: 'Registers logbear-mcp at user scope, through the claude CLI if you have it and in ~/.claude.json if not. Claude Code gets five tools: logbear_schema, logbear_query (read-only SQL), logbear_wait (waits for new output), logbear_diff (compares two runs) and logbear_ack (marks errors as read).'
	},
	mcp_codex: {
		title: 'MCP server — Codex',
		what: 'The same five tools, added to ~/.codex/config.toml.'
	},
	mcp_pi: {
		title: 'MCP server — pi',
		what: 'The same five tools, added to ~/.pi/agent/mcp.json with direct exposure, so pi sees them without searching. Needs pi 1.0 or later.'
	},
	mcp_hermes: {
		title: 'MCP server — Hermes',
		what: 'The same five tools, added under mcp_servers in ~/.hermes/config.yaml between marker comments, so removing them leaves the rest of the file alone.'
	},
	hook_claude: {
		title: 'Agent hook — Claude Code',
		what: 'Adds SessionStart, PostToolUse, Stop and SubagentStop hooks to ~/.claude/settings.json. This hook blocks the agent: when your process logs an error signature the session has not seen, Claude Code cannot finish its turn until it has dealt with it, and an edit that causes one can be interrupted. Blocks are limited per session, then errors arrive as advice. If the daemon is down the hook allows everything.'
	},
	hook_codex: {
		title: 'Agent hook — Codex',
		what: 'The same four hooks, with the same blocking, in ~/.codex/hooks.json.'
	},
	hook_pi: {
		title: 'Extension — pi',
		what: 'pi has no command hooks, so this writes a small extension to ~/.pi/agent/extensions/logbear.ts that runs the logbear hook. It blocks the same way: pi keeps working while new errors are unread.'
	},
	hook_hermes: {
		title: 'Shell hooks — Hermes',
		what: 'Adds pre_llm_call and pre_verify hooks to ~/.hermes/config.yaml. They block too: Hermes holds back from finishing a turn that edited code while new errors are unread. Approve both when Hermes asks.'
	}
};

const AGENT_STEPS: [string, InstallStep][] = [
	['Claude Code', 'hook_claude'],
	['Codex', 'hook_codex'],
	['pi', 'hook_pi'],
	['Hermes', 'hook_hermes']
];

/** Long enough for a slow login shell; `install_status` starts several. */
export const INSTALL_READ_TIMEOUT_MS = 20_000;

/** Reject if `work` has not settled in time, so a stuck call can reach an error and a retry. */
export function withTimeout<T>(work: Promise<T>, ms: number, message: string): Promise<T> {
	return new Promise((resolve, reject) => {
		const timer = setTimeout(() => reject(new Error(message)), ms);
		work.then(
			(value) => {
				clearTimeout(timer);
				resolve(value);
			},
			(err) => {
				clearTimeout(timer);
				reject(err);
			}
		);
	});
}

export type InstallRead =
	| { state: 'loading' }
	| { state: 'failed'; error: string }
	| { state: 'ready'; status: InstallStatus };

/**
 * Read the install state, always ending in `ready` or `failed`. It needs no daemon, so
 * nothing that failed elsewhere on the screen can hold it back: it is how a person whose
 * daemon is down gets it running again.
 */
export async function readInstall(
	read: () => Promise<InstallStatus> = installStatus,
	timeoutMs = INSTALL_READ_TIMEOUT_MS
): Promise<Exclude<InstallRead, { state: 'loading' }>> {
	try {
		const status = await withTimeout(
			read(),
			timeoutMs,
			`The install state did not come back within ${timeoutMs / 1000} seconds.`
		);
		return { state: 'ready', status };
	} catch (err) {
		return { state: 'failed', error: apiError(err) };
	}
}

/**
 * Install steps one at a time, and keep going past a failure.
 *
 * Sequential rather than parallel: several steps write to the same shell profiles and
 * harness configs, and two of them racing would interleave their edits. One failure does
 * not stop the rest: a machine missing Codex should still get its Claude Code integration.
 */
export async function installInOrder(
	steps: InstallStep[],
	install: (step: InstallStep) => Promise<StepState>,
	onStep: (step: InstallStep, done: number, state: StepState | null) => void = () => {}
): Promise<string[]> {
	const failures: string[] = [];
	let done = 0;
	for (const step of steps) {
		let state: StepState | null = null;
		try {
			state = await install(step);
		} catch (err) {
			failures.push(`${STEP_COPY[step].title}: ${apiError(err)}`);
		}
		onStep(step, ++done, state);
	}
	return failures;
}

/** Steps that could be installed right now: available, and not already done. */
export const installable = (status: InstallStatus): InstallStep[] =>
	INSTALL_STEPS.filter((s) => status[s].available && !status[s].installed);

export const installed = (status: InstallStatus): InstallStep[] =>
	INSTALL_STEPS.filter((s) => status[s].installed);

/** Agents with no trace on this machine, so there is nothing to offer for them. */
export const undetectedAgents = (status: InstallStatus): string[] =>
	AGENT_STEPS.filter(([, step]) => !status[step].available).map(([name]) => name);

/** The chosen steps in install order, whatever order they were picked in. */
export const chosenSteps = (status: InstallStatus, chosen: Set<InstallStep>): InstallStep[] =>
	installable(status).filter((s) => chosen.has(s));
