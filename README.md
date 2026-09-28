<p align="center">
  <img src="crates/desktop/assets/harnessharlot-banner.png" alt="Harness Harlot banner" width="720">
</p>

<h1 align="center">Harness Harlot</h1>

<p align="center">
  A lightweight native terminal workstation for local and SSH work,
  with tabs, splits, nested workstations, tmux integration, and embedded browser tabs.
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

A workstation is where your terminals live: a folder of tabs on your own
computer or on an SSH host. The hammer in the sidebar toolbar shows them.

- **This Mac** (**This Computer** on Linux) is your home workstation. You can rename, recolor, and pin it or give it a root folder, but it can never be deleted.
- Add more local workstations whenever you like. Each has a root folder, set from the workstation menu (**Set Root Folder…**); new tabs, the first terminal of a new workstation, and the first terminal after all its tabs close open there. Split panes keep following the directory of the pane they split from.
- Workstations nest up to four levels deep: choose **New Workstation Inside…** on a workstation's menu. A nested workstation runs on the same machine as its parent and uses its parent's root folder until you give it its own. A collapsed workstation's card wears one status border for itself and everything inside it (needs you, then done, then working). Deleting a workstation also removes the workstations nested inside it.
- Drag a tab onto another workstation on the same machine to move it there.
- Rename workstations, give them their own colors, and pin the ones you use most; drag to reorder them among their siblings.
- SSH workstations launch your installed OpenSSH client, so your `~/.ssh/config`, keys, agents, and host verification are always the authority. Saved SSH workstations reconnect into their saved layout, together with the workstations nested inside them; credentials are never stored.
- Projects from earlier versions become nested workstations on upgrade, keeping their folder, title, color, icon, tabs, and running terminals.

## Terminals

- Fast native terminals with tabs, split panes, drag-to-rearrange layouts, selection/copy/paste, scrollback, and search.
- Paste or drop images into terminals: apps that support kitty paste events (OSC 5522), such as omp, receive the image directly; others get the PNG's path.
- Rename any terminal tab, pick its color, or give it its own icon.
- Known agent CLIs are recognized and labeled with their official icons automatically: omp, Pi, Claude Code, Codex, Cursor, Gemini, OpenCode, Amp, Qwen Code, Grok Build, Kimi Code, Antigravity, Kiro, Mistral Vibe, Crush, goose, Cline, Auggie, Continue, Droid, Kilo Code, Aider, Hermes, and GitHub Copilot CLI. See [terminal identity](docs/terminal-identity.md).
- Your terminals keep running if the app closes, crashes, or updates. They live in a small local session service, so reopening the app puts you right back where you were. Ending a session is always explicit: close its tab or exit the shell.
- On macOS, programs in your terminals get Harness Harlot's Screen Recording and Accessibility permissions, even after the window closes. **Settings → Permissions** shows and requests them.

## Tabs and splits

A tab can hold several terminals side by side — and a browser pane alongside them — so one glance covers a whole task. Drag panes between tabs to rearrange them.

## Browser tabs

Full embedded Chromium tabs on macOS and Linux, isolated to the app's own profile directory.

## Bots

Bots are the agents you talk to. The sidebar toolbar's hammer shows your
workstations; click the robot beside it to switch the sidebar to your bots,
create one with the ＋ in the Bots header, and pick
which installed agent CLI runs it: omp, Pi, Hermes, Claude Code, Codex, Gemini, or
any other recognized agent found in your shell's `PATH`. Each bot runs its agent's own interface, so the agent's
commands, settings, and voice mode work as usual. Right-click a bot to rename
it, change its agent, restart it, set its home folder, or delete it. Bots
survive app and service restarts like any other local terminal.

Each bot runs in its own home folder, by default a private folder in the app's
state directory. On every launch Harness Harlot writes an `AGENTS.md` there with
the coordinator instructions, the bot's name, its project folder, and your
instructions, so every agent that reads `AGENTS.md` (omp, Claude Code, Codex,
Hermes, and others) knows it is a bot and how to drive Harness Harlot with the
`hh` CLI. The optional **Project folder** chosen when you create a bot is where
its workers open by default; the home folder is only for the bot's notes. A
custom home folder never gets an `AGENTS.md` over one that you wrote yourself.

