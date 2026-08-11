# Session recovery boundary

Rust Mux keeps two intentionally different kinds of state:

- **Runtime state** lives only in the session service: PTY masters, child handles and PIDs, terminal grids and output, environment, sockets, and input/output buffers. A connected desktop is only a projection of this state. Disconnecting or restarting the desktop does not terminate a shell.
- **Desired state** is the small on-disk recovery snapshot: stable workspace/tab/pane IDs, titles, split axes and ratios, active tab IDs, and the last valid local working directory. It contains no terminal output, environment, process/PTY handles, PIDs, agent sockets, credentials, or secrets.

A service restart cannot preserve arbitrary live processes. Rust Mux instead validates the desired-state snapshot, starts a fresh configured shell for each pane at the last valid local CWD (falling back to the user's home directory), preserves the safe layout metadata, and labels the pane `recovered with a fresh shell`. This is recovery, not seamless process continuation.

Natural child exits remain in the layout with their final terminal grid and an `exited` label. Explicit pane close is different: the service marks the pane as terminating, requests termination when needed, waits until exit is observed, and only then removes the runtime pane and collapses its layout branch. A client disconnect performs neither transition.

## Storage safety

The service writes `sessions.json` under `~/Library/Application Support/Rust Mux` on macOS and `$XDG_STATE_HOME/rust-mux` (or `~/.local/state/rust-mux`) on Linux. `RUST_MUX_STATE_DIR` is available for isolated tests and packaging. The directory is mode `0700` and snapshots are mode `0600`.

Snapshots use a versioned, deny-unknown-fields schema with byte, workspace, tab, pane, nesting-depth, ID uniqueness, title, path, and split-ratio limits. Writes use a same-directory mode-`0600` temporary file, file sync, atomic replace, and directory sync. A malformed, oversized, unsupported, symlinked, or structurally invalid snapshot is moved to a restricted `sessions.corrupt-*.json` quarantine file and the service starts from a safe seeded workspace.

Recovery is entirely local and introduces no network access, account, analytics, or telemetry behavior. Live system-SSH panes and their host aliases remain daemon runtime metadata and are removed from the persisted projection. If no local pane remains, Rust Mux retains the previous complete local snapshot rather than serializing remote intent. A daemon restart therefore never reconnects SSH automatically; the user must repeat the desktop client's explicit review-and-confirm action.
