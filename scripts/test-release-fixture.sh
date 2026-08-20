#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=$(
  cd "$repository_root"
  cargo metadata --locked --format-version 1 --no-deps |
    python3 -c 'import json,sys; print(next(package["version"] for package in json.load(sys.stdin)["packages"] if package["name"] == "hh-desktop"))'
)
case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-release-fixture.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM
key="$work/update-key"
printf '********************************' | base64 > "$key"
chmod 600 "$key"

if HH_RELEASE_TEST_MODE=1 "$repository_root/scripts/build-macos-app.sh" release \
  >"$work/release-fixture.out" 2>&1
then
  echo "release app build accepted fixture-enabled updater" >&2
  exit 1
fi
grep -F "fixture-enabled updater is forbidden in release app bundles" \
  "$work/release-fixture.out" >/dev/null

cargo build --locked --release -p hh-release-signer --bin hh-release-sign
public_key=$("$repository_root/target/release/hh-release-sign" public-key --private-key "$key")
distribution=$(
  HH_RELEASE_TEST_MODE=1 \
  HH_ALLOW_DIRTY_TEST_PACKAGE=1 \
  HH_UPDATE_SIGNING_KEY_FILE="$key" \
  HH_UPDATE_PUBLIC_KEY="$public_key" \
  HH_UPDATE_CHANNEL=edge \
  "$repository_root/scripts/package-macos-release.sh" "$version" 1 | sed -n '$p'
)

case "$(basename "$distribution")" in TESTONLY-*) ;; *) echo "fixture artifact is not TESTONLY-prefixed" >&2; exit 1 ;; esac
app="$repository_root/target/release/Harness Harlot.app"
[ "$(find "$app/Contents/MacOS" -type f -perm -111 | wc -l | tr -d ' ')" -eq 3 ]
[ -x "$app/Contents/MacOS/hh" ]
[ -x "$app/Contents/MacOS/hh-service" ]
[ -x "$app/Contents/MacOS/hh-update-tool" ]
[ -f "$app/Contents/Resources/licenses/LICENSE" ]
[ -f "$app/Contents/Resources/licenses/THIRD_PARTY_NOTICES.md" ]
[ -f "$app/Contents/Resources/licenses/ASSET_NOTICES.md" ]
[ -f "$app/Contents/Resources/licenses/third_party/licenses/Apache-2.0.txt" ]
for binary in \
  "$app/Contents/MacOS/hh" \
  "$app/Contents/MacOS/hh-service" \
  "$app/Contents/MacOS/hh-update-tool"; do
  if strings "$binary" | grep -E 'test-only-v1|updates\.example\.invalid' >/dev/null 2>&1; then
    echo "fixture trust material leaked into shipped binary $(basename "$binary")" >&2
    exit 1
  fi
done
if "$app/Contents/MacOS/hh-update-tool" install --fixture \
  >"$work/embedded-updater.out" 2>&1
then
  echo "release app embedded a fixture-enabled updater" >&2
  exit 1
fi
grep -F "fixture support is not compiled into this updater" \
  "$work/embedded-updater.out" >/dev/null


[ -f "$distribution/manifest-macos-${architecture}.update.json" ]
[ -f "$distribution/manifest-macos-${architecture}.update.json.sig" ]
grep -F '"channel": "edge"' "$distribution/manifest-macos-${architecture}.update.json" >/dev/null

echo "release fixture signs inside-out, publishes channel manifests, and bundles hh, hh-service, and hh-update-tool"
