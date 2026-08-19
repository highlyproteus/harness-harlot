# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| Unreleased `main` | Yes |
| 0.1.x | Yes |
| Earlier versions | No |

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/highlyproteus/harness-harlot/security/advisories/new) on the release mirror. Do not open a public issue for an unpatched vulnerability. Include the affected version, reproduction steps, impact, and any proposed mitigation.

The project aims to acknowledge a complete report within 7 days, coordinate a fix and release, and publish a disclosure within 90 days. Active exploitation or broad user risk may require a shorter timeline.

## Security boundaries

Harness Harlot is an unsandboxed terminal emulator. It intentionally starts shells, creates PTYs, accesses the user's files, and can run SSH and tmux. Hardened runtime and minimal entitlements reduce ambient risk; they do not turn a terminal into a sandbox.

CEF renderer, GPU, and utility subprocesses remain inside Chromium's sandbox.
macOS uses the bundled CEF sandbox integration. User-local Linux packages rely
on unprivileged user namespaces because they cannot install a setuid
`chrome-sandbox`; browser tabs are disabled when the kernel restricts those
namespaces. Harness Harlot never falls back to `--no-sandbox`. Browser profile
data is local under the owner-only application-state directory's
`browser-cache` subtree.

Processes running as the same local user are not a security boundary. Socket ownership, permissions, peer credentials, frame bounds, and timeouts provide defense in depth against cross-account access and confused-deputy failures. A malicious process already running as the user can access that user's files and interfere with their terminal sessions.

Terminal history is stored on-device under the Harness Harlot application-state directory. It is enabled by default with the product's existing indefinite, 5-GiB policy. PTY output can include echoed commands and secrets printed by programs. File permissions and storage validation reduce accidental exposure; terminal output cannot be reliably scrubbed of secrets.

History chunk checksums detect accidental corruption and bit rot. They are not cryptographic tamper evidence against a process running as the user.

First-party Rust denies `unsafe_code` workspace-wide. Item-level allows are limited to seven Objective-C bridge sites in `crates/cef-view/src/cef_macos.rs`, four AppKit/Foundation sites in `crates/macos-icon/src/lib.rs`, and the process-entry Linux environment update that selects GPUI's X11 backend in `crates/desktop/src/browser.rs`. Every site has a local `SAFETY` justification; third-party dependencies may contain unsafe code.

## Release trust status

The production feed host and Ed25519 public key are compiled into the updater.
Test-only keys and `.invalid` hosts are rejected by production policy. macOS has
two deliberately isolated trust modes:

1. **Community:** the user first verifies the bootstrap installer's GitHub
   build-provenance attestation. That installer verifies attestations for every
   downloaded input before mounting the ad-hoc-signed DMG, then verifies the
   unexpired owner-signed Ed25519 manifest and exact artifact bytes. It never
   disables Gatekeeper or removes quarantine. Automatic replacement is compiled
   out; future updates are notifications only.
2. **Developer ID:** the DMG and mounted app must additionally satisfy the
   pinned Apple Team requirement, hardened runtime checks, and notarization
   staple validation. This path remains fail-closed until the Team ID is
   configured after paid Developer Program enrollment.

The feeds and artifact names differ, preventing a Developer ID client from
selecting a community DMG or the reverse. macOS app bundles contain `hh`,
`hh-service`, and `hh-update-tool`. Linux archives additionally contain
`hh-cef-helper` and the pinned CEF runtime. Release signing tools and fixture
keys are never bundled.

## Release-key rotation

Rotation is a two-release trust transition followed by retirement:

1. Release N ships both old and new public keys but manifests remain signed by the old key. The installer trust material is updated in the same release.
2. Release N+1 signs with the new key and keeps both public keys so clients at N can verify it.
3. Release N+2 removes the old public key, after which the old seed is destroyed.

A client must never be asked to trust a key it has not already received through an independently trusted release. `key_id` binding and manifest expiry are required before rotation.

Suspected key compromise is not an in-band rotation. Publish a new installer trust root through the private-reporting and release channels, revoke the compromised feed key out of band, and treat artifacts signed only under that key as suspect. Apple Developer ID verification is an independent second root when enabled. Community macOS and Linux release provenance is independently attestable through GitHub, while runtime update checks still treat the compiled Ed25519 key as their cryptographic trust root.

## Residual risks

- Same-user malware can read or alter local application state directly.
- The app is unsandboxed by product requirement.
- Known-advisory and license tooling does not audit third-party source code.
- The stable feed uses a single Ed25519 signing key, not threshold signatures.
- Rollback cannot restore live processes, SSH authentication, or terminal output.
- Reproducible byte-identical DMGs are not claimed; signed provenance must accompany any public release as its build record.
- Community macOS installation explicitly trusts the repository's GitHub
  Actions provenance and requires a per-app Gatekeeper override; it is not
  equivalent to Apple notarization.
