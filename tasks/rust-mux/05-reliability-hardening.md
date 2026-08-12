# macOS Reliability Hardening

## Goal
Validate the reliability claim under desktop, display, daemon, and SSH lifecycle stress on macOS and Linux.

## Context
Avoiding macOS Spaces and screen-switching freezes is the project's motivating constraint.

## Requirements
- [ ] Add structured service logs and opt-in diagnostics without recording terminal contents.
- [ ] Add crash/reconnect, slow-client, high-output, and service-upgrade scenarios.
- [x] Define and measure idle-pane CPU, memory, serialized-byte, UI-thread, and focus-resume targets with diagnostics that never record terminal contents.
- [ ] Test repeated Space switches, display attach/detach, sleep/wake, and fullscreen transitions.
- [ ] Test Linux compositor/display-session changes and SSH network loss, host-key failure, jump-host, and desktop-reconnect behavior.
- [x] Add atomic state persistence and corruption recovery.
- [ ] Package the service and desktop with clear start/stop/uninstall behavior.

## Technical Notes
Define measurable pass criteria before tuning. Keep terminal contents out of logs and crash artifacts by default. The idle-pane policy is a delivery optimization only: after roughly 60 seconds without attention, stop pushing live screen updates while the daemon continues draining the PTY into bounded history. Never disconnect a PTY or silently discard output to meet a performance target.

## Acceptance Criteria
- [ ] Repeatable macOS and Linux soak tests run without UI freezes or lost local/SSH sessions.
- [x] Recovery behavior is documented for desktop crash, service crash, and corrupt state.
- [ ] Release packaging preserves the independent service lifecycle.
- [x] Focused/recent panes remain responsive, stale panes stop generating live desktop traffic, and refocus performs one bounded fresh-snapshot resync before live deltas resume.
