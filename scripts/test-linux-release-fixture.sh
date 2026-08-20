#!/bin/sh
set -eu

if [ "$(uname -s)" != Linux ]; then
  echo "Linux release fixture requires Linux" >&2
  exit 2
fi
repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=$(
  cd "$repository_root"
  cargo metadata --locked --format-version 1 --no-deps |
    python3 -c 'import json,sys; print(next(package["version"] for package in json.load(sys.stdin)["packages"] if package["name"] == "hh-desktop"))'
)
target_directory=${CARGO_TARGET_DIR:-"$repository_root/target"}
case "$target_directory" in
  /*) ;;
  *) target_directory="$repository_root/$target_directory" ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-linux-release-fixture.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM
key="$work/update-key"
printf '********************************' | base64 > "$key"
chmod 600 "$key"
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
public_key=$("$target_directory/release/hh-release-sign" public-key --private-key "$key")

HH_RELEASE_TEST_MODE=1 \
HH_ALLOW_DIRTY_TEST_PACKAGE=1 \
HH_UPDATE_SIGNING_KEY_FILE="$key" \
HH_UPDATE_PUBLIC_KEY="$public_key" \
  "$repository_root/scripts/package-linux-release.sh" "$version" 97 >/dev/null

case "$(uname -m)" in
  aarch64 | arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported Linux test architecture" >&2; exit 1 ;;
esac
distribution="$target_directory/release-dist/linux-$architecture"
artifact="$distribution/Harness-Harlot-${version}-b97-linux-$architecture.tar.gz"
manifest="$distribution/Harness-Harlot-${version}-b97-linux-$architecture.update.json"
signature="$manifest.sig"
[ -f "$artifact" ]
[ -f "$manifest" ]
[ -f "$signature" ]
[ -f "$distribution/manifest-linux-$architecture.update.json" ]
[ -f "$distribution/manifest-linux-$architecture.update.json.sig" ]

python3 - "$manifest" "$architecture" "$(basename "$artifact")" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1], encoding="utf-8"))
assert manifest["schema"] == "hh-update-manifest-v2"
assert manifest["platform"] == "linux"
assert manifest["minimum_glibc"] == "2.35"
assert "minimum_macos" not in manifest
assert manifest["build"] == 97
artifact = manifest["artifacts"][0]
assert artifact["platform"] == "linux"
assert artifact["architecture"] == sys.argv[2]
assert artifact["format"] == "tar.gz"
assert artifact["file_name"] == sys.argv[3]
PY

archive_list="$work/archive-list"
tar -tzf "$artifact" | sed 's:/$::' | sort -u > "$archive_list"
for required in \
  Harness-Harlot/install.sh \
  Harness-Harlot/bin/hh \
  Harness-Harlot/bin/hh-service \
  Harness-Harlot/bin/hh-update-tool \
  Harness-Harlot/share/applications/com.harnessharlot.desktop.desktop \
  Harness-Harlot/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png \
  Harness-Harlot/share/licenses/harness-harlot/LICENSE \
  Harness-Harlot/share/licenses/harness-harlot/THIRD_PARTY_NOTICES.md \
  Harness-Harlot/share/licenses/harness-harlot/ASSET_NOTICES.md \
  Harness-Harlot/share/harness-harlot/install-id
do
  grep -Fx "$required" "$archive_list" >/dev/null
done
if grep -Fx "Harness-Harlot/bin/libcef.so" "$archive_list" >/dev/null; then
  echo "CEF runtime leaked into the CEF-free Linux release fixture" >&2
  exit 1
fi
if tar -tvzf "$artifact" | grep -E '^[lhcbps]' >/dev/null; then
  echo "Linux release archive contains a link or special file" >&2
  exit 1
fi

up_to_date=$(HOME="$work/home" HH_SOCKET="$work/session.sock" "$target_directory/release/hh-update-tool" install \
  --fixture --platform linux --architecture "$architecture" \
  --key-id test-only-v1 --public-key "$public_key" --host updates.example.invalid \
  --manifest "$manifest" --signature "$signature" --artifact "$artifact")

mkdir -p "$work/extracted" "$work/install-home"
tar -xzf "$artifact" -C "$work/extracted"
cat > "$work/extracted/Harness-Harlot/bin/hh" <<'EOF'
#!/bin/sh
printf 'launched\n' > "${HH_LINUX_UPDATE_LAUNCH_LOG:?missing launch log}"
EOF
chmod 0755 "$work/extracted/Harness-Harlot/bin/hh"
HOME="$work/install-home" HH_SOCKET="$work/install-session.sock" \
  HH_LINUX_UPDATE_LAUNCH_LOG="$work/local-launched" \
  "$work/extracted/Harness-Harlot/install.sh" \
  --prefix "$work/install-home/.local/lib" >/dev/null
installed="$work/install-home/.local/lib/harness-harlot"
[ -x "$installed/bin/hh" ]
[ "$(readlink "$work/install-home/.local/bin/hh")" = "$installed/bin/hh" ]
[ "$(readlink "$work/install-home/.local/share/applications/com.harnessharlot.desktop.desktop")" = "$installed/share/applications/com.harnessharlot.desktop.desktop" ]
[ "$(readlink "$work/install-home/.local/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png")" = "$installed/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png" ]
for _ in 1 2 3 4 5; do
  [ -f "$work/local-launched" ] && break
  sleep 0.1
done
[ "$(cat "$work/local-launched")" = launched ]
[ "$up_to_date" = "up to date" ]

mkdir -p "$work/mock-bin"
cat > "$work/mock-bin/gh" <<'EOF'
#!/bin/sh
set -eu
case "$1:$2" in
  release:view)
    printf '%s\n' "$HH_TEST_RELEASE_TAG"
    ;;
  release:download)
    shift 3
    destination=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --dir) destination=$2; shift 2 ;;
        --repo | --pattern) shift 2 ;;
        *) exit 2 ;;
      esac
    done
    [ -n "$destination" ]
    mkdir -p "$destination"
    cp "$HH_TEST_RELEASE_ARTIFACT" "$destination/$(basename "$HH_TEST_RELEASE_ARTIFACT")"
    ;;
  attestation:verify)
    : > "$HH_TEST_ATTESTATION_LOG"
    ;;
  *)
    exit 2
    ;;
esac
EOF
chmod 0755 "$work/mock-bin/gh"
HH_TEST_RELEASE_TAG="v$version" \
HH_TEST_RELEASE_ARTIFACT="$artifact" \
HH_TEST_ATTESTATION_LOG="$work/attestation-verified" \
PATH="$work/mock-bin:$PATH" \
  "$repository_root/install-linux.sh" --verify-only > "$work/bootstrap.out"
grep -F "verified Harness Harlot Linux release v$version for $architecture" \
  "$work/bootstrap.out" >/dev/null
[ -f "$work/attestation-verified" ]

echo "Linux release fixture bundles all runtime files, verifies the bootstrap path, signs stable manifests, and embeds its build number"
