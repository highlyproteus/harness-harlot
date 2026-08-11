# Native GPU UI spike decision

## Decision

Proceed provisionally with GPUI 0.2.2 for the macOS/Linux MVP client. Keep the UI model, protocol, session service, and terminal adapter toolkit-neutral, and retain Iced/wgpu (then eframe/wgpu) as the future portability or maturity fallback. GPUI is pre-1.0, so this is a reviewed checkpoint rather than an irreversible framework lock.

## Evidence

- The desktop is a GPUI `Application`; it does not contain a WebView dependency or browser surface.
- The macOS arm64 client built, linked GPUI's Metal stack, launched as a native window, connected to the separate session service, and continued updating while panes were focused, resized, and drag-swapped.
- Runtime verification exercised keyboard focus/input, pane-local tabs, horizontal and vertical splits, divider resize, pane-targeted header actions, guarded close, rename, and directional drag-to-split. Stable pane IDs remain attached to service-owned PTYs through layout mutations.
- Each pane displays a real configured shell whose PTY, child process, terminal parser/grid, and bounded scrollback are owned by the separate service. The macOS proof typed distinct commands into three PTYs, then killed and relaunched only the desktop; layouts and output remained visible without restarting the shells.
- A clean `rust:1.96-bookworm` Linux container compiled the desktop against GPUI's native Vulkan, Wayland, and X11 dependencies. Linux runtime/compositor coverage remains required before packaging.
- Local native-window captures verified real ANSI SGR output, two-pane action targeting, guarded close, pointer-local directional previews, lone-tab replacement, and tab-strip merge. Those generated verification artifacts are intentionally excluded from source control.

## Known limits and fallback gates

- The current renderer projects styled visible runs from Alacritty's grid and maps ANSI 16-color, indexed, and truecolor foreground/background plus bold, dim, italic, underline, strike, and cursor state. Unicode-width shaping, selection, clipboard, search, mouse reporting, IME, and scrollback controls are not complete.
- Live state survives a desktop restart but is not yet journaled to disk, so a service restart still ends shells and loses the layout.
- SSH is not implemented yet.
- GPUI's pre-1.0 API and large native dependency graph increase upgrade and packaging cost.
- Linux must still pass Wayland and X11 runtime tests on real GPU drivers, and macOS must pass Spaces/display-switching soak tests.
- Accessibility, IME, text selection, glyph throughput, and a sustained 120x40 multi-pane benchmark are not proven by this spike.
- If those gates block the MVP, port this same behavior test to Iced/wgpu. The service and protocol boundary prevents that change from affecting session ownership.

## Security and ownership boundary

The UI exchanges bounded requests and screen snapshots over the versioned, owner-only local Unix socket. The GPUI view never owns a PTY, child process, SSH credential, or SSH process. No remote or browser placeholder appears in the product UI.
