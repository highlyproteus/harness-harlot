# Harness Harlot macOS stable release foundation

Harness Harlot has one production channel: `stable`. There is no nightly feed or
automatic update network request in the app today.
This document defines the local release surface that can safely become a public
update path once Apple credentials and a public immutable host are selected.

## What is implemented now

- `scripts/build-macos-app.sh release` builds a self-contained app bundle with
  exactly `hh`, `hh-service`, and tracked resources. Release-signing and
  update-verification tools are not copied into the bundle.
- `scripts/package-macos-release.sh VERSION BUILD` signs nested binaries
  inside-out, signs and notarizes the DMG in production mode, and creates a
  detached Ed25519 signature over the exact architecture-qualified manifest bytes. Artifacts are
  immutable and architecture-specific.
- `hh-updater` is verifier-only. Production trust keys and the update host are
  intentionally unconfigured and fail closed until the owner selects them.
  The non-shipped `hh-release-sign` binary owns the signing operation.
- Every manifest requires a quiescent session service. This is an intentional
  update safety boundary, not a temporary warning: a running service owns PTYs
  and terminal processes, so an app update must never stop or replace it.

The production package script refuses any dirty checkout, including untracked files, and requires a
Developer ID signing identity, expected Team ID, notarytool keychain profile,
and distinct Ed25519 feed-signing key. `HH_RELEASE_TEST_MODE=1` still requires
an explicitly supplied fixture seed and matching public key; its `TESTONLY-`
artifact and `.invalid` URL can never pass production policy.

## Why Sparkle is not bundled yet

Sparkle 2 is a mature macOS updater and MIT-licensed, so its license does not
conflict with Harness Harlot's MIT project. It is not being added as an opaque
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
   Upload the DMG, architecture-qualified `*.update.json`, and matching
   `*.update.json.sig` atomically after local verification. Preserve all old DMGs and manifests for rollback.
4. Inject `HH_CODESIGN_IDENTITY`, `HH_EXPECTED_TEAM_ID`,
   `HH_NOTARY_PROFILE`, `HH_UPDATE_SIGNING_KEY_FILE`,
   `HH_UPDATE_PUBLIC_KEY`, `HH_UPDATE_KEY_ID`, and
   `HH_UPDATE_BASE_URL` only into the isolated release environment. Build
   from a clean, pinned checkout:

   ```sh
   update_host=${HH_UPDATE_BASE_URL#https://}
   update_host=${update_host%%/*}
   scripts/package-macos-release.sh 0.1.0 1
   scripts/verify-macos-release.sh "$HH_EXPECTED_TEAM_ID" com.harnessharlot.desktop \
     target/release-dist/Harness-Harlot-0.1.0+1-macos-arm64/Harness-Harlot-0.1.0+1-macos-arm64.update.json \
     target/release-dist/Harness-Harlot-0.1.0+1-macos-arm64/Harness-Harlot-0.1.0+1-macos-arm64.update.json.sig
   ```

5. On a clean macOS test account, mount the notarized DMG, drag the app to
   `/Applications`, run it, confirm the nested `hh-service` starts, and check
   `codesign --verify --deep --strict`, `spctl --assess --type execute`, and
   `stapler validate`. Use both Apple Silicon and Intel test machines where
   they are supported.

The pinned `.github/workflows/release.yml` workflow runs only for pushed tags.
Its protected `release` environment must define repository variables
`HH_CODESIGN_IDENTITY`, `HH_EXPECTED_TEAM_ID`, `HH_UPDATE_BASE_URL`, and
`HH_UPDATE_KEY_ID`, plus secrets `RELEASE_TAG_GPG_PUBLIC_KEY`,
`MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD`, `APPLE_API_KEY_P8`,
`APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`, `HH_UPDATE_SIGNING_SEED`, and
`HH_UPDATE_PUBLIC_KEY`. It rejects a tag whose signature cannot be verified,
builds and notarizes on separate Apple Silicon and Intel runners, attests each
release file, generates an attested CycloneDX SBOM archive, and creates a
non-latest GitHub release. Publishing those verified files to the separately
chosen stable update host remains an explicit owner operation.

## Install, update, and rollback behavior

Installation uses the notarized, signed DMG without `sudo`, targeting
`~/Applications/Harness Harlot.app` and `~/.local/bin/hh`. The generic checked-in
installer remains fail-closed until the release engineer supplies the final
Team ID, immutable artifact URL, signed manifest URL, signature URL, update host,
key ID, and public key. A published convenience script must embed those exact
values and its SHA-256 must be published beside the signed release.

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

Rollback means quitting the desktop, confirming no `hh-service` owns active
PTYS, restoring the previous signed app bundle, and relaunching it. The
versioned prior DMG and signed manifest remain the authority for that restore.
Desired-state recovery can recreate local shells after a service stop, but it
does not preserve arbitrary live processes, SSH authentication, or terminal
output; release notes must say this plainly.

## Release checklist

- [ ] Bump the workspace semantic version and choose a never-reused positive
  build number.
- [ ] Create and push a signed annotated release tag at the exact commit being
  packaged; set `HH_RELEASE_TAG` to that tag.
- [ ] Run `cargo fmt --all --check`, `cargo clippy --locked --workspace
  --all-targets --all-features -- -D warnings`, and `cargo test --locked
  --workspace --all-targets --all-features`.
- [ ] Build and inspect the app bundle. Confirm `Contents/MacOS` contains
  exactly `hh` and `hh-service`, plus `Harness-Harlot.icns` in Resources.
- [ ] Sign, notarize, staple, and validate using the production identities.
- [ ] Make and independently verify the signed immutable manifest and DMG.
- [ ] Test install, first launch, normal desktop quit/reopen, clean service
  shutdown, update deferral with live local/SSH sessions, update after
  quiescence, and signed rollback.
- [ ] Publish artifacts first, then the stable signed manifest; perform a
  fresh-host download verification. Do not publish a “latest” mutable DMG.
