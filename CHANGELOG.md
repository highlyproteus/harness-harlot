# Changelog

All notable changes to Harness Harlot are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
