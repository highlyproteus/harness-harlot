<p align="center">
  <img src="crates/desktop/assets/harnessharlot-banner.png" alt="Harness Harlot banner" width="720">
</p>

<h1 align="center">Harness Harlot</h1>

<p align="center">
  A lightweight native terminal workstation for local and SSH work,
  with tabs, splits, groups, tmux integration, and embedded browser tabs.
</p>

<p align="center">
  <a href="https://github.com/highlyproteus/harness-harlot/releases/latest">
    <img src="https://img.shields.io/badge/macOS-download-black?logo=apple&logoColor=white" alt="Download for macOS">
  </a>
  <a href="https://github.com/highlyproteus/harness-harlot/releases/latest">
    <img src="https://img.shields.io/badge/Linux-download-orange?logo=linux&logoColor=white" alt="Download for Linux">
  </a>
  <a href="https://github.com/highlyproteus/harness-harlot/releases">
    <img src="https://img.shields.io/github/v/release/highlyproteus/harness-harlot?include_prereleases&label=release" alt="Latest release">
  </a>
  <a href="https://github.com/highlyproteus/harness-harlot/actions/workflows/ci.yml">
    <img src="https://github.com/highlyproteus/harness-harlot/actions/workflows/ci.yml/badge.svg" alt="CI status">
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT License">
  </a>
</p>

---

## Install

The same command installs Harness Harlot on macOS and Linux:

```bash
curl -fsS https://harnessharlot.com/install | sh
```

The installer detects the operating system and CPU architecture automatically.
It does not require `sudo`, GitHub CLI, or a GitHub account. It verifies
website-pinned checksums, the signed update manifest, the downloaded package,
and the application before replacing anything. HTTPS content from
`harnessharlot.com` is the bootstrap trust root; website CI verifies GitHub
provenance and signed release metadata before publishing those pinned bytes.
The stable indexes do not expose arbitrary historical-tag selection, and
publication rejects version or build rollback.

On macOS, Harness Harlot installs to `/Applications` when writable and otherwise
falls back to `~/Applications`. On Linux, it installs under `~/.local`. Both
platforms create `~/.local/bin/hh`; add that directory to `PATH` once if your
shell does not already include it.

```bash
hh version
hh update --check
```

`hh update` verifies and stages updates, retains the previous application for
rollback, and relaunches Harness Harlot after a successful replacement on Linux
and packaged community macOS builds. The sidebar Update button keeps the app
open while downloading and asks once before restarting an incompatible terminal
service. Local terminals managed by the private HH tmux server resume with
their running programs and output; unsupported or failed tmux recovery falls
back to fresh shells in the last valid directories. SSH tabs stay offline until
reconnected. For the CLI, use `hh update --restart-service` to authorize that
restart, or end active terminal sessions first. Compatible service updates
preserve live shells without restarting the service.
Contributors can opt into the independently published main-branch feed with
`hh update --channel edge`.

### Linux desktop dependencies

The bootstrap requires `curl`, `python3`, GNU `sha256sum`, and `tar`; these are
present by default on supported Ubuntu installations or available from the
standard package repositories.

The desktop package uses the matching distribution runtime libraries. Browser tabs
need GTK, NSS, ALSA, and GBM:

```text
Ubuntu 22.04:  libgtk-3-0 libnss3 libasound2 libgbm1
Ubuntu 24.04+: libgtk-3-0t64 libnss3 libasound2t64 libgbm1
Fedora:        gtk3 nss alsa-lib mesa-libgbm
Arch:          gtk3 nss alsa-lib mesa
```

Linux packages target a glibc 2.35 baseline. Details: [Linux releases](docs/linux-release.md) · [macOS releases](docs/macos-release.md).

## Workstations

A workstation is a machine — your local computer or an SSH host.

- Each workstation has its own working directory. Change it from the workstation menu and every new terminal in that workstation opens there from then on.
- Rename workstations, give them their own colors, and pin the ones you use most.
- SSH workstations launch your installed OpenSSH client, so your `~/.ssh/config`, keys, agents, and host verification are always the authority. Saved SSH workstations reconnect into their saved layout; credentials are never stored.

## Terminals

- Fast native terminals with tabs, split panes, drag-to-rearrange layouts, selection/copy/paste, scrollback, and search.
- Rename any terminal tab, pick its color, or give it its own icon.
- Known agent CLIs — Codex, Claude Code, Cursor, Aider, Gemini, and more — are recognized and labeled with their official icons automatically.
- Your terminals keep running if the app closes, crashes, or updates. They live in a small local session service, so reopening the app puts you right back where you were. Ending a session is always explicit: close its tab or exit the shell.

## Groups

A group displays several terminals together in one view — and can include a browser pane alongside them — so one glance covers a whole task.

## Browser tabs

