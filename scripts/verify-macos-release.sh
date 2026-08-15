#!/bin/sh
set -eu

usage() {
  echo "usage: $0 TEAM_ID BUNDLE_ID UPDATE_JSON UPDATE_SIGNATURE" >&2
  echo "       $0 --fixture BUNDLE_ID UPDATE_HOST UPDATE_KEY_ID UPDATE_PUBLIC_KEY UPDATE_JSON UPDATE_SIGNATURE" >&2
  echo "       $0 --app-only-fixture APP_DIR BUNDLE_ID" >&2
  exit 2
}

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
entitlements="$repository_root/packaging/macos/hh.entitlements"
fixture=0
app_only=0
expected_team_id=

if [ "${1:-}" = "--app-only-fixture" ]; then
  [ "$#" -eq 3 ] || usage
  fixture=1
  app_only=1
  app=$2
  expected_bundle_id=$3
elif [ "${1:-}" = "--fixture" ]; then
  [ "$#" -eq 7 ] || usage
  fixture=1
  expected_bundle_id=$2
  expected_update_host=$3
  expected_key_id=$4
  public_key=$5
  manifest=$6
  signature=$7
else
  [ "$#" -eq 4 ] || usage
  expected_team_id=$1
  expected_bundle_id=$2
  manifest=$3
  signature=$4
  case "$expected_team_id" in
    '' | *[!A-Z0-9]*) echo "TEAM_ID must contain only upper-case letters and digits" >&2; exit 2 ;;
  esac
fi

verify_code() {
  code=$1
  label=$2
  details=$(mktemp "${TMPDIR:-/tmp}/hh-codesign-details.XXXXXX")
  codesign -dv --verbose=4 "$code" >"$details" 2>&1 || {
    cat "$details" >&2
    rm -f "$details"
    echo "$label is not signed" >&2
    return 1
  }
  if ! grep 'flags=.*runtime' "$details" >/dev/null 2>&1; then
    cat "$details" >&2
    rm -f "$details"
    echo "$label does not have hardened runtime enabled" >&2
    return 1
  fi
  if [ "$fixture" = 1 ]; then
    if ! grep 'Signature=adhoc' "$details" >/dev/null 2>&1; then
      cat "$details" >&2
      rm -f "$details"
      echo "$label fixture is not ad-hoc signed" >&2
      return 1
    fi
  else
    if ! grep "TeamIdentifier=$expected_team_id" "$details" >/dev/null 2>&1; then
      cat "$details" >&2
      rm -f "$details"
      echo "$label has the wrong Apple Team ID" >&2
      return 1
    fi
    if ! grep '^Timestamp=' "$details" >/dev/null 2>&1; then
      cat "$details" >&2
      rm -f "$details"
      echo "$label has no secure signing timestamp" >&2
      return 1
    fi
    requirement="=anchor apple generic and certificate leaf[subject.OU] = \"$expected_team_id\""
    codesign --verify --strict -R "$requirement" "$code" || {
      rm -f "$details"
      echo "$label does not satisfy the expected Developer ID requirement" >&2
      return 1
    }
  fi
  rm -f "$details"

  entitlement_output=$(mktemp "${TMPDIR:-/tmp}/hh-entitlements.XXXXXX")
  codesign -d --entitlements :- "$code" >"$entitlement_output" 2>/dev/null || true
  if grep '<key>' "$entitlement_output" >/dev/null 2>&1; then
    cat "$entitlement_output" >&2
    rm -f "$entitlement_output"
    echo "$label has unapproved entitlements; expected empty $entitlements" >&2
    return 1
  fi
  rm -f "$entitlement_output"
}

verify_app() {
  app_to_verify=$1
  plist="$app_to_verify/Contents/Info.plist"
  [ -f "$plist" ] || { echo "mounted app has no Info.plist" >&2; return 1; }
  [ -f "$app_to_verify/Contents/Resources/Harness-Harlot.icns" ] || {
    echo "mounted app has no application icon" >&2
    return 1
  }
  [ -x "$app_to_verify/Contents/MacOS/hh" ] || { echo "mounted app has no hh executable" >&2; return 1; }
  [ -x "$app_to_verify/Contents/MacOS/hh-service" ] || { echo "mounted app has no hh-service executable" >&2; return 1; }
  executable_count=$(find "$app_to_verify/Contents/MacOS" -type f -perm -111 -print | wc -l | tr -d ' ')
  if [ "$executable_count" -ne 2 ]; then
    echo "Contents/MacOS must contain exactly hh and hh-service" >&2
    find "$app_to_verify/Contents/MacOS" -type f -perm -111 -print >&2
    return 1
  fi
  codesign --verify --deep --strict --verbose=2 "$app_to_verify"
  verify_code "$app_to_verify/Contents/MacOS/hh-service" hh-service
  verify_code "$app_to_verify/Contents/MacOS/hh" hh
  verify_code "$app_to_verify" "Harness Harlot.app"
  actual_bundle_id=$(plutil -extract CFBundleIdentifier raw -o - "$plist")
  if [ "$actual_bundle_id" != "$expected_bundle_id" ]; then
    echo "bundle ID mismatch: expected $expected_bundle_id, got $actual_bundle_id" >&2
    return 1
  fi
}

