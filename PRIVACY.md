# Voice Mode privacy and data handling

Effective: August 28, 2026

This document describes the data behavior of Voice Mode in Harness Harlot. It
covers the application as distributed by this project; OpenAI, an optional
Honcho server, the operating system, and terminal programs have their own terms
and data practices.

## Voice Mode is optional

Voice Mode is inactive until you use an Assistant pane. Typing a message,
attaching an image, or reopening a saved thread may connect the Assistant to
OpenAI, but those actions do not grant microphone access. Microphone capture
hardware is not enumerated, configured, or opened until you use the visible
start-voice control. Muting, suspending, or stopping the Assistant disables
capture, and a later text-only start revokes any earlier microphone consent.

## Data sent to OpenAI

When an Assistant is connected, Harness Harlot uses the OpenAI Realtime API.
Depending on what you choose to do, it may send:

- typed messages and attached images;
- microphone audio captured after explicit voice start;
- a non-path conversation label, configured Assistant instructions, and a
  bounded prior-conversation summary; and
- optional Honcho memory context when you separately enable Honcho.

Harness Harlot does not send terminal output, terminal notifications, OSC
payloads, pane contents, filesystem paths, directory listings, Git state, or
workspace control data to the Voice provider. It advertises no provider tools
or tool-choice capability. Historical or unsolicited provider function calls
fail locally and cannot invoke an RPC, approval, terminal action, filesystem
operation, or memory query.

OpenAI returns generated text, audio, and transcripts. OpenAI's handling and
retention of sent data are governed by the terms and settings of the OpenAI
account associated with `HH_OPENAI_API_KEY`.

Voice is conversation-only. The model cannot inspect or control terminals,
panes, workstations, tabs, projects, threads, directories, filesystems, Git,
agents, or local memory. Voice has no approval UI, and model output, speech,
terminal content, restored context, or prior summaries cannot authorize an
action.

## Optional Honcho memory

Honcho memory is disabled by default. If you configure it, Harness Harlot sends
accepted user and assistant text turns to the Honcho server you selected. At
session start, the application may request a bounded memory preamble and place
that text in the conversational provider context. The model cannot issue its
own Honcho recall or deletion requests.

Remote Honcho endpoints must use HTTPS. Plain HTTP is accepted only for a
parsed loopback destination such as `localhost`, `127.0.0.1`, or `::1`.
Redirects are disabled for Honcho requests so credentials and conversation data
are never forwarded to another origin.

Honcho data retention and deletion are controlled by that Honcho deployment.
Deleting or clearing local Harness Harlot threads does not delete a remote
Honcho server's copy. Use that server's administrative controls to inspect or
delete remote data.

## Local storage

Harness Harlot stores non-secret Voice settings in its owner-only application
state directory. OpenAI API keys and Honcho bearer tokens are deliberately not
serialized to the settings file. Supply them to future launches through:

```text
HH_OPENAI_API_KEY
HH_HONCHO_BEARER
```

Saved Assistant threads contain bounded text turns, titles, conversation and
workspace identifiers, conversation labels, and session summaries. They do not
contain microphone audio, attached image bytes, terminal output, filesystem
paths, provider credentials, tool calls, or approval records. Thread files are
owner-only regular files, opened without following symbolic links, and bounded
to 8 MiB and 10,000 records per thread.

Default local thread retention keeps at most 200 threads, 90 days of activity,
and 64 MiB in total, deleting the oldest or expired files when a limit is
exceeded. The Assistant history UI labels its controls as local-only. Delete,
clear-all, and retention revoke active writers before removing files and sync
the containing directory before reporting success. Session summaries live in
those same retained thread files, so local controls cover summaries as well as
visible turns.

This disclosure is included in macOS and Linux packages and is linked from the
Voice settings panel.

Attached images are read only after you select them. Harness Harlot rejects
symbolic links, non-regular or foreign-owned files, oversized input, mismatched
file signatures, and invalid image decodes before sending accepted PNG, JPEG,
or WebP data to OpenAI.

## Terminal and filesystem boundary

Voice has no terminal or filesystem capability. Terminal output and OSC
notifications remain local and do not become model context. Ordinary terminal,
workspace, browser, and file-transfer features remain human-operated desktop
features outside the Voice provider boundary.

## Security and questions

Do not include secrets in public issues. Contact the maintainers privately for
sensitive vulnerability reports. For non-sensitive questions, use the project's
[GitLab issue tracker](https://gitlab.com/highlyproteus/harness-harlot/-/issues).

Material changes to Voice data handling are documented here and in the project
changelog as part of release preparation.
