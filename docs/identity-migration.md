# Not a Harness runtime identity

The public product name is **Not a Harness**. Its technical prefix is `nah`.
The supported environment interface is `NAH_SOCKET`, `NAH_STATE_DIR`,
`NAH_CONFIG`, and `NAH_PANE_ID`.

Fresh runtime data uses `nah-session.sock` in the temporary directory, the
macOS `~/Library/Application Support/Not a Harness` state directory, and `nah`
directories under XDG state and config roots on Linux and other Unix platforms.
No legacy configuration or runtime fallback is supported by this release.

The migration audit performed with this change found no existing local state,
configuration, or socket data to move. No user data was removed. The only
remaining historical references are the checkout path and Codex project mapping,
plus preserved historical task and PDR identifiers; changing those can detach
established Codex task history.
