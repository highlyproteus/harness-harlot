# Terminal Rendering and Input

## Goal
Render interactive terminal panes in the native desktop without duplicating terminal-emulation logic.

## Context
Alacritty's engine should own VT parsing and terminal cell state while Rust Mux owns UI composition and transport.

## Requirements
- [x] Expand the `alacritty_terminal` adapter around parsing, resize, and visible-grid snapshots.
- [x] Add a GPUI terminal surface with monospaced text and HiDPI-aware PTY sizing.
- [x] Translate core keyboard and focus events into PTY input.
- [ ] Add styled cells, cursor rendering, Unicode-width handling, IME, mouse reporting, and paste/clipboard integration.
- [x] Add scrollback, selection, copy, and search.
- [x] Keep terminal-model APIs independent of GPUI.

## Technical Notes
Use representative shell fixtures and escape-sequence recordings rather than hand-authored assumptions about VT behavior.

Current checkpoint: the original `Harbor Night` built-in theme maps Alacritty ANSI, indexed, and truecolor cells plus bold, dim, italic, underline, strike, background, and cursor state. The unchecked combined requirement remains open because Unicode shaping, IME, mouse, and clipboard work is not complete.

## Acceptance Criteria
- [x] Interactive local shells render and accept input.
- [x] Unicode, colors, cursor modes, resize, and scrollback have regression coverage.
- [x] Desktop restart reconnects to the current rendered state.
