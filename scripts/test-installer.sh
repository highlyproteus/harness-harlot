#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
root=$(mktemp -d "${TMPDIR:-/tmp}/hh-installer-test.XXXXXX")
cleanup() { rm -rf "$root"; }
trap cleanup EXIT HUP INT TERM
mock_bin="$root/bin"
home="$root/home"
log="$root/commands.log"
good_dmg="$root/good.dmg"
bad_dmg="$root/bad.dmg"
good_manifest="$root/good.update.json"
bad_manifest="$root/bad.update.json"
good_signature="$root/good.update.json.sig"
mkdir -p "$mock_bin" "$home"
printf 'SIGNED-DMG\n' > "$good_dmg"
printf 'TAMPERED-DMG\n' > "$bad_dmg"
printf '{"signed":"manifest"}\n' > "$good_manifest"
printf '{"tampered":"manifest"}\n' > "$bad_manifest"
printf 'signature\n' > "$good_signature"
: > "$log"

cat > "$mock_bin/codesign" <<'EOF'
#!/bin/sh
set -eu
printf 'codesign %s\n' "$*" >> "$HH_TEST_LOG"
last=
for argument in "$@"; do last=$argument; done
case " $* " in
  *" -dv "*)
    echo 'flags=0x10000(runtime)' >&2
    echo "TeamIdentifier=$HH_INSTALLER_FIXTURE_TEAM_ID" >&2
    exit 0
    ;;
esac
case "$last" in
  */release.dmg) cmp "$last" "$HH_TEST_GOOD_DMG" >/dev/null ;;
esac
EOF
cat > "$mock_bin/xcrun" <<'EOF'
#!/bin/sh
set -eu
printf 'xcrun %s\n' "$*" >> "$HH_TEST_LOG"
[ "${HH_TEST_FAIL_STAPLE:-0}" != 1 ]
EOF
cat > "$mock_bin/spctl" <<'EOF'
#!/bin/sh
set -eu
printf 'spctl %s\n' "$*" >> "$HH_TEST_LOG"
EOF
cat > "$mock_bin/hdiutil" <<'EOF'
#!/bin/sh
set -eu
printf 'hdiutil %s\n' "$*" >> "$HH_TEST_LOG"
case "${1:-}" in
  attach)
    mount=
    previous=
    for argument in "$@"; do
      if [ "$previous" = -mountpoint ]; then mount=$argument; fi
      previous=$argument
    done
    app="$mount/Harness Harlot.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.harnessharlot.desktop</string></dict></plist>
PLIST
    printf '#!/bin/sh\n' > "$app/Contents/MacOS/hh"
    printf '#!/bin/sh\n' > "$app/Contents/MacOS/hh-service"
    printf '#!/bin/sh\n' > "$app/Contents/MacOS/hh-update-tool"
    chmod 755 "$app/Contents/MacOS/hh" "$app/Contents/MacOS/hh-service" \
      "$app/Contents/MacOS/hh-update-tool"
    cat > "$mount/hh-update-tool" <<'TOOL'
#!/bin/sh
set -eu
printf 'hh-update-tool %s\n' "$*" >> "$HH_TEST_LOG"
manifest=
signature=
artifact=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --manifest) manifest=$2; shift 2 ;;
    --signature) signature=$2; shift 2 ;;
    --artifact) artifact=$2; shift 2 ;;
    *) shift ;;
  esac
done
cmp "$manifest" "$HH_TEST_GOOD_MANIFEST" >/dev/null
cmp "$signature" "$HH_TEST_GOOD_SIGNATURE" >/dev/null
cmp "$artifact" "$HH_TEST_GOOD_DMG" >/dev/null
TOOL
    chmod 755 "$mount/hh-update-tool"
    ;;
esac
EOF
cat > "$mock_bin/ln" <<'EOF'
#!/bin/sh
set -eu
if [ -n "${HH_TEST_FAIL_LINK_ONCE:-}" ] && [ ! -e "$HH_TEST_FAIL_LINK_ONCE" ]; then
  : > "$HH_TEST_FAIL_LINK_ONCE"
  exit 1
