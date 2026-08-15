# Revision-aware pane streaming

Protocol v17 retains coalesced, revision-aware pane delivery over the existing owner-only Unix socket and adds saved workspace lifecycle metadata, explicit empty-workspace terminal creation, and controls. The same coherent protocol also carries private history controls and terminal identity metadata; official identity artwork is compiled into the desktop and never crosses IPC or the network. It adds no listener, account, telemetry, or non-Rust terminal engine.

## Ownership and visibility

Every PTY has an independent daemon reader thread. That thread always reads available bytes, applies them to the Alacritty terminal model, advances the pane revision, and retains only the terminal model's bounded history. Desktop subscriptions change screen delivery only; they never pause a reader, child process, local shell, or system-OpenSSH process.

The desktop sends its last applied revision for each pane and subscribes exactly the panes it is rendering: the tab holding the focused pane, with zoom applied. That is the same projection the desktop resizes, so what is sized is what streams. The focused pane is subscribed on every poll; every other on-screen pane is paced, subscribed again once 120 ms have passed since its last delivered screen, so a four-way split cannot multiply the focused pane's payload every 33 ms. Visibility, not attention, decides: an on-screen pane keeps streaming for as long as it is displayed, however long the user leaves the keyboard alone.

A pane that is not on screen is content-free: its `PaneStreamState` can advance and become dirty, but no screen is serialized for it. Displaying it requests one targeted current snapshot before desktop focus changes, so a background tab shows current content the moment it appears. An empty cursor set after receiver reconnect deterministically returns one current screen for each requested subscription.

Polling runs at 33 ms while output keeps arriving and backs off to 250 ms when it stops. After one hour with no delivered output and no user input, it relaxes to 2 s; pane states still arrive at that cadence, so the first byte of new output restores 33 ms within one poll.

Terminal input and selection updates are one-way requests: the daemon runs them and writes no response frame. The desktop keeps a separate connection for screen traffic, so a keystroke is never serialized behind an in-flight screen payload and no unread acknowledgement can accumulate in a receive buffer.

There is no unbounded per-client queue. Each request coalesces all intermediate revisions into at most one current screen per subscribed pane. A revision gap, stale cursor, or reconnect therefore resynchronizes from current bounded state instead of replaying an unbounded output log.

## UI-thread boundary

The recurring desktop poll builds only small cursor/subscription metadata on the UI thread. Socket I/O, JSON decoding, and response ownership run on GPUI's background executor. The UI update merges already decoded screens and repaints only when layout metadata, a delivered screen, or connection state changes. Dirty metadata for an inactive pane does not repaint that pane. Explicit interaction commands remain synchronous local control operations; focus resync is a single targeted screen rather than a workspace snapshot.

## Content-safe diagnostics

Every update response exposes only:

- pane and delivery counts;
- coalesced revision count;
- serialized snapshot and screen byte counts, which cost a second full serialization of the payload and are therefore opt-in: the socket update path reports zero for both, and only tests that assert on payload size request them;
- daemon preparation and desktop decoded-response apply microseconds;
- daemon CPU in milli-percent and resident memory bytes, sampled at most once per second.

The diagnostics contain no terminal cells or text, keyboard input, clipboard data, commands, environment values, SSH destinations, keys, agent state, or remote telemetry. They remain local protocol fields and are not transmitted anywhere else.

## Local validation

Run the focused activity-matrix smoke test and the keystroke-echo measurement with diagnostics visible:

```sh
cargo test -p hh-session-service --test pane_streaming -- --nocapture
```

The pass conditions are exact zero screen bytes for unchanged and unsubscribed panes, one changed screen for the active burst, a current targeted refocus screen in under 500 ms on the local owner-only socket path, a nonzero bounded daemon resident-memory measurement, and a delivered keystroke echo in under 300 ms. The complete suite also covers changed-pane isolation, off-screen draining through a high-output burst, final-output visibility, refocus, cursor catch-up, cursorless receiver reconnect, and the opt-in semantics of the byte counters.

The 2026-08-11 local debug-profile run across four panes sized from 80x24 through 140x48 measured 3,095 initial screen bytes, 0 unchanged bytes, 953 bytes for the single changed pane, 0 cold bytes, 222 microseconds focus resync, 171 microseconds daemon preparation, and 21,037,056 bytes daemon resident memory. The 2026-08-13 run on the same shapes, after the byte counters became opt-in, measured 3,011 initial screen bytes, 913 bytes for the changed pane, 139 microseconds focus resync, 237 microseconds daemon preparation with measurement on, 86 microseconds preparation on the unmeasured path the socket uses, and 6,370 microseconds from `write_input` to a delivered screen containing the echoed keystrokes. Sampled CPU was 0 milli-percent during these sub-second smokes, so longer platform soaks remain the authority for sustained CPU targets. These numbers are evidence for the transport behavior, not release-grade macOS/Linux compositor benchmarks.

Run the full gate before committing:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```
