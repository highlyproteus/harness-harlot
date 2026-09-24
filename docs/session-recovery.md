# Session recovery boundary

Harness Harlot keeps two intentionally different kinds of state:

- **Managed local runtime state** lives in an HH-owned tmux server on a private
  `hh` socket (`hh-dev` in development). The session service owns tmux's control
  connection and terminal projection, while tmux owns the local shell process
  and pane output. Closing or restarting only the desktop or session service
  does not terminate those managed shells.
- **Other runtime state** remains service-local: system-SSH PTYs, child handles,
  terminal projections, sockets, and input/output buffers. A service restart
  leaves SSH tabs offline until explicit reconnection.
- **Desired state** is the small on-disk recovery snapshot: stable
  workspace/tab/pane IDs, titles, split axes and ratios, active tab IDs, last
  valid local working directories, and opaque private-tmux window/pane IDs. It
  contains no environment, process handles, PIDs, credentials, or secrets.
  Optional terminal history remains a separate owner-only archive; see
  [local terminal history storage](terminal-history-storage.md).

On restart, the service validates the desired-state snapshot and reattaches each
managed local pane to its existing private tmux window. Missing tmux 3.2+,
unsupported versions, an invalid `HH_TMUX_BINARY`, or a failed/missing target
cause an explicit fallback:
the pane starts a fresh configured shell at its last valid local directory and
the desktop receives a notification. Explicitly closing a pane, tab, or
workspace terminates its managed tmux window; disconnecting a client does not.
Recovery retains windows referenced by a saved pane even if the window contains
additional tmux panes; only unreferenced windows are removed.

Protocol-changing in-app updates explain this boundary and request confirmation
before restarting the service. The updater downloads and verifies the package
before the desktop quits, then uses `--restart-service` to request shutdown and,
if necessary, SIGTERM the managed service so it can persist. Compatible updates
leave the service and live shells running without a service restart.

## Storage safety

Fresh stable installs write `sessions.json` under `~/Library/Application Support/Harness Harlot` on macOS and `$XDG_STATE_HOME/hh` (or `~/.local/state/hh`) on Linux. Development builds use the separate durable `Harness Harlot Dev` or `hh-dev` state directory by default. `HH_STATE_DIR` remains available for disposable isolated tests and packaging. The directory is mode `0700` and snapshots are mode `0600`.

Snapshots use a versioned, deny-unknown-fields schema with byte, workspace, tab, pane, nesting-depth, ID uniqueness, title, path, and split-ratio limits. Writes use a same-directory mode-`0600` temporary file, file sync, atomic replace, and directory sync. A malformed, oversized, unsupported, symlinked, or structurally invalid snapshot is moved to a restricted `sessions.corrupt-*.json` quarantine file and the service starts from a safe seeded workspace.

When a user confirms a System SSH workstation, its reconnectable desired state is written before the live SSH PTY is attached. If the live session cannot be attached or the app stops during that transition, the named workstation remains as an offline entry that can be reconnected later.

Recovery is entirely local and introduces no automatic network access, account, analytics, or telemetry behavior. Live system-SSH PTYs and process state remain daemon runtime metadata. A saved SSH workspace persists only its validated destination/config alias, user-chosen name, pin/order, offline connection status, and safe pane/tab layout. A daemon restart restores that workspace visibly offline and never reconnects it automatically; explicit reconnect starts fresh system-OpenSSH PTYs in the preserved pane positions. Passwords, private keys, agent material, SSH config contents, terminal output, and prompt responses are never part of the recovery snapshot.

An SSH tab opened directly inside a local workstation follows the same no-automatic-network rule. After a daemon restart its tab remains in the layout without a running process and is labeled `SSH <destination> — Offline; reconnect required`; it is never silently replaced by a local shell. The label preserves only the validated destination needed to explain the offline tab, not credentials or SSH configuration.