A bot is a coordinator. Ask it to "spin up three worktrees and have omp
implement the plan" and it opens named worker tabs in a workstation (by default
one titled after the bot), each running a coding agent on its task. Open the
workstation to watch the workers or take over any of them yourself.
When a worker needs input or approval, the bot tells you what it is asking;
answer the bot and it relays your decision to the worker.

Each bot is its own space with thread tabs you can split and rearrange like a
workstation: its card in the Bots sidebar looks like a workstation card, and
each open conversation (thread) is a tab. Click ＋ on the bot's card for a new
thread tab. omp bots list their saved threads below the open ones, pinned
first and newest next; click one to reopen it with its full history, or
right-click it to pin it. Up to five threads stay open; older idle ones close
and stay saved. Starting `/new` or `/resume` inside omp shows up as that tab's
thread, and each thread hears only about the workers it opened.

omp bots load the bundled Harness Harlot plugin automatically, which also
reports worker status changes into the bot's conversation. Claude Code and
Codex bots get the `hh mcp` server attached at launch. Other agents need a
one-time MCP setup for the tools; **Settings → Bots** shows the exact command.
Claude Code and Codex may ask once to trust the bot's folder. See
[Bots privacy and data handling](PRIVACY.md).

## Status and notifications

Every terminal tab shows what its agent is doing with its border — in the
sidebar rows, the tab strip, pane headers, and the sidebar's pane chips. A
bright segment runs clockwise around the border; with **Reduce motion** on
(macOS Accessibility, or GNOME's animations switched off) the border keeps its
colour and a steady glow instead.

- **Blue** — the agent is working. Agents that keep a task list fill the
  border from the top-left corner as tasks complete (a dim blue track shows
  the rest); hover the tab for "3 of 7 done" and the current task. Claude Code
  and Codex show the fill while their task list is unfinished.
- **Magenta** — the agent needs you: it asks for input or approval, or rang
  the terminal bell.
- **Green** — the agent finished while you were elsewhere. It stays until you
  look: click the tab or pane, switch to its tab or workstation, click its
  notification, or type into it. Merely having the window in front does not
  clear it, and it survives restarts.
- No border — idle, or finished and already seen.

Where one border stands for several tabs — a collapsed workstation card, a
bot's icon on its card, and the toolbar's Bots button — it shows the most
urgent: magenta, then green, then blue.

The bell switches the sidebar to Notifications: **Needs you** and **Running**
list live terminal tabs and bots, and **Recent** keeps the last 200 finished
and attention notifications across restarts, newest first, unread ones marked
with a blue dot. **Mark all read** clears them; clicking any row jumps to its
pane and marks it seen, and viewing a pane marks its notifications read. The
bell counts unread notifications: magenta when one of them needs you, blue
otherwise. The Dock icon shows the same count in macOS's standard red badge.

### Task progress from agents

Task progress is opt-in per agent. Enable it in **Settings → Bots → Agent
task progress**, or from a terminal:

```bash
hh progress install omp      # omp extension in ~/.omp/agent/extensions (or $PI_CODING_AGENT_DIR)
hh progress install claude   # task tools hook in ~/.claude/settings.json (or $CLAUDE_CONFIG_DIR)
hh progress install codex    # update_plan hook in ~/.codex/hooks.json (or $CODEX_HOME)
hh progress status           # add --json for machine-readable output
hh progress uninstall claude
```

Installing keeps every other setting and hook, writes through a symlinked
settings file, and never replaces an omp extension file you edited. The Claude
Code hook follows `TaskCreate`/`TaskUpdate` (reading the session's task list)
and the older `TodoWrite`; subagents' lists never replace the pane's. Codex asks you to trust the new hook the next time
it starts. omp and Claude Code bots report progress without any setup. The
integrations call `hh progress report --done N --total M [--current T]
[--phase T] --source omp|claude|codex` and `hh progress clear` for the pane in
`HH_PANE_ID` (or `--pane`); tasks an agent abandons do not count.

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
  newer is installed, preserving processes and output across service restarts
  and app updates. Only closing a tab ends its program.
- SSH workstation terminals run in HH's tmux on the remote host, so they
  survive dropped connections and restarts; Reconnect reattaches them with
  full scrollback. The login must work without a prompt (known host key, key
  in ssh-agent or the Keychain); otherwise a **Sign in to** *host* tab asks
  once. See [session recovery](docs/session-recovery.md#ssh-workstations).
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
