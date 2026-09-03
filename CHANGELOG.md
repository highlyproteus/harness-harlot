# Changelog

All notable changes to Harness Harlot are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.18] - 2026-09-03

### Fixed

- Prevented the desktop updater from closing when service state is unavailable,
  and reopened the exact managed app after a failed GUI update handoff.
- Forced a fresh macOS app instance after successful replacement and removed
  abandoned staged bundles when installation fails before the atomic swap.
- Corrected the immutable v0.1.16 legacy bridge asset names so signed stable
  releases can complete publication for existing updater generations.

## [0.1.17] - 2026-08-31

### Fixed

- Added a renewable, short-lived stable-v2 update feed with isolated daily
  signing, attested publication, and exact first-party URL validation so
  installers and installed clients do not expire between application releases.
- Preserved the GitHub-hosted v0.1.16 migration path while moving new clients
  to the renewable feed.

## [0.1.16] - 2026-08-28

### Added

- Added conversational-only Voice Mode with OpenAI Realtime audio and
  transcripts, Assistant panes, typed text and image attachments, persistent
  conversation threads, cancellation, and optional Honcho-backed memory.
- Added service-projected agent status badges for workspace tabs and pane tabs,
  including stable OSC status events and omp/Codex heuristics.
- Added persistent Voice settings and dock state plus reusable session-client
  support for the desktop and voice engine.
- Added clipboard-image paste and file/image drop transfer for focused local and
  SSH terminal panes with private staging, bounded transfer, and rollback.
- Added native modifier-arrow terminal navigation and URL click routing,
  including macOS Command-click embedded browser splits.
- Added bounded, noninteractive tmux discovery for saved SSH workstations that
  are not currently connected.

### Fixed

- Kept Assistant transcripts visible while suspended, added live listening
  feedback, and prevented idle suspension during active voice exchanges.
- Made Assistant tab rename, color, and custom-icon controls behave consistently
  with other pane types.
- Preserved every accepted typed turn across bounded queues and reconnects,
  surfaced failed or incomplete provider responses, and prevented silent replay
  or loss during cancellation.
- Added spoken barge-in, bounded playback/provider queues and waits, persistence
  rollback, composer draft preservation, and bounded subprocess shutdown.
- Made local thread delete, clear-all, and retention revocation crash-durable by
  revoking active writers before unlink and syncing the containing directory
  before success is reported.

### Security

- Removed all provider/model tools, tool-choice capability, Voice approval cards,
  and model execution paths. Voice cannot inspect or control terminals, panes,
  workstations, tabs, projects, threads, directories, filesystems, Git, agents,
  or memory retrieval.
- Made historical or unsolicited provider function calls fail locally before any
  RPC or effect, and appended a final capability boundary after all configured
  instructions and restored context.
- Kept terminal output, pane content, and OSC payloads out of provider context;
  terminal notifications remain fixed-vocabulary local cues only.
- Deferred microphone device discovery and stream creation until the visible
  start-voice action; text, image, and history paths remain microphone-free.
- Kept OpenAI and Honcho credentials process/environment-backed and omitted from
  persisted settings. Honcho requests require HTTPS (except parsed loopback HTTP)
  and do not follow redirects.
- Hardened saved threads and image attachments with owner-only descriptor checks,
  no-follow file access, format/size validation, bounded retention, and explicit
  local-only deletion labels that distinguish remote Honcho retention.
- Rotated stable and edge update authorities, separated build and signing jobs,
  removed signing secrets from Cargo/package execution, protected immutable
  signed `v*` tags, and replaced CEF SHA-1 archive pins with SHA-256.
- Required tagged release publication to pass exact-commit tests, strict Clippy,
  audit, deny, notice-drift, ShellCheck, release-policy, signer-interoperability,
  and secret-scanning gates before secret-free publication.

### Documentation

- Replaced the Voice privacy disclosure with the exact conversation-only data
  boundary and clarified local thread deletion versus optional remote Honcho
  retention.
- Clarified that the distributed macOS community build is ad-hoc signed and not
  Apple Developer ID notarized, so macOS may request privacy permissions again.
- The disclosure ships in macOS and Linux artifacts and is linked from Voice
  settings.

## [0.1.13] - 2026-08-23

### Fixed

- Restored local and SSH tmux session discovery with printable, bounded metadata
  that current tmux releases do not sanitize into an unparseable form.
- Made ordinary left-button drags create native text selections inside tmux and
  other mouse-aware terminal programs while preserving normal click handling.
- Preserved word, line, and block selection along with macOS and Linux clipboard
  copy/paste shortcuts and bracketed-paste protections.

