# Assistant and Voice Mode privacy and data handling

Effective: September 18, 2026

This document describes the data behavior of Assistant panes and optional Voice
Mode in Harness Harlot. The selected pi model provider, OpenAI, the operating
system, terminal programs, browsers, and any coding agents launched in terminals
have their own terms and data practices.

## Assistant orchestration

Each Assistant pane runs a local, service-owned `pi --mode rpc` subprocess. pi
connects to the model provider selected in its own configuration. Harness Harlot
starts pi with built-in tools, skills, prompt templates, themes, and context-file
loading disabled, then loads only the bundled HH workspace extension.

Depending on what you ask the Assistant to do, the selected pi model provider
may receive:

- typed messages and attached PNG, JPEG, or WebP images;
- the Assistant system prompt, configured operator instructions, configured
  coding-agent command, and resolved working directory;
- workspace, window, pane, browser URL, process-status, and terminal-screen
  information returned by HH tools; and
- tool arguments, results, errors, and subsequent Assistant messages.

The HH extension can list and create workstations, windows, terminal panes, and
browser panes; read terminal screens; send terminal input; wait for terminal
state; close panes; and focus panes. It has no direct filesystem or shell tool.
A command or coding agent launched inside a terminal is a separate local
program and has whatever operating-system permissions that program normally
has.

`Full` access automatically approves guarded HH actions. `Confirm` requires an
inline Allow/Deny decision before the extension sends terminal input, closes a
pane, or creates a terminal with a command. Model text, tool output, terminal
content, and voice output do not themselves approve a pending action.

Provider handling and retention of Assistant data are governed by pi's selected
provider and account configuration. pi manages its own provider credentials;
Harness Harlot does not copy them into Voice settings.

## Voice Mode is optional

Voice Mode is inactive until you use the visible start-voice control in an
Assistant pane. Typing a message or attaching an image does not grant microphone
access. Microphone capture hardware is not opened until voice starts. Muting,
suspending, or stopping Voice Mode disables capture.

When Voice Mode is active, Harness Harlot uses the OpenAI Realtime API only as a
speech relay. It may send:

- microphone audio captured after explicit voice start;
- the Assistant workspace title in the relay instructions; and
- bounded final Assistant text prefixed as an orchestrator update so Realtime
  can speak it.

OpenAI returns input transcripts plus generated relay text and audio. Final user
transcripts are forwarded to the pi orchestrator. Realtime advertises no HH
tools and cannot invoke or approve HH actions. It does not receive attached
images or raw HH tool results directly, but final Assistant text sent for speech
can contain information that the orchestrator chose to report.

OpenAI's handling and retention of Voice data are governed by the terms and
settings of the account associated with `HH_OPENAI_API_KEY`.

## Local storage

Harness Harlot stores non-secret Voice settings in its owner-only application
state directory. The OpenAI API key is not serialized; supply it through:

```text
HH_OPENAI_API_KEY
```

Retired Honcho settings are accepted only to migrate older settings files, then
ignored and omitted from future writes. Harness Harlot no longer sends
conversation data to Honcho and no longer stores its previous local
conversation-thread or summary format.

pi session files live under the owner-only Assistant state directory so an
Assistant pane can resume its newest session after a service restart. Those
files may contain user and Assistant messages, attached-image data, tool calls,
tool results, terminal excerpts, browser URLs, and working-directory paths.
Closing the Assistant pane, tab, or workspace removes that pane's local pi
session directory. pi and the selected provider may have additional independent
retention behavior.

Attached images are read only after you select them. Harness Harlot rejects
symbolic links, non-regular or foreign-owned files, oversized input, mismatched
file signatures, and invalid image decodes before sending accepted PNG, JPEG,
or WebP data to pi.

## Terminal and filesystem boundary

HH tool requests cross the versioned, owner-only local Unix socket and are
validated by the session service. The pi subprocess receives only the bundled
HH extension; it does not receive Harness Harlot's internal process handles,
tmux control connection, provider secrets, or SSH credentials. Terminal
content explicitly read by an HH tool can become model context, and commands
typed into a terminal can operate on local files according to that terminal
program's permissions.

## Security and questions

Do not include secrets in public issues. Contact the maintainers privately for
sensitive vulnerability reports. For non-sensitive questions, use the project's
[GitLab issue tracker](https://gitlab.com/highlyproteus/harness-harlot/-/issues).

Material changes to Voice data handling are documented here and in the project
changelog as part of release preparation.
