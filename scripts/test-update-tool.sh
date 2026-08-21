#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-update-tool-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM
mkdir -p "$work/mock-bin" "$work/home/Applications" "$work/home/.local/bin" "$work/mounted-app/Contents/MacOS"

key="$work/update-key"
printf '********************************' | base64 > "$key"
chmod 600 "$key"
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
cargo build --locked --release -p hh-updater --features fetch,fixture,community-macos --bin hh-update-tool
public_key=$("$repository_root/target/release/hh-release-sign" public-key --private-key "$key")

case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
artifact="$work/Harness-Harlot-0.2.0-b1-macos-${architecture}-community.dmg"
printf 'fixture dmg bytes\n' > "$artifact"
size=$(stat -f %z "$artifact")
sha256=$(shasum -a 256 "$artifact" | sed 's/[[:space:]].*$//')
published_at=$(date -u -v-1H '+%Y-%m-%dT%H:%M:%SZ')
valid_until=$(date -u -v+1d '+%Y-%m-%dT%H:%M:%SZ')
manifest="$work/manifest.json"
signature="$manifest.sig"
cat > "$manifest" <<EOF
{
  "schema": "hh-update-manifest-v2",
  "product": "Harness Harlot",
  "channel": "stable",
  "key_id": "test-only-v1",
  "version": "0.2.0",
  "build": 1,
  "published_at": "$published_at",
  "valid_until": "$valid_until",
  "platform": "macos",
  "minimum_macos": "13.0",
  "session_service": {
    "protocol_version": 1,
    "requires_quiescent_service": true
  },
  "artifacts": [
    {
      "platform": "macos",
      "architecture": "$architecture",
      "format": "dmg",
      "file_name": "$(basename "$artifact")",
      "url": "https://updates.example.invalid/$(basename "$artifact")",
      "sha256": "$sha256",
      "size": $size
    }
  ]
}
EOF
"$repository_root/target/release/hh-release-sign" sign \
  --manifest "$manifest" --signature "$signature" --private-key "$key"

if "$repository_root/target/release/hh-update-tool" check --current-version 0.1.0 \
  >"$work/unpackaged-build.out" 2>&1; then
  echo "unpackaged updater accessed the production feed" >&2
  exit 1
fi
grep -q "updates are available only from a packaged build" "$work/unpackaged-build.out"

check_output=$(
  "$repository_root/target/release/hh-update-tool" check \
    --fixture --key-id test-only-v1 --public-key "$public_key" \
    --host updates.example.invalid --manifest "$manifest" \
    --signature "$signature" --artifact "$artifact" \
    --current-version 0.1.0
)
[ "$check_output" = "update available: 0.2.0 build 1" ]

write_app() {
  app=$1
  marker=$2
  mkdir -p "$app/Contents/MacOS"
  cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.harnessharlot.desktop</string></dict></plist>
EOF
  printf '#!/bin/sh\necho %s\n' "$marker" > "$app/Contents/MacOS/hh"
  chmod +x "$app/Contents/MacOS/hh"
}

write_app "$work/mounted-app" new
write_app "$work/home/Applications/Harness Harlot.app" old
write_app "$work/home/Applications/Harness Harlot.previous.app" legacy-backup
ln -s "$work/home/Applications/Harness Harlot.app/Contents/MacOS/hh" "$work/home/.local/bin/hh"

cat > "$work/mock-bin/codesign" <<'EOF'
#!/bin/sh
if [ "${1:-}" = -dv ]; then
  echo 'Signature=adhoc' >&2
fi
exit 0
EOF
cat > "$work/mock-bin/hdiutil" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$HH_UPDATE_FIXTURE_HDIUTIL_LOG"
case "$1" in
  attach)
    mount=
    while [ "$#" -gt 0 ]; do
      if [ "$1" = -mountpoint ]; then mount=$2; break; fi
      shift
    done
    [ -n "$mount" ]
    /usr/bin/ditto "$HH_UPDATE_FIXTURE_APP" "$mount/Harness Harlot.app"
    ;;
  detach) ;;
  *) exit 2 ;;
esac
EOF
cat > "$work/mock-bin/open" <<'EOF'
#!/bin/sh
if [ "${HH_UPDATE_FIXTURE_OPEN_FAIL:-0}" = 1 ]; then exit 42; fi
printf '%s\n' "$1" > "$HH_UPDATE_FIXTURE_OPEN_LOG"
EOF
chmod +x "$work/mock-bin/codesign" "$work/mock-bin/hdiutil" "$work/mock-bin/open"

install_fixture() {
  HOME="$work/home" \
  HH_SOCKET="$work/session.sock" \
  PATH="$work/mock-bin:$PATH" \
  HH_UPDATE_FIXTURE_APP="$work/mounted-app" \
  HH_UPDATE_FIXTURE_HDIUTIL_LOG="$work/hdiutil.log" \
  HH_UPDATE_FIXTURE_OPEN_LOG="$work/open.log" \
  "$repository_root/target/release/hh-update-tool" install \
    --fixture --community --key-id test-only-v1 --public-key "$public_key" \
    --host updates.example.invalid \
    --manifest "$manifest" --signature "$signature" --artifact "$artifact" \
    --current-version 0.1.0 --prefix "$work/home/Applications"
}

install_fixture
grep -F 'attach -quiet ' "$work/hdiutil.log" >/dev/null
grep -F 'detach -quiet ' "$work/hdiutil.log" >/dev/null
[ "$("$work/home/Applications/Harness Harlot.app/Contents/MacOS/hh")" = new ]
[ "$("$work/home/Applications/.Harness Harlot.previous.app/Contents/MacOS/hh")" = old ]
[ ! -e "$work/home/Applications/Harness Harlot.previous.app" ]
[ "$(readlink "$work/home/.local/bin/hh")" = "$work/home/Applications/Harness Harlot.app/Contents/MacOS/hh" ]
[ -s "$work/open.log" ]

printf '#!/bin/sh\necho current-before-failure\n' > "$work/home/Applications/Harness Harlot.app/Contents/MacOS/hh"
chmod +x "$work/home/Applications/Harness Harlot.app/Contents/MacOS/hh"
HH_UPDATE_FIXTURE_OPEN_FAIL=1 install_fixture >"$work/relaunch-failure.out" 2>&1
grep -F "update installed, but Harness Harlot could not be relaunched" \
  "$work/relaunch-failure.out" >/dev/null
[ "$("$work/home/Applications/Harness Harlot.app/Contents/MacOS/hh")" = new ]
[ "$("$work/home/Applications/.Harness Harlot.previous.app/Contents/MacOS/hh")" = current-before-failure ]
[ "$(readlink "$work/home/.local/bin/hh")" = "$work/home/Applications/Harness Harlot.app/Contents/MacOS/hh" ]

echo "hh-update-tool fixture installs atomically, preserves rollback, and reports relaunch failures"
