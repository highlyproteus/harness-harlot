# Voice Mode privacy and data handling

Effective: August 27, 2026

This document describes the data behavior of Voice Mode in Harness Harlot. It
covers the application as distributed by this project; OpenAI, an optional
Honcho server, the operating system, and terminal programs have their own terms
and data practices.

## Voice Mode is optional

Voice Mode is inactive until you use an Assistant pane. Typing a message,
attaching an image, or reopening a saved thread may connect the Assistant to
OpenAI, but those actions do not grant microphone access. Microphone capture
starts only after you use the visible start-voice control. Muting, suspending,
or stopping the Assistant disables capture, and a later text-only start revokes
any earlier microphone consent.

## Data sent to OpenAI

When an Assistant is connected, Harness Harlot uses the OpenAI Realtime API.
Depending on what you choose to do, it may send:

- typed messages and attached images;
- microphone audio captured after explicit voice start;
- workspace context such as the workspace name, working directory, configured
  Assistant instructions, and bounded prior-conversation context;
- bounded tool results and status information from the authorized workspace;
- terminal notifications, marked and delimited as untrusted user data.

OpenAI returns generated text, audio, transcripts, and tool requests. OpenAI's
handling and retention of this data are governed by the terms and settings of
the OpenAI account associated with `HH_OPENAI_API_KEY`.

The model cannot approve its own terminal mutations. Sending terminal input,
opening or closing terminals or workstations, renaming tabs, creating project
or worktree tabs, and launching agents require a separate approval in the
Harness Harlot UI. Pane, thread, and directory reads are restricted to the
Assistant's authorized workspace boundary.

## Optional Honcho memory

Honcho memory is disabled by default. If you configure it, Harness Harlot sends
conversation turns and recall queries to the Honcho server you selected. Remote
Honcho endpoints must use HTTPS. Plain HTTP is accepted only for a parsed
loopback destination such as `localhost`, `127.0.0.1`, or `::1`.

Honcho data retention and deletion are controlled by that Honcho deployment.
Clearing local Harness Harlot threads does not delete a remote Honcho server's
copy. Use the server's administrative controls to inspect or delete that data.

## Local storage

Harness Harlot stores non-secret Voice settings in its owner-only application
state directory. OpenAI API keys and Honcho bearer tokens are deliberately not
serialized to the settings file. Supply them to future launches through:

```text
HH_OPENAI_API_KEY
HH_HONCHO_BEARER
```

Saved Assistant threads contain bounded text turns, tool summaries, titles,
workspace identifiers, and session summaries. They do not contain microphone
audio or attached image bytes. Thread files are owner-only regular files,
opened without following symbolic links, and bounded to 8 MiB and 10,000
records per thread.

Default local thread retention keeps at most 200 threads, 90 days of activity,
and 64 MiB in total, deleting the oldest or expired files when a limit is
exceeded. The Assistant history UI can delete one saved thread or clear all
saved threads. Those controls affect local thread files only.

Attached images are read only after you select them. Harness Harlot rejects
symbolic links, non-regular or foreign-owned files, oversized input, mismatched
file signatures, and invalid image decodes before sending accepted PNG, JPEG,
or WebP data to OpenAI.

## Terminal and filesystem access

Harness Harlot does not give the Assistant unrestricted access to every terminal
or directory. Read operations are scoped to the authorized workspace and its
canonical directory root. Terminal output and OSC notifications are treated as
untrusted data, not system instructions. Mutation approvals display in the UI
and cannot be resolved by the model or by spoken confirmation.

## Security and questions

Do not include secrets in public issues. Report a suspected vulnerability using
[GitHub private vulnerability reporting](https://github.com/highlyproteus/harness-harlot/security/advisories/new).
For non-sensitive questions about this document, use the project's GitHub issue
tracker.

Material changes to Voice data handling are documented here and in the project
changelog as part of release preparation.
