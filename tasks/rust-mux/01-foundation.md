# Repository and Process-Boundary Foundation

## Goal
Create a locally validated open-source Rust workspace that proves the desktop/service boundary.

## Context
All later features depend on being able to evolve protocol, terminal state, service behavior, and UI separately.

## Requirements
- [x] Create workspace crates for protocol, service, terminal model, and desktop.
- [x] Add a versioned handshake and snapshot request over a Unix socket.
- [x] Add a seeded workspace/tab/pane state owned by the service.
- [x] Add a native desktop shell that reconnects and renders the snapshot structure.
- [x] Add license, contributing guidance, ignore rules, pinned toolchain, and CI.

## Technical Notes
The service is foreground-only and state is in memory at this stage. This is intentional: daemon installation and durable storage belong with lifecycle hardening.

## Acceptance Criteria
- [x] Formatting passes.
- [x] Clippy passes with warnings denied.
- [x] Workspace tests pass.
- [x] A live service handshake and snapshot fetch are verified.
