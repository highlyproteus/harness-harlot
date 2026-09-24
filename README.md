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
need GTK, NSS, and GBM; Voice Mode needs ALSA:

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

## Assistant and Voice Mode

Assistant panes run a service-owned `pi --mode rpc` orchestrator. Typed messages
and image attachments go to the model provider configured in pi. The bundled
extension gives pi only HH workspace tools: it can list and create workstations,
windows, terminals, and browsers; read, send to, wait for, close, and focus
panes; and launch a configured coding-agent command in a terminal. `Full`
access auto-allows guarded actions; `Confirm` shows an inline Allow/Deny card
before sending input, closing a pane, or launching a command.
Choose `Full` or `Confirm` in the Assistant header; this setting survives service
restarts. With the composer inactive, Enter allows the focused pane's pending
action and Escape denies it. While composing, these keys retain their normal
submit/close behavior; typing `y` or `n` never approves or denies an action.
Prompt drafts and attachments remain in the composer until submission succeeds.

Voice Mode is optional and uses the OpenAI Realtime API as a relay. The
microphone remains off until you use the visible start-voice control. Final
speech transcripts are forwarded to pi, and only bounded final orchestrator
updates are sent back to Realtime for speech. The Realtime session receives no
HH tools and cannot authorize actions.
Replies received while the speaker is muted or voice is suspended are skipped,
not replayed when listening resumes. Spoken transcripts stay attached to the
Assistant entry being voiced even as older entries leave the visible history.

OpenAI credentials are not saved to the settings file. Set
`HH_OPENAI_API_KEY` in the launch environment. pi manages credentials for its
selected model provider. See [Assistant and Voice Mode privacy and data
handling](PRIVACY.md) for provider and local-storage boundaries.

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

`hh mcp` exposes the same operations as a stdio MCP server. `hh skill install`
installs the bundled agent instructions for Claude Code, Codex, and pi. The
Assistant settings show the MCP configuration and skill installer.

Gallery images are copied into private per-workstation application storage.
Opening or importing from a terminal creates or reuses a Gallery without taking
focus from the terminal. The desktop Gallery supports file drops, previews, and
revealing the selected image in Finder or the platform file manager.

## tmux

- Local terminal panes use an HH-owned private tmux server when tmux 3.2 or
  newer is installed, preserving processes and output across service restarts.
- The managed server uses a private `hh` (`hh-dev` in development) socket and
  does not alter the user's default tmux server.
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
[Voice Mode privacy and data handling](PRIVACY.md),
[security reporting](SECURITY.md), and [third-party notices](THIRD_PARTY_NOTICES.md).
