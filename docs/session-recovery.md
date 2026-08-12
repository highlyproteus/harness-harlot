# Session recovery boundary

Not a Harness keeps two intentionally different kinds of state:

- **Runtime state** lives only in the session service: PTY masters, child handles and PIDs, terminal grids and output, environment, sockets, and input/output buffers. A connected desktop is only a projection of this state. Disconnecting or restarting the desktop does not terminate a shell.
- **Desired state** is the small on-disk recovery snapshot: stable workspace/tab/pane IDs, titles, split axes and ratios, active tab IDs, and the last valid local working directory. It contains no terminal output, environment, process/PTY handles, PIDs, agent sockets, credentials, or secrets. Optional terminal output lives in a separate owner-only history archive with independent settings, integrity, retention, clear, and lazy-load behavior; see [local terminal history storage](terminal-history-storage.md).

A service restart cannot preserve arbitrary live processes. Not a Harness instead validates the desired-state snapshot, starts a fresh configured shell for each pane at the last valid local CWD (falling back to the user's home directory), preserves the safe layout metadata, and labels the pane `recovered with a fresh shell`. This is recovery, not seamless process continuation.

Natural child exits remain in the layout with their final terminal grid and an `exited` label. Explicit pane close is different: the service marks the pane as terminating, requests termination when needed, waits until exit is observed, and only then removes the runtime pane and collapses its layout branch. A client disconnect performs neither transition.

## Storage safety

Fresh stable installs write `sessions.json` under `~/Library/Application Support/Not a Harness` on macOS and `$XDG_STATE_HOME/nah` (or `~/.local/state/nah`) on Linux. Development builds use the separate durable `Not a Harness Dev` or `nah-dev` state directory by default. `NAH_STATE_DIR` remains available for disposable isolated tests and packaging. The directory is mode `0700` and snapshots are mode `0600`.

Snapshots use a versioned, deny-unknown-fields schema with byte, workspace, tab, pane, nesting-depth, ID uniqueness, title, path, and split-ratio limits. Writes use a same-directory mode-`0600` temporary file, file sync, atomic replace, and directory sync. A malformed, oversized, unsupported, symlinked, or structurally invalid snapshot is moved to a restricted `sessions.corrupt-*.json` quarantine file and the service starts from a safe seeded workspace.

Recovery is entirely local and introduces no automatic network access, account, analytics, or telemetry behavior. Live system-SSH PTYs and process state remain daemon runtime metadata. A saved SSH workspace persists only its validated destination/config alias, user-chosen name, pin/order, offline connection status, and safe pane/tab layout. A daemon restart restores that workspace visibly offline and never reconnects it automatically; explicit reconnect starts fresh system-OpenSSH PTYs in the preserved pane positions. Passwords, private keys, agent material, SSH config contents, terminal output, and prompt responses are never part of the recovery snapshot.
