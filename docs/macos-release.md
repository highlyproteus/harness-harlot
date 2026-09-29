# Harness Harlot macOS release channels

Harness Harlot has one version stream with two isolated macOS trust modes:
an unnotarized `community` artifact available without Apple enrollment, and an
optional Developer ID artifact. Both use the owner-held Ed25519 update key and
GitHub build provenance. Their manifest and DMG names differ, so neither build
can select the other mode's artifact.

## What is implemented now

- `scripts/build-macos-app.sh release --browser --community` builds the
  no-cost app with CEF and a production verifier supporting explicit updates.
  `scripts/build-macos-app.sh release --browser` retains the Developer ID layout.
- `scripts/package-macos-release.sh VERSION BUILD --community` signs nested
  code ad hoc, emits a `*-community.dmg`, and publishes
  `manifest-macos-community-ARCH-v2.update.json`. Production packaging is
  unsigned and requires a signed tag and pinned CEF, but no update or Apple credential.
- `scripts/package-macos-release.sh VERSION BUILD` retains the separate
  Developer ID, notarization, stapling, Team-ID, and normal
  `manifest-macos-ARCH-v2.update.json` path.
- `https://harnessharlot.com/install` serves `install-community-macos.sh`
  without requiring GitHub CLI. The website release index pins the SHA-256 of
  every release input; publication verifies GitHub provenance before updating
  that index. The installer checks those hashes before mounting a DMG, then
  uses the bundled verifier to validate the Ed25519 manifest and exact artifact
  bytes.
- Packaged community apps support explicit verified updates from the sidebar or
  `hh update`; Developer ID builds retain the separate staged-swap trust path
  once `TRUSTED_APPLE_TEAM_ID` is set.
- Manual community replacement still requires terminal sessions to end. The
  in-app updater preserves compatible services and asks once before restarting
  an incompatible service with live terminals.

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
2. The publication workflow verifies every release input's attestation against
   the exact signed-tag release workflow, then publishes their URLs, sizes, and
   SHA-256 values through the HTTPS website trust root.
3. The installer verifies each downloaded input against that website-pinned
   metadata before mounting anything.
4. The verifier from that authenticated DMG checks the owner-held Ed25519
   signature and exact DMG size/SHA-256. Ad-hoc code signatures are then checked
   for bundle integrity and exact bundle identity/architecture.
5. Installation uses `/Applications` when writable and otherwise
   `~/Applications`; an existing Developer ID app is not
   silently replaced by a community app. Subsequent explicit updates authenticate
   release metadata and artifacts using the compiled update trust policy.