## [0.1.12] - 2026-08-22

### Added

- Added a visual HSV color picker with clearer selected-color states for
  terminal and workspace appearance controls.
- Added richer top-tab drag-and-drop behavior and human-readable terminal
  identity naming.

### Changed

- Improved terminal rendering, polling, input, selection, URL interaction, and
  scroll-to-bottom behavior.
- Refined workspace, sidebar, menu, and embedded-browser interactions.

## [0.1.11] - 2026-08-22

### Changed

- A single `curl -fsS https://harnessharlot.com/install | sh` command now
  installs Harness Harlot on both macOS and Linux.
- The README presents installation before product concepts and uses one shared
  command instead of separate platform instructions.
- The Linux bootstrap no longer requires GitHub CLI or a GitHub account.

### Security

- Linux installation verifies website-pinned archive and manifest checksums,
  rejects unsafe archive entries, and verifies the signed update manifest with
  the packaged updater before installation.

## [0.1.10] - 2026-08-22

### Changed

- The macOS installer no longer requires GitHub CLI. It downloads release
  assets with the system `curl` and verifies SHA-256 values pinned by the
  HTTPS release index at `harnessharlot.com` before mounting any disk image.
- The landing site now presents a concise, HTTPS-only installation command.

### Security

- Website publication automation verifies GitHub build attestations against
  the exact signed-tag release workflow before updating the release index.
- Release verification and website publication use separate read-only and
  write-scoped jobs so package lifecycle code never receives a push token.

## [0.1.9] - 2026-08-21

### Fixed

- Prevented inherited development and fixture settings from redirecting a
  normal macOS installation.
- Made release verification non-disruptive to running terminal sessions.
- Checked both system and per-user application locations during migration.
- Restored HTTPS-only redirects and TLS 1.2 for the macOS bootstrap.

## [0.1.8] - 2026-08-21

### Fixed

- Suppressed low-level disk-image mounting output during successful macOS
  updates.

## [0.1.7] - 2026-08-21

### Added

- Added a concise macOS bootstrap and the `hh version`, `hh doctor`, and
  `hh update` maintenance workflow.

### Fixed

- Resolved bundled service and updater discovery when `hh` is launched through
  `~/.local/bin/hh`.
- Hid updater rollback applications from Finder and Spotlight.
- Preserved the original CLI target if installation rollback is required.

## [0.1.6] - 2026-08-20

### Changed

- Unified terminal update delivery across the desktop and command-line flows.
- Gated edge publication on successful CI before replacing the update feed.

## [0.1.5] - 2026-08-20

### Added

- Added hourly automatic update checks and explicit manual update checks.

## [0.1.4] - 2026-08-19

### Added

- A one-command Linux bootstrap installer selects the native package, verifies
  its GitHub build provenance, and rejects unsafe archives before installation.

### Changed

- The README now provides copy-and-paste macOS and Linux install commands and
  makes the temporary manual macOS update procedure explicit.

### Fixed

- Release automation now verifies the owner's SSH-signed tags, and release
  fixtures derive package versions from workspace metadata.

## [0.1.3] - 2026-08-19

### Changed

- Routine Linux updates preserve the compatible session service and live
  terminals; protocol-changing updates still wait for sessions to end.
- Community macOS updates remain notify-only until Developer ID signing and
  Apple notarization are enabled.

## [0.1.1] - 2026-08-19

### Fixed

- The update feed, installers, and documentation now point at the
  `highlyproteus/harness-harlot` repository.
- Update verification trusts the owner-held `hh-stable-2026` signing key.

## [0.1.0] - 2026-08-19

### Added

- Persistent terminal workstations with restartable native desktop views, split panes, and daemon-owned local shell sessions.
- SSH workstations through system OpenSSH and local or remote tmux session attachment.
- On-device terminal history archival with recovery, integrity checks, search, and bounded storage controls.
- Chromium browser tabs in both macOS application bundles and packaged Linux
  releases through X11 or XWayland.
- An Ed25519-signed update channel with CPU-specific artifacts, desktop update
  notifications, one-click or command-line installation for trusted
  Developer ID/Linux packages, and notify-only community macOS updates.
- A no-cost macOS community installer that verifies GitHub build provenance
  before mounting, validates the signed manifest and exact DMG, requires
  explicit unnotarized-build acknowledgement, and preserves Gatekeeper.
- Native glibc 2.35 Linux packages for x86_64 and arm64 with an unprivileged installer, desktop integration, signed automatic updates, atomic rollback, and graceful service restart.
