<p align="center">
  <img src="crates/desktop/assets/harnessharlot-banner.png" alt="Harness Harlot banner" width="720">
</p>

<h1 align="center">Harness Harlot</h1>

<p align="center">
  A lightweight native terminal workstation for local and SSH work, with embedded Chromium browser tabs.
  <br>
  Terminals live in a persistent background service, so closing the app never kills your sessions.
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

## What it does

- **Workstations.** A workstation is a machine: your local computer or an SSH host. Each workstation has its own working directory — change it from the workstation menu and every new terminal in that workstation opens there from then on. Rename workstations, recolor them, and pin the ones you use most.
- **Terminals, tabs, and splits.** Fast native terminals with tabs, split panes, drag-to-rearrange layouts, selection/copy/paste, scrollback, and search. Rename any terminal, give it its own color, and known agent CLIs (Codex, Claude Code, Cursor, Aider, and more) are labeled with their icons automatically.
- **Groups.** A group displays several terminals together in one view — and can include a browser pane alongside them — so one glance covers a whole task.
- **Browser tabs.** Full embedded Chromium tabs on macOS and Linux, isolated to the app's own profile directory.
- **tmux integration.** Scan the local or remote tmux server from the workstation menu and open selected sessions as tabs, attached exactly like a hand-run `tmux attach-session`. tmux stays in charge of its own windows and panes; nothing is scanned in the background.
- **SSH the safe way.** SSH workstations launch your installed OpenSSH client — your `~/.ssh/config`, keys, agents, and host verification are always the authority. Saved SSH workstations reconnect into their saved layout; credentials are never stored.
- **Sessions outlive the window.** The desktop app is just a view. Close it, reopen it, or restart it — the service keeps your terminals running and the layout comes right back.
- **Optional terminal history archive.** Beyond the live scrollback, an opt-in owner-only disk archive lets you scroll and search older output, with explicit quotas and retention you control.

## Install

### macOS

Download the community DMG installer from the [latest release](https://github.com/highlyproteus/harness-harlot/releases/latest), or install from the terminal with verified provenance:

```bash
gh release download --repo highlyproteus/harness-harlot --pattern install-community-macos.sh
gh attestation verify install-community-macos.sh --repo highlyproteus/harness-harlot
chmod +x install-community-macos.sh
./install-community-macos.sh --acknowledge-unnotarized
```

Community builds are integrity-signed but not Apple-notarized, so the first launch may need **System Settings → Privacy & Security → Open Anyway**. In-app update checks report new releases; rerun the installer to update.

### Linux

Download the architecture-matched `.tar.gz` from the [latest release](https://github.com/highlyproteus/harness-harlot/releases/latest), verify its GitHub build-provenance attestation, extract it, and run:

```bash
./Harness-Harlot/install.sh
```

No `sudo` required — everything installs under `~/.local`. Built on Ubuntu 22.04 (glibc 2.35 baseline) for x86_64 and arm64. Browser tabs need these distribution packages:

```text
Ubuntu 22.04:  libgtk-3-0 libnss3 libasound2 libgbm1
Ubuntu 24.04+: libgtk-3-0t64 libnss3 libasound2t64 libgbm1
Fedora:        gtk3 nss alsa-lib mesa-libgbm
Arch:          gtk3 nss alsa-lib mesa
```

Browser tabs run under X11, including XWayland on the default Ubuntu, Fedora, and Arch Wayland sessions. The in-app update button verifies, stages, and swaps the install with rollback. Details: [Linux releases](docs/linux-release.md).

## Run locally

Requirements: Rust 1.96 or newer.

```bash
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
