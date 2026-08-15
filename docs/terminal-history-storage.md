# Local terminal history storage

Harness Harlot keeps two deliberately separate history layers:

- **Live memory** is the daemon-owned Alacritty grid plus a 2,000-line scrollback buffer per running pane. It stays bounded and is the authoritative interactive terminal state.
- **Local history storage** is an optional owner-only archive of raw PTY output from future sessions. It is used only when the desktop requests an older page or an archived search. It does not enlarge the live terminal model or keep a full session in UI memory.

The archive does not reconstruct output from sessions that predate this feature. Disabling it also means output produced while it is disabled cannot later be recovered.

## Storage and privacy boundary

The session service is the only writer. Stable archive data lives under `history/` beside `sessions.json` in `~/Library/Application Support/Harness Harlot` on macOS and `$XDG_STATE_HOME/hh` (or `~/.local/state/hh`) on Linux and other Unix platforms. Development builds use `Harness Harlot Dev` or `hh-dev`; `HH_STATE_DIR` moves both archive data and `sessions.json` under the selected local state directory. Directories are forced to mode `0700` and metadata/chunks to `0600`. Symbolic-link archive directories and files are rejected or skipped. On macOS, the desktop marks the history directory as excluded from Time Machine whenever it exists. The archive records raw PTY output without inspecting it for credentials. Shell echo can therefore include typed commands and values such as `export TOKEN=...`; no-echo password prompts normally do not echo their input into the PTY output stream.

Each terminal run receives a distinct archive session ID even when a recovered pane keeps the same pane ID. Output is accumulated on a dedicated writer thread and published as independently checksummed 128 KiB chunks. A chunk is written to an owner-only temporary file, synced, renamed atomically, and followed by a directory sync. Manifests and settings use the same atomic-replace pattern. On restart, unpublished temporary files are removed and an unfinished session is closed with a visible gap marker; already published chunks remain readable. A corrupt, truncated, reordered, or checksum-mismatched chunk becomes a visible archive gap instead of being rendered as trusted output. These checks detect accidental corruption and bit rot; they are not evidence against a malicious process running as the same user.

The archive view is a bounded plain-text projection of one chunk at a time. It is intentionally labeled **LOCAL HISTORY** so it cannot be mistaken for the styled live Alacritty grid. Scrolling upward from the top of live scrollback loads one older page on demand; scrolling down returns through newer pages and then to the live terminal. A live literal-search miss falls back to archived chunks without loading the entire retained session.

## Backpressure and capacity

PTY reads never wait for disk. The reader first advances the bounded live terminal model, then uses a non-blocking send into a fixed 256-item archive queue. A full or disconnected queue records the exact rejected byte count. The next accepted chunk carries a gap flag, and Settings reports the uncommitted gap immediately. Terminal input, output, and rendering continue normally.

The default archive policy is:

- enabled, local only;
- keep indefinitely;
- 5 GiB quota;
- warn at 80%;
- pause new archive writes at capacity.

Capacity never silently deletes retained sessions under the default policy. Settings asks the user to increase the quota or explicitly clear terminal, workspace, or all history. Clear actions require a second click. Disabling local history pauses future archive writes but preserves existing bytes until the user invokes an explicit clear action; output produced while disabled is not retained. Users may opt into a finite 7/30/90/custom-day retention period, which permits removal of closed sessions after that age, or separately opt into oldest-first cleanup at capacity. Finite retention is enforced during long-running sessions as well as at startup and session close. Active terminal sessions are never removed to satisfy a quota. Switching either deletion policy on is an explicit local choice.

Settings shows live memory separately from archive bytes, retained-session count, oldest date, retention and quota presets/custom values, the capacity policy, and any corruption/overflow/capacity warning. Archive status contains sizes, counts, dates, and gap state; it does not include terminal text.

## Limits

The archive records the PTY byte stream, not periodic full terminal snapshots. Programs that redraw in place, alternate-screen applications, and escape sequences split across chunk boundaries may produce a less faithful plain-text archive projection than the live terminal. The raw checksummed bytes remain the integrity source, and the UI labels the projection rather than presenting it as a restored live terminal state. Full styled historical replay would require a separately approved checkpoint format and is not claimed here.
