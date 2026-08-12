# Terminal theme architecture

Not a Harness ships with one original built-in theme, **Harbor Night**. It defines the desktop chrome, terminal foreground/background, focus and selection treatment, cursor, danger state, and the 16-color ANSI base palette in one `AppTheme` value. Indexed xterm colors and terminal-provided truecolor are resolved through the same interface, so additional built-in or user themes can be added without changing PTY, parser, protocol, or layout code.

The visual checkpoint uses compact SF Mono terminal text, cool near-black surfaces, restrained blue focus, a translucent block cursor, and clear but subdued ANSI accents. These choices were informed by observing the locally installed Zed application's typography, contrast, and editor restraint. Not a Harness does not reuse Zed source, palette files, branding, icons, or assets.

Shells and programs remain responsible for emitting ANSI SGR sequences. The Alacritty terminal engine parses those sequences in the session service; the protocol carries styled runs; and the restartable GPUI client maps terminal colors and attributes through the selected theme. This keeps terminal semantics out of the renderer and preserves the daemon ownership boundary.

Implemented in this checkpoint:

- default, ANSI 16-color, indexed 256-color, and RGB foreground/background mapping;
- bold/bright, dim, italic, underline, strike, inverse, and hidden-cell handling;
- themed cursor, selected tab, and focused-pane treatment;
- regression coverage for SGR attributes, truecolor, cursor state, and theme mapping.

Still open: Unicode-width and shaping hardening, selection/copy, scrollback UI, search, mouse reporting, IME, and user-configurable theme loading. The hard all-Rust renderer decision and phased implementation gate are recorded in [the terminal renderer roadmap](terminal-renderer-roadmap.md).
