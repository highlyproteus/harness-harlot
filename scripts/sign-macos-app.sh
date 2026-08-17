#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 IDENTITY APP_DIR" >&2
  exit 2
fi

identity=$1
app_directory=$2
repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
jit_entitlements="$repository_root/packaging/macos/cef-helper-jit.entitlements"

if [ ! -d "$app_directory" ]; then
  echo "application bundle not found: $app_directory" >&2
  exit 1
fi
if [ ! -x "$app_directory/Contents/MacOS/hh" ] || \
   [ ! -x "$app_directory/Contents/MacOS/hh-service" ]; then
  echo "bundle must contain executable hh and hh-service binaries" >&2
  exit 1
fi
for executable in "$app_directory"/Contents/MacOS/*; do
  [ -e "$executable" ] || continue
  case "${executable##*/}" in
    hh | hh-service | hh-dev) ;;
    *) echo "unexpected executable or helper in Contents/MacOS: $executable" >&2; exit 1 ;;
  esac
done

bundle_identifier=$(plutil -extract CFBundleIdentifier raw -o - "$app_directory/Contents/Info.plist")
app_name=${app_directory##*/}
app_name=${app_name%.app}

sign_code() {
  code_path=$1
  identifier=$2
  entitlements=${3:-}
  if [ "$identity" = "-" ]; then
    if [ -n "$entitlements" ]; then
      codesign --force --options runtime --entitlements "$entitlements" \
        --identifier "$identifier" --sign - "$code_path"
    else
      codesign --force --options runtime --identifier "$identifier" --sign - "$code_path"
    fi
  elif [ -n "$entitlements" ]; then
    codesign --force --options runtime --timestamp --entitlements "$entitlements" \
      --identifier "$identifier" --sign "$identity" "$code_path"
  else
    codesign --force --options runtime --timestamp --identifier "$identifier" \
      --sign "$identity" "$code_path"
  fi
}

frameworks_directory="$app_directory/Contents/Frameworks"
framework="$frameworks_directory/Chromium Embedded Framework.framework"
if [ -e "$framework" ]; then
  if [ ! -d "$framework" ] || [ ! -x "$framework/Chromium Embedded Framework" ]; then
    echo "invalid CEF framework bundle: $framework" >&2
    exit 1
  fi
  for library in "$framework"/Libraries/*.dylib; do
    [ -e "$library" ] || continue
    library_name=${library##*/}
    sign_code "$library" "$bundle_identifier.cef.${library_name%.dylib}"
  done
  sign_code "$framework/Chromium Embedded Framework" org.cef.framework
  sign_code "$framework" org.cef.framework

  sign_cef_helper() {
    role=$1
    identifier_suffix=$2
    entitlements=${3:-}
    if [ -n "$role" ]; then
      helper_name="hh Helper ($role)"
    else
      helper_name="hh Helper"
    fi
    helper="$frameworks_directory/$helper_name.app"
    helper_executable="$helper/Contents/MacOS/$helper_name"
    if [ ! -x "$helper_executable" ]; then
      echo "CEF helper executable not found: $helper_executable" >&2
      exit 1
    fi
    helper_identifier="$bundle_identifier.helper${identifier_suffix:+.$identifier_suffix}"
    sign_code "$helper_executable" "$helper_identifier" "$entitlements"
    sign_code "$helper" "$helper_identifier" "$entitlements"
  }

  # The default CEF loader resolves one generic helper and four role-specific
  # helpers relative to the outer bundle name.
  sign_cef_helper "" ""
  sign_cef_helper GPU gpu "$jit_entitlements"
  sign_cef_helper Renderer renderer "$jit_entitlements"
  sign_cef_helper Plugin plugin
  sign_cef_helper Alerts alerts
fi

# Finish the outer bundle's own nested executables after the CEF framework and
# helpers. The Dev launcher is a sibling executable only in the Dev bundle.
sign_code "$app_directory/Contents/MacOS/hh-service" "$bundle_identifier.service"
sign_code "$app_directory/Contents/MacOS/hh" "$bundle_identifier.executable"
if [ -x "$app_directory/Contents/MacOS/hh-dev" ]; then
  sign_code "$app_directory/Contents/MacOS/hh-dev" "$bundle_identifier.launcher"
fi
sign_code "$app_directory" "$bundle_identifier"
