# Persistent PTY Sessions

## Goal
Make the session service own real local and system-SSH shell processes and terminal byte streams.

## Context
This is the core reliability promise: desktop restarts must not terminate or orphan terminal work.

## Requirements
- [x] Add `portable-pty` behind a narrow service-owned Unix PTY abstraction on macOS and Linux.
- [ ] Create, resize, write to, and terminate panes through explicit protocol commands.
- [ ] Stream ordered output with bounded queues and reconnect-safe sequence numbers.
- [ ] Define graceful exit, forced termination, and service-shutdown semantics.
- [x] Prove that closing and reopening a client preserves the shell and its output.
- [ ] Launch configured-host remote shells through system OpenSSH without bypassing SSH config, agents, host-key checks, or jump hosts.
- [ ] Prove that desktop reconnect and pane movement preserve the OpenSSH PID and remote session identity.

## Technical Notes
The service is the only layer allowed to own PTY or child-process handles. OpenSSH, not Rust Mux, owns SSH protocol/authentication. Avoid unbounded per-client output buffering.

## Acceptance Criteria
- [ ] Integration tests cover create/input/output/resize/exit.
- [ ] A reconnect test proves the PTY survives desktop-client teardown.
- [ ] Backpressure behavior is documented and tested.
