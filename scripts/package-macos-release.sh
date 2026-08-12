#!/bin/sh
set -eu

usage() {
  echo "usage: $0 VERSION BUILD" >&2
  echo "set NAH_RELEASE_TEST_MODE=1 for a local fixture package, or provide NAH_CODESIGN_IDENTITY, NAH_UPDATE_SIGNING_KEY_FILE, NAH_UPDATE_PUBLIC_KEY, NAH_UPDATE_KEY_ID, and NAH_UPDATE_BASE_URL for a publishable package." >&2
  exit 2
}

if [ "$#" -ne 2 ]; then
  usage
fi

version=$1
build=$2
case "$version" in
  *[!0-9A-Za-z.-]* | '' | *..* | .* | *.) echo "VERSION must be a plain semantic version" >&2; exit 2 ;;
esac
case "$build" in
  *[!0-9]* | 0 | '') echo "BUILD must be a positive integer" >&2; exit 2 ;;
esac

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

workspace_version=$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)
if [ "$workspace_version" != "$version" ]; then
  echo "VERSION $version does not match workspace version $workspace_version" >&2
  exit 2
fi

case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported macOS architecture: $(uname -m)" >&2; exit 2 ;;
esac

test_mode=${NAH_RELEASE_TEST_MODE:-0}
if [ "$test_mode" = 1 ]; then
  key_id=test-only-v1
  base_url=${NAH_UPDATE_BASE_URL:-https://updates.example.invalid/stable}
else
  : "${NAH_CODESIGN_IDENTITY:?set NAH_CODESIGN_IDENTITY for a publishable package}"
  : "${NAH_UPDATE_SIGNING_KEY_FILE:?set NAH_UPDATE_SIGNING_KEY_FILE for a publishable package}"
  : "${NAH_UPDATE_PUBLIC_KEY:?set NAH_UPDATE_PUBLIC_KEY for a publishable package}"
  : "${NAH_UPDATE_KEY_ID:?set NAH_UPDATE_KEY_ID for a publishable package}"
  : "${NAH_UPDATE_BASE_URL:?set NAH_UPDATE_BASE_URL for a publishable package}"
  key_id=$NAH_UPDATE_KEY_ID
  base_url=$NAH_UPDATE_BASE_URL
fi
case "$base_url" in
  https://*) ;;
  *) echo "NAH_UPDATE_BASE_URL must use HTTPS" >&2; exit 2 ;;
esac
base_url=${base_url%/}

if ! git diff --quiet || ! git diff --cached --quiet; then
  if [ "$test_mode" = 1 ] && [ "${NAH_ALLOW_DIRTY_TEST_PACKAGE:-0}" = 1 ]; then
    echo "warning: creating a test-only package from a dirty worktree" >&2
  else
    echo "refusing to package a tracked dirty worktree" >&2
    exit 2
  fi
fi

cargo build --locked --release -p nah-desktop --bin nah
cargo build --locked --release -p nah-session-service --bin nah-service
cargo build --locked --release -p nah-updater --bin nah-update-tool
"$repository_root/scripts/build-macos-app.sh" release

app_directory="$repository_root/target/release/Not a Harness.app"
plist="$app_directory/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$plist"
plutil -replace CFBundleVersion -string "$build" "$plist"

if [ "$test_mode" = 1 ]; then
  codesign --force --sign - "$app_directory"
else
  codesign --force --options runtime --timestamp --sign "$NAH_CODESIGN_IDENTITY" "$app_directory"
fi
codesign --verify --deep --strict --verbose=2 "$app_directory"

artifact_stem="Not-a-Harness-${version}+${build}-macos-${architecture}"
distribution_directory="$repository_root/target/release-dist/$artifact_stem"
rm -rf "$distribution_directory"
mkdir -p "$distribution_directory"
dmg="$distribution_directory/$artifact_stem.dmg"
hdiutil create -quiet -volname "Not a Harness" -srcfolder "$app_directory" -format UDZO -ov "$dmg"

artifact_name=$(basename "$dmg")
artifact_size=$(stat -f %z "$dmg")
artifact_sha256=$(shasum -a 256 "$dmg" | awk '{ print $1 }')
manifest="$distribution_directory/update.json"
signature="$distribution_directory/update.json.sig"
cat > "$manifest" <<EOF
{
  "schema": "nah-update-manifest-v1",
  "product": "Not a Harness",
  "channel": "stable",
  "key_id": "$key_id",
  "version": "$version",
  "build": $build,
  "minimum_macos": "13.0",
  "session_service": {
    "protocol_version": 11,
    "requires_quiescent_service": true
  },
  "artifacts": [
    {
      "platform": "macos",
      "architecture": "$architecture",
      "format": "dmg",
      "file_name": "$artifact_name",
      "url": "$base_url/$artifact_name",
      "sha256": "$artifact_sha256",
      "size": $artifact_size
    }
  ]
}
EOF

update_tool="$app_directory/Contents/MacOS/nah-update-tool"
if [ "$test_mode" = 1 ]; then
  "$update_tool" test-sign --manifest "$manifest" --signature "$signature"
  public_key=$("$update_tool" test-public-key)
else
  "$update_tool" sign --manifest "$manifest" --signature "$signature" --private-key "$NAH_UPDATE_SIGNING_KEY_FILE"
  public_key=$NAH_UPDATE_PUBLIC_KEY
fi
"$update_tool" verify --public-key "$public_key" --manifest "$manifest" --signature "$signature" --artifact "$dmg"
plutil -lint "$plist"

printf '%s\n' "$distribution_directory"
