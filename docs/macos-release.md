# Harness Harlot macOS release channels

Harness Harlot has one version stream with two isolated macOS trust modes:
an unnotarized `community` artifact available without Apple enrollment, and an
optional Developer ID artifact. Both use the owner-held Ed25519 update key and
GitHub build provenance. Their manifest and DMG names differ, so neither build
can select the other mode's artifact.

## What is implemented now

- `scripts/build-macos-app.sh release --browser --community` builds the
  no-cost app with CEF, a production verifier, and the compile-time
  notify-only update policy. `scripts/build-macos-app.sh release --browser`
  retains the Developer ID layout.
- `scripts/package-macos-release.sh VERSION BUILD --community` signs nested
  code ad hoc, emits a `*-community.dmg`, and publishes
  `manifest-macos-community-ARCH.update.json`. It requires the signed tag,
  release update key, and pinned CEF, but no Apple credential.
- `scripts/package-macos-release.sh VERSION BUILD` retains the separate
  Developer ID, notarization, stapling, Team-ID, and normal
  `manifest-macos-ARCH.update.json` path.
- The attested `install-community-macos.sh` authenticates release inputs with
  GitHub provenance before mounting a DMG, then uses the bundled verifier to
  validate the Ed25519 manifest and exact artifact bytes.
- Community apps notify about newer community releases but both the UI and
  `hh-update-tool install` refuse automatic replacement. Developer ID builds
  retain the verified staged-swap updater once `TRUSTED_APPLE_TEAM_ID` is set.
- Every manifest requires a quiescent session service. Manual community
  replacement and automatic Developer ID replacement must not strand live PTYs.

The package script refuses any dirty checkout, including untracked files.
Every production mode requires a signed tag, pinned CEF, and the distinct
Ed25519 feed-signing key. Developer ID mode additionally requires its signing
identity, expected Team ID, and notarytool profile. `HH_RELEASE_TEST_MODE=1`
still requires an explicitly supplied fixture seed and matching public key; its
`TESTONLY-` artifact and `.invalid` URL can never pass production policy.

## No-cost community trust boundary

Apple does not provide Developer ID certificates or notarization to free
accounts. A Homebrew formula, DMG container, or self-signed certificate cannot
remove that Gatekeeper boundary. The community channel therefore makes the
tradeoff visible instead of weakening the Developer ID checks:

1. The release workflow verifies the GPG-signed tag, builds on the matching
   macOS architecture, and publishes GitHub build-provenance attestations for
   the installer, DMG, manifest, and signature.
2. The user independently verifies the installer attestation. The installer
   then verifies every downloaded input's attestation before mounting anything.
3. The verifier from that authenticated DMG checks the owner-held Ed25519
   signature and exact DMG size/SHA-256. Ad-hoc code signatures are then checked
   for bundle integrity and exact bundle identity/architecture.
4. Installation targets `~/Applications`; an existing Developer ID app is not
   silently replaced by a community app. Automatic update installation remains
   disabled, so every future replacement repeats the explicit trust step.

The installer never removes quarantine, disables Gatekeeper, uses `sudo`, or
pipes network content into a shell. If Gatekeeper blocks the first launch, use
Apple's per-app **Privacy & Security → Open Anyway** action.

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

## Optional Developer ID handoff

This section is not required for community releases. When an owner later has
Apple credentials:

1. Create a Developer ID Application certificate and record the final Team ID.
   Sign every nested executable and the outer app using hardened runtime,
   notarize the DMG, and staple its ticket. Pin that Team ID in installer
   policy rather than trusting an arbitrary valid Apple signature.
2. Keep using the same offline Ed25519 feed-signing key. Retain its public
   key/key ID in source review and rotate by shipping one release that trusts
   both old and new keys before using the new key alone. Never put the seed in
   a DMG or app.
3. Inject `HH_CODESIGN_IDENTITY`, `HH_EXPECTED_TEAM_ID`,
   `HH_NOTARY_PROFILE`, `HH_UPDATE_SIGNING_KEY_FILE`,
   `HH_UPDATE_PUBLIC_KEY`, `HH_UPDATE_KEY_ID`, `HH_UPDATE_BASE_URL`, and
   `HH_RELEASE_TAG` only into the isolated release environment. From a clean,
   signed-tag checkout:

   ```sh
   scripts/package-macos-release.sh 0.1.0 1
   scripts/verify-macos-release.sh "$HH_EXPECTED_TEAM_ID" \
     com.harnessharlot.desktop \
     target/release-dist/Harness-Harlot-0.1.0-b1-macos-arm64/manifest-macos-arm64.update.json \
     target/release-dist/Harness-Harlot-0.1.0-b1-macos-arm64/manifest-macos-arm64.update.json.sig
   ```

