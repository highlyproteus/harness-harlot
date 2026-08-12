# Revision-aware pane streaming

Protocol v8 replaces repeated full-workspace terminal-screen polling with coalesced, revision-aware pane delivery over the existing owner-only Unix socket. The same coherent protocol also carries the separately authorized private history controls and terminal identity metadata; it adds no listener, account, telemetry, vendor asset, or non-Rust terminal engine.

## Ownership and attention

Every PTY has an independent daemon reader thread. That thread always reads available bytes, applies them to the Alacritty terminal model, advances the pane revision, and retains only the terminal model's bounded history. Desktop attention changes screen delivery only; it never pauses a reader, child process, local shell, or system-OpenSSH process.

The desktop sends its last applied revision for each pane. A focused pane and panes focused within the last 60 seconds are subscribed. The daemon prepares a `TerminalScreen` only when a subscribed pane's revision differs from the receiver cursor. An inactive pane older than 60 seconds is cold: its content-free `PaneStreamState` can advance and become dirty, but no screen is serialized. Selecting any pane requests one targeted current snapshot before changing desktop focus, records fresh attention, and resumes revision-aware updates. An empty cursor set after receiver reconnect deterministically returns one current screen for each requested subscription.

There is no unbounded per-client queue. Each request coalesces all intermediate revisions into at most one current screen per subscribed pane. A revision gap, stale cursor, or reconnect therefore resynchronizes from current bounded state instead of replaying an unbounded output log.

## UI-thread boundary

The recurring desktop poll builds only small cursor/subscription metadata on the UI thread. Socket I/O, JSON decoding, and response ownership run on GPUI's background executor. The UI update merges already decoded screens and repaints only when layout metadata, a delivered screen, or connection state changes. Dirty metadata for an inactive pane does not repaint that pane. Explicit interaction commands remain synchronous local control operations; focus resync is a single targeted screen rather than a workspace snapshot.

## Content-safe diagnostics

Every update response exposes only:

- pane and delivery counts;
- coalesced revision count;
- serialized snapshot and screen byte counts;
- daemon preparation and desktop decoded-response apply microseconds;
- daemon CPU in milli-percent and resident memory bytes, sampled at most once per second.

The diagnostics contain no terminal cells or text, keyboard input, clipboard data, commands, environment values, SSH destinations, keys, agent state, or remote telemetry. They remain local protocol fields and are not transmitted anywhere else.

## Local validation

Run the focused activity-matrix smoke test with diagnostics visible:

```sh
cargo test -p rust-mux-session-service --test pane_streaming local_activity_matrix_smoke_reports_bounded_change_only_delivery -- --nocapture
```

The pass conditions are exact zero screen bytes for unchanged and cold subscriptions, one changed screen for the active burst, a current targeted refocus screen in under 500 ms on the local owner-only socket path, and a nonzero bounded daemon resident-memory measurement. The complete suite also covers changed-pane isolation, cold draining through a high-output burst, final-output visibility, refocus, cursor catch-up, and cursorless receiver reconnect.

The 2026-08-11 local debug-profile run across four panes sized from 80x24 through 140x48 measured 3,095 initial screen bytes, 0 unchanged bytes, 953 bytes for the single changed pane, 0 cold bytes, 222 microseconds focus resync, 171 microseconds daemon preparation, and 21,037,056 bytes daemon resident memory. Sampled CPU was 0 milli-percent during this sub-second smoke, so longer platform soaks remain the authority for sustained CPU targets. These numbers are evidence for the transport behavior, not release-grade macOS/Linux compositor benchmarks.

Run the full gate before committing:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```
