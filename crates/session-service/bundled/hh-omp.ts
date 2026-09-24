// Harness Harlot extension for omp bots. Every tool shells out to the
// Harness Harlot CLI (`$HH_CLI … --json`) so the Rust CLI stays the single
// implementation. A watcher tells the bot when one of its workers needs the
// user or finishes.
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

const HH_CLI = process.env.HH_CLI || "hh";
const CLI_TIMEOUT_MS = 60_000;
const WAIT_DEFAULT_TIMEOUT_MS = 120_000;
const WAIT_MAX_TIMEOUT_MS = 600_000;
const WATCH_INTERVAL_MS = 2_000;
/** Minimum spacing between injected worker updates; later ones are batched. */
const NOTIFY_COOLDOWN_MS = 10_000;
const NOTIFY_TAIL_LINES = 15;
const TERMINAL_KEYS = [
  "enter",
  "ctrl-c",
  "ctrl-d",
  "escape",
  "tab",
  "shift-tab",
  "up",
  "down",
  "left",
  "right",
  "backspace",
  "space",
] as const;
const WAIT_CONDITIONS = ["needs-you", "done", "idle", "exited", "any"] as const;

type ToolOutput = {
  content: Array<{ type: "text"; text: string } | { type: "image"; data: string; mimeType: string }>;
  details?: unknown;
};

type WorkerPane = {
  pane_id: string;
  title: string;
  status: string;
  exited: boolean;
};

type WorkerTab = { tab_id: string; title: string; panes: WorkerPane[] };
type Workstation = { workstation_id: string; title: string; tabs: WorkerTab[] };

/** Worker states the watcher reports. `busy` covers working and idle. */
type Category = "busy" | "needs-you" | "done" | "exited";

type WorkerEvent = {
  paneId: string;
  worker: string;
  workstation: string;
  category: Exclude<Category, "busy">;
  status: string;
};

function jsonResult(value: unknown): ToolOutput {
  return {
    content: [{ type: "text", text: JSON.stringify(value, null, 2) }],
    details: value,
  };
}

function flag(args: string[], name: string, value: string | number | undefined): void {
  if (value !== undefined) args.push(name, String(value));
}

function categoryOf(pane: WorkerPane): Category {
  if (pane.exited) return "exited";
  switch (pane.status) {
    case "needs_approval":
    case "needs_input":
    case "attention":
      return "needs-you";
    case "done":
      return "done";
    default:
      return "busy";
  }
}

function describe(event: WorkerEvent): string {
  switch (event.category) {
    case "needs-you":
      return event.status === "needs_approval"
        ? "needs approval"
        : event.status === "needs_input"
          ? "needs input"
          : "needs attention";
    case "done":
      return "is done";
    case "exited":
      return "exited";
  }
}

