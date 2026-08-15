#!/bin/sh
set -eu

profile="${1:-debug}"
case "$profile" in
  debug)
    cargo build --locked -p hh-desktop --bin hh
    cargo build --locked -p hh-session-service --bin hh-service
    ;;
  release)
    cargo build --locked --release -p hh-desktop --bin hh
    cargo build --locked --release -p hh-session-service --bin hh-service
    ;;
  *)
    echo "usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app_directory="$repository_root/target/$profile/Harness Harlot Dev.app"
contents_directory="$app_directory/Contents"
rm -rf "$app_directory"

notices_directory="$contents_directory/Resources/licenses"
mkdir -p "$contents_directory/MacOS" "$notices_directory/third_party/licenses"
cp "$repository_root/packaging/macos/Info-dev.plist" "$contents_directory/Info.plist"
cp "$repository_root/packaging/macos/Harness-Harlot-Dev.icns" \
  "$contents_directory/Resources/Harness-Harlot-Dev.icns"
cp "$repository_root/crates/desktop/assets/harnessharlot-banner.png" \
  "$contents_directory/Resources/harnessharlot-banner.png"
cp "$repository_root/LICENSE" "$notices_directory/LICENSE"
cp "$repository_root/THIRD_PARTY_NOTICES.md" "$notices_directory/THIRD_PARTY_NOTICES.md"
cp "$repository_root/ASSET_NOTICES.md" "$notices_directory/ASSET_NOTICES.md"
cp "$repository_root"/third_party/licenses/* "$notices_directory/third_party/licenses/"
cp "$repository_root/packaging/macos/hh-dev" "$contents_directory/MacOS/hh-dev"
cp "$repository_root/target/$profile/hh" "$contents_directory/MacOS/hh"
cp "$repository_root/target/$profile/hh-service" "$contents_directory/MacOS/hh-service"
chmod 755 "$contents_directory/MacOS/hh-dev" "$contents_directory/MacOS/hh" \
  "$contents_directory/MacOS/hh-service"

# Replacing nested executables invalidates any signature left on a prior bundle.
# Re-sign the complete development app so macOS can launch the rebuilt binaries.
codesign --force --deep --sign - "$app_directory"

echo "$app_directory"
