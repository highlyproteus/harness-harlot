---
name: harness-harlot
description: Coordinate worker terminals in Harness Harlot, relay their questions to the user, drive browser panes, and publish images to the workstation gallery.
---

# Harness Harlot

Use the `hh` command available in every Harness Harlot terminal (`$HH_CLI` is its absolute path). Each terminal exports `HH_WORKSPACE_ID`, `HH_PANE_ID`, `HH_GALLERY_DIR` and `HH_CLI`. Never invent IDs or paths: take them from command output. Add `--json` to any command for machine-readable output. Agents with MCP support can run `hh mcp` as a stdio server, which exposes every command below as a tool with the same arguments (`terminal_list`, `terminal_new`, `terminal_send`, `terminal_read`, `terminal_wait`, `terminal_focus`, `terminal_close`, `terminal_rename`, `workstation_new`, `browser_*` and `gallery_*`).

## Coordinator workflow (bots)

A bot talks with the user and delegates the actual work to worker terminals. Do not do substantial work in your own terminal.

1. **Delegate.** Open one named worker per task with a coding agent and the task as its prompt:
   `hh terminal new --title "api: add pagination" --cwd /path/to/worktree --command 'omp "Add cursor pagination to /users"' --json`
   It returns `workstation_id`, `tab_id` and `pane_id`. A bot's workers go to the bot's own workstation unless you pass `--workstation ID`. Use a separate worktree or directory per worker when the user asks for parallel work, e.g. create it first with `git worktree add` in a worker, or give each worker its own `--cwd`.
2. **Monitor.** `hh terminal list --mine --json` shows your workers with live `status` (`working`, `needs_approval`, `needs_input`, `attention`, `done`, `idle`), `status_changed_at_ms` and `exited`. `hh terminal wait PANE --until needs-you --json` blocks until a worker needs attention. In omp, the Harness Harlot extension also watches your workers and tells you when one needs you or finishes.
3. **Tell the user.** When a worker needs input or approval, read its screen (`hh terminal read PANE --lines 40 --json`) and tell the user concisely what it asks and which options it offers. Do not decide on the user's behalf.
4. **Relay.** Send the user's answer exactly: `hh terminal send PANE --text "2" --enter`, or keys such as `--key down --key enter`. Confirm with `hh terminal read` that the worker continued.
5. **Report.** When asked for progress, summarize each worker from `terminal list` and `terminal read`. Use `hh terminal focus PANE` to show the user a worker's tab.

## Terminal commands

- `hh terminal list [--mine]`: workstations → tabs → terminal panes with `pane_id`, `tab_id`, titles, `profile`, `status`, `status_changed_at_ms`, `exited`, `cwd` and `owner_bot`. The Bots workspace is never listed.
- `hh terminal new [--workstation ID] [--cwd DIR] [--title T] [--command CMD]`: opens a worker tab. The command is typed into the new shell once it starts.
- `hh terminal send PANE [--text T] [--key K]... [--enter]`: writes text, then each key, then Enter. Keys: `enter`, `ctrl-c`, `ctrl-d`, `escape`, `tab`, `shift-tab`, `up`, `down`, `left`, `right`, `backspace`, `space`. Text alone is not submitted; add `--enter`.
- `hh terminal read PANE [--lines N]`: the visible screen text (optionally only the last N lines), status and exit state.
- `hh terminal wait PANE [--until needs-you|done|idle|exited|any] [--pattern REGEX] [--timeout-ms MS]`: polls until the condition holds and returns `reason` (`needs-you`, `done`, `idle`, `exited`, `pattern`, `closed` or `timeout`), `status` and the screen `tail`. Without `--until`, it waits for any condition, or only for `--pattern` when one is given. An exit always ends the wait. The default timeout is 120000 ms (at most 600000).
- `hh terminal focus PANE`: shows the pane's tab to the user.
- `hh terminal close PANE`: closes the pane and ends its process. Only close workers the user no longer needs.
- `hh terminal rename TAB TITLE`: renames a tab.
- `hh workstation new --cwd DIR [--title T]`: creates a workstation rooted at an existing directory.

## Browser

- List browser panes: `hh browser list --json`
- Open a browser beside the current terminal: `hh browser open [URL] --json`
- Navigate: `hh browser goto URL --pane PANE_ID --json`
- Read visible text: `hh browser read [SELECTOR] --pane PANE_ID --json`
- Evaluate JavaScript: `hh browser eval EXPRESSION --pane PANE_ID --json`
- Click: `hh browser click SELECTOR --pane PANE_ID --json`
- Fill a form control: `hh browser fill SELECTOR VALUE --pane PANE_ID --json`
- Type into the focused control: `hh browser type TEXT --pane PANE_ID --json`
- Press a key: `hh browser press KEY --pane PANE_ID --json`
- Capture and publish a screenshot: `hh browser screenshot --pane PANE_ID --json`
- Send a raw CDP command: `hh browser cdp METHOD [PARAMS_JSON] --pane PANE_ID --json`

Browser commands target a browser pane, not the terminal pane. After opening a browser, use the returned `pane_id` for later commands. Prefer semantic selectors and `browser read`; use `browser eval` or raw CDP only when the higher-level commands cannot express the operation.

## Gallery

- Publish an image: `hh gallery add /absolute/path/to/image --json`
- List gallery images: `hh gallery list --json`
- Print the gallery directory: `hh gallery dir`

Publishing an image creates or reuses the workstation gallery without stealing focus from the current pane. Supported image formats are PNG, JPEG, GIF, and WebP.