Full embedded Chromium tabs on macOS and Linux, isolated to the app's own profile directory.

## Bots

Bots are the agents you talk to. Click the robot icon next to the notifications
bell to switch the sidebar to your bots, create one with **New bot**, and pick
which installed agent CLI runs it: omp, Hermes, Claude Code, Codex, Gemini, or
another supported agent. Each bot runs its agent's own interface, so the agent's
commands, settings, and voice mode work as usual. Right-click a bot to rename
it, change its agent, restart it, or delete it. Bots survive app and service
restarts like any other local terminal.

A bot is a coordinator, not a workspace. Ask it to "spin up three worktrees and
have omp implement the plan" and it opens named worker tabs in a workstation
(by default one titled after the bot), each running a coding agent on its task.
Each bot's row in the Bots sidebar counts its workers and shows one chip per
worker tab with its live status; click a chip to jump straight to that tab.
Open the workstation to watch the workers or take over any of them yourself.
When a worker needs input or approval, the bot tells you what it is asking;
answer the bot and it relays your decision to the worker.

omp bots load the bundled Harness Harlot plugin automatically, which also
reports worker status changes into the bot's conversation. Claude Code and
Codex bots get the `hh mcp` server attached at launch. Other agents need a
one-time MCP setup; **Settings → Bots** shows the exact command. See
[Bots privacy and data handling](PRIVACY.md).

## Notifications

The bell switches the sidebar to Notifications, which lists terminal tabs and
bots by live status: **Needs you** (waiting for input or approval), then
**Running**, then **Done**, newest first within each group. Click a row to jump
to it. The bell and Dock badges count what needs you.

## Browser automation and Galleries

Harness Harlot terminals receive `HH_WORKSPACE_ID`, `HH_PANE_ID`,
`HH_GALLERY_DIR`, and `HH_CLI`. The bundled `hh` command uses that context to
control browser panes and publish images without exposing a remote network
endpoint:

```bash
hh browser open https://example.com --json
hh browser read body --pane BROWSER_PANE_ID --json
hh browser screenshot --pane BROWSER_PANE_ID --json
hh gallery add /absolute/path/to/image.png --json
hh gallery list --json
```

`hh terminal` controls worker terminals the same way (`list`, `new`, `send`,
`read`, `wait`, `focus`, `close`, `rename`). `hh mcp` exposes all of these as a
stdio MCP server. `hh skill install` installs the bundled agent instructions for
omp, Claude Code, Codex, and pi. **Settings → Bots** shows the MCP
configuration and skill installer.

Gallery images are copied into private per-workstation application storage.
Opening or importing from a terminal creates or reuses a Gallery without taking
focus from the terminal. The desktop Gallery supports file drops, previews, and
revealing the selected image in Finder or the platform file manager.

## tmux

- Local terminal panes use an HH-owned private tmux server when tmux 3.2 or
  newer is installed, preserving processes and output across service restarts.
- The managed server uses a private `hh` (`hh-dev` in development) socket and
  does not alter the user's default tmux server. A custom `HH_STATE_DIR` gets
  its own private server (`hh-<hash of the state directory>`).
- The workstation menu can still scan an explicitly requested local or remote
  tmux server and attach selected sessions as tabs. Nothing is scanned in the
  background.

## Run locally

Requirements: Rust 1.96 or newer.

```bash
git clone https://github.com/highlyproteus/harness-harlot.git
cd harness-harlot
cargo run -p hh-session-service
```

In another terminal:

```bash
cargo run -p hh-desktop
```

On macOS you can build a proper app bundle instead:

```bash
scripts/build-macos-app.sh debug      # target/debug/Harness Harlot.app
scripts/build-macos-app.sh release    # target/release/Harness Harlot.app
```

Embedded browser tabs need a CEF distribution and an app bundle (they cannot run from a bare `cargo run`):

```bash
brew install cmake ninja
export CEF_PATH="$HOME/.local/share/cef"   # unpacked CEF distribution
scripts/build-macos-app.sh release --browser
```

For side-by-side development, `scripts/build-macos-dev-app.sh` produces a separate `Harness Harlot Dev.app` with its own socket, state, and icon so it never touches your stable install.

## Keybindings

Press `Cmd-Shift-P` for the command palette with every action and binding. Optional JSON config lives at `~/.config/hh/config.json`:

```json
{
  "keybindings": {
    "app.command-palette": ["cmd-shift-p", "ctrl-b p"],
    "pane.split-down": []
  }
}
```

A configured action replaces its defaults; an empty list unbinds it.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

Harness Harlot is available under the [MIT License](LICENSE). See also
[Bots privacy and data handling](PRIVACY.md),
[security reporting](SECURITY.md), and [third-party notices](THIRD_PARTY_NOTICES.md).