The installer never removes quarantine, disables Gatekeeper, or uses `sudo`.
The short bootstrap pipes only the HTTPS-authenticated website script to the
system shell; all release artifacts are downloaded to files and verified before
they are mounted or executed. If Gatekeeper blocks the first launch, use Apple's
per-app **Privacy & Security → Open Anyway** action.

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
     target/release-dist/Harness-Harlot-0.1.0-b1-macos-arm64/manifest-macos-arm64-v2.update.json \
     target/release-dist/Harness-Harlot-0.1.0-b1-macos-arm64/manifest-macos-arm64-v2.update.json.sig
   ```

4. On a clean Apple silicon test account, install the notarized DMG,
   run it, confirm the nested service starts, and check
   `codesign --verify --deep --strict`, `spctl --assess --type execute`, and
   `stapler validate`.

The pinned `.github/workflows/release.yml` workflow runs only for pushed tags.
The protected `release` environment always needs variable
`HH_UPDATE_KEY_ID=hh-stable-2026` and secrets
`RELEASE_TAG_GPG_PUBLIC_KEY`, `HH_UPDATE_SIGNING_SEED`, and
`HH_UPDATE_PUBLIC_KEY`. That is sufficient for community macOS and both Linux
architectures. The workflow verifies the tag, packages the community build on
an Apple silicon runner, attests every release file plus the bootstrap
installer, generates an attested CycloneDX SBOM, and publishes one immutable
GitHub release.

Developer ID matrix entries are disabled unless repository variable
`HH_ENABLE_APPLE_SIGNING=true`. Enabling them additionally requires variables
`HH_CODESIGN_IDENTITY` and `HH_EXPECTED_TEAM_ID`, plus secrets
`MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD`, `APPLE_API_KEY_P8`,
`APPLE_API_KEY_ID`, and `APPLE_API_ISSUER_ID`.

### Intel Macs

macOS releases after 0.1.27 are Apple silicon only; the `macos-15-intel`
package job is gone. Intel installs are never offered an arm64 build: the
client's manifest name is fixed at compile time
(`manifest-macos-community-x86_64-v2.update.json`) and the signed artifact
architecture must match. The daily stable-v2 refresh no longer publishes that
Intel alias, so Intel apps keep working but report "Unable to check for
updates". `install-community-macos.sh` refuses Intel Macs with a pointer to
the v0.1.27 release, and treats a Rosetta shell on Apple silicon as arm64.
The frozen v0.1.16 legacy bridge (including its Intel pair) is unchanged.
The website sync (`harness-harlot-landing`) must accept the three-alias
refresh before this change reaches `main`.

## Install, update, and rollback behavior

Both modes install without `sudo` at `/Applications/Harness Harlot.app` when
writable and otherwise `~/Applications/Harness Harlot.app`, with
`~/.local/bin/hh`.

For a community first install, run the HTTPS-only command published at
`harnessharlot.com`:

```sh
curl -fsS https://harnessharlot.com/install | sh
```

The script verifies website-pinned checksums, the Ed25519 manifest, exact DMG
bytes, ad-hoc signatures, bundle identifier, primary executable set, and CPU
architecture before staging. It refuses a
running desktop, asks the current managed service to persist and stop only
after all terminal sessions have ended, and never overwrites a Developer ID
app. A failed staged replacement restores the prior community bundle. This manual
bootstrap remains separate from the explicit in-app update flow.

Developer ID installation uses `install.sh` after the Team ID is configured.
Before an explicit update, the UI compares the signed session-service protocol
with the running build. Routine app-only updates retain the compatible service
and its live PTYs. A protocol-changing update asks for confirmation when terminals
are live or their count is unavailable. The installer then:

1. Downloads and verifies signed metadata and exact DMG size/hash while the
   desktop stays open with a **Downloading…** banner. A download or integrity
   failure restores the update button and displays an error without quitting.
2. Emits `download-complete`, waits for the desktop to exit, and validates the
   staged app against its community ad-hoc or Developer ID trust policy.
3. For a protocol change, requests a quiescent service shutdown first. With
   user-confirmed `--restart-service`, refusal triggers SIGTERM to the exact
   managed executable, allowing persistence and PTY termination. It never sends
   SIGKILL.
4. Replaces the app and command link, retains `.Harness Harlot.previous.app`,
   and launches a fresh desktop. Replacement failures restore the prior bundle.

Local terminals managed by HH's private tmux server resume with their processes
and output after the service restarts. Missing or failed tmux recovery falls
back to fresh shells in the last valid directories. SSH tabs remain offline
until explicitly reconnected. CLI updates without `--restart-service` still
require a quiescent incompatible service. Already-installed older updater
binaries retain their old quiescence gate until they have themselves been
replaced.

Linux releases use the verified `.tar.gz` and `hh-update-tool install-local`
flow instead of a DMG. The unprivileged installer stages the application at
`~/.local/lib/harness-harlot`, retains
`~/.local/lib/harness-harlot.previous`, and manages symlinks at
`~/.local/bin/hh`, `~/.local/share/applications`, and
`~/.local/share/icons`. See [Linux releases](linux-release.md) for exact asset
names, integration-link paths, trust checks, rollback, and command-line update
instructions.

## Fast release path

From "branches finished" to "users are offered the update" in about 45
minutes, with one approval: starting `scripts/release.sh` is the approval to
publish, and nothing prompts afterwards.

1. **Assemble.** Merge the finished branches into `release/vX.Y.Z` (cut from
   `main`), resolve conflicts, and describe the changes under
   `## [Unreleased]` in `CHANGELOG.md`.
