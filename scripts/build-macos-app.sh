#!/bin/sh
set -eu

usage() {
  echo "usage: $0 [debug|release] [--browser]" >&2
  exit 2
}

profile=debug
browser_enabled=0
if [ "$#" -gt 0 ]; then
  case "$1" in
    debug | release)
      profile=$1
      shift
      ;;
    --browser)
      browser_enabled=1
      shift
      ;;
    *) usage ;;
  esac
fi
if [ "$#" -gt 0 ]; then
  if [ "$1" = "--browser" ] && [ "$browser_enabled" -eq 0 ]; then
    browser_enabled=1
    shift
  else
    usage
  fi
fi
if [ "$#" -ne 0 ]; then
  usage
fi

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

cef_path=
cef_framework_source=
if [ "$browser_enabled" -eq 1 ]; then
  : "${CEF_PATH:?set CEF_PATH to an unpacked CEF distribution before using --browser}"
  cef_path=$(CDPATH= cd -- "$CEF_PATH" && pwd)
  cef_framework_source="$cef_path/Release/Chromium Embedded Framework.framework"
  if [ ! -d "$cef_framework_source" ]; then
    cef_framework_source="$cef_path/Chromium Embedded Framework.framework"
  fi
  if [ ! -d "$cef_framework_source" ]; then
    echo "CEF framework not found under $cef_path/Release or $cef_path" >&2
    exit 1
  fi
  if [ ! -f "$cef_path/CREDITS.html" ]; then
    echo "CEF third-party notices not found: $cef_path/CREDITS.html" >&2
    exit 1
  fi
  export CEF_PATH="$cef_path"
fi

cargo_release=
if [ "$profile" = release ]; then
  cargo_release=--release
fi
if [ "$browser_enabled" -eq 1 ]; then
  # shellcheck disable=SC2086
  cargo build --locked $cargo_release -p hh-desktop --bin hh --features browser
  # shellcheck disable=SC2086
  cargo build --locked $cargo_release -p hh-cef-view --bin hh-cef-helper --features cef
else
  # shellcheck disable=SC2086
  cargo build --locked $cargo_release -p hh-desktop --bin hh
fi
# shellcheck disable=SC2086
cargo build --locked $cargo_release -p hh-session-service --bin hh-service

app_name="Harness Harlot"
app_directory="$repository_root/target/$profile/$app_name.app"
contents_directory="$app_directory/Contents"
notices_directory="$contents_directory/Resources/licenses"
rm -rf "$app_directory"
mkdir -p "$contents_directory/MacOS" "$notices_directory/third_party/licenses"

cp "$repository_root/packaging/macos/Info.plist" "$contents_directory/Info.plist"
cp "$repository_root/packaging/macos/Harness-Harlot.icns" \
  "$contents_directory/Resources/Harness-Harlot.icns"
cp "$repository_root/crates/desktop/assets/harnessharlot-banner.png" \
  "$contents_directory/Resources/harnessharlot-banner.png"
cp "$repository_root/LICENSE" "$notices_directory/LICENSE"
cp "$repository_root/THIRD_PARTY_NOTICES.md" "$notices_directory/THIRD_PARTY_NOTICES.md"
cp "$repository_root/ASSET_NOTICES.md" "$notices_directory/ASSET_NOTICES.md"
cp "$repository_root"/third_party/licenses/* "$notices_directory/third_party/licenses/"
cp "$repository_root/target/$profile/hh" "$contents_directory/MacOS/hh"
cp "$repository_root/target/$profile/hh-service" "$contents_directory/MacOS/hh-service"
chmod 755 "$contents_directory/MacOS/hh" "$contents_directory/MacOS/hh-service"

if [ "$browser_enabled" -eq 1 ]; then
  frameworks_directory="$contents_directory/Frameworks"
  framework="$frameworks_directory/Chromium Embedded Framework.framework"
  mkdir -p "$frameworks_directory"
  ditto "$cef_framework_source" "$framework"
  cp "$cef_path/CREDITS.html" "$notices_directory/CEF-CREDITS.html"
  cp "$repository_root/packaging/macos/CEF-LICENSE.txt" \
    "$notices_directory/CEF-LICENSE.txt"

  create_cef_helper() {
    role=$1
    identifier_suffix=$2
    if [ -n "$role" ]; then
      helper_name="hh Helper ($role)"
    else
      helper_name="hh Helper"
    fi
    helper_directory="$frameworks_directory/$helper_name.app/Contents"
    mkdir -p "$helper_directory/MacOS"
    cp "$repository_root/target/$profile/hh-cef-helper" \
      "$helper_directory/MacOS/$helper_name"
    chmod 755 "$helper_directory/MacOS/$helper_name"
    sed \
      -e "s|__HELPER_NAME__|$helper_name|g" \
      -e "s|__HELPER_IDENTIFIER__|com.harnessharlot.desktop.helper$identifier_suffix|g" \
      "$repository_root/packaging/macos/Info-cef-helper.plist" \
      > "$helper_directory/Info.plist"
  }

  # CEF's default macOS subprocess lookup needs the generic helper as well as
  # the four process-role helper bundles.
  create_cef_helper "" ""
  create_cef_helper GPU .gpu
  create_cef_helper Renderer .renderer
  create_cef_helper Plugin .plugin
  create_cef_helper Alerts .alerts
fi

# This script only lays out the bundle. Signing is an explicit inside-out step
# performed by sign-macos-app.sh so stale nested signatures cannot survive.
printf '%s\n' "$app_directory"
