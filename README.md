# Harness Harlot

Harness Harlot is a lightweight native terminal workstation for local and SSH work. macOS is the first supported release target; Linux currently has compile/test coverage but still needs real Wayland/X11 GPU-session validation before it is advertised as a packaged release. It is not an agent harness or runtime: terminals and agents run as ordinary shell workloads while Harness Harlot stays out of the way and avoids competing for their CPU or memory. It takes behavior-level inspiration from a compact terminal rail, tab, split-pane, and freely rearrangeable pane experience while deliberately separating session lifetime from desktop UI lifetime.

The repository now contains runnable local terminals plus a thin system-SSH checkpoint: the GPUI client opens the user's configured shell, sends keyboard and mouse input to service-owned PTYs, renders ANSI SGR color and style from the Alacritty grid, supports selection/copy/paste, bounded scrollback and literal search, resizes PTYs, and creates pane-local tabs and splits. The daemon persists a restricted local desired-state snapshot for fresh-shell recovery and, separately, can keep an owner-only chunked archive of future PTY output for lazy upward scroll/search. An explicit two-step SSH action can launch the installed OpenSSH client in the same kind of daemon-owned PTY. Unicode shaping and broader platform soak remain roadmap work.

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
        +-- optional owner-only, disk-backed historical chunks