2. **Smoke only what can break upgrades.** If anything under `crates/updater`,
   `crates/protocol`, or `crates/session-service` changed since the previous tag,
   build the fixture pair with
   `scripts/build-upgrade-fixtures.sh vPREVIOUS --next-version X.Y.Z` (the
   previous version's fixture is cached and reused), install the old fixture,
   upgrade to the new one, and confirm running terminals, SSH workstations, and
   bots survive. `release.sh` refuses such a release without
   `--upgrade-smoke-passed`. Linux rendering/CEF changes need the GPU smoke in
   `docs/linux-release.md`. Otherwise skip this step.
3. **Ship.** `scripts/release.sh X.Y.Z --title "short summary"`.

| Stage (`release.sh` subcommand) | What happens | Target |
|---|---|---|
| `prepare` | Version bump (Cargo.toml, Cargo.lock, notices), dated changelog heading, `scripts/preflight.sh` (the exact CI gate), commit, push, PR | 2–5 min warm |
| `watch-pr` | CI (Linux gate + full macOS gate), Security, Packaging Assurance; exits at the first failed job | 10–12 min |
| `publish` | Squash-merge, signed tag at the merge commit, push tag to GitHub and GitLab, push `main` to GitLab | 1 min |
| `watch-release` | Tag workflow: CI binding, security, packages (macOS arm64, Linux x86_64/arm64), SBOM, isolated signing, publish; exits at the first failed job | ~20 min |
| `land` | Dispatches the stable-v2 refresh and the website sync, waits until harnessharlot.com serves the version | ~10 min |

Each subcommand resumes a failed run (`release.sh watch-pr N`,
`publish X.Y.Z`, `watch-release vX.Y.Z`, `land vX.Y.Z`). A failed Release job
can be re-run with `gh run rerun ID --failed` only after the whole run
finishes; network steps retry on their own first.

What the automation already proves, so it is not repeated by hand:

- **Code quality:** `preflight.sh` locally, then CI on the PR head tree. The
  tag does not re-run tests: `Verify CI passed for the tagged tree` requires a
  green `Fast quality gate` and `macOS quality gate` for a commit with exactly
  the tagged tree (`scripts/find-green-ci-run.py`). This tree-equality binding
  is newer than the same-SHA binding `edge.yml` uses: it trusts a green run
  from a same-repository pull request whose head tree equals the merge commit's
  tree. Pull request CI checks out the PR head, so that is the tree it tested.
  If `main` moved and the trees differ, the main-push CI runs and the tag waits
  for it (up to 45 minutes).
- **Dependencies and secrets:** `cargo audit`, `cargo deny`, notices,
  shellcheck, and gitleaks run again on the tag; signing waits for them.
- **Tag and artifact trust:** protected signed tag, verified commit author,
  runner architecture, pinned CEF SHA-256, `verify-macos-release.sh` bundle
  and signature checks, provenance attestations, isolated manifest signing,
  and the exact-file publication check.

CI speed rules: build caches are written only from `main` (nightly CI and
main pushes) and pull requests restore them; tag runs never restore or save
build caches. A push to `main` skips CI, Security, and Packaging Assurance when
the same workflow already passed on the pull request with the identical tree;
the nightly runs keep full coverage of `main`.

For Developer ID builds (`HH_ENABLE_APPLE_SIGNING=true`), additionally sign,
notarize, staple, verify the pinned Team ID, and exercise automatic
update/rollback before announcing the release.

## Release handoff runbook

Ordered owner steps from a staged repository to a real no-cost release:

1. **Add the repository SSH key.** Add `~/.ssh/highly_ssh.pub` to both
   gitlab.com → Preferences → SSH Keys on the `highlyproteus` account and
   github.com → Settings → SSH keys on the `highlyproteus` account.
2. **Create both repositories.** Create
   `gitlab.com/highlyproteus/harness-harlot` as the canonical source and
   `github.com/highlyproteus/harness-harlot` as the downstream release mirror.
   Local `origin` points to GitLab; the read-only `github` remote exists only
   for inspection and release diagnostics.
3. **Configure the GitLab push mirror.** Under GitLab **Settings → Repository →
   Mirroring repositories**, push-mirror to
   `https://github.com/highlyproteus/harness-harlot.git`. Authenticate with a
   narrowly scoped GitHub fine-grained token granting repository Contents and
   Workflows read/write. Do not push commits directly to the GitHub mirror.
4. **Commit and push the canonical repository.** Run
   `git push -u origin main`, then force one mirror update and confirm the same
   commit and signed tags appear on GitHub.
5. **Store the required GitHub secrets**:
   - `RELEASE_TAG_GPG_PUBLIC_KEY` — public key for tag signing
   - `HH_UPDATE_SIGNING_SEED` — contents of
     `~/.config/harness-harlot/hh-stable-2026.seed`
   - `HH_UPDATE_PUBLIC_KEY` — `Cy/alHdZ5R7fSJEeuvqu1UXH9j5O0f34hWv4Rv8TFwo=`
6. **Store the required GitHub variable**:
   `HH_UPDATE_KEY_ID=hh-stable-2026`. Leave
   `HH_ENABLE_APPLE_SIGNING` unset or `false`.
7. **Cut a community release.** Follow "Fast release path" above:
   `scripts/release.sh X.Y.Z --title "..."` squash-merges the release PR on
   GitHub, creates the signed annotated tag `vX.Y.Z` at the merge commit, and
   pushes the tag to GitHub and GitLab (plus `main` to GitLab). The tag workflow
   packages, attests, and publishes community macOS (Apple silicon) plus Linux
   artifacts without an Apple account.
8. **Verify from a clean Mac.** Run the website bootstrap with `--verify-only`
   from a saved copy, exercise first launch and Open Anyway if macOS asks, then
   confirm a newer fixture is notification only.

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

`~/.config/harness-harlot/hh-stable-2026.seed` is the stable-channel signing seed. Keep an
offline copy (password manager or printed). Rotation: generate a new seed,
add a second `TrustedKey` entry with a new `key_id` (e.g.
`hh-stable-2027`), publish one release signed with both keys' manifests if
needed, then remove the retired key in the next release. Never place the
seed in the repository, CI logs, or any machine you do not control.
