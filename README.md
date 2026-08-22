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

On Linux, `hh update` verifies and stages updates, retains the previous
application for rollback, and relaunches Harness Harlot after a successful
replacement. Community macOS builds intentionally use a notify-only update
policy: `hh update --check` reports a newer release, and rerunning the install
command performs the verified manual replacement. End active terminal sessions
before an update that changes the session-service protocol. Contributors can
opt into the independently published main-branch feed with
`hh update --channel edge`.

### Linux desktop dependencies

The bootstrap requires `curl`, `python3`, GNU `sha256sum`, and `tar`; these are
present by default on supported Ubuntu installations or available from the
standard package repositories.

Browser tabs require the matching distribution packages:

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
- Optional terminal history archive: beyond live scrollback, an opt-in owner-only disk archive lets explicit searches reach older output, with quotas and retention you control.

## Groups

A group displays several terminals together in one view — and can include a browser pane alongside them — so one glance covers a whole task.

## Browser tabs

Full embedded Chromium tabs on macOS and Linux, isolated to the app's own profile directory.

## tmux

- Scan the local or remote tmux server from the workstation menu and open selected sessions as tabs.
- Sessions attach exactly like a hand-run `tmux attach-session`: tmux stays in charge of its own windows and panes, and detaching leaves the session running on the server.
- Nothing is scanned in the background — only when you ask.

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

Harness Harlot is available under the [MIT License](LICENSE).
