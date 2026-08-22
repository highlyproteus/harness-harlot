# Changelog

All notable changes to Harness Harlot are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
