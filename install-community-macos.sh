#!/bin/sh
set -eu

# A bootstrap launched from a development terminal must manage the normal app,
# not a disposable socket or state directory inherited from that shell.
unset HH_SOCKET HH_STATE_DIR HH_CONFIG HH_PANE_ID HH_DEVELOPMENT_BUILD

REPOSITORY='highlyproteus/harness-harlot'
BUNDLE_ID='com.harnessharlot.desktop'

usage() {
  cat <<'EOF'
Harness Harlot installer for macOS

Usage:
  curl -fsSL https://github.com/highlyproteus/harness-harlot/releases/latest/download/install-community-macos.sh | sh

Options:
  --tag vVERSION  Install a specific release
  --verify-only   Verify the release without installing it
  --plan          Show the selected install location without changing anything
  --verbose       Show verification command output
  -h, --help      Show this help
EOF
}

[ "$(id -u)" -ne 0 ] || { echo "refusing to install as root" >&2; exit 1; }

tag=
verify_only=0
plan_only=0
verbose=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --acknowledge-unnotarized) shift ;; # Accepted for compatibility with v0.1.6 instructions.
    --tag) [ "$#" -ge 2 ] || usage; tag=$2; shift 2 ;;
    --verify-only) verify_only=1; shift ;;
    --plan) plan_only=1; shift ;;
    --verbose) verbose=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

home=${HOME:?HOME is not set}
user_prefix="$home/Applications"
system_prefix=/Applications
if [ "${HH_RELEASE_TEST_MODE:-0}" = 1 ] && [ -n "${HH_INSTALLER_APPLICATIONS_DIR:-}" ]; then
  system_prefix=$HH_INSTALLER_APPLICATIONS_DIR
fi
if [ -d "$system_prefix" ] && [ -w "$system_prefix" ]; then
  prefix=$system_prefix
  alternate_prefix=$user_prefix
else
  prefix=$user_prefix
  alternate_prefix=$system_prefix
fi
app="$prefix/Harness Harlot.app"
backup="$prefix/.Harness Harlot.previous.app"
legacy_backup="$prefix/Harness Harlot.previous.app"
alternate_app="$alternate_prefix/Harness Harlot.app"
bin_directory="$home/.local/bin"
link="$bin_directory/hh"

if [ "$plan_only" -eq 1 ]; then
  echo "Install location: $app"
  if [ -d "$alternate_app" ]; then
    echo "Another installation exists: $alternate_app"
  fi
  exit 0
fi

command -v gh >/dev/null 2>&1 || {
  echo "GitHub CLI (gh) is required to verify the downloaded release." >&2
  echo "Install it with: brew install gh" >&2
  exit 1
}
case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
esac
if [ -z "$tag" ]; then
  tag=$(gh release view --repo "$REPOSITORY" --json tagName --jq .tagName)
fi
case "$tag" in
  v[0-9A-Za-z.-]*) ;;
  *) echo "invalid release tag: $tag" >&2; exit 2 ;;
