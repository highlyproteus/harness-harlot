# All-Rust terminal renderer decision and roadmap

## Hard decision

Not a Harness will remain an all-Rust application. The daemon continues to own PTYs and `alacritty_terminal` continues to own VT parsing, grid state, and scrollback. The native GPUI client will render that grid through Rust and GPUI's GPU/text facilities. Ghostty, libghostty, GhosttyKit, and other non-Rust terminal engines are not candidates unless a future approved PDR revision explicitly reverses this decision.

This is an architecture boundary, not a judgment about Ghostty's quality. Embedding libghostty would add a Zig/C ABI build and lifecycle boundary, couple the GPUI surface to an evolving library API, and split terminal behavior across two engines while the daemon still needs a serializable canonical grid. That change is too large for a typography fix and would weaken the current Rust ownership and restart-isolation model.

## Historical pre-R1 diagnosis

At the start of R1, the PTY and terminal model were not the likely source of the visible text problem. The service already parsed real output with `alacritty_terminal` and transmitted styled visible runs. The weak link was the first-pass GPUI projection:

- it hard-codes `SF Mono`, a `13 px` size, `18 px` line height, and a `7.83 px` cell width;
- it renders each row as ordinary `StyledText`, then positions the cursor with the hard-coded width;
- it does not resolve a tested monospace family/fallback stack per platform;
- it does not derive baseline, ascent, descent, advance, or line height from the selected font;
- it flattens terminal cells into text runs, so cell occupancy, fallback glyph width, selection, and background painting are not yet authoritative.

GPUI already provides OS font discovery, font resolution, shaping, platform rasterization, subpixel variants, and a GPU sprite atlas. On Linux its text system uses `cosmic-text`; on macOS it uses the native platform text system. Not a Harness should use this existing GPUI path before adding a second rasterizer or atlas.

## R1 implementation checkpoint

The first typography slice now resolves an installed macOS/Linux monospace family through GPUI, installs explicit monospace and symbol/emoji fallbacks, and disables contextual ligatures for stable terminal cells. Measured advance, ascent, descent, baseline, and line height drive glyph placement, fixed cell spans, ANSI backgrounds, cursor geometry, and PTY viewport sizing. The Alacritty adapter now preserves each styled run's authoritative cell count, including wide-character spacer cells, while protocol-v4 clients retain a text-length fallback for older daemon snapshots.

Terminal rows use explicit no-wrap shaping. This is required because ordinary UI whitespace behavior can wrap long `ls`-style space-padded rows inside their exact cell span and then clip the wrapped fragments. Tab normalization distinguishes two protocol-v4 cases: an older daemon run without authoritative cell counts expands tabs at eight-column terminal stops, while a current terminal-model run replaces each tab glyph with one blank cell because Alacritty has already populated the intervening grid cells. Applying tab stops a second time reproduced the clipped tail of wide `ls` listings. Unicode display width remains the legacy fallback only; current model cell counts are authoritative. Generated captures remain local and excluded from source control.

The responsive follow-up now derives each pane's PTY grid from its live pixel allocation after sidebar width, workspace header, pane header, terminal padding, focus border, split dividers, and the effective local split ratio. GPUI window-bound observation pushes size changes immediately, the daemon applies the exact requested rows and columns without a second hidden clamp, and tab/control chrome shrinks independently of terminal content. Native macOS validation exercised one-pane and two-pane layouts at 720×460, 1280×820, and 1600×900. Shell-reported sizes changed from 20×62 to 39×131 to 43×171 for one pane and from 20×29/30 to 39×64/64 to 43×84/83 for two panes. Fresh directory listings selected natural one-, two-, three-, or four-column layouts for the available grid with complete filenames; real ANSI color, underline, 256-color background, and truecolor remained aligned.

This checkpoint did not claim the rest of R1/R2. Subsequent terminal-interaction work integrated selection, guarded clipboard paste, bounded scrollback and literal search, mouse reporting, and foundational IME handling. User font overrides, Linux runtime screenshots, broader scale-factor coverage, comprehensive fallback-glyph fixtures, grapheme shaping, remaining wide-cell cases, richer search, and accessibility remain open.

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

### Phase R3: interaction fidelity (foundational slice integrated)

- Add block, beam, and underline cursors with blink and unfocused states.
- Add drag selection, word/line selection, copy, paste guards, and a visible selection palette.
- Map mouse coordinates through measured cell metrics and implement terminal mouse reporting without breaking selection modifiers.
- Implement IME composition through GPUI input handling, with pre-edit text positioned at the active cell.