```

The Rust workspace keeps those responsibilities explicit:

- `hh-desktop`: restartable GPUI client with the compact workstation rail, pane-local human-named tabs, real themed terminal surfaces, keyboard focus/input, pane-targeted controls, divider resize, and drag-to-split movement.
- `hh-session-service`: long-lived local authority for PTYs, configured-shell processes, terminal state, and live layouts.
- `hh-protocol`: versioned transport messages and shared layout types.
- `hh-terminal-model`: a narrow adapter around Alacritty's established terminal engine. Harness Harlot will not implement VT parsing from scratch.

The client is only a projection of daemon state. Closing it does not stop the service or its PTYs; a new client fetches current layout metadata and revision-aware pane updates from the owner-only Unix socket. A daemon restart recreates fresh local shells from a restricted owner-only snapshot; it does not claim to preserve live processes.

### Performance contract

Harness Harlot is designed to make hidden panes nearly free: stream only the panes that are on screen, update only the ones whose output changes, parse and prepare updates away from the UI thread, keep terminal history bounded, and expose performance diagnostics without recording terminal contents. Every pane rendered right now streams continuously — the focused pane on every poll, other on-screen panes paced at about 8 frames per second — for as long as it stays visible. A pane on a tab that is not displayed has its PTY drained into bounded daemon-owned history so a local or remote process can never block, but no screen is serialized for it; displaying it requests one fresh targeted snapshot before rendering and then resumes live deltas, so tab switching remains immediate and no output is lost. After an hour with no output and no input anywhere, polling relaxes to once every two seconds and the first byte of new output restores the active cadence within one poll.

Protocol v21 keeps that streaming policy—coalesced changed-pane screen payloads, content-free dirty/revision metadata for off-screen panes, targeted focus snapshots, and background desktop polling—alongside saved workstation lifecycle metadata, explicit empty-workstation terminal creation, controls, private history, notifications, and the expanded terminal identity registry. Terminal input and selection updates are one-way messages the daemon never answers, and the desktop carries them on a separate connection from screen traffic, so a keystroke never waits behind a screen payload. See [the pane streaming design and validation guide](docs/pane-streaming.md).

For SSH, the New Workstation flow accepts one conservative host/alias or `[user@]host` destination; it can also normalize an exact pasted `ssh <destination>` command. It then launches structured argv equivalent to `ssh -- <destination>`. Options, extra commands, and shell syntax are rejected rather than interpreted. Harness Harlot does not read SSH keys or config, probe hosts with `ssh -G`, add agent forwarding, change host-key policy, or answer prompts. The installed OpenSSH client remains the sole authority for `~/.ssh/config`, `Include`/`Match`, agents and identity files, known hosts, proxies, multiplexing, authentication, and host-key verification. A successful SSH workstation saves only its name, validated destination, pin/order, connection state, and safe pane/tab layout locally. New tabs and splits within that workstation reuse the saved destination; they never fall back to a local Mac shell. Disconnect and service restart retain that workstation offline without making a network connection; reconnect starts fresh runtime-only OpenSSH PTYs in the saved layout. Passwords, keys, agent material, SSH config contents, terminal output, and process state are never workstation metadata.

An explicit workstation-menu **Scan tmux sessions** action can read bounded session metadata from the default local tmux server or from a currently connected saved SSH workstation. No tmux scan runs at startup, in the background, or as part of reconnect. Opening selected sessions creates one runtime-only terminal tab per session, attached exactly like a hand-run `tmux attach-session`: no helper session is created and no tmux option is set, so tmux remains the authority for its own windows, panes, layouts, and status bar. Harness Harlot does not mirror or manage tmux internals, persist tmux targets or scan results, discover custom tmux sockets, or run arbitrary local or remote commands. Remote scan and attach use the already selected system-SSH destination with fixed structured commands only.

The centered **New Workstation** control and the existing `Cmd-N` binding open the same creation flow for local or system-SSH workstations. Workstation rows start collapsed while their terminal-count badges remain visible. The rail header uses bundled Harness Harlot artwork; connected SSH rows keep the destination behind a compact information control and require confirmation before disconnecting. Offline SSH rows expose quick reconnect and delete controls, with deletion still confirmed. Right-click a row to rename it, pin/unpin it, choose an inline color, or manage it; right-click an open terminal in the expanded rail to rename that terminal. Disconnect is non-destructive and distinct from deletion.

The workstation sidebar has a thin drag boundary and a 120–420 px preferred-width range. Its defaults are 25% narrower while Harness Harlot still preserves at least 320 px for terminal content in compact windows without forgetting a wider user preference, then restores that preference when space returns. The width is stored atomically in the same owner-only state directory as `ui-state.json`; it contains no terminal, SSH, or credential data.

Closing the final terminal never deletes its workstation or silently creates a replacement. The saved workstation remains in a deliberate empty state with one **Open Terminal** action and the normal new-terminal shortcut. That action is accepted only while the workstation has no layout, preventing repeated clicks or retries from creating duplicate terminals.

See [the terminal theme architecture](docs/terminal-theme.md) for the `Harbor Night` palette boundary and [the all-Rust renderer roadmap](docs/terminal-renderer-roadmap.md) for the hard no-libghostty decision and the measured typography/cell-rendering plan.

### History storage

The gear opens compact local settings that distinguish the fast 2,000-line live memory buffer from the optional disk archive. The archive defaults to keep-indefinitely with a 5 GiB quota and a pause-and-warn capacity policy: the terminal keeps running when storage is slow or full, gaps are reported honestly, and retained sessions are never silently deleted. Finite retention and oldest-first capacity cleanup delete only after the user opts into those policies. Terminal/workspace/all clears require confirmation.

Scrolling upward from the top of live history loads one bounded local archive page at a time; live-search misses can search older chunks. Archived pages are visibly labeled and do not turn the UI into an unbounded terminal buffer. History begins only for sessions/output recorded after this feature is active—older output cannot be recovered. See [local terminal history storage](docs/terminal-history-storage.md) for formats, permissions, corruption/restart behavior, local-storage boundaries, and fidelity limits.

### Appearance colors

Harbor Night remains the built-in visual foundation, with two independent local defaults: a terminal accent for focus rails, active tabs, and cursor treatment, and a workstation color for the selected workstation in the sidebar. Appearance settings offer a restrained preset/recent-color picker. A terminal tab or workstation can override only its own color from its right-click menu, or return to its matching default. These choices persist in the owner-only desired-state file.

Terminal tabs can identify known locally running Codex CLI, Claude Code, Droid, Hermes Agent, Kilo Code, Cursor, OpenCode, Aider, GitHub Copilot CLI, and Gemini CLI sessions. Official bundled product icons appear unchanged beside complete labels where an authoritative asset is available; otherwise Harness Harlot uses a neutral terminal glyph. Resolution follows user rename, selected profile, verified exact OSC title token, bounded exact local child-process basename or official runtime executable signature, then generic fallback. Detection does not inspect terminal output, shell history, arguments, environment, working directory, or agent conversations, and only explicit identity overrides are persisted. See [Automatic terminal identity](docs/terminal-identity.md) and [Asset notices](ASSET_NOTICES.md).

The notification center records bounded daemon-runtime events from terminal BEL, OSC 9/777 notification sequences, and child-process exit; it never scans terminal grid text for prompts or conversation content. tmux forwards BEL to attached clients under its normal bell settings. OSC notifications from programs inside tmux require tmux passthrough wrapping and `allow-passthrough`; Harness Harlot does not change that tmux policy.

## MVP boundary

The first usable milestone includes:

- local workstations, tabs, and split terminal panes;
- a persistent Rust service that owns PTYs and child processes;
- a restartable native desktop client;
- drag-to-rearrange panes and persisted layouts;
- terminal rendering/input through the Alacritty terminal engine adapter;
- configured-host SSH workstations implemented by running system OpenSSH inside daemon-owned PTYs;
- crash/reconnect and session-lifecycle tests on macOS and Linux.

Explicitly deferred: embedded web browsing, mobile access, generic remote-control UI, worktree/diff review, and AI-specific integrations. SSH terminal workstations are part of the early MVP; an optional remote `rmuxd` for durable reattachment comes later.

### Workstation terminology and saved-state compatibility

The app now calls the user-facing unit a **workstation**. The Rust crate names, IPC request/response variants, persisted JSON fields such as `workspaces`, and existing action IDs such as `workspace.new` intentionally retain their established technical names so existing local desired-state snapshots and keymaps continue to work without migration or data loss.

## Run the local terminal checkpoint

Requirements: Rust 1.96 or newer.

```bash
cargo run -p hh-session-service
```

In another terminal:

```bash
cargo run -p hh-desktop
```

On macOS, `scripts/build-macos-app.sh` creates the debug
`target/debug/Harness Harlot.app`; pass `release` for a release bundle. The
bundle includes the desktop, its service, the Harness Harlot icon, and license
notices. The update verifier is distributed beside the app only inside a
release DMG; release-signing code is never bundled. The display name,
executable, and bundle identifier are
`Harness Harlot`, `hh`, and `com.harnessharlot.desktop`. The desktop starts its bundled
service only when a local service is unavailable; closing the desktop leaves
active terminal sessions alone.

For side-by-side local development inspection, `scripts/build-macos-dev-app.sh`
creates `target/debug/Harness Harlot Dev.app` with bundle identifier
`com.harnessharlot.desktop.dev`, executable `hh-dev`, and the separate monochrome
development icon. The Dev launcher automatically uses a separate socket plus
durable `Harness Harlot Dev` state and `hh-dev` configuration, so saved Dev
workstations survive a Dev relaunch without reading or writing stable app data.
Explicit `HH_SOCKET`, `HH_STATE_DIR`, and `HH_CONFIG` values still take
precedence for disposable test launches.

The technical package, crate, and executable prefix is `hh`. The built
executables are `hh-service` and `hh`. Set `HH_SOCKET` in both processes to
override the default socket path. The default is an owner-only
`run/hh-session.sock` under the application state directory.
`HH_DISABLE_BUNDLED_SERVICE=1` keeps desktop
startup from launching a sibling service for focused development tests.

## macOS releases and updates

Harness Harlot has one stable channel. It does not make update network requests
yet. The checked-in release scripts make versioned DMGs and signed metadata,
sign nested code inside-out, and require notarization/stapling in production
mode. Production trust values, Apple credentials, and an immutable public host
are intentionally not configured in source. See the [macOS release foundation](docs/macos-release.md) for the
credential/hosting handoff, update safety rule for active PTYs, rollback
behavior, Sparkle assessment, and release checklist.

### Command bindings

Harness Harlot loads optional JSON configuration from `HH_CONFIG`, then
`$XDG_CONFIG_HOME/hh/config.json`, then `$HOME/.config/hh/config.json`.
`HH_STATE_DIR` selects an alternate owner-only state directory, and child
terminals receive `HH_PANE_ID`. Key sequences use GPUI syntax; multiple
space-separated strokes form a chord. A configured action replaces its defaults,
and an empty list unbinds it while keeping it in the command palette.

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

See [the project plan](index.html), the [identity migration notes](docs/identity-migration.md),
and [`tasks/rust-mux`](tasks/rust-mux) for phased implementation details. The
task/PDR paths retain their original historical IDs so established records and
links do not break.

## Roadmap

1. Harden terminal interaction beyond the current selection, clipboard, bounded scrollback, literal-search, mouse-reporting, and foundational IME checkpoint: grapheme shaping, wide-cell edge cases, richer search, and accessibility.
2. Continue hardening protocol-v20 revision-aware pane delivery with request IDs, longer slow-client/high-output soaks, and macOS/Linux release-profile measurements for update volume, focus-resume latency, UI-thread time, CPU, and memory.
3. Harden the current CWD inheritance, exit/close semantics, and atomic desired-state recovery with crash fault injection and longer lifecycle soak.
4. Add conservative, side-effect-free configured-host suggestions and harden SSH child-exit presentation without changing the system-OpenSSH authority boundary.
5. Validate GPUI on real Linux Wayland/X11 GPU sessions and retain Iced/wgpu as the portability fallback.
6. Soak macOS Spaces/display switching, Linux compositors, high-output terminals, and service/client crash paths before packaging.

## Contributing

Early contributions should stay within the MVP boundary and keep the service/UI ownership line intact. Read the [cmux-informed product and architecture review](docs/cmux-informed-product-and-architecture.md) and the [PDR](pdr/goals/rust-mux-mvp/PDR.html) before foundational changes. Please run formatting, Clippy, and tests before opening a change.

Harness Harlot is available under the MIT License.
