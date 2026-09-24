# Bots privacy and data handling

Effective: September 24, 2026

This document describes the data behavior of Bots in Harness Harlot. The agent
CLIs you choose for bots and workers (for example omp, Hermes, Claude Code, or
Codex), their model providers, the operating system, terminal programs, and
browsers have their own terms and data practices.

## Harness Harlot sends nothing to model providers

Harness Harlot has no built-in model, voice, or speech integration and makes no
model-provider or speech-provider requests of its own. A bot is the agent CLI
you selected, running its own interface in a local terminal. That agent uses its
own configuration, credentials, provider, and optional voice mode. Harness
Harlot does not read, copy, or store agent or provider credentials.

## What a bot's agent can access

When Harness Harlot launches a bot, it gives the agent:

- a coordinator prompt with the bot's name and any instructions you entered;
- the Harness Harlot tools: the bundled omp plugin for omp, or the local `hh mcp`
  server for agents whose launch command accepts an MCP configuration; and
- the `HH_*` environment variables that identify the bot's terminal.

With these tools the agent can list workstations and terminal tabs, open worker
tabs that run commands, read worker terminal screens, send input to them, wait
for their status, rename, focus, and close them, and drive embedded browser and
Gallery panes. Anything a tool returns, including terminal screen text, browser
page content, and screenshots, can become context that the agent sends to its
model provider. Worker programs are separate local processes with their normal
operating-system permissions.

The omp plugin reports worker status changes (needs input, needs approval,
done) with a short excerpt of that worker's screen into the bot's conversation
so the bot can tell you about them.

## Local storage

The session snapshot records each bot's name, agent, working directory, and
instructions, plus which worker tabs a bot created. It stores no terminal
output, credentials, or conversation content. Bot terminals keep their output
in the private HH tmux server like other local terminals; Harness Harlot keeps
no disk archive of terminal output.

Harness Harlot writes the bundled omp plugin, each bot's coordinator prompt, and
MCP launch configuration (containing only the `hh` executable path) to its
owner-only application state directory. Agents keep their own conversation
history according to their own settings.

Earlier releases stored Assistant conversation files and Voice settings under
the application state directory. This release no longer reads them and does not
delete them automatically; remove the `assistant` directory in the application
state directory if you no longer need it.

## Terminal and browser boundary

Harness Harlot tool requests cross the versioned, owner-only local Unix socket
and are validated by the session service. Agents never receive the service's
process handles, tmux control connection, or SSH credentials.

## Security and questions

Do not include secrets in public issues. Contact the maintainers privately for
sensitive vulnerability reports. For non-sensitive questions, use the project's
[GitLab issue tracker](https://gitlab.com/highlyproteus/harness-harlot/-/issues).

Material changes to Bots data handling are documented here and in the project
changelog as part of release preparation.