fi
exec /bin/ln "$@"
EOF
chmod 755 "$mock_bin"/*

export HOME="$home"
export PATH="$mock_bin:/usr/bin:/bin:/usr/sbin:/sbin"
export HH_TEST_LOG="$log"
export HH_TEST_GOOD_DMG="$good_dmg"
export HH_TEST_GOOD_MANIFEST="$good_manifest"
export HH_TEST_GOOD_SIGNATURE="$good_signature"
export HH_INSTALLER_FIXTURE_TEAM_ID=ABC123TEAM
export HH_INSTALLER_FIXTURE_MANIFEST="$good_manifest"
export HH_INSTALLER_FIXTURE_SIGNATURE="$good_signature"
export HH_INSTALLER_FIXTURE_UPDATE_HOST=updates.example.invalid
export HH_INSTALLER_FIXTURE_KEY_ID=test-only-v1
export HH_INSTALLER_FIXTURE_PUBLIC_KEY=fixture-public-key

if "$repository_root/install.sh" --version 0.1.0+1 >/dev/null 2>&1; then
  echo "unconfigured production installer unexpectedly succeeded" >&2
  exit 1
fi

if "$repository_root/install.sh" --version 0.1.0+1 \
  --prefix "$HOME/../escaped" --print-plan >/dev/null 2>&1; then
  echo "installer accepted a parent-directory install prefix" >&2
  exit 1
fi
[ ! -e "$root/escaped" ]

if "$repository_root/install.sh" --version 0.1.0+1 \
  --prefix ../escaped --print-plan >/dev/null 2>&1; then
  echo "installer accepted a relative install prefix" >&2
  exit 1
fi

export HH_INSTALLER_TEST_MODE=1
export HH_INSTALLER_FIXTURE_DMG="$bad_dmg"
: > "$log"
if "$repository_root/install.sh" --version 0.1.0+1 --verify-only >/dev/null 2>&1; then
  echo "tampered DMG unexpectedly passed verification" >&2
  exit 1
fi
if grep '^hdiutil attach' "$log" >/dev/null 2>&1; then
  echo "tampered DMG reached hdiutil attach" >&2
  exit 1
fi

export HH_INSTALLER_FIXTURE_DMG="$good_dmg"
export HH_TEST_FAIL_STAPLE=1
: > "$log"
if "$repository_root/install.sh" --version 0.1.0+1 --verify-only >/dev/null 2>&1; then
  echo "unstapled DMG unexpectedly passed verification" >&2
  exit 1
fi
if grep '^hdiutil attach' "$log" >/dev/null 2>&1; then
  echo "unstapled DMG reached hdiutil attach" >&2
  exit 1
fi
unset HH_TEST_FAIL_STAPLE
export HH_INSTALLER_FIXTURE_MANIFEST="$bad_manifest"
: > "$log"
if "$repository_root/install.sh" --version 0.1.0+1 --verify-only >/dev/null 2>&1; then
  echo "tampered manifest unexpectedly passed verification" >&2
  exit 1
fi
grep '^hdiutil attach' "$log" >/dev/null
grep '^hh-update-tool ' "$log" >/dev/null
export HH_INSTALLER_FIXTURE_MANIFEST="$good_manifest"

: > "$log"
"$repository_root/install.sh" --version 0.1.0+1 --verify-only >/dev/null
[ ! -e "$home/Applications/Harness Harlot.app" ]
grep '^hdiutil attach' "$log" >/dev/null

# Never replace an unrelated app or command that happens to use the same path.
unrelated="$home/Applications/Harness Harlot.app"
mkdir -p "$unrelated/Contents/MacOS"
cat > "$unrelated/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>example.unrelated</string></dict></plist>
PLIST
printf '#!/bin/sh\n' > "$unrelated/Contents/MacOS/hh"
chmod 755 "$unrelated/Contents/MacOS/hh"
if "$repository_root/install.sh" --version 0.1.0+1 >/dev/null 2>&1; then
  echo "installer replaced an unrelated app" >&2
  exit 1
fi
[ "$(plutil -extract CFBundleIdentifier raw -o - "$unrelated/Contents/Info.plist")" = example.unrelated ]
rm -rf "$unrelated"

mkdir -p "$home/.local/bin"
printf 'unrelated command\n' > "$home/.local/bin/hh"
if "$repository_root/install.sh" --version 0.1.0+1 >/dev/null 2>&1; then
  echo "installer replaced an unrelated command" >&2
  exit 1
fi
grep 'unrelated command' "$home/.local/bin/hh" >/dev/null
rm -f "$home/.local/bin/hh"

"$repository_root/install.sh" --version 0.1.0+1 >/dev/null
[ -x "$home/Applications/Harness Harlot.app/Contents/MacOS/hh" ]
[ -L "$home/.local/bin/hh" ]
printf 'original\n' > "$home/Applications/Harness Harlot.app/rollback-marker"
export HH_TEST_FAIL_LINK_ONCE="$root/link-failed"
if "$repository_root/install.sh" --version 0.1.0+1 >/dev/null 2>&1; then
  echo "installer did not report a post-swap link failure" >&2
  exit 1
fi
unset HH_TEST_FAIL_LINK_ONCE
[ -f "$home/Applications/Harness Harlot.app/rollback-marker" ]
[ -L "$home/.local/bin/hh" ]
# Reinstall keeps one validated rollback bundle instead of deleting in place.
"$repository_root/install.sh" --version 0.1.0+1 >/dev/null
[ -x "$home/Applications/Harness Harlot.previous.app/Contents/MacOS/hh" ]
mkdir -p "$home/Library/Application Support/Harness Harlot/history"
printf 'retain\n' > "$home/Library/Application Support/Harness Harlot/history/local"
"$repository_root/install.sh" --uninstall >/dev/null
[ ! -e "$home/Applications/Harness Harlot.app" ]
[ ! -e "$home/Applications/Harness Harlot.previous.app" ]
[ ! -e "$home/.local/bin/hh" ]
[ -f "$home/Library/Application Support/Harness Harlot/history/local" ]

echo "installer rejects unconfigured, tampered, unstapled, and invalid-manifest inputs; dual-root install and uninstall fixtures pass"
