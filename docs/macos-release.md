# Not a Harness macOS stable release foundation

Not a Harness has one production channel: `stable`. There is no nightly feed,
automatic background check, telemetry, or network request in the app today.
This document defines the local release surface that can safely become a public
update path once Apple credentials and a public immutable host are selected.

## What is implemented now

- `scripts/build-macos-app.sh release` builds a self-contained app bundle with
  `nah`, `nah-service`, the update verifier, and the tracked app icon.
- `scripts/package-macos-release.sh VERSION BUILD` makes a versioned,
  architecture-specific DMG and a signed detached `update.json.sig` beside the
  exact JSON bytes it covers. Artifacts are named
  `Not-a-Harness-<version>+<build>-macos-<arch>.dmg`; publishing must never
  replace one of those names.
- `nah-updater` verifies Ed25519 metadata before parsing it, permits only the
  stable product/feed schema, requires immutable HTTPS DMG URLs, and checks the
  exact downloaded size and SHA-256 before any DMG mount.
- Every manifest requires a quiescent session service. This is an intentional
  update safety boundary, not a temporary warning: a running service owns PTYs
  and terminal processes, so an app update must never stop or replace it.

The production package script refuses a tracked-dirty checkout and requires a
Developer ID signing identity plus a distinct Ed25519 feed-signing key. Its
`NAH_RELEASE_TEST_MODE=1` fixture mode uses an intentionally public test key
and `updates.example.invalid`; that output is only for local validation and
must never be uploaded.

## Why Sparkle is not bundled yet

Sparkle 2 is a mature macOS updater and MIT-licensed, so its license does not
conflict with Not a Harness's MIT project. It is not being added as an opaque
runtime dependency at this stage because this app is a Rust/GPUI executable,
not an AppKit lifecycle owned by Swift or Objective-C, and it has a separate
long-lived PTY service. A safe integration needs an Objective-C bridge,
Sparkle's EdDSA feed setup, re-signing of embedded framework/XPC components,
notarized DMGs, and a proof that a relaunch cannot kill or strand the service.

The checked-in verifier is deliberately framework-neutral. It gives a future
Sparkle integration an independent preflight: fetch only the configured stable
feed over HTTPS, verify the exact detached manifest with the compiled release
public key, verify the completed DMG bytes, then let Sparkle perform its own
signature and Gatekeeper checks. Do not replace this with unsigned JSON or a
redirecting “latest” download URL. If the bridge cannot meet the service
quiescence rule, retain the manual signed-DMG updater instead.

## Signing and hosting handoff

These items are deliberately blocked until an owner provides the credentials
and host decision; no credential discovery or network publication is part of
this repository work.

1. Create a Developer ID Application certificate and choose the final Team ID.
   Sign every nested executable and the outer app using hardened runtime, then
   notarize the DMG and staple its ticket. Record the Team ID as installer
   policy, rather than trusting an arbitrary valid Apple signature.
2. Generate an offline Ed25519 feed-signing key. Put the 32-byte base64 seed in
   the release secret store, retain the public key/key ID in source review, and
   rotate by shipping a release that trusts both old and new keys before using
   the new key alone. Never put the seed in this repository, a DMG, or an app.
3. Select one HTTPS host that supports immutable object names, no content
   rewriting, and a separately cache-controlled stable manifest endpoint.
   Upload the DMG, `update.json`, and `update.json.sig` atomically after local
   verification. Preserve all old DMGs and manifests for rollback.
4. Inject `NAH_CODESIGN_IDENTITY`, `NAH_UPDATE_SIGNING_KEY_FILE`,
   `NAH_UPDATE_PUBLIC_KEY`, `NAH_UPDATE_KEY_ID`, and `NAH_UPDATE_BASE_URL` only
   into the isolated release environment. Build from a clean, pinned checkout:

   ```sh
   scripts/package-macos-release.sh 0.1.0 1
   scripts/verify-macos-release.sh "$NAH_UPDATE_PUBLIC_KEY" \
     target/release-dist/Not-a-Harness-0.1.0+1-macos-arm64/update.json \
     target/release-dist/Not-a-Harness-0.1.0+1-macos-arm64/update.json.sig
   ```

5. On a clean macOS test account, mount the notarized DMG, drag the app to
   `/Applications`, run it, confirm the nested `nah-service` starts, and check
   `codesign --verify --deep --strict`, `spctl --assess --type execute`, and
   `stapler validate`. Use both Apple Silicon and Intel test machines where
   they are supported.

## Install, update, and rollback behavior

Installation is a signed-DMG drag install to `/Applications/Not a Harness.app`.
The desktop starts the bundled `nah-service` only when no local service is
reachable. Closing the desktop leaves the daemon and PTYs alive by design.

Before an update, the future UI must query the service for active panes and
show **Update after sessions end** whenever any PTY or SSH workspace is live.
It must not terminate, force-restart, or relaunch the service. Once the user
has ended all terminal sessions and the service is stopped, the installer can:

1. Verify the signed metadata, exact DMG size/hash, Developer ID Team ID,
   hardened runtime, notarization, and bundle identifier.
2. Copy the new app to a same-volume temporary sibling of `/Applications`,
   atomically replace the old app, and relaunch only the desktop executable.
3. Keep the prior versioned DMG and app backup until the new desktop reaches a
   successful local service handshake. A protocol mismatch must fail closed and
   offer rollback; it must never attempt to “upgrade” a live service.

Rollback means quitting the desktop, confirming no `nah-service` owns active
PTYS, restoring the previous signed app bundle, and relaunching it. The
versioned prior DMG and signed manifest remain the authority for that restore.
Desired-state recovery can recreate local shells after a service stop, but it
does not preserve arbitrary live processes, SSH authentication, or terminal
output; release notes must say this plainly.

## Release checklist

- [ ] Bump the workspace semantic version and choose a never-reused positive
  build number.
- [ ] Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, and `cargo test --workspace --all-targets --all-features`.
- [ ] Build and inspect the app bundle. Confirm `nah`, `nah-service`, and
  `nah-update-tool` in `Contents/MacOS`, plus `Not-a-Harness.icns` in
  `Contents/Resources`.
- [ ] Sign, notarize, staple, and validate using the production identities.
- [ ] Make and independently verify the signed immutable manifest and DMG.
- [ ] Test install, first launch, normal desktop quit/reopen, clean service
  shutdown, update deferral with live local/SSH sessions, update after
  quiescence, and signed rollback.
- [ ] Publish artifacts first, then the stable signed manifest; perform a
  fresh-host download verification. Do not publish a “latest” mutable DMG.
