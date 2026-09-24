import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import * as net from "node:net";

const MAX_FRAME_BYTES = 4 * 1024 * 1024;
const MAX_WRITE_BYTES = 64 * 1024;
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const encoder = new TextEncoder();

type JsonObject = Record<string, unknown>;

type PaneLocation = {
  workspace: JsonObject;
  tab: JsonObject;
  pane: JsonObject;
};

type ToolResult = {
  content: Array<
    | { type: "text"; text: string }
    | { type: "image"; data: string; mimeType: string }
  >;
  details?: JsonObject;
};

function textResult(text: string, paneId?: string): ToolResult {
  return {
    content: [{ type: "text", text }],
    ...(paneId ? { details: { pane_id: paneId } } : {}),
  };
}

function requiredEnv(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set`);
  return value;
}

function uuid(value: string): string {
  if (!UUID_RE.test(value)) throw new Error(`unknown pane id ${value}`);
  return value;
}

function requestFrame(value: unknown): Buffer {
  const payload = Buffer.from(JSON.stringify(value), "utf8");
  if (payload.length > MAX_FRAME_BYTES) throw new Error("HH request frame is too large");
  const frame = Buffer.allocUnsafe(payload.length + 4);
  frame.writeUInt32BE(payload.length, 0);
  payload.copy(frame, 4);
  return frame;
}

class HhClient {
  private socket: net.Socket | undefined;
  private buffered = Buffer.alloc(0);
  private frameWaiters: Array<{
    resolve: (value: JsonObject) => void;
    reject: (error: Error) => void;
  }> = [];
  private tail: Promise<void> = Promise.resolve();

  call(request: JsonObject): Promise<JsonObject> {
    const result = this.tail.then(() => this.callSerial(request));
    this.tail = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }

  private async callSerial(request: JsonObject): Promise<JsonObject> {
    const socket = await this.connect();
    try {
      socket.write(requestFrame(request));
      const response = await this.readFrame();
      const type = response.type;
      if (type === "error" || type === "delivery_error") {
        throw new Error(String(response.message ?? "Harness Harlot request failed"));
      }
      return response;
    } catch (error) {
      this.disconnect(error instanceof Error ? error : new Error(String(error)));
      throw error;
    }
  }

  private async connect(): Promise<net.Socket> {
    if (this.socket && !this.socket.destroyed) return this.socket;
    const socketPath = requiredEnv("HH_SOCKET");
    const socket = net.createConnection(socketPath);
    this.socket = socket;
    this.buffered = Buffer.alloc(0);
    socket.on("data", (chunk: Buffer) => {
      this.buffered = Buffer.concat([this.buffered, chunk]);
      this.deliverFrames();
    });
    socket.on("error", (error) => this.disconnect(error));
    socket.on("close", () => this.disconnect(new Error("Harness Harlot socket closed")));
    const connected = Promise.withResolvers<void>();
    socket.once("connect", connected.resolve);
    socket.once("error", connected.reject);
    await connected.promise;
    socket.write(
      requestFrame({
        type: "hello",
        protocol_version: Number(requiredEnv("HH_PROTOCOL_VERSION")),
      }),
    );
    const hello = await this.readFrame();
    if (hello.type !== "hello") {
      this.disconnect(new Error(String(hello.message ?? "HH handshake failed")));
      throw new Error(String(hello.message ?? "HH handshake failed"));
    }
    return socket;
  }

  private readFrame(): Promise<JsonObject> {
    const frame = Promise.withResolvers<JsonObject>();
    this.frameWaiters.push({ resolve: frame.resolve, reject: frame.reject });
    this.deliverFrames();
    return frame.promise;
  }

  private deliverFrames(): void {
    while (this.frameWaiters.length > 0 && this.buffered.length >= 4) {
      const size = this.buffered.readUInt32BE(0);
      if (size > MAX_FRAME_BYTES) {
        this.disconnect(new Error("HH sent an oversized frame"));
        return;
      }
      if (this.buffered.length < size + 4) return;
      const payload = this.buffered.subarray(4, size + 4);
      this.buffered = this.buffered.subarray(size + 4);
      const waiter = this.frameWaiters.shift();
      if (!waiter) return;
      try {
        waiter.resolve(JSON.parse(payload.toString("utf8")) as JsonObject);
      } catch (error) {
        waiter.reject(new Error(`HH sent invalid JSON: ${String(error)}`));
      }
    }
  }

  private disconnect(error: Error): void {
    const socket = this.socket;
    this.socket = undefined;
    if (socket && !socket.destroyed) socket.destroy();
    this.buffered = Buffer.alloc(0);
    for (const waiter of this.frameWaiters.splice(0)) waiter.reject(error);
  }
}

const client = new HhClient();

async function snapshot(): Promise<JsonObject> {
  const response = await client.call({ type: "get_snapshot" });
  return object(response.snapshot, "snapshot");
}

function object(value: unknown, label: string): JsonObject {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`HH returned an invalid ${label}`);
  }
  return value as JsonObject;
}

function array(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function panesInLayout(layoutValue: unknown): JsonObject[] {
  const layout = object(layoutValue, "pane layout");
  switch (layout.kind) {
    case "leaf":
      return [object(layout.pane, "pane")];
    case "stack":
      return array(layout.panes).map((pane) => object(pane, "pane"));
    case "split":
      return [...panesInLayout(layout.first), ...panesInLayout(layout.second)];
    default:
      throw new Error("HH returned an unknown pane layout");
  }
}

function workspacesOf(state: JsonObject): JsonObject[] {
  return array(state.workspaces).map((workspace) => object(workspace, "workstation"));
}

function tabsOf(workspace: JsonObject): JsonObject[] {
  return array(workspace.tabs).map((tab) => object(tab, "window"));
}

function findPane(state: JsonObject, paneId: string): PaneLocation {
  uuid(paneId);
  for (const workspace of workspacesOf(state)) {
    for (const tab of tabsOf(workspace)) {
      const pane = panesInLayout(tab.layout).find((candidate) => candidate.id === paneId);
      if (pane) return { workspace, tab, pane };
    }
  }
  throw new Error(`unknown pane id ${paneId}`);
}

function findWorkspace(state: JsonObject, workspaceId: string): JsonObject {
  if (!UUID_RE.test(workspaceId)) throw new Error(`unknown workstation id ${workspaceId}`);
  const workspace = workspacesOf(state).find((candidate) => candidate.id === workspaceId);
  if (!workspace) throw new Error(`unknown workstation id ${workspaceId}`);
  return workspace;
}

function findTab(workspace: JsonObject, tabId: string): JsonObject {
  if (!UUID_RE.test(tabId)) throw new Error(`unknown window id ${tabId}`);
  const tab = tabsOf(workspace).find((candidate) => candidate.id === tabId);
  if (!tab) throw new Error(`unknown window id ${tabId}`);
  return tab;
}

function firstTerminalPaneOfTab(tab: JsonObject): JsonObject {
  const pane = panesInLayout(tab.layout).find(
    (candidate) => object(candidate.kind, "pane kind").type === "terminal",
  );
  if (!pane) throw new Error(`window ${String(tab.id)} has no terminal pane`);
  return pane;
}

async function paneStates(): Promise<Map<string, JsonObject>> {
  const response = await client.call({
    type: "get_updates",
    snapshot_revision: null,
    pane_revisions: [],
    assistant_revisions: [],
    subscribed_panes: [],
    notifications_after: 0,
    browser_executor: false,
  });
  return new Map(
    array(response.pane_states).map((value) => {
      const state = object(value, "pane state");
      return [String(state.pane_id), state];
    }),
  );
}

async function screenText(paneId: string): Promise<string> {
  uuid(paneId);
  const response = await client.call({ type: "get_pane_snapshot", pane_id: paneId });
  return array(object(response.screen, "screen").lines)
    .map((line) =>
      array(object(line, "line").runs)
        .map((run) => String(object(run, "terminal run").text ?? ""))
        .join(""),
    )
    .join("\n")
    .replace(/\s+$/, "");
}

async function paneSummary(paneId: string): Promise<{ title: string; exited: boolean; text: string }> {
  const state = await snapshot();
  const location = findPane(state, paneId);
  const states = await paneStates();
  return {
    title: String(location.pane.title ?? ""),
    exited: states.get(paneId)?.exited === true,
    text: await screenText(paneId),
  };
}

function responsePaneId(response: JsonObject): string {
  return uuid(String(response.pane_id ?? ""));
}

function responseWorkspaceId(response: JsonObject): string {
  const value = String(response.workspace_id ?? "");
  if (!UUID_RE.test(value)) throw new Error(`unknown workstation id ${value}`);
  return value;
}

function bytesForSend(params: {
  text?: string;
  keys?: string[];
  enter?: boolean;
}): Uint8Array {
  const chunks: Uint8Array[] = [];
  if (params.text !== undefined) chunks.push(encoder.encode(params.text));
  const keys: Record<string, Uint8Array> = {
    enter: Uint8Array.of(13),
    "ctrl-c": Uint8Array.of(3),
    "ctrl-d": Uint8Array.of(4),
    escape: Uint8Array.of(27),
    tab: Uint8Array.of(9),
    up: Uint8Array.of(27, 91, 65),
    down: Uint8Array.of(27, 91, 66),
  };
  for (const key of params.keys ?? []) {
    const bytes = keys[key];
    if (!bytes) throw new Error(`unknown terminal key ${key}`);
    chunks.push(bytes);
  }
  if (params.enter ?? params.text !== undefined) chunks.push(Uint8Array.of(13));
  const length = chunks.reduce((total, chunk) => total + chunk.length, 0);
  if (length > MAX_WRITE_BYTES) throw new Error(`input exceeds ${MAX_WRITE_BYTES} bytes`);
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  return bytes;
}

async function browserCommand(paneId: string, action: JsonObject): Promise<unknown> {
  uuid(paneId);
  const response = await client.call({
    type: "browser_command",
    pane_id: paneId,
    action,
  });
  const outcome = object(response.outcome, "browser command outcome");
  if (outcome.type === "error") {
    throw new Error(String(outcome.message ?? "browser command failed"));
  }
  if (outcome.type !== "ok") throw new Error("HH returned an invalid browser command outcome");
  return outcome.result;
}

function cdp(paneId: string, method: string, params: JsonObject = {}): Promise<unknown> {
  return browserCommand(paneId, { type: "dev_tools", method, params });
}

function runtimeEvaluation(resultValue: unknown): JsonObject {
  return object(resultValue, "Runtime.evaluate result");
}

async function browserEval(paneId: string, expression: string): Promise<JsonObject> {
  return runtimeEvaluation(
    await cdp(paneId, "Runtime.evaluate", {
      expression,
      returnByValue: true,
      awaitPromise: true,
    }),
  );
}

async function browserClick(paneId: string, selector: string): Promise<void> {
  const selected = JSON.stringify(selector);
  const evaluated = await browserEval(
    paneId,
    `(() => { const e = document.querySelector(${selected}); if (!e) return null; e.scrollIntoView({block:"center",inline:"center"}); const r = e.getBoundingClientRect(); return {x: r.left + r.width/2, y: r.top + r.height/2}; })()`,
  );
  const value = object(evaluated.result, "Runtime.evaluate value").value;
  if (value === null) throw new Error(`no element matches ${selector}`);
  const point = object(value, "element center");
  const x = Number(point.x);
  const y = Number(point.y);
  if (!Number.isFinite(x) || !Number.isFinite(y)) throw new Error(`no element matches ${selector}`);
  await cdp(paneId, "Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
  await cdp(paneId, "Input.dispatchMouseEvent", {
    type: "mousePressed",
    x,
    y,
    button: "left",
    clickCount: 1,
  });
  await cdp(paneId, "Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x,
    y,
    button: "left",
    clickCount: 1,
  });
}

async function browserFill(paneId: string, selector: string, text: string): Promise<void> {
  const selected = JSON.stringify(selector);
  const evaluated = await browserEval(
    paneId,
    `(() => { const e = document.querySelector(${selected}); if (!e) return false; e.focus(); if ("value" in e) { e.value = ""; e.dispatchEvent(new Event("input", {bubbles:true})); } return true; })()`,
  );
  if (object(evaluated.result, "Runtime.evaluate value").value !== true) {
    throw new Error(`no element matches ${selector}`);
  }
  await cdp(paneId, "Input.insertText", { text });
}

function browserKey(key: string): JsonObject {
  const keys: Record<string, JsonObject> = {
    Enter: { key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" },
    Tab: { key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 },
    Escape: { key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 },
    Backspace: { key: "Backspace", code: "Backspace", windowsVirtualKeyCode: 8 },
    ArrowUp: { key: "ArrowUp", code: "ArrowUp", windowsVirtualKeyCode: 38 },
    ArrowDown: { key: "ArrowDown", code: "ArrowDown", windowsVirtualKeyCode: 40 },
    ArrowLeft: { key: "ArrowLeft", code: "ArrowLeft", windowsVirtualKeyCode: 37 },
    ArrowRight: { key: "ArrowRight", code: "ArrowRight", windowsVirtualKeyCode: 39 },
    Space: { key: " ", code: "Space", windowsVirtualKeyCode: 32, text: " " },
  };
  const value = keys[key];
  if (!value) throw new Error("unsupported key");
  return value;
}

async function browserPress(paneId: string, key: string): Promise<void> {
  const payload = browserKey(key);
  await cdp(paneId, "Input.dispatchKeyEvent", { type: "keyDown", ...payload });
  await cdp(paneId, "Input.dispatchKeyEvent", { type: "keyUp", ...payload });
}

function sleep(milliseconds: number, signal?: AbortSignal): Promise<void> {
  const delay = Promise.withResolvers<void>();
  if (signal?.aborted) {
    delay.reject(new Error("aborted"));
    return delay.promise;
  }
  const timer = setTimeout(delay.resolve, milliseconds);
  signal?.addEventListener(
    "abort",
    () => {
      clearTimeout(timer);
      delay.reject(new Error("aborted"));
    },
    { once: true },
  );
  return delay.promise;
}

export default function hhOrchestrator(pi: ExtensionAPI): void {
  pi.on("tool_call", async (event, ctx) => {
    let description: string | undefined;
    if (event.toolName === "hh_send") {
      description = String(event.input.text ?? event.input.pane_id ?? "send terminal input");
    } else if (event.toolName === "hh_close") {
      description = String(event.input.pane_id ?? "close pane");
    } else if (event.toolName === "hh_terminal_new" && event.input.command) {
      description = String(event.input.command);
    } else if (
      [
        "hh_browser_eval",
        "hh_browser_click",
        "hh_browser_fill",
        "hh_browser_press",
        "hh_browser_goto",
      ].includes(event.toolName)
    ) {
      description = String(
        event.input.url ??
          event.input.selector ??
          event.input.expression ??
          event.input.pane_id ??
          "browser action",
      );
    }
    if (description !== undefined) {
      const ok = await ctx.ui.confirm(`Allow ${event.toolName}?`, description.replace(/\s+/g, " "));
      if (!ok) return { block: true, reason: "Denied by user" };
    }
    return undefined;
  });

  pi.registerTool({
    name: "hh_list",
    label: "List Harness Harlot",
    description: "List non-assistant workstations, their windows, panes, browser URLs, and exit state.",
    parameters: Type.Object({}),
    async execute() {
      const state = await snapshot();
      const states = await paneStates();
      const result = workspacesOf(state)
        .filter((workspace) => workspace.kind !== "assistant")
        .map((workspace) => ({
          workstation_id: workspace.id,
          title: workspace.title,
          working_dir: workspace.working_dir ?? null,
          connection: workspace.connection,
          windows: tabsOf(workspace).map((tab) => ({
            window_id: tab.id,
            title: tab.title,
            parent_window: tab.parent_tab ?? null,
            panes: panesInLayout(tab.layout).map((pane) => {
              const kind = object(pane.kind, "pane kind");
              return {
                pane_id: pane.id,
                title: pane.title,
                kind: kind.type,
                ...(kind.type === "browser" ? { url: kind.url } : {}),
                exited: states.get(String(pane.id))?.exited === true,
              };
            }),
          })),
        }));
      return textResult(JSON.stringify(result, null, 2));
    },
  });

  pi.registerTool({
    name: "hh_workstation_new",
    label: "New workstation",
    description: "Create a workstation and set its local working directory.",
    parameters: Type.Object({
      working_dir: Type.String(),
      title: Type.Optional(Type.String()),
    }),
    async execute(_toolCallId, params) {
      const created = await client.call({ type: "create_workspace", title: params.title ?? null });
      const workstationId = responseWorkspaceId(created);
      const paneId = responsePaneId(created);
      try {
        await client.call({
          type: "set_workspace_working_dir",
          workspace_id: workstationId,
          working_dir: params.working_dir,
        });
      } catch (error) {
        await client.call({ type: "delete_workspace", workspace_id: workstationId }).catch(() => undefined);
        throw error;
      }
      return textResult(
        JSON.stringify({ workstation_id: workstationId, pane_id: paneId }),
        paneId,
      );
    },
  });

  pi.registerTool({
    name: "hh_window_new",
    label: "New window",
    description: "Create a window in a workstation and give it the requested title.",
    parameters: Type.Object({
      workstation_id: Type.String(),
      title: Type.String(),
    }),
    async execute(_toolCallId, params) {
      const state = await snapshot();
      findWorkspace(state, params.workstation_id);
      const created = await client.call({
        type: "create_workspace_group",
        workspace_id: params.workstation_id,
        parent_tab: null,
      });
      const paneId = responsePaneId(created);
      const updated = await snapshot();
      const location = findPane(updated, paneId);
      const windowId = String(location.tab.id);
      await client.call({ type: "rename_tab", tab_id: windowId, title: params.title });
      return textResult(JSON.stringify({ window_id: windowId, pane_id: paneId }), paneId);
    },
  });

  pi.registerTool({
    name: "hh_terminal_new",
    label: "New terminal",
    description: "Create a terminal in a workstation or window, optionally name it and run a command.",
    parameters: Type.Object({
      workstation_id: Type.String(),
      window_id: Type.Optional(Type.String()),
      title: Type.Optional(Type.String()),
      command: Type.Optional(Type.String()),
    }),
    async execute(_toolCallId, params) {
      const state = await snapshot();
      const workspace = findWorkspace(state, params.workstation_id);
      let created: JsonObject;
      if (params.window_id) {
        const tab = findTab(workspace, params.window_id);
        const target = firstTerminalPaneOfTab(tab);
        created = await client.call({ type: "create_group_terminal", target_pane: target.id });
      } else {
        created = await client.call({
          type: "create_workspace_tab",
          workspace_id: params.workstation_id,
        });
      }
      const paneId = responsePaneId(created);
      if (params.title) {
        await client.call({ type: "rename_pane", pane_id: paneId, title: params.title });
      }
      if (params.command) {
        await sleep(300);
        const bytes = encoder.encode(`${params.command}\n`);
        if (bytes.length > MAX_WRITE_BYTES) throw new Error(`command exceeds ${MAX_WRITE_BYTES} bytes`);
        await client.call({ type: "write_input", pane_id: paneId, bytes: [...bytes] });
      }
      const updated = await snapshot();
      const location = findPane(updated, paneId);
      return textResult(
        JSON.stringify({ pane_id: paneId, window_id: location.tab.id }),
        paneId,
      );
    },
  });

  pi.registerTool({
    name: "hh_browser_new",
    label: "New browser",
    description: "Create a browser pane in a workstation or an existing window at the requested URL.",
    parameters: Type.Object({
      workstation_id: Type.String(),
      window_id: Type.Optional(Type.String()),
      url: Type.String(),
    }),
    async execute(_toolCallId, params) {
      const state = await snapshot();
      const workspace = findWorkspace(state, params.workstation_id);
      const created = params.window_id
        ? await client.call({
            type: "create_group_browser",
            target_pane: firstTerminalPaneOfTab(findTab(workspace, params.window_id)).id,
            url: params.url,
          })
        : await client.call({
            type: "create_browser_tab",
            workspace_id: params.workstation_id,
            url: params.url,
          });
      const paneId = responsePaneId(created);
      return textResult(JSON.stringify({ pane_id: paneId }), paneId);
    },
  });

  pi.registerTool({
    name: "hh_browser_goto",
    label: "Navigate browser",
    description: "Navigate an existing Harness Harlot browser pane.",
    parameters: Type.Object({ pane_id: Type.String(), url: Type.String() }),
    async execute(_toolCallId, params) {
      await browserCommand(params.pane_id, { type: "navigate", url: params.url });
      return textResult(`navigated ${params.pane_id} to ${params.url}`, params.pane_id);
    },
  });

  pi.registerTool({
    name: "hh_browser_read",
    label: "Read browser",
    description: "Read the visible page text from a Harness Harlot browser pane.",
    parameters: Type.Object({ pane_id: Type.String() }),
    async execute(_toolCallId, params) {
      const evaluated = await browserEval(params.pane_id, "document.body.innerText");
      const value = String(object(evaluated.result, "Runtime.evaluate value").value ?? "");
      const text = value.length > 100_000 ? `${value.slice(0, 100_000)}\n… truncated` : value;
      return textResult(text, params.pane_id);
    },
  });

  pi.registerTool({
    name: "hh_browser_eval",
    label: "Evaluate in browser",
    description: "Evaluate JavaScript in a Harness Harlot browser pane.",
    parameters: Type.Object({ pane_id: Type.String(), expression: Type.String() }),
    async execute(_toolCallId, params) {
      const evaluated = await browserEval(params.pane_id, params.expression);
      if (evaluated.exceptionDetails) {
        const details = object(evaluated.exceptionDetails, "JavaScript exception");
        const exception =
          details.exception && typeof details.exception === "object"
            ? object(details.exception, "JavaScript exception value")
            : undefined;
        throw new Error(String(exception?.description ?? details.text ?? "JavaScript evaluation failed"));
      }
      return textResult(
        JSON.stringify(object(evaluated.result, "Runtime.evaluate value").value ?? null),
        params.pane_id,
      );
    },
  });

  pi.registerTool({
    name: "hh_browser_screenshot",
    label: "Screenshot browser",
    description: "Capture a PNG screenshot from a Harness Harlot browser pane.",
    parameters: Type.Object({ pane_id: Type.String() }),
    async execute(_toolCallId, params) {
      const result = object(
        await cdp(params.pane_id, "Page.captureScreenshot", { format: "png" }),
        "screenshot result",
      );
      const data = String(result.data ?? "");
      if (!data) throw new Error("screenshot failed");
      return {
        content: [{ type: "image" as const, data, mimeType: "image/png" }],
        details: { pane_id: params.pane_id },
      };
    },
  });

  pi.registerTool({
    name: "hh_browser_click",
    label: "Click browser element",
    description: "Click an element selected by CSS in a Harness Harlot browser pane.",
    parameters: Type.Object({ pane_id: Type.String(), selector: Type.String() }),
    async execute(_toolCallId, params) {
      await browserClick(params.pane_id, params.selector);
      return textResult(`clicked ${params.selector}`, params.pane_id);
    },
  });

  pi.registerTool({
    name: "hh_browser_fill",
    label: "Fill browser field",
    description: "Clear and fill an element selected by CSS in a Harness Harlot browser pane.",
    parameters: Type.Object({
      pane_id: Type.String(),
      selector: Type.String(),
      text: Type.String(),
    }),
    async execute(_toolCallId, params) {
      await browserFill(params.pane_id, params.selector, params.text);
      return textResult(`filled ${params.selector}`, params.pane_id);
    },
  });

  pi.registerTool({
    name: "hh_browser_press",
    label: "Press browser key",
    description: "Press a supported key in a Harness Harlot browser pane.",
    parameters: Type.Object({ pane_id: Type.String(), key: Type.String() }),
    async execute(_toolCallId, params) {
      await browserPress(params.pane_id, params.key);
      return textResult(`pressed ${params.key}`, params.pane_id);
    },
  });

  pi.registerTool({
    name: "hh_read",
    label: "Read pane",
    description: "Read a terminal pane's visible screen, title, and exit state.",
    parameters: Type.Object({ pane_id: Type.String() }),
    async execute(_toolCallId, params) {
      uuid(params.pane_id);
      const pane = await paneSummary(params.pane_id);
      return textResult(
        `title:${pane.title}\nexited:${pane.exited}\n---\n${pane.text}`,
        params.pane_id,
      );
    },
  });

  pi.registerTool({
    name: "hh_send",
    label: "Send to pane",
    description: "Send text and terminal keys to a terminal pane.",
    parameters: Type.Object({
      pane_id: Type.String(),
      text: Type.Optional(Type.String()),
      keys: Type.Optional(
        Type.Array(
          Type.Union([
            Type.Literal("enter"),
            Type.Literal("ctrl-c"),
            Type.Literal("ctrl-d"),
            Type.Literal("escape"),
            Type.Literal("tab"),
            Type.Literal("up"),
            Type.Literal("down"),
          ]),
        ),
      ),
      enter: Type.Optional(Type.Boolean()),
    }),
    async execute(_toolCallId, params) {
      uuid(params.pane_id);
      const bytes = bytesForSend(params);
      await client.call({ type: "write_input", pane_id: params.pane_id, bytes: [...bytes] });
      return textResult(`sent ${bytes.length} bytes`, params.pane_id);
    },
  });

  pi.registerTool({
    name: "hh_wait",
    label: "Wait for pane",
    description: "Wait until a pane contains literal text, becomes idle, exits, or reaches a timeout.",
    parameters: Type.Object({
      pane_id: Type.String(),
      pattern: Type.Optional(Type.String({ maxLength: 4096 })),
      idle_ms: Type.Optional(Type.Number()),
      timeout_ms: Type.Optional(Type.Number()),
    }),
    async execute(_toolCallId, params, signal, onUpdate) {
      uuid(params.pane_id);
      const pattern = params.pattern;
      const idleMs = Math.min(Math.max(params.idle_ms ?? 3000, 0), 60000);
      const timeoutMs = Math.min(Math.max(params.timeout_ms ?? 120000, 0), 600000);
      const started = Date.now();
      let lastChanged = started;
      let lastUpdate = started;
      let previous: string | undefined;
      while (true) {
        if (signal?.aborted) throw new Error("aborted");
        const pane = await paneSummary(params.pane_id);
        const now = Date.now();
        if (pane.text !== previous) {
          previous = pane.text;
          lastChanged = now;
        }
        let reason: "matched" | "idle" | "exited" | "timeout" | undefined;
        if (pattern !== undefined && pane.text.includes(pattern)) reason = "matched";
        else if (pane.exited) reason = "exited";
        else if (now - lastChanged >= idleMs) reason = "idle";
        else if (now - started >= timeoutMs) reason = "timeout";
        if (reason) return textResult(`reason: ${reason}\n---\n${pane.text}`, params.pane_id);
        if (onUpdate && now - lastUpdate >= 5000) {
          lastUpdate = now;
          onUpdate(textResult(pane.text.split("\n").slice(-20).join("\n"), params.pane_id));
        }
        await sleep(500, signal);
      }
    },
  });

  pi.registerTool({
    name: "hh_close",
    label: "Close pane",
    description: "Close a pane and terminate its owned process when applicable.",
    parameters: Type.Object({ pane_id: Type.String() }),
    async execute(_toolCallId, params) {
      uuid(params.pane_id);
      await client.call({ type: "close_pane", pane_id: params.pane_id });
      return textResult("closed", params.pane_id);
    },
  });

  pi.registerTool({
    name: "hh_focus",
    label: "Focus pane",
    description: "Activate the window and stack entry containing a pane.",
    parameters: Type.Object({ pane_id: Type.String() }),
    async execute(_toolCallId, params) {
      uuid(params.pane_id);
      await client.call({ type: "activate_tab", pane_id: params.pane_id });
      return textResult("focused", params.pane_id);
    },
  });
}
