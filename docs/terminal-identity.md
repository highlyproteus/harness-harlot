# Automatic terminal identity

Harness Harlot gives ordinary terminal tabs a small, local identity hint without becoming an agent harness. Codex CLI, Claude Code, Droid, Hermes Agent, Kilo Code, Cursor, OpenCode, Aider, GitHub Copilot CLI, and Gemini CLI have full product labels and bounded local process detection. Where an official redistributable or installed product asset is available, the unchanged icon appears beside the label. Everything else uses a neutral `>_` terminal glyph.

GitHub Copilot CLI currently uses the neutral glyph because its official public CLI repository and npm package expose no standalone icon asset. Harness Harlot does not substitute a third-party logo or a lookalike.

## Precedence and correction

Identity resolution is deterministic:

1. An explicit user rename wins.
2. A user-selected terminal profile wins over detection.
3. A verified exact OSC terminal-title token can win over command detection.
4. A recognized local child-process executable basename is used when available.
5. Unknown tools use the generic terminal fallback.

The current registry deliberately has no terminal-title tokens: none of the reviewed upstream or installed sources established a stable exact OSC title contract. The resolver remains bounded and ready for a later verified token without broad or fuzzy matching.

The tab context menu exposes Automatic, Terminal, and every supported product profile. Choosing a profile is an explicit correction and clears an older free-form name. Reset clears both persisted overrides and returns the tab to automatic detection.

## Exact local detection

The process registry recognizes only these executable basenames (case-insensitive, with an optional Windows `.exe` suffix):

| Product | Exact basenames |
| --- | --- |
| Hermes Agent | `hermes`, `hermes-agent` |
| Codex CLI | `codex` |
| Claude Code | `claude` |
| Droid | `droid` |
| Kilo Code | `kilo`, `kilocode` |
| Cursor | `cursor-agent` |
| OpenCode | `opencode` |
| Aider | `aider` |
| GitHub Copilot CLI | `copilot` |
| Gemini CLI | `gemini` |

Generic aliases such as `agent`, related product names such as `chatgpt`, and partial or decorated strings are intentionally not recognized. Command names were checked against official package manifests, installer scripts, repositories, or installed executables on 2026-08-11.

Hermes Agent's installed `hermes` and `hermes-agent` shell launchers immediately replace themselves with a generic Python interpreter, so their names are not stable long enough for the two-second sampler. Harness Harlot additionally recognizes a Python executable only when its executable path contains the exact adjacent `.hermes/hermes-agent` installation namespace. The supported venv and self-contained runtime layouts are regression-tested; an ordinary Python process or a similar path outside that namespace remains a generic terminal.

## Privacy and performance boundary

- Harness Harlot never scans terminal grid text, scrollback, prompts, agent messages, or conversation output to infer identity.
- OSC title metadata is capped at 80 visible, non-control characters and can match only exact registry tokens. Raw titles are ephemeral and are not logged or persisted.
- Command discovery reads only local process executable basenames and, for the generic Hermes interpreter, its executable location. It never reads full argv, environment variables, files, credentials, shell history, current working directories, or terminal content.
- Discovery runs no more than once every two seconds, skips a system with more than 4,096 visible processes, and inspects at most 64 descendants across four levels per pane.
- Live detection, including the Hermes executable location, is memory-only and is never logged, sent to the desktop, or persisted. Desired-state recovery stores only the explicit custom name and selected profile in the existing owner-only atomic snapshot.
- Icons are compile-time embedded local assets. The feature adds no socket, network request, upload, analytics, or telemetry.

PTY ownership, input, output parsing, resizing, and child lifetime stay in the session service exactly as before.

## Branding and assets

The icons are secondary UI identifiers placed directly beside the complete product name. They are not endorsements, sponsorship claims, or Harness Harlot branding. Artwork is stored byte-for-byte from the documented official source, is not recolored or redrawn, and is never fetched at runtime.

Exact source revisions, file hashes, license copies, brand-policy links, and the Copilot fallback decision are recorded in [Asset notices](../ASSET_NOTICES.md). Product names and marks remain the property of their respective owners.
