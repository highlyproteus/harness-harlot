---
name: harness-harlot
description: Control Harness Harlot browser panes and publish images to the workstation gallery.
---

# Harness Harlot

Use the `hh` command already available in Harness Harlot terminals. The terminal exports `HH_WORKSPACE_ID`, `HH_PANE_ID`, `HH_GALLERY_DIR`, and `HH_CLI`; do not invent IDs or paths.

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

## MCP

Run `hh mcp` as a stdio MCP server when the agent supports MCP. The server exposes the same browser and gallery operations and inherits the terminal context.