if [ "$app_only" = 1 ]; then
  verify_app "$app"
  echo "verified fixture app signing layout"
  exit 0
fi

if [ "$fixture" = 1 ]; then
  case "$expected_update_host" in
    '' | */* | *:* | *@*) echo "UPDATE_HOST must be a bare hostname" >&2; exit 2 ;;
  esac
  case "$expected_key_id" in
    '' | *[!A-Za-z0-9._-]*) echo "UPDATE_KEY_ID contains unsupported characters" >&2; exit 2 ;;
  esac
fi
[ -f "$manifest" ] || { echo "update manifest not found: $manifest" >&2; exit 1; }
[ -f "$signature" ] || { echo "update signature not found: $signature" >&2; exit 1; }
case "$manifest" in
  *.update.json) dmg=${manifest%.update.json}.dmg ;;
  *) echo "update manifest must use the architecture-qualified *.update.json name" >&2; exit 1 ;;
esac
[ -f "$dmg" ] || { echo "release DMG matching the manifest name is missing: $dmg" >&2; exit 1; }

# Establish the Apple trust root before mounting any downloaded bytes.
codesign --verify --verbose=2 "$dmg"
if [ "$fixture" = 0 ]; then
  requirement="=anchor apple generic and certificate leaf[subject.OU] = \"$expected_team_id\""
  codesign --verify -R "$requirement" "$dmg"
  xcrun stapler validate "$dmg"
  spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg"
fi

mount_directory=$(mktemp -d "${TMPDIR:-/tmp}/hh-release-mount.XXXXXX")
cleanup() {
  hdiutil detach "$mount_directory" -quiet 2>/dev/null || true
  rmdir "$mount_directory" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM
hdiutil attach -readonly -nobrowse -mountpoint "$mount_directory" "$dmg" >/dev/null
app="$mount_directory/Harness Harlot.app"
tool="$mount_directory/hh-update-tool"
[ -x "$tool" ] || { echo "mounted DMG has no verifier-only hh-update-tool" >&2; exit 1; }
verify_code "$tool" hh-update-tool

# The verifier is trusted through the already-verified DMG. It authenticates
# the manifest before any attacker-controlled artifact name is inspected.
if [ "$fixture" = 1 ]; then
  "$tool" verify --key-id "$expected_key_id" --public-key "$public_key" \
    --host "$expected_update_host" --manifest "$manifest" \
    --signature "$signature" --fixture
else
  "$tool" verify-trusted --manifest "$manifest" --signature "$signature"
fi

artifact=$(plutil -extract artifacts.0.file_name raw -o - "$manifest")
if [ "$artifact" != "$(basename "$dmg")" ]; then
  echo "signed manifest artifact does not match its architecture-qualified DMG" >&2
  exit 1
fi
if [ "$fixture" = 1 ]; then
  "$tool" verify --key-id "$expected_key_id" --public-key "$public_key" \
    --host "$expected_update_host" --manifest "$manifest" \
    --signature "$signature" --artifact "$dmg" --fixture
else
  "$tool" verify-trusted --manifest "$manifest" --signature "$signature" --artifact "$dmg"
fi
verify_app "$app"

manifest_version=$(plutil -extract version raw -o - "$manifest")
manifest_build=$(plutil -extract build raw -o - "$manifest")
plist="$app/Contents/Info.plist"
[ "$(plutil -extract CFBundleShortVersionString raw -o - "$plist")" = "$manifest_version" ] || {
  echo "bundle version does not match signed manifest" >&2
  exit 1
}
[ "$(plutil -extract CFBundleVersion raw -o - "$plist")" = "$manifest_build" ] || {
  echo "bundle build does not match signed manifest" >&2
  exit 1
}
case "$artifact" in
  *-macos-arm64.dmg) expected_architecture=arm64 ;;
  *-macos-x86_64.dmg) expected_architecture=x86_64 ;;
  *) echo "artifact name does not encode a supported architecture" >&2; exit 1 ;;
esac
for binary in "$app/Contents/MacOS/hh" "$app/Contents/MacOS/hh-service"; do
  architectures=$(lipo -archs "$binary")
  if [ "$architectures" != "$expected_architecture" ]; then
    echo "$binary architecture mismatch: expected $expected_architecture, got $architectures" >&2
    exit 1
  fi
done
if [ "$fixture" = 0 ]; then
  spctl --assess --type execute --verbose=4 "$app"
fi

echo "verified mounted Harness Harlot release artifact"
