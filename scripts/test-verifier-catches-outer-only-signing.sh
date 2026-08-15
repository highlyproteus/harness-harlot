#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app=$(
  "$repository_root/scripts/build-macos-app.sh" release
)

codesign --force --sign - "$app/Contents/MacOS/hh-service"
codesign --force --sign - "$app/Contents/MacOS/hh"
codesign --force --options runtime --sign - "$app"

if output=$("$repository_root/scripts/verify-macos-release.sh" \
  --app-only-fixture "$app" com.harnessharlot.desktop 2>&1); then
  echo "verifier accepted an outer-only hardened-runtime signature" >&2
  exit 1
fi
case "$output" in
  *hh-service*runtime*) ;;
  *)
    printf '%s\n' "$output" >&2
    echo "verifier failed without naming the mis-signed hh-service binary" >&2
    exit 1
    ;;
esac

"$repository_root/scripts/sign-macos-app.sh" - "$app"
"$repository_root/scripts/verify-macos-release.sh" \
  --app-only-fixture "$app" com.harnessharlot.desktop

echo "verifier rejects outer-only signing and accepts inside-out signing"