export default function harnessHarlot(pi: ExtensionAPI): void {
  const z = pi.zod;

  async function hh(args: string[], signal?: AbortSignal, timeout = CLI_TIMEOUT_MS): Promise<unknown> {
    const result = await pi.exec(HH_CLI, [...args, "--json"], { signal, timeout });
    if (result.code !== 0) {
      const message = (result.stderr || result.stdout).trim();
      throw new Error(message || `${HH_CLI} ${args.join(" ")} exited with ${result.code}`);
    }
    return JSON.parse(result.stdout);
  }

  // ---------------------------------------------------------------- terminals

  const terminalListParams = z.object({ mine: z.boolean().optional() });
  pi.registerTool<typeof terminalListParams>({
    name: "hh_terminal_list",
    label: "List terminals",
    description:
      "List workstations, their terminal tabs and panes with live status (working, needs_approval, needs_input, attention, done, idle), exit state and owner bot. mine=true lists only the workers you created.",
    parameters: terminalListParams,
    approval: "read",
    async execute(_id, params, signal) {
      const args = ["terminal", "list"];
      if (params.mine) args.push("--mine");
      return jsonResult(await hh(args, signal));
    },
  });

  const terminalNewParams = z.object({
    title: z.string().optional(),
    command: z.string().optional(),
    cwd: z.string().optional(),
    workstation_id: z.string().optional(),
  });
  pi.registerTool<typeof terminalNewParams>({
    name: "hh_terminal_new",
    label: "New worker",
    description:
      'Open a named worker terminal tab and type a command into its shell, e.g. command=`omp "<task>"`. Without workstation_id, workers open in your own workstation. cwd must be an existing directory.',
    parameters: terminalNewParams,
    async execute(_id, params, signal) {
      const args = ["terminal", "new"];
      flag(args, "--workstation", params.workstation_id);
      flag(args, "--cwd", params.cwd);
      flag(args, "--title", params.title);
      flag(args, "--command", params.command);
      return jsonResult(await hh(args, signal));
    },
  });

  const terminalSendParams = z.object({
    pane_id: z.string(),
    text: z.string().optional(),
    keys: z.array(z.enum(TERMINAL_KEYS)).optional(),
    enter: z.boolean().optional(),
  });
  pi.registerTool<typeof terminalSendParams>({
    name: "hh_terminal_send",
    label: "Send to terminal",
    description:
      "Type into a worker terminal: text first, then keys in order, then Enter when enter=true. Text alone is not submitted.",
    parameters: terminalSendParams,
    approval: "write",
    async execute(_id, params, signal) {
      const args = ["terminal", "send", params.pane_id];
      flag(args, "--text", params.text);
      for (const key of params.keys ?? []) args.push("--key", key);
      if (params.enter) args.push("--enter");
      return jsonResult(await hh(args, signal));
    },
  });

  const terminalReadParams = z.object({ pane_id: z.string(), lines: z.number().int().min(0).optional() });
  pi.registerTool<typeof terminalReadParams>({
    name: "hh_terminal_read",
    label: "Read terminal",
    description: "Read a terminal pane's visible screen text (optionally only the last lines), status and exit state.",
    parameters: terminalReadParams,
    approval: "read",
    async execute(_id, params, signal) {
      const args = ["terminal", "read", params.pane_id];
      flag(args, "--lines", params.lines);
      return jsonResult(await hh(args, signal));
    },
  });

  const terminalWaitParams = z.object({
    pane_id: z.string(),
    until: z.enum(WAIT_CONDITIONS).optional(),
    pattern: z.string().optional(),
    timeout_ms: z.number().int().min(0).max(WAIT_MAX_TIMEOUT_MS).optional(),
  });
  pi.registerTool<typeof terminalWaitParams>({
    name: "hh_terminal_wait",
    label: "Wait for terminal",
    description:
      "Wait until a terminal needs you, is done, is idle, exits, or its screen matches a regular expression. Returns reason (needs-you, done, idle, exited, pattern, closed, timeout), status and the screen tail.",
    parameters: terminalWaitParams,
    approval: "read",
    async execute(_id, params, signal) {
      const args = ["terminal", "wait", params.pane_id];
      flag(args, "--until", params.until);
      flag(args, "--pattern", params.pattern);
      flag(args, "--timeout-ms", params.timeout_ms);
      const timeout = (params.timeout_ms ?? WAIT_DEFAULT_TIMEOUT_MS) + CLI_TIMEOUT_MS;
      return jsonResult(await hh(args, signal, timeout));
    },
  });

  const terminalFocusParams = z.object({ pane_id: z.string() });
  pi.registerTool<typeof terminalFocusParams>({
    name: "hh_terminal_focus",
    label: "Show terminal",
    description: "Show a terminal pane's tab to the user.",
    parameters: terminalFocusParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(await hh(["terminal", "focus", params.pane_id], signal));
    },
  });

  const terminalCloseParams = z.object({ pane_id: z.string() });
  pi.registerTool<typeof terminalCloseParams>({
    name: "hh_terminal_close",
    label: "Close terminal",
    description: "Close a terminal pane and end its process. Only close workers the user no longer needs.",
    parameters: terminalCloseParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(await hh(["terminal", "close", params.pane_id], signal));
    },
  });

  const terminalRenameParams = z.object({ tab_id: z.string(), title: z.string() });
  pi.registerTool<typeof terminalRenameParams>({
    name: "hh_terminal_rename",
    label: "Rename tab",
    description: "Rename a worker tab.",
    parameters: terminalRenameParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(await hh(["terminal", "rename", params.tab_id, params.title], signal));
    },
  });

  const workstationNewParams = z.object({ cwd: z.string(), title: z.string().optional() });
  pi.registerTool<typeof workstationNewParams>({
    name: "hh_workstation_new",
    label: "New workstation",
    description: "Create a workstation rooted at an existing directory.",
    parameters: workstationNewParams,
    approval: "write",
    async execute(_id, params, signal) {
      const args = ["workstation", "new", "--cwd", params.cwd];
      flag(args, "--title", params.title);
      return jsonResult(await hh(args, signal));
    },
  });

  // ----------------------------------------------------------------- browsers

  const browserListParams = z.object({});
  pi.registerTool<typeof browserListParams>({
    name: "hh_browser_list",
    label: "List browsers",
    description: "List Harness Harlot browser panes with their URLs.",
    parameters: browserListParams,
    approval: "read",
    async execute(_id, _params, signal) {
      return jsonResult(await hh(["browser", "list"], signal));
    },
  });

  const browserGotoParams = z.object({ pane_id: z.string(), url: z.string() });
  pi.registerTool<typeof browserGotoParams>({
    name: "hh_browser_goto",
    label: "Navigate browser",
    description: "Navigate a browser pane and wait for the page to load.",
    parameters: browserGotoParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(await hh(["browser", "goto", params.url, "--pane", params.pane_id], signal));
    },
  });

  const browserReadParams = z.object({ pane_id: z.string(), selector: z.string().optional() });
  pi.registerTool<typeof browserReadParams>({
    name: "hh_browser_read",
    label: "Read browser",
    description: "Read the visible text of a browser page or of the element matching a CSS selector.",
    parameters: browserReadParams,
    approval: "read",
    async execute(_id, params, signal) {
      const args = ["browser", "read"];
      if (params.selector !== undefined) args.push(params.selector);
      args.push("--pane", params.pane_id);
      return jsonResult(await hh(args, signal));
    },
  });

  const browserEvalParams = z.object({ pane_id: z.string(), expression: z.string() });
  pi.registerTool<typeof browserEvalParams>({
    name: "hh_browser_eval",
    label: "Evaluate in browser",
    description: "Evaluate a JavaScript expression in a browser pane and return its value.",
    parameters: browserEvalParams,
    async execute(_id, params, signal) {
      return jsonResult(
        await hh(["browser", "eval", params.expression, "--pane", params.pane_id], signal),
      );
    },
  });

  const browserClickParams = z.object({ pane_id: z.string(), selector: z.string() });
  pi.registerTool<typeof browserClickParams>({
    name: "hh_browser_click",
    label: "Click in browser",
    description: "Click the center of the element matching a CSS selector.",
    parameters: browserClickParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(
        await hh(["browser", "click", params.selector, "--pane", params.pane_id], signal),
      );
    },
  });

  const browserFillParams = z.object({ pane_id: z.string(), selector: z.string(), value: z.string() });
  pi.registerTool<typeof browserFillParams>({
    name: "hh_browser_fill",
    label: "Fill browser field",
    description: "Replace the value of the form control matching a CSS selector.",
    parameters: browserFillParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(
        await hh(
          ["browser", "fill", params.selector, params.value, "--pane", params.pane_id],
          signal,
        ),
      );
    },
  });

  const browserTypeParams = z.object({ pane_id: z.string(), text: z.string() });
  pi.registerTool<typeof browserTypeParams>({
    name: "hh_browser_type",
    label: "Type in browser",
    description: "Insert text at the browser focus.",
    parameters: browserTypeParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(await hh(["browser", "type", params.text, "--pane", params.pane_id], signal));
    },
  });

  const browserPressParams = z.object({ pane_id: z.string(), key: z.string() });
  pi.registerTool<typeof browserPressParams>({
    name: "hh_browser_press",
    label: "Press browser key",
    description: "Dispatch a key press such as Enter, Tab, Escape or ArrowDown in a browser pane.",
    parameters: browserPressParams,
    approval: "write",
    async execute(_id, params, signal) {
      return jsonResult(await hh(["browser", "press", params.key, "--pane", params.pane_id], signal));
    },
  });

  const browserScreenshotParams = z.object({ pane_id: z.string() });
  pi.registerTool<typeof browserScreenshotParams>({
    name: "hh_browser_screenshot",
    label: "Screenshot browser",
    description: "Capture a PNG screenshot of a browser pane.",
    parameters: browserScreenshotParams,
    approval: "read",
    async execute(_id, params, signal) {
      const directory = await fs.mkdtemp(path.join(os.tmpdir(), "hh-omp-"));
      const output = path.join(directory, "screenshot.png");
      try {
        await hh(["browser", "screenshot", "--out", output, "--pane", params.pane_id], signal);
        const data = (await fs.readFile(output)).toString("base64");
        return {
          content: [{ type: "image", data, mimeType: "image/png" }],
          details: { pane_id: params.pane_id },
        };
      } finally {
        await fs.rm(directory, { recursive: true, force: true });
      }
    },
  });

  // ------------------------------------------------------------ worker watcher

  // Only bot terminals carry HH_BOT_TAB_ID; `terminal list --mine` resolves the
  // bot from HH_PANE_ID.
  if (!process.env.HH_BOT_TAB_ID) return;

  /** Last confirmed category per worker pane; unset until the first poll. */
  let stable: Map<string, Category> | undefined;
  /** A category seen once that differs from `stable`, awaiting confirmation. */
  const pending = new Map<string, Category>();
  const outbox = new Map<string, WorkerEvent>();
  let lastSentAt = 0;
  let polling = false;
  let failing = false;
  let watching = false;

  async function screenTail(paneId: string): Promise<string> {
    try {
      const read = (await hh(["terminal", "read", paneId, "--lines", String(NOTIFY_TAIL_LINES)])) as {
        text?: string;
      };
      return read.text ?? "";
    } catch {
      return "";
    }
  }

  async function flush(): Promise<void> {
    if (outbox.size === 0 || Date.now() - lastSentAt < NOTIFY_COOLDOWN_MS) return;
    const events = [...outbox.values()];
    outbox.clear();
    lastSentAt = Date.now();
    const sections = await Promise.all(
      events.map(async (event) => {
        const tail = await screenTail(event.paneId);
        const heading = `Worker "${event.worker}" in ${event.workstation} ${describe(event)} (pane_id ${event.paneId}).`;
        return tail ? `${heading}\nScreen tail:\n\`\`\`\n${tail}\n\`\`\`` : heading;
      }),
    );
    const needsYou = events.some((event) => event.category === "needs-you");
    const instruction = needsYou
      ? "Tell the user concisely what the worker is asking and which options it offers, then relay their answer with hh_terminal_send. Do not answer on the user's behalf."
      : "Tell the user briefly.";
    pi.sendMessage(
      {
        customType: "harness-harlot.worker-update",
        content: `[Harness Harlot worker update]\n\n${sections.join("\n\n")}\n\n${instruction}`,
        display: true,
        attribution: "agent",
        details: { events },
      },
      { deliverAs: "followUp", triggerTurn: true },
    );
  }

  async function poll(): Promise<void> {
    if (polling) return;
    polling = true;
    try {
      const workstations = (await hh(["terminal", "list", "--mine"])) as Workstation[];
      failing = false;
      const seen = new Map<string, { pane: WorkerPane; worker: string; workstation: string }>();
      for (const workstation of workstations) {
        for (const tab of workstation.tabs) {
          for (const pane of tab.panes) {
            seen.set(pane.pane_id, { pane, worker: tab.title, workstation: workstation.title });
          }
        }
      }
      if (stable === undefined) {
        // Baseline: report only transitions that happen while watching.
        stable = new Map([...seen].map(([id, entry]) => [id, categoryOf(entry.pane)]));
        return;
      }
      for (const id of [...stable.keys()]) {
        if (!seen.has(id)) {
          stable.delete(id);
          pending.delete(id);
          outbox.delete(id);
        }
      }
      for (const [id, { pane, worker, workstation }] of seen) {
        const category = categoryOf(pane);
        const confirmed = stable.get(id) ?? "busy";
        if (category === confirmed) {
          pending.delete(id);
          if (!stable.has(id)) stable.set(id, category);
          continue;
        }
        // Require two consecutive samples so a flickering status stays quiet.
        if (pending.get(id) !== category) {
          pending.set(id, category);
          continue;
        }
        pending.delete(id);
        stable.set(id, category);
        if (category === "busy") {
          outbox.delete(id);
        } else {
          outbox.set(id, { paneId: id, worker, workstation, category, status: pane.status });
        }
      }
    } catch (error) {
      if (!failing) {
        failing = true;
        pi.logger.warn("Harness Harlot worker watcher failed", { error: String(error) });
      }
    } finally {
      polling = false;
    }
    await flush();
  }

  pi.on("session_start", async (_event, ctx: ExtensionContext) => {
    if (watching) return;
    watching = true;
    ctx.setInterval(() => poll(), WATCH_INTERVAL_MS);
  });

  pi.on("session_shutdown", async () => {
    watching = false;
  });
}
