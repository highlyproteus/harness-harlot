#!/bin/sh
set -eu

profile="${1:-debug}"
case "$profile" in
  debug)
    cargo build -p nah-desktop --bin nah
    cargo build -p nah-session-service --bin nah-service
    cargo build -p nah-updater --bin nah-update-tool
    ;;
  release)
    cargo build --release -p nah-desktop --bin nah
    cargo build --release -p nah-session-service --bin nah-service
    cargo build --release -p nah-updater --bin nah-update-tool
    ;;
  *)
    echo "usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app_directory="$repository_root/target/$profile/Not a Harness.app"
contents_directory="$app_directory/Contents"

mkdir -p "$contents_directory/MacOS" "$contents_directory/Resources"
cp "$repository_root/packaging/macos/Info.plist" "$contents_directory/Info.plist"
cp "$repository_root/packaging/macos/Not-a-Harness.icns" \
  "$contents_directory/Resources/Not-a-Harness.icns"
cp "$repository_root/crates/desktop/assets/notaharness-banner.png" \
  "$contents_directory/Resources/notaharness-banner.png"
cp "$repository_root/target/$profile/nah" \
  "$contents_directory/MacOS/nah"
cp "$repository_root/target/$profile/nah-service" \
  "$contents_directory/MacOS/nah-service"
if [ -x "$repository_root/target/$profile/nah-update-tool" ]; then
  cp "$repository_root/target/$profile/nah-update-tool" \
    "$contents_directory/MacOS/nah-update-tool"
fi
chmod 755 "$contents_directory/MacOS/nah"
chmod 755 "$contents_directory/MacOS/nah-service"
if [ -f "$contents_directory/MacOS/nah-update-tool" ]; then
  chmod 755 "$contents_directory/MacOS/nah-update-tool"
fi

# Replacing nested executables invalidates any signature left on a prior bundle.
# Re-sign the complete local app. Release packaging replaces this ad-hoc
# signature with the configured distribution identity.
codesign --force --deep --sign - "$app_directory"

echo "$app_directory"
