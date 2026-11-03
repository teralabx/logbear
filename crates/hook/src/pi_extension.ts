// logbear for pi: runs logbear-hook on pi's lifecycle events and applies its reply.
//
// Written by the logbear app (`logbear-hook --emit-config pi`). Reinstall from the app rather
// than editing: the hook path below is baked in at install time.
//
// pi has no command hooks, so this speaks Claude Code's hook dialect to logbear-hook on both
// sides — a Claude-shaped payload in, a Claude-shaped reply out — and maps the reply onto
// pi's extension API. Every failure is silent and allows, like the hook itself.

import { spawn } from "node:child_process";

const HOOK: string = __LOGBEAR_HOOK__;
const TIMEOUT_MS = 10_000;

// pi's built-in tools, under the names logbear's rules know them by.
const TOOL_NAMES: Record<string, string> = { bash: "Bash", edit: "Edit", write: "Write" };

type Reply = {
	decision?: string;
	reason?: string;
	systemMessage?: string;
	hookSpecificOutput?: { additionalContext?: string };
};

function ask(payload: Record<string, unknown>): Promise<Reply> {
	return new Promise((resolve) => {
		let out = "";
		let done = false;
		const finish = (reply: Reply) => {
			if (done) return;
			done = true;
			clearTimeout(timer);
			resolve(reply);
		};
		const child = spawn(HOOK, ["--harness", "pi"], { stdio: ["pipe", "pipe", "ignore"] });
		const timer = setTimeout(() => {
			child.kill();
			finish({});
		}, TIMEOUT_MS);
		child.on("error", () => finish({}));
		child.stdout.on("data", (chunk) => (out += chunk));
		child.on("close", () => {
			try {
				finish(JSON.parse(out) ?? {});
			} catch {
				finish({});
			}
		});
		child.stdin.on("error", () => {});
		child.stdin.end(JSON.stringify(payload));
	});
}

/// The text the agent should see: a block's reason or an advisory's context.
function told(reply: Reply): string | undefined {
	return reply.decision === "block" ? reply.reason : reply.hookSpecificOutput?.additionalContext;
}

export default function (pi: any) {
	// The standing rule is said once per session. A settle block forces at most one extra
	// turn per prompt: `stopBlocked` is pi's missing `stop_hook_active`.
	let ruled = false;
	let stopBlocked = false;

	const base = (event: string, ctx: any) => ({
		session_id: ctx.sessionManager.getSessionId(),
		cwd: ctx.cwd,
		hook_event_name: event,
	});

	pi.on("session_start", () => {
		ruled = false;
		stopBlocked = false;
	});

	pi.on("before_agent_start", async (event: any, ctx: any) => {
		// A run started by our own settle message is the same prompt continuing.
		if (event.messageType !== "user") return;
		stopBlocked = false;
		if (ruled) return;
		ruled = true;
		const text = told(await ask({ ...base("SessionStart", ctx), source: "startup" }));
		if (text) return { message: { customType: "logbear", content: text, display: false } };
	});

	pi.on("tool_result", async (event: any, ctx: any) => {
		const text = told(
			await ask({
				...base("PostToolUse", ctx),
				tool_name: TOOL_NAMES[event.toolName] ?? event.toolName,
				tool_input: event.input ?? {},
			}),
		);
		// Appended to what the tool returned, which is where pi lets an extension speak
		// mid-turn. Replacing `content` drops `structuredContent`; the text is what the model
		// reads either way.
		if (text) return { content: [...(event.content ?? []), { type: "text", text }] };
	});

	pi.on("agent_before_settle", async (event: any, ctx: any) => {
		const reply = await ask({ ...base("Stop", ctx), stop_hook_active: stopBlocked });
		if (reply.decision === "block" && reply.reason) {
			stopBlocked = true;
			return {
				entries: [
					...(event.entries ?? []),
					{ type: "custom_message", customType: "logbear", content: reply.reason, display: true },
				],
				continue: true,
			};
		}
		// Past the block budget the hook degrades to a warning for the human, as in Claude Code.
		if (reply.systemMessage && ctx.hasUI) ctx.ui.notify(reply.systemMessage, "warning");
	});
}
