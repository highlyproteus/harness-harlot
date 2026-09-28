// Harness Harlot task progress for omp. Inside a Harness Harlot terminal
// (HH_PANE_ID set) it mirrors the main agent's todo list into the pane's
// progress ring by running `$HH_CLI progress report …`. Elsewhere it is inert.
// Every failure is swallowed: progress is cosmetic and must never disturb omp.
import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

const HH_CLI = process.env.HH_CLI || "hh";
const CLI_TIMEOUT_MS = 10_000;
const MAX_TEXT_CHARS = 200;
const MAX_TASKS = 1_000;

type TodoItem = { content?: unknown; status?: unknown };
type TodoPhase = { name?: unknown; tasks?: unknown };
type Progress = { done: number; total: number; current?: string; phase?: string };

/** The value as a plain record, for reading untyped session data. */
function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" ? (value as Record<string, unknown>) : undefined;
}

/** Strips control characters and truncates to the protocol's text bound. */
function clean(text: unknown): string | undefined {
  if (typeof text !== "string") return undefined;
  const stripped = text.replace(/[\u0000-\u001f\u007f-\u009f]+/g, " ").trim();
  if (!stripped) return undefined;
  const chars = Array.from(stripped);
  return chars.length > MAX_TEXT_CHARS ? chars.slice(0, MAX_TEXT_CHARS).join("") : stripped;
}

/** done = completed; total = all but abandoned; `current` is the first
 * in-progress task and `phase` the phase holding it, else the first phase with
 * unfinished work. `undefined` for an empty or unreadable list. */
function progressOf(phases: unknown): Progress | undefined {
  if (!Array.isArray(phases)) return undefined;
  let done = 0;
  let total = 0;
  let current: string | undefined;
  let currentPhase: string | undefined;
  let openPhase: string | undefined;
  for (const phase of phases as TodoPhase[]) {
    if (!phase || !Array.isArray(phase.tasks)) continue;
    for (const task of phase.tasks as TodoItem[]) {
      const status = task?.status;
      if (status === "abandoned") continue;
      total += 1;
      if (status === "completed") {
        done += 1;
        continue;
      }
      openPhase ??= clean(phase.name);
      if (status === "in_progress" && current === undefined) {
        current = clean(task.content);
        currentPhase = clean(phase.name);
      }
    }
  }
  if (total === 0) return undefined;
  total = Math.min(total, MAX_TASKS);
  done = Math.min(done, total);
  return { done, total, current, phase: currentPhase ?? openPhase };
}

function reportArgs(progress: Progress | undefined): string[] {
  if (!progress) return ["progress", "clear"];
  const args = [
    "progress",
    "report",
    "--source",
    "omp",
    "--done",
    String(progress.done),
    "--total",
    String(progress.total),
  ];
  if (progress.current) args.push("--current", progress.current);
  if (progress.phase) args.push("--phase", progress.phase);
  return args;
}

/** Phases from the branch's latest successful todo result or `/todo` edit. */
function latestPhases(ctx: ExtensionContext): unknown {
  let phases: unknown;
  for (const value of ctx.sessionManager.getBranch() as unknown[]) {
    const entry = record(value);
    if (entry?.type === "custom" && entry.customType === "user_todo_edit") {
      phases = record(entry.data)?.phases;
      continue;
    }
    const message = entry?.type === "message" ? record(entry.message) : undefined;
    const details = record(message?.details);
    if (
      message?.role === "toolResult" &&
      message.toolName === "todo" &&
      !message.isError &&
      Array.isArray(details?.phases)
    ) {
      phases = details.phases;
    }
  }
  return phases;
}

export default function harnessHarlotProgress(pi: ExtensionAPI): void {
  if (!process.env.HH_PANE_ID) return;

  /** Arguments of the last report the CLI accepted, to skip repeats. */
  let reported: string | undefined;
  /** Latest wanted report while one is in flight; only the newest is sent. */
  let queued: string[] | undefined;
  let sending = false;

  async function flush(): Promise<void> {
    if (sending) return;
    sending = true;
    try {
      while (queued) {
        const args = queued;
        queued = undefined;
        const key = JSON.stringify(args);
        if (key === reported) continue;
        try {
          const result = await pi.exec(HH_CLI, args, { timeout: CLI_TIMEOUT_MS });
          if (result.code === 0) {
            reported = key;
          } else {
            pi.logger.debug("Harness Harlot progress report failed", {
              code: result.code,
              stderr: String(result.stderr ?? "").slice(0, 500),
            });
          }
        } catch (error) {
          pi.logger.debug("Harness Harlot progress report failed", { error: String(error) });
        }
      }
    } finally {
      sending = false;
    }
  }

  function report(progress: Progress | undefined): void {
    queued = reportArgs(progress);
    void flush().catch(() => {});
  }

  function reconstruct(ctx: ExtensionContext): void {
    try {
      // Subagents share this process (and HH_PANE_ID) but not the pane's list.
      if (ctx.agent?.kind === "sub") return;
      report(progressOf(latestPhases(ctx)));
    } catch (error) {
      pi.logger.debug("Harness Harlot progress restore failed", { error: String(error) });
    }
  }

  pi.on("tool_result", async (event, ctx: ExtensionContext) => {
    try {
      if (event.toolName !== "todo" || event.isError || ctx.agent?.kind === "sub") return;
      const phases = record(event.details)?.phases;
      if (!Array.isArray(phases)) return;
      report(progressOf(phases));
    } catch (error) {
      pi.logger.debug("Harness Harlot progress update failed", { error: String(error) });
    }
  });

  // Also after every turn and agent run: `/todo` edits and todos a subagent
  // completes change the branch without a main-agent `todo` result. Repeats
  // of the last report are skipped.
  for (const name of [
    "session_start",
    "session_switch",
    "session_branch",
    "session_tree",
    "turn_end",
    "agent_end",
  ] as const) {
    pi.on(name, async (_event: unknown, ctx: ExtensionContext) => {
      reconstruct(ctx);
    });
  }
}