esac
case "$tag" in *..* | *. | *[!0-9A-Za-z.v-]*) echo "invalid release tag: $tag" >&2; exit 2 ;; esac
version=${tag#v}

work=$(mktemp -d "${TMPDIR:-/tmp}/hh-community-install.XXXXXX")
mount="$work/mount"
staging="$prefix/.Harness Harlot.app.community.$$"
log="$work/install.log"
mounted=0
install_in_progress=0
old_app_moved=0
new_app_installed=0
had_link=0
previous_link_target=
link_mutated=0

cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  if [ "$mounted" -eq 1 ]; then
    hdiutil detach "$mount" -quiet >/dev/null 2>&1 || true
  fi
  rm -rf "$staging" "$work"
  if [ "$status" -ne 0 ] && [ "$install_in_progress" -eq 1 ]; then
    if [ "$new_app_installed" -eq 1 ]; then
      rm -rf "$app"
    fi
    if [ "$old_app_moved" -eq 1 ]; then
      if mv "$backup" "$app" 2>/dev/null; then
        echo "installation failed; restored the previous community app" >&2
      else
        echo "installation failed and rollback could not restore $app" >&2
      fi
    elif [ "$new_app_installed" -eq 1 ]; then
      echo "installation failed; removed the partial community app" >&2
    fi
    if [ "$link_mutated" -eq 1 ]; then
      rm -f "$link"
      if [ "$had_link" -eq 1 ]; then
        ln -s "$previous_link_target" "$link" 2>/dev/null || {
          echo "installation rollback could not restore $link" >&2
        }
      fi
    fi
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$mount"

if [ -t 1 ]; then
  green='\033[0;32m'
  yellow='\033[0;33m'
  red='\033[0;31m'
  bold='\033[1m'
  reset='\033[0m'
else
  green=
  yellow=
  red=
  bold=
  reset=
fi

info() { printf '%b→%b %s\n' "$bold" "$reset" "$1"; }
success() { printf '%b✓%b %s\n' "$green" "$reset" "$1"; }
warn() { printf '%b⚠%b %s\n' "$yellow" "$reset" "$1"; }
fail() { printf '%b✗%b %s\n' "$red" "$reset" "$1" >&2; }

run_quiet() {
  label=$1
  shift
  if [ "$verbose" -eq 1 ]; then
    info "$label"
    "$@"
    success "$label"
  elif "$@" >>"$log" 2>&1; then
    success "$label"
  else
    fail "$label"
    echo "Verification details:" >&2
    tail -50 "$log" >&2
    exit 1
  fi
}

validate_community_app() {
  candidate=$1
  [ ! -L "$candidate" ] || { echo "refusing symlink app path: $candidate" >&2; return 1; }
  plist="$candidate/Contents/Info.plist"
  [ -f "$plist" ] || { echo "app has no Info.plist: $candidate" >&2; return 1; }
  [ "$(plutil -extract CFBundleIdentifier raw -o - "$plist")" = "$BUNDLE_ID" ] || {
    echo "refusing an app with a different bundle identifier: $candidate" >&2
    return 1
  }
  [ "$(plutil -extract CFBundleExecutable raw -o - "$plist")" = hh ] || {
    echo "refusing an app with a different bundle executable: $candidate" >&2
    return 1
  }
  for binary in hh hh-service hh-update-tool; do
    path="$candidate/Contents/MacOS/$binary"
    [ -x "$path" ] || { echo "app is missing $binary" >&2; return 1; }
    [ "$(lipo -archs "$path")" = "$architecture" ] || {
      echo "app contains the wrong $binary architecture" >&2
      return 1
    }
  done
  executable_count=$(find "$candidate/Contents/MacOS" -type f -perm -111 -print | wc -l | tr -d ' ')
  [ "$executable_count" -eq 3 ] || { echo "app contains unexpected primary executables" >&2; return 1; }
  frameworks="$candidate/Contents/Frameworks"
  cef="$frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework"
  [ -x "$cef" ] || { echo "community app has no CEF runtime" >&2; return 1; }
  [ "$(lipo -archs "$cef")" = "$architecture" ] || {
    echo "community app contains the wrong CEF architecture" >&2
    return 1
  }
  [ -f "$candidate/Contents/Resources/licenses/CEF-CREDITS.html" ] || {
    echo "community app has no CEF third-party credits" >&2
    return 1
  }
  for helper_name in \
    "hh Helper" "hh Helper (GPU)" "hh Helper (Renderer)" \
    "hh Helper (Plugin)" "hh Helper (Alerts)"; do
    helper="$frameworks/$helper_name.app/Contents/MacOS/$helper_name"
    [ -x "$helper" ] || { echo "community app is missing $helper_name" >&2; return 1; }
    [ "$(lipo -archs "$helper")" = "$architecture" ] || {
      echo "community app contains the wrong $helper_name architecture" >&2
      return 1
    }
  done
  codesign --verify --deep --strict --verbose=2 "$candidate" || {
    echo "app bundle signature verification failed: $candidate" >&2
    return 1
  }
  details=$(codesign -dv --verbose=4 "$candidate" 2>&1) || return 1
  printf '%s\n' "$details" | grep 'Signature=adhoc' >/dev/null || {
    echo "refusing a non-community app at $candidate" >&2
    return 1
  }
}

preflight_existing_install() {
  if pgrep -x hh >/dev/null 2>&1; then
    fail "Harness Harlot is running"
    echo "Quit the app and close every Harness Harlot terminal, then run this installer again." >&2
    exit 1
  fi
  [ -d "$app" ] || return 0
  run_quiet "Validate current installation" validate_community_app "$app"
  run_quiet "Check active Harness Harlot terminals" \
    "$app/Contents/MacOS/hh-update-tool" prepare-community-install
}

printf '\n%bHarness Harlot%b\n\n' "$bold" "$reset"
info "Installing version $version for macOS $architecture"
warn "Community builds are signed and verified, but not Apple-notarized."
if [ -d "$alternate_app" ]; then
  warn "Another installation exists at $alternate_app"
  info "This installation will use $app"
fi
preflight_existing_install

manifest="$work/manifest-macos-community-${architecture}.update.json"
signature="$manifest.sig"
artifact_pattern="Harness-Harlot-${version}-b*-macos-${architecture}-community.dmg"
run_quiet "Download Harness Harlot $version" gh release download "$tag" --repo "$REPOSITORY" --dir "$work" \
  --pattern "$(basename "$manifest")" \
  --pattern "$(basename "$signature")" \
  --pattern "$artifact_pattern"
# The unquoted pattern is intentional: exactly one downloaded artifact must match.
# shellcheck disable=SC2086
set -- "$work"/$artifact_pattern
if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
  echo "release must contain exactly one architecture-matched community DMG" >&2
  exit 1
fi
dmg=$1
if [ ! -f "$manifest" ] || [ ! -f "$signature" ]; then
  echo "release is missing the community update manifest or signature" >&2
  exit 1
fi

# GitHub's signed workflow provenance is the bootstrap trust root. Only after
# all downloaded inputs pass this check do we inspect or mount the DMG.
for subject in "$dmg" "$manifest" "$signature"; do
  run_quiet "Verify release provenance: $(basename "$subject")" \
    gh attestation verify "$subject" --repo "$REPOSITORY"
done
run_quiet "Verify disk image signature" codesign --verify --strict --verbose=2 "$dmg"
hdiutil attach -readonly -nobrowse -mountpoint "$mount" "$dmg" >/dev/null
mounted=1
mounted_app="$mount/Harness Harlot.app"
update_tool="$mount/hh-update-tool"
[ -x "$update_tool" ] || { echo "community DMG has no bootstrap hh-update-tool" >&2; exit 1; }
run_quiet "Verify signed update manifest" \
  "$update_tool" verify-trusted --manifest "$manifest" --signature "$signature" --artifact "$dmg"

validate_managed_link() {
  [ ! -e "$link" ] && [ ! -L "$link" ] && return 0
  [ -L "$link" ] || { echo "refusing to overwrite non-symlink command: $link" >&2; return 1; }
  link_target=$(readlink "$link")
  case "$link_target" in
    "$app/Contents/MacOS/hh"|"$alternate_app/Contents/MacOS/hh") ;;
    *)
      echo "refusing to overwrite a command symlink not owned by a recognized Harness Harlot install: $link" >&2
      return 1
      ;;
  esac
}

