# Session recovery boundary

Harness Harlot keeps these kinds of state apart:

- **Managed local runtime state** lives in an HH-owned tmux server on a private
  `hh` socket (`hh-dev` in development). The session service owns tmux's control
  connection and terminal projection, while tmux owns the local shell process
  and pane output. Closing or restarting only the desktop or session service
  does not terminate those managed shells. A custom `HH_STATE_DIR` gets its own
  private server, named from a hash of that state directory.
- **Managed remote runtime state** lives in an HH-owned tmux server on each SSH
  host, on the same socket name (`tmux -L hh` there too), one session per
  workstation (`hh-<workstation id>`). The service drives it in control mode
  over `ssh`, exactly like the local server. See [SSH workstations](#ssh-workstations).
- **Other runtime state** remains service-local: fallback plain PTYs (no tmux),
  terminal projections, sockets, and input/output buffers.
- **Desired state** is the small on-disk recovery snapshot: stable
  workspace/tab/pane IDs, titles, split axes and ratios, active tab IDs, last
  valid local working directories, and opaque private-tmux window/pane IDs. It
  contains no environment, process handles, PIDs, credentials, or secrets.
  Terminal output is never archived to disk; a `history/` directory left by an
  earlier build's optional archive is deleted when the service starts.

## What ends a terminal

Only closing its tab (or its workstation) ends a terminal's program: its tmux
window is killed, locally or on the SSH host. Quitting or updating the app,
restarting the service, disconnecting a workstation, and a dropped connection
all leave the program running in tmux. A tab closed while its SSH host was
unreachable has its window killed when the workstation next connects.

Every window HH creates carries a `@hh-pane` tag naming its pane, so a window
can be found without its saved ids and windows of closed tabs can be told apart
from live ones.

## Reattaching after a restart

On restart, the service validates the desired-state snapshot and reattaches each
managed local pane to its private tmux window, found by its saved ids or else
by its tag, and rebuilds the terminal from the window's whole scrollback. That
read may take up to 30 seconds; if only the scrollback cannot be read, the
window is attached with a blank screen and its program redraws on the next
output or resize. A failed attach is retried once on a fresh tmux connection.

Recovery never kills or covers a saved window. If one still cannot be attached,
the pane stays in the layout marked as not reattached, its window and ids are
kept, a notification says its program is still running, and **Reattach Exited
Terminal** (or the next restart) tries again. Only windows that no saved or live
pane refers to, by id or by tag, are removed: those belong to tabs closed while
the service could not act on them. A pane whose saved window is gone gets a new
window at its last valid local directory. Missing tmux 3.2+, unsupported
versions, an invalid `HH_TMUX_BINARY`, or no working tmux connection cause an
explicit fallback: the pane starts a fresh configured shell and the desktop
receives a notification. If recovery must stop part-way, it ends only the plain
shells it started; tmux windows are released, never killed.

If the service loses its connection to the local tmux server while running, it
reconnects within a few seconds and every pane carries on; nothing is reported
as exited unless its window is really gone.

`<state>/recovery.log` (owner-only, truncated past 512 KiB) records how each
pane came back, windows removed for closed tabs, and re-established
connections. It holds ids, timings, and error text, never terminal output.

Protocol-changing in-app updates explain this boundary and request confirmation
before restarting the service. The updater downloads and verifies the package
before the desktop quits, then uses `--restart-service` to request shutdown and,
if necessary, SIGTERM the managed service so it can persist. Compatible updates
leave the service and live shells running without a service restart.

## macOS privacy attribution

macOS grants Screen Recording and Accessibility to a process's *responsible
process*, inherited from the app that launched it. When that app process
exits, its descendants become responsible for themselves and lose the app's
permissions. The desktop therefore starts the session service through
`hh session-host`: the app's own `hh` binary, spawned with responsibility
disclaimed (`responsibility_spawnattrs_setdisclaim`) in a new session. The host
runs `hh-service` as a child and, after it exits, stays alive while any process
it is responsible for still runs, which includes the detached private tmux
server and every shell in it. Terminals therefore keep the app's grants
across window restarts and compatible updates.

A tmux server started by an older service, or by a host from a different app
build, keeps that attribution. **Settings → Permissions** detects this by
comparing the code hash of the service's and tmux server's responsible
processes (found through `LOCAL_PEERPID` on their sockets) with the running
app's; paths are not enough because a rebuilt or updated bundle reuses them.
It explains that such terminals keep their earlier permissions until their
programs restart, without a toolbar warning, and offers **Restart Terminals**
behind a warning: SIGTERM the service so it persists the layout, SIGTERM the
tmux server (ending every program in it), then start a new host. Panes reopen
through the fallback described above.

macOS keys each grant to the exact build it was given to. An unnotarized
build differs on every update, so an older build's entry keeps its switch in
System Settings while silently blocking the new build's prompt. The first
**Allow…** click therefore runs `tccutil reset <service> <bundle id>` for the
missing permission before asking, which removes only entries that no longer
work for this build.

## Storage safety

Fresh stable installs write `sessions.json` under `~/Library/Application Support/Harness Harlot` on macOS and `$XDG_STATE_HOME/hh` (or `~/.local/state/hh`) on Linux. Development builds use the separate durable `Harness Harlot Dev` or `hh-dev` state directory by default. `HH_STATE_DIR` remains available for disposable isolated tests and packaging. The directory is mode `0700` and snapshots are mode `0600`.

Snapshots use a versioned, deny-unknown-fields schema with byte, workspace, tab, pane, nesting-depth, ID uniqueness, title, path, and split-ratio limits. Writes use a same-directory mode-`0600` temporary file, file sync, atomic replace, and directory sync. A malformed, oversized, unsupported, symlinked, or structurally invalid snapshot is moved to a restricted `sessions.corrupt-*.json` quarantine file and the service starts from a safe seeded workspace.

## SSH workstations

Each SSH workstation's terminals are windows of HH's tmux server on the host.
The service connects with `ssh -T -o BatchMode=yes` (so ssh never prompts) and
a fixed POSIX `sh` bootstrap that finds tmux 3.2 or newer (on `PATH`, then
Homebrew, `/usr/local/bin`, `/usr/bin`, `~/.local/bin`, Linuxbrew) and attaches
in control mode, starting the server with no config file so the user's own
`~/.tmux.conf` cannot change HH's panes; HH then applies its bundled settings.
Output, input, resize, and the full scrollback on reattach work as they do
locally. Pastes are sent as keys, since the host cannot read a local file.
Remote windows get `HH_PANE_ID` and terminal settings; local-only variables such
as the service socket are not passed. Direct SSH tabs inside a local workstation
use the same mechanism, in that workstation's session on their host.

A lost connection (ssh notices within about 30 seconds) marks the workstation
offline; its programs keep running. **Reconnect** reattaches every tab with its
scrollback. After a service restart a saved SSH workstation is restored offline
and never reconnects automatically; the user clicks Reconnect.

Control mode uses ssh's stdin and stdout for the tmux protocol, so the login
must work without a prompt: the host key is already known and a key in
ssh-agent or the macOS Keychain (or an unencrypted key, or Tailscale SSH) signs
in. When it does not, the workstation's first tab becomes **Sign in to**
*host*: an interactive `ssh -tt -o ControlMaster=yes -o ControlPersist=yes`
where the user answers ssh's prompts. It leaves a shared connection under
`/tmp/hh-ssh-<uid>/` that later connections reuse (`ControlMaster=no` with that
`ControlPath`) without prompting until the network drops, after which the next
Reconnect signs in again. A host without tmux 3.2+ falls back to plain SSH
shells that end with the connection, and a notification explains how to keep
them running.

When a user confirms a System SSH workstation, its reconnectable desired state is written before the live SSH connection is attached. If the connection cannot be attached or the app stops during that transition, the named workstation remains as an offline entry that can be reconnected later.

Recovery introduces no automatic network access, account, analytics, or telemetry behavior. A saved SSH workspace persists only its validated destination/config alias, user-chosen name, pin/order, offline connection status, and safe pane/tab layout. Passwords, private keys, agent material, SSH config contents, terminal output, and prompt responses are never part of the recovery snapshot.

An SSH tab opened directly inside a local workstation follows the same no-automatic-network rule. After a daemon restart its tab remains in the layout labeled `SSH <destination> — Offline; reconnect required` until it is reattached; it is never silently replaced by a local shell. The label is dropped once the tab is connected again.
