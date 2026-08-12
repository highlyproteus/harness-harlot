#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: $0 UPDATE_PUBLIC_KEY UPDATE_JSON UPDATE_SIGNATURE" >&2
  exit 2
fi

public_key=$1
manifest=$2
signature=$3
distribution_directory=$(CDPATH= cd -- "$(dirname -- "$manifest")" && pwd)
artifact=$(awk -F '"' '/"file_name"/ { print $4; exit }' "$manifest")
if [ -z "$artifact" ] || [ ! -f "$distribution_directory/$artifact" ]; then
  echo "signed manifest does not name a local artifact" >&2
  exit 1
fi

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tool="$repository_root/target/release/nah-update-tool"
if [ ! -x "$tool" ]; then
  echo "build nah-update-tool first" >&2
  exit 1
fi
"$tool" verify --public-key "$public_key" --manifest "$manifest" --signature "$signature" --artifact "$distribution_directory/$artifact"

mount_directory=$(mktemp -d "${TMPDIR:-/tmp}/nah-release-mount.XXXXXX")
cleanup() {
  hdiutil detach "$mount_directory" -quiet 2>/dev/null || true
  rmdir "$mount_directory" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM
hdiutil attach -readonly -nobrowse -mountpoint "$mount_directory" "$distribution_directory/$artifact" >/dev/null
app="$mount_directory/Not a Harness.app"
test -f "$app/Contents/Resources/Not-a-Harness.icns"
test -x "$app/Contents/MacOS/nah-update-tool"
plutil -lint "$app/Contents/Info.plist"
codesign --verify --deep --strict --verbose=2 "$app"
echo "verified mounted Not a Harness release artifact"
