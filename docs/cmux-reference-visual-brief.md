# CMUX-reference visual brief

This brief derives observable layout and interaction direction from the public cmux README screenshots and documentation. It does not authorize reuse of cmux's GPL source code, logo, icons, image assets, product text, or branding. Rust Mux uses original GPUI code and simple text/geometry primitives.

## Whole-window hierarchy

- Treat the native window as one terminal workspace, not a dashboard. Chrome is a thin integrated titlebar/tool row; everything below is either the narrow workspace rail or terminal surface.
- Keep the left rail compact and resizable within the product's 150–420 px bounds. Its top row aligns tiny navigation/new-workspace controls with the macOS traffic lights. Below it, show a dense workspace-and-terminal hierarchy without a generic directory subtitle.
- The selected workspace is one restrained bright-blue rounded rectangle. Unselected workspaces sit directly on charcoal with no card border, badge block, metric, or health panel.
- The right side has a compact workspace title strip, then an edge-to-edge pane tree. There is no marketing header, global dashboard tab bar, status card, footer, or floating overlay.

## Workspace rail

- Use system UI text around 12–13 px with roughly 8–10 px horizontal insets and tight 8–12 px vertical rhythm.
- Each workspace is a standalone row with a compact terminal-tab count. Expanding it shows visually independent indented terminal rows with their actual identity icon and tab name. Local shells do not need a status badge or generic path line.
- New workspace is a tiny top-strip action. Selection and keyboard focus use the same blue family; inactive metadata remains low contrast.

## Terminal panes and per-pane tabs

- One terminal starts full-bleed in the right side. Splits subdivide the same surface with 1 px muted lines and larger invisible hit targets; panes are not rounded cards and have no outer gutters.
- Every pane begins with a 28–30 px local tab/control strip. Tabs belong to that pane, not to a large global tab bar. Keep the terminal marker and title at the left; new-tab and split-right/split-down controls sit at the edge in subdued gray.
- The active pane gets a restrained blue accent line or control tint, not a glowing card outline. Drag/drop and directional focus preserve the pane/session identity.
- Repeated splits produce a dense tiled tree. Dividers move immediately under pointer drag; no animated dashboard transitions are needed.

## Terminal surface

- Use a near-black, slightly warm canvas, monospaced text, compact line height, and no fake prompt or sample output. Every visible line must come from the daemon-owned terminal model.
- Shell output owns the contrast hierarchy. Chrome is darker/subtler than terminal text; green and blue are accents, not large fills except selected workspace.
- Empty state is the real configured shell prompt. A new pane is never represented by instructional placeholder copy.

## Interaction feel

- `⌘N` creates a workspace with a real shell, `⌘T` adds a real tab to the focused pane, `⌘D` splits right, and `⇧⌘D` splits down. Directional focus remains `⌥⌘` plus arrow keys.
- Clicking a workspace, pane, or pane-local tab activates it directly. Input routes only to the focused daemon-owned PTY.
- UI motion should be immediate and restrained: cursor changes on divider hover, direct resize feedback, and simple selection color changes.

## Reference and license boundary

Observed from the public [cmux repository README](https://github.com/manaflow-ai/cmux) and its published `main-first-image.png` and `vertical-horizontal-tabs-and-splits.png` screenshots. Those images remain external reference material and are not included in this repository. cmux is GPL-3.0-or-later; copying its source or bundled creative assets would impose obligations that this behavior-level brief deliberately avoids.
