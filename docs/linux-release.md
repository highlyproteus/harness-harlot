# Harness Harlot Linux releases

Harness Harlot publishes native `x86_64` and `arm64` Linux packages from Ubuntu 22.04 containers. The release baseline is glibc 2.35. Linux packages do not use Apple signing, Gatekeeper, notarization, or an Apple Developer account.

## Install a release

Download these assets for the machine's architecture from the same GitHub Release:

- `Harness-Harlot-VERSION-bBUILD-linux-ARCH.tar.gz`
- `Harness-Harlot-VERSION-bBUILD-linux-ARCH.update.json`
- `Harness-Harlot-VERSION-bBUILD-linux-ARCH.update.json.sig`

GitHub Actions publishes a build-provenance attestation for every asset. Verify the archive before extracting it when GitHub CLI is available:

```bash
gh attestation verify Harness-Harlot-*-linux-*.tar.gz \
  --repo HighlyProtean/harness-harlot
```

Then extract and run the unprivileged installer:

```bash
tar -xzf Harness-Harlot-*-linux-*.tar.gz
./Harness-Harlot/install.sh
```

The installer refuses root and writes only below the current user's home directory:

- application: `~/.local/lib/harness-harlot`
- previous version: `~/.local/lib/harness-harlot.previous`
- command: `~/.local/bin/hh`
- desktop entry: `~/.local/share/applications/com.harnessharlot.desktop.desktop`
- icon: `~/.local/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png`

The command, desktop entry, and icon are managed symlinks. Installation aborts rather than replacing unrelated files. Use `./Harness-Harlot/install.sh --prefix "$HOME/PATH"` to choose another application directory inside the home directory; the integration links remain under `~/.local`.

Start the app from the desktop launcher or run:

```bash
~/.local/bin/hh
```

The package includes `hh`, `hh-service`, `hh-update-tool`, `hh-cef-helper`, the pinned CEF runtime, the desktop launcher, icon, license notices, and its installer.

## Chromium browser support

Browser tabs are supported on X11 and on Wayland desktops through XWayland.
Ubuntu, Fedora, and Arch default desktop installations provide a qualifying
XWayland session. A browser-enabled build checks `DISPLAY` at process entry; if
it is non-empty, Harness Harlot removes `WAYLAND_DISPLAY` before GPUI starts so
GPUI selects its X11 backend and CEF can embed an X11 child window. A
Wayland-only session without XWayland continues to run terminals and reports
that browser tabs require X11 or XWayland.

CEF resolves `libcef.so` beside `hh` and `hh-cef-helper` through an `$ORIGIN`
rpath. This also works when `hh` is launched through the managed
`~/.local/bin/hh` symlink. Browser profile and cache data stays owner-local
under `$XDG_STATE_HOME/hh/browser-cache` or
`~/.local/state/hh/browser-cache`; `HH_STATE_DIR` relocates the state root.

The user-local installer does not install a setuid sandbox helper. Chromium
therefore uses unprivileged user namespaces. Harness Harlot disables browser
tabs, without starting a helper or adding `--no-sandbox`, when either
`apparmor_restrict_unprivileged_userns=1` or
`unprivileged_userns_clone=0`. Terminal panes remain available.

Install CEF's dynamic runtime dependencies with the distribution packages:

```text
Ubuntu 22.04: libgtk-3-0 libnss3 libasound2 libgbm1
Ubuntu 24.04+: libgtk-3-0t64 libnss3 libasound2t64 libgbm1
Fedora:       gtk3 nss alsa-lib mesa-libgbm
Arch:         gtk3 nss alsa-lib mesa
```

The installer and updater run `ldd` against packaged `libcef.so` and print a
non-fatal warning listing unresolved libraries. A missing optional desktop
dependency does not invalidate an otherwise authenticated installation.

## Automatic updates

A packaged production build checks the architecture-specific signed manifest shortly after launch and every 24 hours. Development builds do not make update requests. A newer version or build displays an update button in the sidebar.

Clicking the button performs this sequence:

1. Refuse the update while any local, SSH, or tmux terminal remains live.
2. Launch the bundled updater and close the desktop process.
3. Fetch `manifest-linux-ARCH.update.json` and its detached Ed25519 signature from GitHub Releases.
4. Verify the compiled release key, stable channel, expiry, platform, architecture, glibc floor, immutable HTTPS host, artifact name, exact byte count, and SHA-256.
5. Reject archive traversal, links, special files, duplicate files, unexpected files, unsafe ownership, and unsafe permissions.
6. Ask the quiescent session service to persist and exit.
7. Stage the new application on the destination filesystem, retain the current application as `harness-harlot.previous`, atomically replace the application and integration links, and relaunch the desktop.
8. Restore the prior application if replacement or relaunch fails.

The equivalent command-line entry point is:

```bash
~/.local/lib/harness-harlot/bin/hh-update-tool install
```

The release manifest and archive are signed with the same offline Ed25519 update key used by the macOS channel. Linux package trust does not depend on Apple credentials. Repository/package signing can be added later for `.deb`, `.rpm`, or AppImage distribution without changing this signed update-feed contract.

## Release automation

`scripts/package-linux-release.sh VERSION BUILD` builds and signs one native architecture. Production mode requires:

- `HH_RELEASE_TAG`: signed annotated tag resolving to `HEAD`
- `HH_UPDATE_SIGNING_KEY_FILE`: owner-only base64 Ed25519 seed file
- `HH_UPDATE_PUBLIC_KEY`: matching base64 public key
- `HH_UPDATE_KEY_ID`: production key ID
- `HH_UPDATE_BASE_URL`: immutable HTTPS release directory
- `CEF_PATH`: unpacked, architecture-matched Linux CEF distribution

The script refuses a dirty tree and test-only keys/hosts in production. It builds
the desktop with the `browser` feature, packages the CEF runtime flat beside the
binaries, and emits immutable versioned assets plus
`manifest-linux-ARCH.update.json` and `.sig` stable aliases under
`target/release-dist/linux-ARCH/`.

`.github/workflows/release.yml` downloads and SHA-1-verifies the CEF 151 minimal
archive pinned to `cef = 151.6.0`, then runs the package job for x86_64 and arm64
inside Ubuntu 22.04 containers, attests the outputs, and publishes them with the
macOS assets. CI compiles the Linux browser feature against the same pinned
archive. Release approval still requires real X11 and Wayland-plus-XWayland GPU
smoke tests on both architectures; container compilation does not replace that
visual/runtime gate.