### Phase R4: history and navigation (bounded slice integrated)

- Bounded Alacritty scrollback, wheel/trackpad scrolling, and literal search are integrated without copying an unbounded client buffer.
- The daemon now records future PTY output into optional owner-only atomic/checksummed chunks through a bounded non-blocking queue. Reaching the top of live scrollback or missing a live search lazily loads one clearly labeled local-history page; the UI never retains a full archived session.
- Rich styled historical replay, a scrollbar affordance spanning live plus archived ranges, and selection across archive-page boundaries remain open. The current archive is an honest bounded plain-text projection of raw output, not a restored live terminal snapshot.
- Preserve deterministic reconnect/snapshot behavior and measure high-output backpressure before calling the renderer complete.

## Approved product sequence after responsive fidelity

This order is canonical and should be advanced from the repository rather than reconstructed in conversation:

1. Finish responsive terminal fidelity and text correctness, including supported scale-factor and Linux runtime coverage.
2. After a clean local checkpoint, open parallel worktree tracks for:
   - terminal interaction: selection, copy/paste, scrollback, search, Unicode/grapheme handling, and IME;
   - session reliability: current-working-directory inheritance, exit/close semantics, disk snapshots, and recovery;
   - commands and navigation: stable action IDs, remapping, command palette, pane zoom, and equalize;
   - native system-OpenSSH panes that honor existing SSH config, agent/key handling, host keys, ProxyJump, and resize.
3. Treat coding agents as ordinary terminal workloads first, then add only a thin status/resume layer that does not couple session ownership to an AI provider.
4. Only after the terminal and reliability core is mature, evaluate a privacy-focused isolated browser pane type and richer optional agent workflows.

The sequence retains four non-negotiable boundaries: the renderer and terminal engine remain all Rust; macOS and Linux are first-class; CMUX influence stays clean-room and behavior-level only; and Not a Harness is network-silent by default. SSH or a later browser/remote feature may make network connections only after an explicit user action, with no unauthenticated listener or background telemetry.

## Idle-pane performance contract

Not a Harness is a lightweight terminal workspace, not an agent harness or runtime. Terminals and agents are ordinary shell workloads, and the workspace must avoid competing with them for CPU or memory. The streaming design therefore has three attention states:

- A focused or recently attended pane remains subscribed to responsive, revision-aware deltas.
- After about 60 seconds without user attention, the daemon keeps draining and parsing the PTY into bounded history, but stops serializing and pushing live screen updates for that pane. The desktop marks its projection stale and receives only coalesced state metadata needed for correctness.
- Selecting a stale pane requests one immediate current snapshot before it is rendered, then resumes live deltas. Normal frequent tab switching stays on the recent path and must not introduce visible waiting.

Idle never means disconnected: local shells and system-SSH processes continue running, their PTYs continue draining, and output stays available within the documented history bound. Revision gaps also force a deterministic fresh snapshot. Protocol decoding, update preparation, and terminal parsing must not run on the UI thread; only bounded paint-ready data crosses into it.

Acceptance targets must be measured before implementation is called complete: per-pane serialized bytes and update rate, UI-thread time, focus-to-fresh-frame latency, daemon/client CPU, memory bounds, and behavior under high output, slow clients, multiple idle panes, reconnect, and revision gaps. Diagnostics are transparent and content-safe: they may expose revisions, queue depth, byte counts, timings, and stale/subscribed state, but never terminal text, keys, SSH secrets, or background telemetry.

## Acceptance gate for R1

- the selected font is genuinely monospace for tested ASCII glyphs and has an explicit fallback chain;
- measured cell metrics drive text, cursor, selection, and PTY resize consistently;
- a fixture covering ASCII, box drawing, CJK width, combining marks, emoji fallback, and ANSI color has screenshot comparisons at supported scale factors;
- no PTY PID, pane ID, parser state, or daemon/client restart behavior changes;
- the existing terminal and interaction tests remain green.

References: [GPUI `TextSystem`](https://docs.rs/gpui/latest/gpui/struct.TextSystem.html), [GPUI `FontFallbacks`](https://docs.rs/gpui/latest/gpui/struct.FontFallbacks.html), [COSMIC Text](https://pop-os.github.io/cosmic-text/cosmic_text/), [Alacritty `Term`](https://docs.rs/alacritty_terminal/latest/alacritty_terminal/term/struct.Term.html).
