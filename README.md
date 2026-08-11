# Rust Mux

Rust Mux is a reliability-first native workspace application for local and SSH terminal work on macOS and Linux. It takes behavior-level inspiration from cmux's workspace, sidebar, tab, split-pane, and freely rearrangeable pane experience while deliberately separating session lifetime from desktop UI lifetime.

The repository now contains a runnable local-terminal checkpoint: the GPUI client opens the user's configured shell, sends keyboard input to service-owned PTYs, renders ANSI SGR color and style from the Alacritty grid, resizes PTYs, creates pane-local tabs and horizontal/vertical splits, and can restart without ending those shells. SSH, Unicode-width hardening, selection, scrollback controls, and durable on-disk recovery are still roadmap work.

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

The client is only a projection of daemon state. Closing it does not stop the service or its local PTYs; a new client fetches the current layouts and terminal screens from the owner-only Unix socket.

See [the terminal theme architecture](docs/terminal-theme.md) for the `Harbor Night` palette boundary and [the all-Rust renderer roadmap](docs/terminal-renderer-roadmap.md) for the hard no-libghostty decision and the measured typography/cell-rendering plan.

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

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [the project plan](index.html) and [`tasks/rust-mux`](tasks/rust-mux) for phased implementation details.

## Roadmap

1. Finish terminal interaction beyond the current ANSI/cursor checkpoint: Unicode-width handling, selection, clipboard, scrollback, search, mouse reporting, and IME.
2. Harden the framed IPC with request IDs, event subscriptions, sequence-gap recovery, and reconnect/backpressure tests.
3. Harden child-exit semantics beyond the current confirmed pane-close path, add CWD inheritance, and add atomic on-disk workspace persistence.
4. Add configured-host SSH workspaces by launching system OpenSSH inside managed PTYs.
5. Validate GPUI on real Linux Wayland/X11 GPU sessions and retain Iced/wgpu as the portability fallback.
6. Soak macOS Spaces/display switching, Linux compositors, high-output terminals, and service/client crash paths before packaging.

## Contributing

Early contributions should stay within the MVP boundary and keep the service/UI ownership line intact. Read the [cmux-informed product and architecture review](docs/cmux-informed-product-and-architecture.md) and the [PDR](pdr/goals/rust-mux-mvp/PDR.html) before foundational changes. Please run formatting, Clippy, and tests before opening a change.

Rust Mux is available under the MIT License.
