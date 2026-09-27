# Automatic terminal identity

Harness Harlot gives ordinary terminal tabs a small, local identity hint without becoming an agent harness. Hermes Agent, omp, Pi, Codex CLI, Claude Code, Droid, Kilo Code, Cursor, OpenCode, Aider, GitHub Copilot CLI, Gemini CLI, Amp, Qwen Code, Grok Build, Kimi Code CLI, Antigravity CLI, Kiro CLI, Mistral Vibe, Crush, goose, Cline CLI, Auggie CLI, and Continue CLI have full product labels and bounded local process detection. Where an official redistributable or installed product asset is available, the unchanged icon appears beside the label. Everything else uses a neutral `>_` terminal glyph.

GitHub Copilot CLI currently uses the neutral glyph because its official public CLI repository and npm package expose no standalone icon asset. Harness Harlot does not substitute a third-party logo or a lookalike.

## Precedence and correction

Identity resolution is deterministic:

1. An explicit user rename wins.
2. A user-selected terminal profile wins over detection.
3. A verified exact OSC terminal-title token can win over command detection.
4. A recognized local child process is used when available: its executable basename, then its install location, then its command line (below).
5. Unknown tools use the generic terminal fallback.

The current registry deliberately has no terminal-title tokens: none of the reviewed upstream or installed sources established a stable exact OSC title contract. The resolver remains bounded and ready for a later verified token without broad or fuzzy matching.

The tab context menu exposes Automatic, Terminal, and every supported product profile. Choosing a profile is an explicit correction and clears an older free-form name. Reset clears both persisted overrides and returns the tab to automatic detection.

## Local detection

The session service walks each pane's descendant processes breadth-first and identifies the first one it recognizes, in this order:

1. **Process name**: the executable basename (case-insensitive, with an optional Windows `.exe` suffix) exactly matches a name in the table.
2. **Install location**: the executable's path contains an exact adjacent run of path components from the table, compared case-insensitively. This covers launchers that run a generic interpreter (Hermes's Python, Cursor's bundled Node) and symlinked installs, which macOS reports under the target's file name (Claude Code's `versions/<version>`, Grok Build's `grok-macos-aarch64`).
3. **Command line**: `argv[0]` matches a name in the table, which keeps the symlink name the user typed and any process title (Pi and omp set theirs). When `argv[0]` is a script interpreter (`node`, `nodejs`, `bun`, `deno`, `python`, `python3`, `python3.N`), the first non-option argument is the script: its file name without `.js`, `.mjs`, `.cjs`, `.ts`, or `.py` must match a name, or its path must contain an install location. This covers npm, Bun, uv, and pipx installs such as Gemini CLI, Pi, Qwen Code, and Kimi Code CLI.

| Product | Names | Install locations |
| --- | --- | --- |
| Hermes Agent | `hermes`, `hermes-agent` | `.hermes/hermes-agent`, `Cellar/hermes-agent` |
| omp | `omp` | `node_modules/@oh-my-pi/pi-coding-agent` |
| Pi | `pi` | `node_modules/@earendil-works/pi-coding-agent`, `node_modules/@mariozechner/pi-coding-agent` |
| Codex CLI | `codex` | `node_modules/@openai/codex` |
| Claude Code | `claude` | `node_modules/@anthropic-ai/claude-code`, `share/claude/versions` |
| Droid | `droid` | |
| Kilo Code | `kilo`, `kilocode`, `.kilo` | `node_modules/@kilocode/cli` |
| Cursor | `cursor-agent` | `cursor-agent/versions` |
| OpenCode | `opencode` | `node_modules/opencode-ai` |
| Aider | `aider` | `uv/tools/aider-chat`, `pipx/venvs/aider-chat`, `Cellar/aider` |
| GitHub Copilot CLI | `copilot` | `node_modules/@github/copilot` |
| Gemini CLI | `gemini` | `node_modules/@google/gemini-cli` |
| Amp | `amp` | `.amp/bin`, `node_modules/@ampcode/cli`, `node_modules/@sourcegraph/amp` |
| Qwen Code | `qwen` | `node_modules/@qwen-code/qwen-code`, `lib/qwen-code` |
| Grok Build | `grok` | `.grok/downloads` |
| Kimi Code CLI | `kimi`, `kimi-code`, `kimi-cli`, `Kimi Code` (process title) | `.kimi-code/bin`, `node_modules/@moonshot-ai/kimi-code`, `uv/tools/kimi-cli` |
| Antigravity CLI | `agy` | |
| Kiro CLI | `kiro-cli`, `kiro-cli-chat` | `Kiro CLI.app/Contents/MacOS` |
| Mistral Vibe | `vibe`, `vibe-rs` | `uv/tools/mistral-vibe`, `Cellar/mistral-vibe` |
| Crush | `crush` | `node_modules/@charmland/crush` |
| goose | `goose` | |
| Cline CLI | `cline`, `.cline` | `node_modules/cline` |
| Auggie CLI | `auggie` | `node_modules/@augmentcode/auggie` |
| Continue CLI | `cn` | `node_modules/@continuedev/cli` |
| tmux | `tmux` | |

Generic or shared aliases are intentionally not recognized: `agent` ships with both Cursor and Grok Build, so each is identified by its install location instead; `antigravity` launches the Antigravity desktop app, not the CLI; Kiro's legacy `q` and `qchat` are too short to be unambiguous. Related product names such as `chatgpt` and partial or decorated strings never match. Names and install layouts were checked against official package manifests, installer scripts, repositories, or installed executables on 2026-09-26.

Bots use the same names to find installed agents: the service reads the `PATH` of an interactive login shell (where most agent installers add their directory), falling back to a login-only shell, and also searches the common per-user install directories (`~/.local/bin`, `~/.bun/bin`, `~/.npm-global/bin`, `~/.opencode/bin`, `~/.grok/bin`, `~/.amp/bin`, `~/.cargo/bin`, `~/.claude/local`) plus `/opt/homebrew/bin` and `/usr/local/bin`. Rescan re-reads the shell configuration.

## Privacy and performance boundary

- Harness Harlot never scans terminal grid text, scrollback, prompts, agent messages, or conversation output to infer identity.
- OSC title metadata is capped at 80 visible, non-control characters and can match only exact registry tokens. Raw titles are ephemeral and are not logged or persisted.
- Command discovery reads local process names and executable paths. Only for a descendant of a pane that neither matches, it reads that process's command line and examines at most `argv[0]` and, after a script interpreter, the first non-option argument's path. Other arguments, environment variables, files, credentials, shell history, and terminal content are never examined.
- Discovery runs no more than once every two seconds, skips a system with more than 4,096 visible processes, and inspects at most 64 descendants across four levels per pane.
- Live detection, including executable paths and command lines, is memory-only and is never logged, sent to the desktop, or persisted. Desired-state recovery stores only the explicit custom name and selected profile in the existing owner-only atomic snapshot.
- Icons are compile-time embedded local assets. The feature adds no socket, network request, upload, analytics, or telemetry.

PTY ownership, input, output parsing, resizing, and child lifetime stay in the session service exactly as before.

## Branding and assets

The icons are secondary UI identifiers placed directly beside the complete product name. They are not endorsements, sponsorship claims, or Harness Harlot branding. Artwork is stored byte-for-byte from the documented official source, is not recolored or redrawn, and is never fetched at runtime.

Exact source revisions, file hashes, license copies, brand-policy links, and the Copilot fallback decision are recorded in [Asset notices](../ASSET_NOTICES.md). Product names and marks remain the property of their respective owners.
