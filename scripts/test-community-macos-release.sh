#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=$(
  cd "$repository_root"
  cargo metadata --locked --format-version 1 --no-deps |
    plutil -extract packages.0.version raw -o - -
)
case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-community-release-fixture.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

sh -n "$repository_root/install-community-macos.sh"
if "$repository_root/install-community-macos.sh" >"$work/no-ack.out" 2>&1; then
  echo "community installer ran without explicit unnotarized acknowledgement" >&2
  exit 1
fi
grep -F -- "--acknowledge-unnotarized" "$work/no-ack.out" >/dev/null

key="$work/update-key"
printf '********************************' | base64 > "$key"
chmod 600 "$key"
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
public_key=$("$repository_root/target/release/hh-release-sign" public-key --private-key "$key")
distribution=$(
  HH_RELEASE_TEST_MODE=1 \
  HH_ALLOW_DIRTY_TEST_PACKAGE=1 \
  HH_UPDATE_SIGNING_KEY_FILE="$key" \
  HH_UPDATE_PUBLIC_KEY="$public_key" \
  "$repository_root/scripts/package-macos-release.sh" "$version" 1 --community | sed -n '$p'
)

case "$(basename "$distribution")" in
  TESTONLY-Harness-Harlot-"$version"-b1-macos-"$architecture"-community) ;;
  *) echo "community fixture artifact name is not isolated" >&2; exit 1 ;;
esac
manifest="$distribution/manifest-macos-community-${architecture}.update.json"
[ -f "$manifest" ]
[ -f "$manifest.sig" ]
[ ! -e "$distribution/manifest-macos-${architecture}.update.json" ]
artifact=$(plutil -extract artifacts.0.file_name raw -o - "$manifest")
case "$artifact" in
  *-macos-"$architecture"-community.dmg) ;;
  *) echo "community manifest does not select a community-only DMG" >&2; exit 1 ;;
esac

app="$repository_root/target/release/Harness Harlot.app"
case "$architecture" in
  arm64) other_architecture=x86_64 ;;
  x86_64) other_architecture=arm64 ;;
esac
if "$app/Contents/MacOS/hh-update-tool" install \
  --architecture "$other_architecture" --current-version 0.0.0 \
  >"$work/automatic-install.out" 2>&1; then
  echo "community updater accepted a non-native architecture" >&2
  exit 1
fi
grep -F "production update architecture must match" "$work/automatic-install.out" >/dev/null
HH_SOCKET="$work/no-service.sock" \
  "$app/Contents/MacOS/hh-update-tool" prepare-community-install

echo "community fixture isolates its feed, requires first-install trust, and enables automatic replacement"
