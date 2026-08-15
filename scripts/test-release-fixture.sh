#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-release-fixture.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM
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
  "$repository_root/scripts/package-macos-release.sh" 0.1.0 1
)

case "$(basename "$distribution")" in TESTONLY-*) ;; *) echo "fixture artifact is not TESTONLY-prefixed" >&2; exit 1 ;; esac
app="$repository_root/target/release/Harness Harlot.app"
[ "$(find "$app/Contents/MacOS" -type f -perm -111 | wc -l | tr -d ' ')" -eq 2 ]
[ -x "$app/Contents/MacOS/hh" ]
[ -x "$app/Contents/MacOS/hh-service" ]
[ ! -e "$app/Contents/MacOS/hh-update-tool" ]
[ -f "$app/Contents/Resources/licenses/LICENSE" ]
[ -f "$app/Contents/Resources/licenses/THIRD_PARTY_NOTICES.md" ]
[ -f "$app/Contents/Resources/licenses/ASSET_NOTICES.md" ]
[ -f "$app/Contents/Resources/licenses/third_party/licenses/Apache-2.0.txt" ]
for binary in "$app/Contents/MacOS/hh" "$app/Contents/MacOS/hh-service"; do
  if strings "$binary" | grep -E 'test-only-v1|updates\.example\.invalid' >/dev/null 2>&1; then
    echo "fixture trust material leaked into shipped binary $(basename "$binary")" >&2
    exit 1
  fi
done

echo "release fixture signs inside-out, verifies before mount, and bundles only hh plus hh-service"
