#!/bin/sh
set -eu

REPOSITORY='highlyproteus/harness-harlot'
BUNDLE_ID='com.harnessharlot.desktop'

usage() {
  echo "usage: $0 --acknowledge-unnotarized [--tag vVERSION] [--verify-only]" >&2
  exit 2
}

[ "$(id -u)" -ne 0 ] || { echo "refusing to install as root" >&2; exit 1; }

acknowledged=0
tag=
verify_only=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --acknowledge-unnotarized) acknowledged=1; shift ;;
    --tag) [ "$#" -ge 2 ] || usage; tag=$2; shift 2 ;;
    --verify-only) verify_only=1; shift ;;
    *) usage ;;
  esac
done

[ "$acknowledged" -eq 1 ] || {
  echo "This community build is ad-hoc signed, not Apple-notarized, and may require Privacy & Security > Open Anyway." >&2
  echo "Re-run with --acknowledge-unnotarized only if you accept that trust model." >&2
  exit 2
}
command -v gh >/dev/null 2>&1 || {
  echo "GitHub CLI (gh) is required to verify build provenance before mounting the DMG" >&2
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

home=${HOME:?HOME is not set}
prefix="$home/Applications"
app="$prefix/Harness Harlot.app"
backup="$prefix/Harness Harlot.previous.app"
bin_directory="$home/.local/bin"
link="$bin_directory/hh"
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-community-install.XXXXXX")
mount="$work/mount"
staging="$prefix/.Harness Harlot.app.community.$$"
mounted=0
install_in_progress=0
old_app_moved=0
new_app_installed=0
had_link=0
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
        ln -s "$app/Contents/MacOS/hh" "$link" 2>/dev/null || {
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

manifest="$work/manifest-macos-community-${architecture}.update.json"
signature="$manifest.sig"
artifact_pattern="Harness-Harlot-${version}-b*-macos-${architecture}-community.dmg"
gh release download "$tag" --repo "$REPOSITORY" --dir "$work" \
  --pattern "$(basename "$manifest")" \
  --pattern "$(basename "$signature")" \
  --pattern "$artifact_pattern"
set -- "$work"/$artifact_pattern
[ "$#" -eq 1 ] && [ -f "$1" ] || {
  echo "release must contain exactly one architecture-matched community DMG" >&2
  exit 1
}
dmg=$1
[ -f "$manifest" ] && [ -f "$signature" ] || {
  echo "release is missing the community update manifest or signature" >&2
  exit 1
}

# GitHub's signed workflow provenance is the bootstrap trust root. Only after
# all downloaded inputs pass this check do we inspect or mount the DMG.
for subject in "$dmg" "$manifest" "$signature"; do
  gh attestation verify "$subject" --repo "$REPOSITORY"
done
codesign --verify --strict --verbose=2 "$dmg"
hdiutil attach -readonly -nobrowse -mountpoint "$mount" "$dmg" >/dev/null
mounted=1
mounted_app="$mount/Harness Harlot.app"
update_tool="$mount/hh-update-tool"
[ -x "$update_tool" ] || { echo "community DMG has no bootstrap hh-update-tool" >&2; exit 1; }
"$update_tool" verify-trusted --manifest "$manifest" --signature "$signature" --artifact "$dmg"

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
  codesign --verify --deep --strict --verbose=2 "$candidate"
  details=$(codesign -dv --verbose=4 "$candidate" 2>&1)
  printf '%s\n' "$details" | grep 'Signature=adhoc' >/dev/null || {
    echo "refusing a non-community app at $candidate" >&2
    return 1
  }
}

validate_managed_link() {
  [ ! -e "$link" ] && [ ! -L "$link" ] && return 0
  [ -L "$link" ] || { echo "refusing to overwrite non-symlink command: $link" >&2; return 1; }
  [ "$(readlink "$link")" = "$app/Contents/MacOS/hh" ] || {
    echo "refusing to overwrite a command symlink not owned by this install: $link" >&2
    return 1
  }
}

validate_community_app "$mounted_app"
manifest_artifact=$(plutil -extract artifacts.0.file_name raw -o - "$manifest")
[ "$manifest_artifact" = "$(basename "$dmg")" ] || {
  echo "signed manifest does not name the downloaded community DMG" >&2
  exit 1
}
if [ "$verify_only" -eq 1 ]; then
  echo "verified unnotarized Harness Harlot community release $tag for $architecture"
  exit 0
fi
if pgrep -x hh >/dev/null 2>&1; then
  echo "quit Harness Harlot before installing; end every terminal session first" >&2
  exit 1
fi

mkdir -p "$prefix" "$bin_directory"
rm -rf "$staging"
ditto "$mounted_app" "$staging"
validate_community_app "$staging"
validate_managed_link
if [ -L "$link" ]; then
  had_link=1
fi
if [ -e "$app" ] || [ -L "$app" ]; then
  validate_community_app "$app"
  "$app/Contents/MacOS/hh-update-tool" prepare-community-install
else
  "$update_tool" prepare-community-install
fi
if [ -e "$backup" ] || [ -L "$backup" ]; then
  validate_community_app "$backup"
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
validate_community_app "$app"
validate_managed_link
open "$app"
install_in_progress=0

echo "installed unnotarized community build at $app"
echo "Automatic replacement is disabled. Future releases appear as manual update notifications."
echo "If macOS blocks first launch, use System Settings > Privacy & Security > Open Anyway."
