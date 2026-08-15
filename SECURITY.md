# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| Unreleased `main` | Yes |
| 0.1.x | Yes |
| Earlier versions | No |

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/justbytes/rust-mux/security/advisories/new). Do not open a public issue for an unpatched vulnerability. Include the affected version, reproduction steps, impact, and any proposed mitigation.

The project aims to acknowledge a complete report within 7 days, coordinate a fix and release, and publish a disclosure within 90 days. Active exploitation or broad user risk may require a shorter timeline.

## Security boundaries

Harness Harlot is an unsandboxed terminal emulator. It intentionally starts shells, creates PTYs, accesses the user's files, and can run SSH and tmux. Hardened runtime and minimal entitlements reduce ambient risk; they do not turn a terminal into a sandbox.

Processes running as the same local user are not a security boundary. Socket ownership, permissions, peer credentials, frame bounds, and timeouts provide defense in depth against cross-account access and confused-deputy failures. A malicious process already running as the user can access that user's files and interfere with their terminal sessions.

Terminal history is stored on-device under the Harness Harlot application-state directory. It is enabled by default with the product's existing indefinite, 5-GiB policy. PTY output can include echoed commands and secrets printed by programs. File permissions and storage validation reduce accidental exposure; terminal output cannot be reliably scrubbed of secrets.

History chunk checksums detect accidental corruption and bit rot. They are not cryptographic tamper evidence against a process running as the user.

First-party Rust denies `unsafe_code` by default. The macOS bridge contains one reviewed AppKit global access and one typed Foundation resource-value call, each locally documented and allowed; third-party dependencies may contain unsafe code.

## Release trust status

No production update host, Ed25519 public key, Apple Team ID, or notarized artifact is claimed until release credentials and immutable distribution names are configured. The updater trust root is intentionally empty and fails closed. Test-only keys and `.invalid` hosts are rejected by production policy.

A public macOS release requires both independent checks:

1. The DMG and mounted app satisfy the expected Apple Developer ID Team requirement, hardened runtime checks, and notarization staple validation.
2. The app/update path verifies an unexpired signed manifest with a compiled Ed25519 key whose `key_id` selects that exact key and whose artifact URL uses the compiled host.

The application bundle contains `hh` and `hh-service` only. Release signing tools and fixture keys are never bundled.

## Release-key rotation

Rotation is a two-release trust transition followed by retirement:

1. Release N ships both old and new public keys but manifests remain signed by the old key. The installer trust material is updated in the same release.
2. Release N+1 signs with the new key and keeps both public keys so clients at N can verify it.
3. Release N+2 removes the old public key, after which the old seed is destroyed.

A client must never be asked to trust a key it has not already received through an independently trusted release. `key_id` binding and manifest expiry are required before rotation.

Suspected key compromise is not an in-band rotation. Publish a new installer trust root and installer hash through the private-reporting and release channels, revoke the compromised feed key out of band, and treat artifacts signed only under that key as suspect. Apple Developer ID verification remains the independent second root; compromise of either single key is insufficient for a valid release.

## Residual risks

- Same-user malware can read or alter local application state directly.
- The app is unsandboxed by product requirement.
- Known-advisory and license tooling does not audit third-party source code.
- The v1 feed uses a single Ed25519 signing key, not threshold signatures.
- Rollback cannot restore live processes, SSH authentication, or terminal output.
- Reproducible byte-identical DMGs are not claimed; signed provenance must accompany any public release as its build record.
