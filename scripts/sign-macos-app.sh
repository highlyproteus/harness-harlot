#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 IDENTITY APP_DIR" >&2
  exit 2
fi

identity=$1
app_directory=$2
repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
entitlements="$repository_root/packaging/macos/hh.entitlements"

if [ ! -d "$app_directory" ]; then
  echo "application bundle not found: $app_directory" >&2
  exit 1
fi
if [ ! -x "$app_directory/Contents/MacOS/hh" ] || [ ! -x "$app_directory/Contents/MacOS/hh-service" ]; then
  echo "bundle must contain executable hh and hh-service binaries" >&2
  exit 1
fi
if find "$app_directory/Contents/MacOS" -type f ! -name hh ! -name hh-service -print | grep . >/dev/null 2>&1; then
  echo "unexpected executable or helper in Contents/MacOS" >&2
  exit 1
fi
if plutil -p "$entitlements" | grep '=>' >/dev/null 2>&1; then
  echo "non-empty entitlements require runtime evidence and verifier updates" >&2
  exit 1
fi

sign_binary() {
  binary=$1
  identifier=$2
  if [ "$identity" = "-" ]; then
    codesign --force --options runtime --identifier "$identifier" --sign - "$binary"
  else
    codesign --force --options runtime --timestamp --identifier "$identifier" --sign "$identity" "$binary"
  fi
}

# Sign nested code first. Never use --deep for signing: it can preserve or
# conceal incorrectly signed nested binaries and does not propagate policy.
sign_binary "$app_directory/Contents/MacOS/hh-service" com.harnessharlot.desktop.service
sign_binary "$app_directory/Contents/MacOS/hh" com.harnessharlot.desktop
if [ "$identity" = "-" ]; then
  codesign --force --options runtime --sign - "$app_directory"
else
  codesign --force --options runtime --timestamp --sign "$identity" "$app_directory"
fi