4. On clean Apple Silicon and Intel test accounts, install the notarized DMG,
   run it, confirm the nested service starts, and check
   `codesign --verify --deep --strict`, `spctl --assess --type execute`, and
   `stapler validate`.

The pinned `.github/workflows/release.yml` workflow runs only for pushed tags.
The protected `release` environment always needs variable
`HH_UPDATE_KEY_ID=hh-stable-2026` and secrets
`RELEASE_TAG_GPG_PUBLIC_KEY`, `HH_UPDATE_SIGNING_SEED`, and
`HH_UPDATE_PUBLIC_KEY`. That is sufficient for community macOS and both Linux
architectures. The workflow verifies the tag, packages community builds on
Apple Silicon and Intel runners, attests every release file plus the bootstrap
installer, generates an attested CycloneDX SBOM, and publishes one immutable
GitHub release.

Developer ID matrix entries are disabled unless repository variable
`HH_ENABLE_APPLE_SIGNING=true`. Enabling them additionally requires variables
`HH_CODESIGN_IDENTITY` and `HH_EXPECTED_TEAM_ID`, plus secrets
`MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD`, `APPLE_API_KEY_P8`,
`APPLE_API_KEY_ID`, and `APPLE_API_ISSUER_ID`.

## Install, update, and rollback behavior

Both modes install without `sudo` at
`~/Applications/Harness Harlot.app` with `~/.local/bin/hh`.

For a community first install, download `install-community-macos.sh` from the
release, verify its GitHub attestation, and run it with the explicit
`--acknowledge-unnotarized` flag. The script verifies all release attestations,
the Ed25519 manifest, exact DMG bytes, ad-hoc signatures, bundle identifier,
primary executable set, and CPU architecture before staging. It refuses a
running desktop, asks the current managed service to persist and stop only
after all terminal sessions have ended, and never overwrites a Developer ID
app. A failed staged replacement restores the prior community bundle. Updates
stay notify-only and repeat this manual process.

Developer ID installation uses `install.sh` after the Team ID is configured.
Before an automatic update, the UI queries the service for active panes and
shows **Update after sessions end** whenever a PTY or SSH workstation is live.
Once the user has ended all terminal sessions, that installer:

1. Downloads and verifies signed metadata, exact DMG size/hash, Developer ID
   Team ID, hardened runtime, notarization, and bundle identifier.
2. Waits for the desktop process to exit, asks the quiescent session service to
   persist and stop, and stages the app on the destination filesystem.
3. Replaces the app bundle and command link as an ordered transaction, retains
   the prior app as `Harness Harlot.previous.app`, and launches the new desktop.
4. On replacement or relaunch failure, restores and validates the prior app and
   command link.

Rollback requires no live service-owned PTYs. Desired-state recovery can
recreate local shells after a service stop, but it does not preserve arbitrary
live processes, SSH authentication, or terminal output; release notes must say
this plainly.

Linux releases use the verified `.tar.gz` and `hh-update-tool install-local`
flow instead of a DMG. The unprivileged installer stages the application at
`~/.local/lib/harness-harlot`, retains
`~/.local/lib/harness-harlot.previous`, and manages symlinks at
`~/.local/bin/hh`, `~/.local/share/applications`, and
`~/.local/share/icons`. See [Linux releases](linux-release.md) for exact asset
names, integration-link paths, trust checks, rollback, and command-line update
instructions.

## Release checklist

- [ ] Bump the workspace semantic version and choose a never-reused positive
  build number.
- [ ] Create and push a signed annotated release tag at the exact commit being
  packaged; set `HH_RELEASE_TAG` to that tag.
- [ ] Run `cargo fmt --all --check`, `cargo clippy --locked --workspace
  --all-targets --all-features -- -D warnings`, and `cargo test --locked
  --workspace --all-targets --all-features`.
- [ ] Build and inspect the app bundle. Confirm `Contents/MacOS` contains
  exactly `hh`, `hh-service`, and `hh-update-tool`; confirm
  `Contents/Frameworks` contains the pinned Chromium Embedded Framework and
  five signed helper app bundles, and `Contents/Resources` contains
  `Harness-Harlot.icns`.
