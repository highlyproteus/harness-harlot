# Automatic terminal identity

Rust Mux gives ordinary terminal tabs a small, local identity hint without becoming an agent harness. Known Hermes, Codex/ChatGPT, and Claude Code sessions use a clear text label plus an original neutral badge. Everything else remains an ordinary `Terminal` with a `>_` badge.

## Precedence and correction

Identity resolution is deterministic:

1. An explicit user rename wins.
2. A user-selected terminal profile wins over detection.
3. A bounded, recognized OSC terminal-title signal wins over command detection.
4. A recognized local child-process basename is used when available.
5. Unknown tools use the generic terminal fallback.

The existing tab context menu exposes Rename, Automatic, Terminal, Hermes, Codex, Claude, and “Reset name and identity.” Choosing a profile is an explicit correction and clears an older free-form name. Reset clears both persisted overrides and returns the tab to automatic detection.

## Privacy and performance boundary

- Rust Mux never scans terminal grid text, scrollback, prompts, agent messages, or conversation output to infer identity.
- OSC title metadata is capped at 80 visible, non-control characters and matched only against the registry’s exact safe titles. Raw titles are ephemeral and are not logged or persisted.
- Command discovery reads only local process basenames. It never reads argv, environment variables, files, credentials, or terminal content.
- Discovery runs no more than once every two seconds, skips a system with more than 4,096 visible processes, and inspects at most 64 descendants across four levels per pane.
- Live detection is memory-only. Desired-state recovery stores only the explicit custom name and selected profile in the existing owner-only atomic snapshot.
- The feature adds no socket, network request, upload, analytics, or telemetry.

PTY ownership, input, output parsing, resizing, and child lifetime stay in the session service exactly as before.

## Branding and asset decision

Assessment recorded 2026-08-11:

- [OpenAI’s official brand guidelines](https://openai.com/brand/) require exact supplied marks, constrain presentation, and allow OpenAI to revise or terminate mark permission.
- No public Anthropic material reviewed for this change established a sufficiently clear standalone license for redistributing Claude logo artwork in this open-source local application.
- The [Hermes Agent repository license](https://github.com/NousResearch/hermes-agent/blob/main/LICENSE) covers the software under MIT, but it does not expressly grant a separate trademark or logo-art license.

Rust Mux therefore ships no OpenAI, ChatGPT, Anthropic, Claude, Hermes, CMUX, or Ghostty logo assets. The local registry uses original neutral text badges: `H`, `CX`, `CL`, and `>_`. Product names appear only as descriptive terminal labels and do not imply affiliation or endorsement.
