# CMUX-Informed Product and Architecture Review

This review informs Not a Harness at the behavior and architecture level. It is not a source-code port, visual clone, or compatibility promise.

## Review baseline

- Repository: [`manaflow-ai/cmux`](https://github.com/manaflow-ai/cmux)
- Inspected commit: [`93118b3b905f9cffc0ff18b275a4385df4f77587`](https://github.com/manaflow-ai/cmux/tree/93118b3b905f9cffc0ff18b275a4385df4f77587)
- Inspection date: 2026-08-11

The public repository describes cmux as a Swift/AppKit macOS app backed by libghostty, with workspace/sidebar navigation, surfaces, nested split panes, keyboard shortcuts, SSH workspaces, and a Unix-socket automation API. Its pane chrome and split behavior use Bonsplit. Its documented session restore recreates layout, working directories, scrollback on a best-effort basis, and browser state, but explicitly does not checkpoint arbitrary live process state. The current tree also shows ongoing decomposition around large app-owned `Workspace`, `TerminalController`, and `ContentView` types.

These observations do not establish the cause of any reported freeze. They identify ownership and coupling boundaries Not a Harness will deliberately change.

## Behavior to preserve independently

| CMUX-inspired behavior | Not a Harness interpretation | MVP treatment |
|---|---|---|
| Vertical workspace sidebar | Compact list of stable workspace identities, titles, selected state, connection type, and tab count | Include |
| Workspace and tab creation, selection, rename, reorder, and close | Keyboard-first commands with visible state and predictable close confirmation | Include |
| Horizontal and vertical split panes | Deterministic binary split tree with draggable dividers | Include |
| Dragging tabs/panes between split targets | Stable-ID drag payload, visible target zones, cancel-safe mutation, no PTY recreation | Include |
| Directional pane focus and numbered navigation | Framework-independent actions routed to the canonical model | Include |
| Split inherits the focused terminal's working directory | Service resolves CWD from the source pane when creating a local child | Include |
| SSH workspace from a configured host | System OpenSSH launched inside a daemon-owned PTY, with remote panes behaving like local panes | Include early |
| Layout and scrollback restoration | Layout is durable; scrollback is bounded; live process continuity comes from the still-running service | Include with stronger semantics |
| CLI/socket composability | Small versioned local control protocol shared by desktop and future CLI | Include necessary commands only |
| Agent notifications, browser panes, mobile access, diff/worktree features | Useful later, but outside the focused terminal-workspace milestone | Defer |

Not a Harness does not need to reproduce cmux's exact labels, colors, dimensions, keymap, iconography, screenshots, notification rings, or branded visual language. Common terminal/workspace conventions can be designed independently and tested with users.

## Implementation pieces to replace

| Public cmux implementation | Not a Harness replacement |
|---|---|
| Swift, SwiftUI, and AppKit application shell | Rust-authored, WebView-free GPU desktop client selected by a short UI spike |
| libghostty/GhosttyKit terminal surface in the app process | `alacritty_terminal` behind a toolkit-neutral terminal-model adapter; selected UI draws cells through its GPU renderer |
| Bonsplit-owned view/layout tree | Pure Rust split-tree model owned canonically by the service, with a thin UI projection and gesture layer |
| App-owned terminal surfaces and best-effort process recreation | Separate Tokio service owns Unix PTYs, local/SSH child handles, output sequence, bounded scrollback, metadata, and layouts |
| App-lifetime socket controller coupled to UI state | Versioned local IPC whose handlers call service commands, not UI objects |
| Broad controller surface spanning terminal, browser, remote, agents, and UI | Narrow crates and commands scoped to terminal sessions, SSH workspaces, workspace state, and layout mutations |
| Main-thread observable model as state authority | Service-side actors/tasks and immutable revisioned snapshots; UI state is disposable cache/presentation state |

## Patterns to deliberately avoid

- Never let a window, widget, renderer, or drag gesture own a PTY, SSH process, or child handle.
- Do not make UI teardown imply session teardown. Pane-close is an explicit service command; client disconnect is not.
- Do not maintain independent mutable split trees in both service and UI. The service accepts revisioned mutations and emits the canonical result.
- Do not let terminal output wait on rendering, layout, HTTP, or a slow client. PTY draining and bounded fan-out are independent tasks.
- Do not put high-frequency geometry writes under multiple owners. During divider drag, the UI previews locally; one settled mutation is committed.
- Do not grow one global controller that imports every feature namespace. Protocol dispatch, session runtime, SSH launch policy, layout, persistence, and UI projection stay separately testable.
- Do not implement SSH authentication, key parsing, host-key policy, or proxy traversal in Rust for the MVP. Delegate those security-sensitive behaviors to the user's OpenSSH.
- Do not add browser, mobile, worktree/diff, or AI-specific APIs to the MVP protocol.
- Do not bind a TCP listener by default. Initial client-to-daemon API is local and per-user.
- Do not claim that a service crash, local machine sleep, or broken network preserves a remote process. Desktop restart survival and durable remote reattachment are different guarantees.

## Frontend framework decision

### Invariants

The desktop must be authored in Rust, render through a native GPU path rather than a system WebView, and remain a replaceable client of the separate service. Framework selection cannot move PTY or layout ownership into the UI.

### Candidates

| Candidate | Fit | Risk / decision |
|---|---|---|
| GPUI | Rust, GPU accelerated, Metal on macOS, designed for IDE-like applications, custom elements, actions, async executor, and test contexts | Primary spike candidate for macOS/Linux. It is pre-1.0, changes frequently, and Linux behavior must be proven. |
| eframe/egui with wgpu | Already present as a small scaffold; Rust-rendered; fast to prototype custom pane interactions | Keep as comparison/fallback. Immediate-mode architecture and sophisticated text/accessibility need proof for an IDE-like terminal. |
| Iced with wgpu | Cross-platform native runtime, Elm-style state/update model, custom widgets, wgpu on Metal/Vulkan/DX12 | Documented portability fallback if GPUI maturity or Linux integration blocks the spike. Upstream also calls it experimental. |
| Dioxus Desktop | Rust-authored components and CSS-like styling | Reject for primary shell: `dioxus-desktop` identifies itself as a WebView renderer and uses Wry's OS WebView. |

GPUI was the first spike and is now the provisional MVP client choice, not a permanent commitment. The checked-in native window proves the narrow workspace rail, pane-local tabs, stable pane identity, runtime splitting, drag rearrangement, divider resize, global shortcuts, and real service-owned configured-shell PTYs rendered from Alacritty grids. The macOS proof entered distinct commands in multiple panes and recovered the same visible terminal state after a desktop-only restart. The same source compiled in a clean Debian Linux container against GPUI's Vulkan/Wayland/X11 stack. A real Linux compositor/GPU runtime soak, accessibility audit, full styled-cell benchmark, and upstream API-churn cost remain explicit fallback gates. If any gate fails, run the same bounded scenario in eframe/wgpu, then Iced/wgpu if a retained update model is needed.

The measured decision and reproduction evidence are in [`ui-spike-decision.md`](ui-spike-decision.md). No cmux source, branding, or assets were used by the prototype.

Windows is a later portability target and does not constrain the MVP choice. Protocol, terminal model, layout model, and session kinds remain UI-toolkit-neutral so a future Windows client can use Iced/wgpu, eframe/wgpu, or a mature GPUI Windows path.

## Daemon and API decision

### Required backend

Not a Harness needs a local Tokio daemon, not a cloud backend. It owns PTYs, local and SSH child processes, session actors, ordered output, bounded replay, layout state, persistence, and client subscriptions. On macOS and Linux, `portable-pty` supplies Unix PTYs behind a narrow adapter.

### MVP IPC choice

Use a thin framed RPC/event protocol over a per-user Unix-domain socket:

- fixed frame-size limit and typed version/capability handshake;
- request IDs with structured success/error responses;
- stable entity IDs and expected state revision on mutations;
- separate event subscription with monotonic sequence numbers;
- bounded queues, resync snapshots, and explicit slow-client policy;
- socket directory/file permissions limited to the current user.

This is preferred over HTTP for the MVP because the command surface is small, terminal events are high-frequency and bidirectional, and precise framing/backpressure matters more than HTTP routing. Tokio tasks can implement it with a length-delimited codec. Transport DTOs remain separate from service commands.

### Where Axum fits

Axum 0.8 can serve directly from Tokio's `UnixListener`, so Axum-over-local-socket is technically valid. It becomes useful when Not a Harness needs HTTP semantics, Tower middleware, WebSocket clients, authenticated remote control, or an optional remote `rmuxd` service.

It is not required for the local daemon and does not imply a hosted/cloud backend. Do not start an unauthenticated TCP listener by default. Any later TCP HTTP/WebSocket endpoint must be opt-in, authenticated, encrypted, capability scoped, rate-limited, and threat-modeled. Embedded browser and generic remote UI access remain outside MVP even though SSH terminals are early.

## Remote workspace design

### First usable SSH path

The daemon launches the system `ssh` executable inside the same managed PTY abstraction as a local shell. A user chooses an alias from their standard SSH configuration. The daemon passes the alias as a validated argument and lets OpenSSH resolve `Host`, `Include`, `IdentityFile`, `IdentityAgent`, `ProxyJump`/`ProxyCommand`, `Match`, known-host files, algorithms, multiplexing, and user preferences.

OpenSSH owns authentication and host-key decisions. Prompts render in the terminal pane. Not a Harness never reads private key material, never disables host-key verification, never invents a parallel credentials store, and never logs prompt responses or terminal contents. PTY resize naturally reaches the SSH process and remote TTY. The local daemon records only necessary metadata: session ID, requested host alias, local SSH PID, connection/lifecycle state, pane binding, timestamps, and non-secret error classification.

Host discovery may parse concrete `Host` aliases for display, but `ssh -G <alias>` is the source of truth for resolution and validation. Wildcard-only entries are not shown as concrete hosts. Reject control characters and option-like aliases rather than composing a shell command. Spawn the executable with an argv vector; never use `sh -c`.

Because the local daemon owns the SSH client PTY, closing/restarting the desktop does not end the SSH process. Moving an SSH pane also does not reconnect. Network loss, local sleep, or service exit may still end the remote shell; expose that honestly.

### Later durable remote reattachment

Protocol session metadata distinguishes local shell, system-SSH shell, and future `rmuxd`-backed shell without changing pane identity. A later optional remote `rmuxd` can own remote PTYs and support reattachment after local sleep/network loss. Bootstrap could travel over system SSH and a stdio tunnel; an authenticated Axum HTTP/WebSocket API is another later option. Neither is required to open the first remote terminal.

### Security and verification

- Preserve unknown-host and changed-host-key failures; never set `StrictHostKeyChecking=no`.
- Use the user's SSH agent and key configuration indirectly through OpenSSH; do not copy keys.
- Honor ProxyJump/ProxyCommand from resolved config and display that a connection is proxied without logging secrets.
- Restrict host aliases and optional remote commands to structured argv; prevent option and shell injection.
- Limit environment inheritance and document which variables cross into OpenSSH.
- Treat remote terminal bytes as sensitive; exclude content from default logs, metrics, and crash reports.
- Test concrete aliases, `Include`, wildcard precedence, ProxyJump, agent/key flows, unknown/changed host keys, resize via remote `stty size`, desktop reconnect preserving SSH PID, pane moves preserving session ID, network exit states, and redaction.

## Staged MVP

1. **Architecture contract and scaffold** — versioned protocol, service/client split, Alacritty adapter, source/licensing record.
2. **UI spike** — GPUI first; prove pane drag, resize, shortcuts/focus, real configured-shell terminal-grid rendering, macOS Metal, and Linux native GPU compilation. Complete for the bounded spike; Linux runtime remains a packaging gate.
3. **Tokio service and IPC** — per-user Unix socket, framed RPC/events, revisions, bounded queues, snapshot/reconnect.
4. **Local persistent sessions** — service-owned Unix PTYs, input/output/resize, bounded screen history, and desktop restart survival are proven; CWD inheritance, close/exit semantics, event replay, and disk persistence remain.
5. **Remote workspaces** — host picker from SSH config, system OpenSSH in a managed PTY, secure config/agent/host-key/ProxyJump behavior, pane parity.
6. **Terminal client** — interactive Alacritty-backed rendering, input, IME, selection, scrollback, search, resync.
7. **Workspace UX** — sidebar, tabs, local/remote badges, split mutations, drag/drop, focus, atomic layout persistence.
8. **Reliability and packaging** — macOS Spaces/display soak, Linux compositor tests, local and SSH reconnect cases, service lifecycle, release notices.

## License and attribution boundary

The reviewed cmux repository is GPL-3.0-or-later by default. Not a Harness's MIT license does not permit copying, translating, or adapting cmux GPL-covered source without changing resulting obligations. No cmux source, tests, documentation prose, branding, screenshots, icons, or other assets should be copied.

Behavioral research may inform an independently designed specification, but contributors should record the behavior and write original Rust code/tests. If a cmux third-party dependency is considered separately, evaluate it at its upstream source and license; cmux's notices list Ghostty and Bonsplit as MIT, but that does not make cmux's own integration code MIT. This is project guidance, not legal advice.

Before release, generate a dependency license inventory and include required notices for `alacritty_terminal`, the selected UI framework, Tokio, `portable-pty`, and other distributed dependencies.

## Primary sources

- [cmux README at reviewed commit](https://github.com/manaflow-ai/cmux/blob/93118b3b905f9cffc0ff18b275a4385df4f77587/README.md)
- [cmux license](https://github.com/manaflow-ai/cmux/blob/93118b3b905f9cffc0ff18b275a4385df4f77587/LICENSE)
- [cmux third-party licenses](https://github.com/manaflow-ai/cmux/blob/93118b3b905f9cffc0ff18b275a4385df4f77587/THIRD_PARTY_LICENSES.md)
- [cmux Workspace model](https://github.com/manaflow-ai/cmux/blob/93118b3b905f9cffc0ff18b275a4385df4f77587/Sources/Workspace.swift)
- [cmux TerminalController](https://github.com/manaflow-ai/cmux/blob/93118b3b905f9cffc0ff18b275a4385df4f77587/Sources/TerminalController.swift)
- [cmux pane/layout package](https://github.com/manaflow-ai/cmux/tree/93118b3b905f9cffc0ff18b275a4385df4f77587/Packages/macOS/CmuxPanes)
- [cmux render ownership discussion](https://github.com/manaflow-ai/cmux/blob/93118b3b905f9cffc0ff18b275a4385df4f77587/docs/remote-tmux-sizing-design.md)
- [cmux remote daemon specification](https://github.com/manaflow-ai/cmux/blob/93118b3b905f9cffc0ff18b275a4385df4f77587/docs/remote-daemon-spec.md)
- [Dioxus Desktop manifest using Wry OS WebView](https://github.com/DioxusLabs/dioxus/blob/main/packages/desktop/Cargo.toml)
- [GPUI framework README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md)
- [Iced framework README](https://github.com/iced-rs/iced/blob/master/README.md)
- [Axum 0.8 listener implementation, including UnixListener](https://github.com/tokio-rs/axum/blob/axum-v0.8.9/axum/src/serve/listener.rs)
- [`portable-pty` platform implementations](https://github.com/wezterm/wezterm/tree/main/pty/src)
- [`alacritty_terminal` crate](https://crates.io/crates/alacritty_terminal)