- [ ] For community artifacts, verify ad-hoc signatures, all GitHub
  attestations, the distinct community manifest name, manual first launch, and
  notify-only update behavior on both architectures.
- [ ] If `HH_ENABLE_APPLE_SIGNING=true`, sign, notarize, staple, verify the
  pinned Team ID, and exercise automatic update/rollback on both architectures.
- [ ] Publish versioned artifacts and regenerate
  `manifest-macos-community-ARCH.update.json` in every release. When Developer
  ID packaging is enabled, regenerate `manifest-macos-ARCH.update.json` in the
  same release. Publish each matching `.sig`, perform a fresh-host attestation
  and install check, and never publish a mutable unversioned DMG.

## Release handoff runbook

Ordered owner steps from a staged repository to a real no-cost release:

1. **Add the repository SSH key.** Add `~/.ssh/highly_ssh.pub` to both
   gitlab.com → Preferences → SSH Keys on the `highlyproteus` account and
   github.com → Settings → SSH keys on the `HighlyProtean` account.
2. **Create both repositories.** Create
   `gitlab.com/highlyproteus/harness-harlot` as the canonical source and
   `github.com/HighlyProtean/harness-harlot` as the downstream release mirror.
   Local `origin` points to GitLab; the read-only `github` remote exists only
   for inspection and release diagnostics.
3. **Configure the GitLab push mirror.** Under GitLab **Settings → Repository →
   Mirroring repositories**, push-mirror to
   `https://github.com/HighlyProtean/harness-harlot.git`. Authenticate with a
   narrowly scoped GitHub fine-grained token granting repository Contents and
   Workflows read/write. Do not push commits directly to the GitHub mirror.
4. **Commit and push the canonical repository.** Run
   `git push -u origin main`, then force one mirror update and confirm the same
   commit and signed tags appear on GitHub.
5. **Store the required GitHub secrets**:
   - `RELEASE_TAG_GPG_PUBLIC_KEY` — public key for tag signing
   - `HH_UPDATE_SIGNING_SEED` — contents of `~/.ssh/hh-update-signing-seed`
   - `HH_UPDATE_PUBLIC_KEY` — `W3xGpnOmpqVPsaJDWI8LF25g3/Y24DkuHJWkOldH9DE=`
6. **Store the required GitHub variable**:
   `HH_UPDATE_KEY_ID=hh-stable-2026`. Leave
   `HH_ENABLE_APPLE_SIGNING` unset or `false`.
7. **Cut a community release.** Bump the workspace version, create a
   GPG-signed annotated tag `vX.Y.Z`, and push it to GitLab. The push mirror
   transfers the tag to GitHub, where the workflow packages, attests, and
   publishes community macOS plus Linux artifacts without an Apple account.
8. **Verify from a clean Mac.** Attest and run
   `install-community-macos.sh --acknowledge-unnotarized`, exercise first launch
   and Open Anyway if macOS asks, then confirm a newer fixture is notification
   only.

Optional Developer ID upgrade, when funding/credentials become available:

9. Enroll in the Apple Developer Program, record the 10-character Team ID,
   export the Developer ID Application certificate/private key as `.p12`, and
   create the App Store Connect notary `.p8` key triple.
10. Add secrets `MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD`,
   `APPLE_API_ISSUER_ID`, `APPLE_API_KEY_ID`, and `APPLE_API_KEY_P8`; add
   variables `HH_CODESIGN_IDENTITY`, `HH_EXPECTED_TEAM_ID`, and
   `HH_ENABLE_APPLE_SIGNING=true`.
11. Fill `EXPECTED_TEAM_ID` in `install.sh` and
   `TRUSTED_APPLE_TEAM_ID` in `crates/updater/src/lib.rs`, then repeat the full
   Developer ID install, Gatekeeper, automatic update, and rollback checks.

### Update-key custody

`~/.ssh/hh-update-signing-seed` is the stable-channel signing seed. Keep an
offline copy (password manager or printed). Rotation: generate a new seed,
add a second `TrustedKey` entry with a new `key_id` (e.g.
`hh-stable-2027`), publish one release signed with both keys' manifests if
needed, then remove the retired key in the next release. Never place the
seed in the repository, CI logs, or any machine you do not control.
