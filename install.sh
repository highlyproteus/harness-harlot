#!/bin/sh
set -eu

# Release engineering must set both values before publishing this script.
EXPECTED_TEAM_ID=''
UPDATE_BASE_URL='https://github.com/highlyproteus/harness-harlot/releases/download'
BUNDLE_ID='com.harnessharlot.desktop'

usage() {
  echo "usage: $0 --version VERSION+BUILD [--prefix DIR] [--verify-only] [--print-plan]" >&2
  echo "       $0 --uninstall [--prefix DIR]" >&2
  exit 2
}

[ "$(id -u)" -ne 0 ] || { echo "refusing to install as root" >&2; exit 1; }

version=
prefix="$HOME/Applications"
verify_only=0
print_plan=0
uninstall=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) [ "$#" -ge 2 ] || usage; version=$2; shift 2 ;;
    --prefix) [ "$#" -ge 2 ] || usage; prefix=$2; shift 2 ;;
    --verify-only) verify_only=1; shift ;;
    --print-plan) print_plan=1; shift ;;
    --uninstall) uninstall=1; shift ;;
    *) usage ;;
  esac
done

case "$prefix" in
  /*) ;;
  *) echo "install prefix must be absolute" >&2; exit 1 ;;
esac
case "$prefix" in
  "$HOME" | "$HOME"/*) ;;
  *) echo "install prefix must be inside HOME" >&2; exit 1 ;;
esac
case "$prefix/" in
  */../* | */./*) echo "install prefix must be normalized" >&2; exit 1 ;;
esac
app="$prefix/Harness Harlot.app"
backup="$prefix/Harness Harlot.previous.app"
bin_directory="$HOME/.local/bin"
link="$bin_directory/hh"

validate_managed_app() {
  candidate=$1
  team_id=${2:-}
  [ ! -L "$candidate" ] || { echo "refusing symlink app path: $candidate" >&2; return 1; }
  [ -f "$candidate/Contents/Info.plist" ] || { echo "existing app has no Info.plist: $candidate" >&2; return 1; }
  [ "$(plutil -extract CFBundleIdentifier raw -o - "$candidate/Contents/Info.plist")" = "$BUNDLE_ID" ] || {
    echo "refusing to replace an app with a different bundle identifier: $candidate" >&2
    return 1
  }
  [ -x "$candidate/Contents/MacOS/hh" ] || { echo "existing app has no hh executable: $candidate" >&2; return 1; }
  if [ -n "$team_id" ]; then
    app_requirement="=anchor apple generic and certificate leaf[subject.OU] = \"$team_id\""
    codesign --verify --deep --strict -R "$app_requirement" "$candidate"
  fi
}

validate_managed_link() {
  [ ! -e "$link" ] && [ ! -L "$link" ] && return 0
  [ -L "$link" ] || { echo "refusing to overwrite non-symlink command: $link" >&2; return 1; }
  [ "$(readlink "$link")" = "$app/Contents/MacOS/hh" ] || {
    echo "refusing to overwrite a command symlink not owned by this install: $link" >&2
    return 1
  }
}

if [ "$uninstall" = 1 ]; then
  printf 'remove %s\nremove %s\nremove %s\n' "$app" "$backup" "$link"
  if [ "$print_plan" = 0 ]; then
    if [ -e "$app" ] || [ -L "$app" ]; then
      validate_managed_app "$app" "$EXPECTED_TEAM_ID"
    fi
    if [ -e "$backup" ] || [ -L "$backup" ]; then
      validate_managed_app "$backup" "$EXPECTED_TEAM_ID"
    fi
    validate_managed_link
    rm -rf "$app"
    rm -rf "$backup"
    if [ -L "$link" ]; then rm -f "$link"; fi
  fi
  echo "local history remains under ~/Library/Application Support/Harness Harlot"
  exit 0
fi

[ -n "$version" ] || usage
case "$version" in *[!0-9A-Za-z.+-]* | '' | .* | *.) echo "invalid release version" >&2; exit 2 ;; esac
case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
esac

fixture=${HH_INSTALLER_TEST_MODE:-0}
if [ "$fixture" = 1 ]; then
  : "${HH_INSTALLER_FIXTURE_DMG:?set HH_INSTALLER_FIXTURE_DMG in test mode}"
  : "${HH_INSTALLER_FIXTURE_MANIFEST:?set HH_INSTALLER_FIXTURE_MANIFEST in test mode}"
  : "${HH_INSTALLER_FIXTURE_SIGNATURE:?set HH_INSTALLER_FIXTURE_SIGNATURE in test mode}"
  : "${HH_INSTALLER_FIXTURE_TEAM_ID:?set HH_INSTALLER_FIXTURE_TEAM_ID in test mode}"
  : "${HH_INSTALLER_FIXTURE_UPDATE_HOST:?set HH_INSTALLER_FIXTURE_UPDATE_HOST in test mode}"
  : "${HH_INSTALLER_FIXTURE_KEY_ID:?set HH_INSTALLER_FIXTURE_KEY_ID in test mode}"
  : "${HH_INSTALLER_FIXTURE_PUBLIC_KEY:?set HH_INSTALLER_FIXTURE_PUBLIC_KEY in test mode}"
  expected_team_id=$HH_INSTALLER_FIXTURE_TEAM_ID
  artifact=release.dmg
  source=$HH_INSTALLER_FIXTURE_DMG
  manifest_source=$HH_INSTALLER_FIXTURE_MANIFEST
  signature_source=$HH_INSTALLER_FIXTURE_SIGNATURE
else
  [ -n "$EXPECTED_TEAM_ID" ] || {
    echo "installer is not release-configured: EXPECTED_TEAM_ID is empty" >&2
    exit 1
  }
  [ -n "$UPDATE_BASE_URL" ] || {
    echo "installer is not release-configured: UPDATE_BASE_URL is empty" >&2
    exit 1
  }
  expected_team_id=$EXPECTED_TEAM_ID
  build=${version#*+}
  bare=${version%%+*}
  artifact="Harness-Harlot-${bare}-b${build}-macos-${architecture}.dmg"
  manifest_name=${artifact%.dmg}.update.json
  source="${UPDATE_BASE_URL%/}/v${bare}/$artifact"
  manifest_source="${UPDATE_BASE_URL%/}/$manifest_name"
  signature_source="$manifest_source.sig"
  expected_host=${UPDATE_BASE_URL#https://}
  expected_host=${expected_host%%/*}
  case "$UPDATE_BASE_URL" in https://"$expected_host"/* | https://"$expected_host") ;; *) echo "invalid compiled update URL" >&2; exit 1 ;; esac
  for release_source in "$source" "$manifest_source" "$signature_source"; do
    case "$release_source" in https://"$expected_host"/*) ;; *) echo "download escaped the compiled update host" >&2; exit 1 ;; esac
  done
fi
case "$expected_team_id" in '' | *[!A-Z0-9]*) echo "invalid expected Apple Team ID" >&2; exit 1 ;; esac

printf 'download %s\nverify Apple Team ID %s before mount\nverify signed manifest %s\ninstall %s\nlink %s\n' \
  "$source" "$expected_team_id" "$manifest_source" "$app" "$link"
[ "$print_plan" = 0 ] || exit 0

umask 077
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-install.XXXXXX")
mount="$work/mount"
dmg="$work/$artifact"
manifest="$work/release.update.json"
signature="$work/release.update.json.sig"
max_dmg_bytes=2147483648
max_manifest_bytes=1048576
max_signature_bytes=4096
mkdir "$mount"
mounted=0
install_in_progress=0
old_app_moved=0
new_app_installed=0
had_link=0
link_mutated=0
cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  if [ "$mounted" = 1 ]; then hdiutil detach "$mount" -quiet >/dev/null 2>&1 || true; fi
  if [ "$status" -ne 0 ] && [ "$install_in_progress" = 1 ]; then
    if [ "$new_app_installed" = 1 ]; then rm -rf "$app"; fi
    if [ "$old_app_moved" = 1 ] && [ -d "$backup" ]; then
      mv "$backup" "$app" 2>/dev/null || true
    fi
    if [ "$link_mutated" = 1 ]; then
      rm -f "$link"
      if [ "$had_link" = 1 ]; then
        ln -s "$app/Contents/MacOS/hh" "$link" 2>/dev/null || true
      fi
    fi
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$fixture" = 1 ]; then
  cp "$source" "$dmg"
  cp "$manifest_source" "$manifest"
  cp "$signature_source" "$signature"
else
  curl --proto '=https' --tlsv1.2 --max-filesize "$max_dmg_bytes" -fsSL "$source" -o "$dmg"
  curl --proto '=https' --tlsv1.2 --max-filesize "$max_manifest_bytes" -fsSL "$manifest_source" -o "$manifest"
  curl --proto '=https' --tlsv1.2 --max-filesize "$max_signature_bytes" -fsSL "$signature_source" -o "$signature"
fi

check_download_size() {
  downloaded_file=$1
  maximum=$2
  actual=$(wc -c < "$downloaded_file" | tr -d ' ')
  [ "$actual" -le "$maximum" ] || {
    echo "download exceeds the allowed size: $downloaded_file" >&2
    exit 1
  }
}
check_download_size "$dmg" "$max_dmg_bytes"
check_download_size "$manifest" "$max_manifest_bytes"
check_download_size "$signature" "$max_signature_bytes"

requirement="=anchor apple generic and certificate leaf[subject.OU] = \"$expected_team_id\""
# These checks run against the downloaded DMG before hdiutil sees its bytes.
codesign --verify --strict -R "$requirement" "$dmg"
xcrun stapler validate "$dmg"
spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg"

hdiutil attach -readonly -nobrowse -mountpoint "$mount" "$dmg" >/dev/null
mounted=1
mounted_app="$mount/Harness Harlot.app"
update_tool="$mount/hh-update-tool"
[ -x "$update_tool" ] || { echo "DMG does not contain bootstrap hh-update-tool" >&2; exit 1; }
tool_details=$(codesign -dv --verbose=4 "$update_tool" 2>&1)
printf '%s\n' "$tool_details" | grep "TeamIdentifier=$expected_team_id" >/dev/null
printf '%s\n' "$tool_details" | grep 'flags=.*runtime' >/dev/null
codesign --verify --strict -R "$requirement" "$update_tool"
if [ "$fixture" = 1 ]; then
  "$update_tool" verify \
    --key-id "$HH_INSTALLER_FIXTURE_KEY_ID" \
    --public-key "$HH_INSTALLER_FIXTURE_PUBLIC_KEY" \
    --host "$HH_INSTALLER_FIXTURE_UPDATE_HOST" \
    --manifest "$manifest" --signature "$signature" --artifact "$dmg" --fixture
else
  "$update_tool" verify-trusted \
    --manifest "$manifest" --signature "$signature" --artifact "$dmg"
fi
[ -d "$mounted_app" ] || { echo "DMG does not contain Harness Harlot.app" >&2; exit 1; }
[ "$(plutil -extract CFBundleIdentifier raw -o - "$mounted_app/Contents/Info.plist")" = "$BUNDLE_ID" ] || {
  echo "mounted app has the wrong bundle identifier" >&2
  exit 1
}
codesign --verify --deep --strict -R "$requirement" "$mounted_app"
for binary in \
  "$mounted_app/Contents/MacOS/hh" \
  "$mounted_app/Contents/MacOS/hh-service" \
  "$mounted_app/Contents/MacOS/hh-update-tool"; do
  [ -x "$binary" ] || { echo "mounted app is missing $(basename "$binary")" >&2; exit 1; }
  details=$(codesign -dv --verbose=4 "$binary" 2>&1)
  printf '%s\n' "$details" | grep "TeamIdentifier=$expected_team_id" >/dev/null
  printf '%s\n' "$details" | grep 'flags=.*runtime' >/dev/null
  codesign --verify --strict -R "$requirement" "$binary"
done
[ "$verify_only" = 0 ] || exit 0

mkdir -p "$prefix" "$bin_directory"
staging="$prefix/.Harness Harlot.app.new.$$"
rm -rf "$staging"
ditto "$mounted_app" "$staging"
validate_managed_app "$staging" "$expected_team_id"
validate_managed_link
if [ -L "$link" ]; then
  had_link=1
fi
if [ -e "$app" ] || [ -L "$app" ]; then
  validate_managed_app "$app" "$expected_team_id"
fi
if [ -e "$backup" ] || [ -L "$backup" ]; then
  validate_managed_app "$backup" "$expected_team_id"
  rm -rf "$backup"
fi
install_in_progress=1
if [ -d "$app" ]; then
  mv "$app" "$backup"
  old_app_moved=1
fi
mv "$staging" "$app"
new_app_installed=1
link_mutated=1
rm -f "$link"
ln -s "$app/Contents/MacOS/hh" "$link"
validate_managed_app "$app" "$expected_team_id"
validate_managed_link
install_in_progress=0
echo "installed $app"