run_quiet "Validate Harness Harlot app" validate_community_app "$mounted_app"
manifest_artifact=$(plutil -extract artifacts.0.file_name raw -o - "$manifest")
[ "$manifest_artifact" = "$(basename "$dmg")" ] || {
  echo "signed manifest does not name the downloaded community DMG" >&2
  exit 1
}
if [ "$verify_only" -eq 1 ]; then
  success "Verified Harness Harlot $version for macOS $architecture"
  exit 0
fi
mkdir -p "$prefix" "$bin_directory"
rm -rf "$staging"
ditto "$mounted_app" "$staging"
run_quiet "Stage Harness Harlot in $prefix" validate_community_app "$staging"
validate_managed_link
if [ -L "$link" ]; then
  had_link=1
  previous_link_target=$(readlink "$link")
fi
if [ -e "$app" ] || [ -L "$app" ]; then
  run_quiet "Validate current installation" validate_community_app "$app"
  run_quiet "Stop the Harness Harlot session service" \
    "$app/Contents/MacOS/hh-update-tool" prepare-community-install
else
  run_quiet "Prepare Harness Harlot services" "$update_tool" prepare-community-install
fi
if [ -e "$backup" ] || [ -L "$backup" ]; then
  validate_community_app "$backup"
  rm -rf "$backup"
fi
if [ -e "$legacy_backup" ] || [ -L "$legacy_backup" ]; then
  validate_community_app "$legacy_backup"
  rm -rf "$legacy_backup"
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
run_quiet "Verify installed app" validate_community_app "$app"
validate_managed_link
run_quiet "Launch Harness Harlot" open "$app"
install_in_progress=0

printf '\n'
success "Installed Harness Harlot $version"
echo "  App:     $app"
echo "  Command: $link"
if [ -d "$alternate_app" ]; then
  warn "The older copy at $alternate_app was left untouched. Remove it after confirming this install works."
fi
echo
echo "Future updates: hh update"
echo "Check first:    hh update --check"
echo "If macOS blocks first launch, use System Settings > Privacy & Security > Open Anyway."
