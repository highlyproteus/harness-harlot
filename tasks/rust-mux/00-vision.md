# Rust Mux Vision

## Goal
Define a reliability-first native local and SSH terminal workspace whose sessions outlive its desktop UI.

## Context
Rust Mux preserves the useful workspace and movable-pane model of cmux while avoiding a UI process that owns every terminal session. Reliability on macOS Spaces and screen changes is the reason for the architecture, not a later optimization.

## Requirements
- [x] Keep PTYs, child processes, session state, and layouts in a separate Rust service.
- [x] Make the native desktop UI a reconnectable client.
- [x] Preserve workspaces, sidebar, tabs, split panes, drag-to-rearrange panes, and configured-host SSH workspaces in the product direction.
- [x] Reuse an established terminal engine rather than implementing VT behavior.
- [x] Defer browser, mobile, generic remote-control UI, worktree/diff, AI integrations, and optional remote-rmuxd reattachment.

## Technical Notes
The initial terminal adapter uses `alacritty_terminal`; GPUI is the first UI spike with eframe/egui as a fallback harness; service communication starts over a local Unix socket and is planned to become Tokio-based framed RPC. System OpenSSH is the MVP SSH transport.

## Acceptance Criteria
- [x] The architecture and scope boundary are documented in the repository.
- [x] Future tasks preserve the service/UI ownership boundary.
