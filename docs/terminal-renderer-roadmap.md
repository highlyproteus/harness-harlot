# All-Rust terminal renderer decision and roadmap

## Hard decision

Rust Mux will remain an all-Rust application. The daemon continues to own PTYs and `alacritty_terminal` continues to own VT parsing, grid state, and scrollback. The native GPUI client will render that grid through Rust and GPUI's GPU/text facilities. Ghostty, libghostty, GhosttyKit, and other non-Rust terminal engines are not candidates unless a future approved PDR revision explicitly reverses this decision.

This is an architecture boundary, not a judgment about Ghostty's quality. Embedding libghostty would add a Zig/C ABI build and lifecycle boundary, couple the GPUI surface to an evolving library API, and split terminal behavior across two engines while the daemon still needs a serializable canonical grid. That change is too large for a typography fix and would weaken the current Rust ownership and restart-isolation model.

## Current diagnosis

The PTY and terminal model are not the likely source of the visible text problem. The service already parses real output with `alacritty_terminal` and transmits styled visible runs. The weak link is the first-pass GPUI projection:

- it hard-codes `SF Mono`, a `13 px` size, `18 px` line height, and a `7.83 px` cell width;
- it renders each row as ordinary `StyledText`, then positions the cursor with the hard-coded width;
- it does not resolve a tested monospace family/fallback stack per platform;
- it does not derive baseline, ascent, descent, advance, or line height from the selected font;
- it flattens terminal cells into text runs, so cell occupancy, fallback glyph width, selection, and background painting are not yet authoritative.

GPUI already provides OS font discovery, font resolution, shaping, platform rasterization, subpixel variants, and a GPU sprite atlas. On Linux its text system uses `cosmic-text`; on macOS it uses the native platform text system. Rust Mux should use this existing GPUI path before adding a second rasterizer or atlas.

## Crate and subsystem choices

| Need | MVP choice | Why |
|---|---|---|
| VT parsing, grid, modes, scrollback, selection primitives | `alacritty_terminal` | Already integrated, Rust, mature terminal semantics, and keeps parsing in the daemon. |
| Font discovery, fallback, shaping, rasterization, atlas | GPUI `TextSystem`, `FontFallbacks`, platform rasterizer, and sprite atlas | Already paid for by GPUI and matches the active GPU/window lifecycle. Avoid a duplicate cache and renderer. |
| Linux shaping/rasterization implementation | GPUI's existing `cosmic-text`/`swash` integration | Mature Rust shaping, font database, fallback, and glyph raster cache without a new app-owned pipeline. |
| Cell/grapheme accounting | `unicode-width` plus `unicode-segmentation`, used behind terminal-grid tests | Small focused Rust crates. The Alacritty grid remains authoritative; these crates validate UI mapping and input/selection boundaries rather than replacing parser behavior. |
| Search | Alacritty grid/search APIs first; add `regex-automata` only if the terminal adapter cannot express the required search model | Avoid copying scrollback or inventing a second text buffer. |

Direct `swash`, `cosmic-text`, `fontdb`, or `skrifa` dependencies are deferred. They are good Rust building blocks if GPUI's public text API proves insufficient, but adding them now would duplicate functionality already present transitively and create a second glyph-cache lifecycle.

## Phased plan

### Phase R1: typography and palette correction

1. Introduce a `TerminalFontProfile` selected at startup from GPUI's `TextSystem::all_font_names`, with a user override and platform-safe monospace candidates. Record the resolved family in diagnostics without logging terminal content.
2. Resolve regular, bold, and italic faces plus explicit fallback families. Disable discretionary ligatures for the terminal grid unless a later cell-aware shaping test proves them safe.
3. Derive cell advance, ascent, descent, baseline, and line height from GPUI's resolved font at the current scale factor. Remove the fixed `7.83 px` cursor math and use the same metrics for PTY column/row calculation.
4. Keep Harbor Night as the theme boundary, but add golden tests for ANSI 16, indexed, truecolor, bold/dim, inverse, underline, cursor, focused/unfocused selection, and real emitted escape bytes.
5. Paint background and cursor rectangles by terminal cell coordinates, then paint glyphs on the measured baseline. Validate at Retina and 1x/1.25x/2x scale factors on macOS and Linux.

This is the first incremental implementation to authorize. It should visibly improve type weight, alignment, baseline, cursor fit, and color contrast without touching PTY ownership, IPC, parser state, or process lifecycle.

### Phase R2: cell-correct GPU line element

- Replace ordinary row `StyledText` with a dedicated GPUI terminal-line element that preserves grid column indices.
- Batch same-style backgrounds and decorations, shape foreground glyphs through GPUI, and use GPUI's existing raster/atlas cache.
- Force terminal-cell advances where required and test wide characters, combining marks, emoji, fallback fonts, box drawing, powerline glyphs, bold/italic synthesis, and resize reflow.
- Benchmark sustained output and atlas churn across many panes before enabling optional ligatures.

### Phase R3: interaction fidelity

- Add block, beam, and underline cursors with blink and unfocused states.
- Add drag selection, word/line selection, copy, paste guards, and a visible selection palette.
- Map mouse coordinates through measured cell metrics and implement terminal mouse reporting without breaking selection modifiers.
- Implement IME composition through GPUI input handling, with pre-edit text positioned at the active cell.

### Phase R4: history and navigation

- Expose bounded Alacritty scrollback through the service protocol without copying an unbounded client buffer.
- Add wheel/trackpad scroll, scrollbar affordance, search, next/previous match, and selection across history.
- Preserve deterministic reconnect/snapshot behavior and measure high-output backpressure before calling the renderer complete.

## Acceptance gate for R1

- the selected font is genuinely monospace for tested ASCII glyphs and has an explicit fallback chain;
- measured cell metrics drive text, cursor, selection, and PTY resize consistently;
- a fixture covering ASCII, box drawing, CJK width, combining marks, emoji fallback, and ANSI color has screenshot comparisons at supported scale factors;
- no PTY PID, pane ID, parser state, or daemon/client restart behavior changes;
- the existing terminal and interaction tests remain green.

References: [GPUI `TextSystem`](https://docs.rs/gpui/latest/gpui/struct.TextSystem.html), [GPUI `FontFallbacks`](https://docs.rs/gpui/latest/gpui/struct.FontFallbacks.html), [COSMIC Text](https://pop-os.github.io/cosmic-text/cosmic_text/), [Alacritty `Term`](https://docs.rs/alacritty_terminal/latest/alacritty_terminal/term/struct.Term.html).
