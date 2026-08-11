# Rust Mux

Rust Mux is a lightweight native terminal workspace for local and SSH work on macOS and Linux. It is not an agent harness or runtime: terminals and agents run as ordinary shell workloads while Rust Mux stays out of the way and avoids competing for their CPU or memory. It takes behavior-level inspiration from cmux's workspace, sidebar, tab, split-pane, and freely rearrangeable pane experience while deliberately separating session lifetime from desktop UI lifetime.

The repository now contains runnable local terminals plus a thin system-SSH checkpoint: the GPUI client opens the user's configured shell, sends keyboard and mouse input to service-owned PTYs, renders ANSI SGR color and style from the Alacritty grid, supports selection/copy/paste, bounded scrollback and literal search, resizes PTYs, and creates pane-local tabs and splits. The daemon persists a restricted local desired-state snapshot for fresh-shell recovery, while an explicit two-step SSH action can launch the installed OpenSSH client in the same kind of daemon-owned PTY. Unicode shaping and broader platform soak remain roadmap work.

## Architecture

```text
restartable desktop UI
        |
        | local versioned JSON protocol over a Unix socket
        v
persistent session service
        |
        +-- workspace/tab/pane state
        +-- PTY and child-process ownership
        +-- canonical live workspace/layout state
        +-- Alacritty terminal grids and bounded scrollback
```

The Rust workspace keeps those responsibilities explicit:

- `rust-mux-desktop`: restartable GPUI client with the compact workspace rail, pane-local human-named tabs, real themed terminal surfaces, keyboard focus/input, pane-targeted controls, divider resize, and drag-to-split movement.
- `rust-mux-session-service`: long-lived local authority for PTYs, configured-shell processes, terminal state, and live layouts.
- `rust-mux-protocol`: versioned transport messages and shared layout types.
- `rust-mux-terminal-model`: a narrow adapter around Alacritty's established terminal engine. Rust Mux will not implement VT parsing from scratch.

The client is only a projection of daemon state. Closing it does not stop the service or its PTYs; a new client fetches the current layouts and terminal screens from the owner-only Unix socket. A daemon restart recreates fresh local shells from a restricted owner-only snapshot; it does not claim to preserve live processes.

### Performance contract

Rust Mux is designed to make idle panes nearly free: update only the panes whose output changes, parse and prepare updates away from the UI thread, keep terminal history bounded, and expose performance diagnostics without recording terminal contents. A focused or recently used pane stays responsive. After roughly 60 seconds without attention, its PTY still drains into bounded daemon-owned history so a local or remote process can never block, but live screen delivery to the desktop is coalesced until the pane is selected again. Selection requests one fresh snapshot before rendering and then resumes live deltas, so normal tab switching remains immediate and no output is lost.

This revision-aware delta-streaming policy is the next IPC/rendering milestone, not a claim about the current full-snapshot checkpoint.

For SSH, Rust Mux validates one conservative host or alias and launches structured argv equivalent to `ssh -- <host>`. It does not read SSH keys or config, probe hosts with `ssh -G`, add agent forwarding, change host-key policy, or answer prompts. The installed OpenSSH client remains the sole authority for `~/.ssh/config`, `Include`/`Match`, agents and identity files, known hosts, proxies, multiplexing, authentication, and host-key verification. SSH sessions are runtime-only in this checkpoint: their host intent is deliberately excluded from recovery snapshots, so restarting the daemon cannot initiate network access.

See [the terminal theme architecture](docs/terminal-theme.md) for the `Harbor Night` palette boundary and [the all-Rust renderer roadmap](docs/terminal-renderer-roadmap.md) for the hard no-libghostty decision and the measured typography/cell-rendering plan.

### Appearance colors

Harbor Night remains the built-in visual foundation, with two independent local defaults: a terminal accent for focus rails, active tabs, and cursor treatment, and a workspace color for the selected workspace in the sidebar. Appearance settings offer a restrained preset/recent-color picker. A terminal tab or workspace can override only its own color from its right-click menu, or return to its matching default. These choices persist in the owner-only desired-state file and never trigger network access or telemetry.

## MVP boundary

The first usable milestone includes:

- local workspaces, tabs, and split terminal panes;
- a persistent Rust service that owns PTYs and child processes;
- a restartable native desktop client;
- drag-to-rearrange panes and persisted layouts;
- terminal rendering/input through the Alacritty terminal engine adapter;
- configured-host SSH workspaces implemented by running system OpenSSH inside daemon-owned PTYs;
- crash/reconnect and session-lifecycle tests on macOS and Linux.

Explicitly deferred: embedded web browsing, mobile access, generic remote-control UI, worktree/diff review, and AI-specific integrations. SSH terminal workspaces are part of the early MVP; an optional remote `rmuxd` for durable reattachment comes later.

## Run the local terminal checkpoint

Requirements: Rust 1.96 or newer.

```bash
cargo run -p rust-mux-session-service
```

In another terminal:

```bash
cargo run -p rust-mux-desktop
```

Set `RUST_MUX_SOCKET` in both processes to override the default socket path. The service is intentionally foreground-only for now so lifecycle behavior is visible during development.

### Command bindings

Rust Mux loads optional JSON configuration from `RUST_MUX_CONFIG`, then
`$XDG_CONFIG_HOME/rust-mux/config.json`, then
`$HOME/.config/rust-mux/config.json`. Key sequences use GPUI syntax; multiple
space-separated strokes form a chord. A configured action replaces its
defaults, and an empty list unbinds it while keeping it in the command palette.

```json
{
  "keybindings": {
    "app.command-palette": ["cmd-shift-p", "ctrl-b p"],
    "pane.equalize": ["ctrl-b ="],
    "pane.split-down": []
  }
}
```

Press `Cmd-Shift-P` to inspect all stable action IDs and bindings. Invalid
configuration is reported on stderr and the complete built-in keymap is used.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [the project plan](index.html) and [`tasks/rust-mux`](tasks/rust-mux) for phased implementation details.

## Roadmap

1. Harden terminal interaction beyond the current selection, clipboard, bounded scrollback, literal-search, mouse-reporting, and foundational IME checkpoint: grapheme shaping, wide-cell edge cases, richer search, and accessibility.
2. Replace repeated full-screen polling with revision-aware delta events, off-UI-thread parsing, bounded queues, sequence-gap recovery, and the documented idle-pane subscription policy. Measure update volume, focus-resume latency, UI-thread time, CPU, and memory before calling idle panes nearly free.
3. Harden the current CWD inheritance, exit/close semantics, and atomic desired-state recovery with crash fault injection and longer lifecycle soak.
4. Add conservative, side-effect-free configured-host suggestions and harden SSH child-exit presentation without changing the system-OpenSSH authority boundary.
5. Validate GPUI on real Linux Wayland/X11 GPU sessions and retain Iced/wgpu as the portability fallback.
6. Soak macOS Spaces/display switching, Linux compositors, high-output terminals, and service/client crash paths before packaging.

## Contributing

Early contributions should stay within the MVP boundary and keep the service/UI ownership line intact. Read the [cmux-informed product and architecture review](docs/cmux-informed-product-and-architecture.md) and the [PDR](pdr/goals/rust-mux-mvp/PDR.html) before foundational changes. Please run formatting, Clippy, and tests before opening a change.

Rust Mux is available under the MIT License.
