# Harness Harlot runtime identity

The public product name is **Harness Harlot**. Its technical prefix is `hh`.
The supported environment interface is `HH_SOCKET`, `HH_STATE_DIR`,
`HH_CONFIG`, and `HH_PANE_ID`.

Runtime data uses `hh-session.sock` under the owner-only runtime directory.
Stable macOS state lives under
`~/Library/Application Support/Harness Harlot`; development state uses
`~/Library/Application Support/Harness Harlot Dev`. Linux and other Unix
platforms use `hh` and `hh-dev` directories under the XDG state and
configuration roots.

The retired pre-rename identity is not read, migrated, or
removed. Any pre-rename local state remains on disk and is ignored. No legacy
configuration, socket, or state fallback exists.

The checkout path and Codex project mapping remain unchanged, and historical
task and PDR identifiers are preserved because they are dated records rather
than living product identity.
