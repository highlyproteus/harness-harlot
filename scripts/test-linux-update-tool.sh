#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
target_directory=${CARGO_TARGET_DIR:-"$repository_root/target"}
case "$target_directory" in
  /*) ;;
  *) target_directory="$repository_root/$target_directory" ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-linux-update-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM
mkdir -p "$work/home/.local/lib" "$work/home/.local/bin"

key="$work/update-key"
printf '********************************' | base64 > "$key"
chmod 600 "$key"
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
cargo build --locked --release -p hh-updater --features fetch,fixture --bin hh-update-tool
signer="$target_directory/release/hh-release-sign"
tool="$target_directory/release/hh-update-tool"
public_key=$("$signer" public-key --private-key "$key")
case "$(uname -m)" in
  arm64 | aarch64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
case "$(uname -s)" in
  Darwin)
    file_size() { stat -f %z "$1"; }
    published_at=$(date -u -v-1H '+%Y-%m-%dT%H:%M:%SZ')
    valid_until=$(date -u -v+1d '+%Y-%m-%dT%H:%M:%SZ')
    ;;
  *)
    file_size() { stat -c %s "$1"; }
    published_at=$(date -u -d '-1 hour' '+%Y-%m-%dT%H:%M:%SZ')
    valid_until=$(date -u -d '+1 day' '+%Y-%m-%dT%H:%M:%SZ')
    ;;
esac

write_install() {
  root=$1
  hh_mode=$2
  marker=$3
  mkdir -p \
    "$root/bin" \
    "$root/share/applications" \
    "$root/share/icons/hicolor/512x512/apps" \
    "$root/share/licenses/harness-harlot" \
    "$root/share/harness-harlot"
  printf '#!/bin/sh\nexit 0\n' > "$root/install.sh"
  if [ "$hh_mode" = broken ]; then
    printf '#!/no/such/interpreter\n# %s\n' "$marker" > "$root/bin/hh"
  else
    cat > "$root/bin/hh" <<EOF
#!/bin/sh
printf '%s\n' '$marker' > "\${HH_LINUX_UPDATE_LAUNCH_LOG:-/dev/null}"
EOF
  fi
  printf '#!/bin/sh\nexit 0\n' > "$root/bin/hh-service"
  printf '#!/bin/sh\nexit 0\n' > "$root/bin/hh-update-tool"
  chmod 0755 "$root/install.sh" "$root/bin/hh" "$root/bin/hh-service" "$root/bin/hh-update-tool"
  printf '[Desktop Entry]\nName=Harness Harlot\n' > "$root/share/applications/com.harnessharlot.desktop.desktop"
  printf 'fixture icon\n' > "$root/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png"
  printf 'license\n' > "$root/share/licenses/harness-harlot/LICENSE"
  printf 'notices\n' > "$root/share/licenses/harness-harlot/THIRD_PARTY_NOTICES.md"
  printf 'asset notices\n' > "$root/share/licenses/harness-harlot/ASSET_NOTICES.md"
  printf 'com.harnessharlot.desktop\n' > "$root/share/harness-harlot/install-id"
  chmod 0644 \
    "$root/share/applications/com.harnessharlot.desktop.desktop" \
    "$root/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png" \
    "$root/share/licenses/harness-harlot/LICENSE" \
    "$root/share/licenses/harness-harlot/THIRD_PARTY_NOTICES.md" \
    "$root/share/licenses/harness-harlot/ASSET_NOTICES.md" \
    "$root/share/harness-harlot/install-id"
}

make_package() {
  name=$1
  mode=$2
  marker=$3
  source="$work/source-$name"
  rm -rf "$source"
  write_install "$source/Harness-Harlot" "$mode" "$marker"
  artifact="$work/Harness-Harlot-${name}-linux-${architecture}.tar.gz"
  COPYFILE_DISABLE=1 tar -czf "$artifact" -C "$source" Harness-Harlot
  printf '%s\n' "$artifact"
}

make_manifest() {
  artifact=$1
  version=$2
  build=$3
  manifest=$4
  sha256=$(shasum -a 256 "$artifact" | sed 's/[[:space:]].*$//')
  size=$(file_size "$artifact")
  cat > "$manifest" <<EOF
{
  "schema": "hh-update-manifest-v2",
  "product": "Harness Harlot",
  "channel": "stable",
  "key_id": "test-only-v1",
  "version": "$version",
  "build": $build,
  "published_at": "$published_at",
  "valid_until": "$valid_until",
  "platform": "linux",
  "minimum_glibc": "2.35",
  "session_service": {
    "protocol_version": 1,
    "requires_quiescent_service": true
  },
  "artifacts": [
    {
      "platform": "linux",
      "architecture": "$architecture",
      "format": "tar.gz",
      "file_name": "$(basename "$artifact")",
      "url": "https://updates.example.invalid/$(basename "$artifact")",
      "sha256": "$sha256",
      "size": $size
    }
  ]
}
EOF
  $signer sign --manifest "$manifest" --signature "$manifest.sig" --private-key "$key"
}

run_install() {
  artifact=$1
  manifest=$2
  install_home=${INSTALL_HOME:-"$work/home"}
  HOME="$install_home" HH_SOCKET="$work/session.sock" HH_LINUX_UPDATE_LAUNCH_LOG="$work/launched" "$tool" install \
    --fixture \
    --platform linux \
    --architecture "$architecture" \
    --current-version 0.1.0 \
    --current-build 0 \
    --prefix "$install_home/.local/lib" \
    --key-id test-only-v1 \
    --public-key "$public_key" \
    --host updates.example.invalid \
    --manifest "$manifest" \
    --signature "$manifest.sig" \
    --artifact "$artifact"
}

app="$work/home/.local/lib/harness-harlot"
backup="$work/home/.local/lib/harness-harlot.previous"
write_install "$app" normal old
ln -s "$app/bin/hh" "$work/home/.local/bin/hh"

artifact=$(make_package good normal new)
manifest="$work/good.json"
make_manifest "$artifact" 0.2.0 1 "$manifest"
run_install "$artifact" "$manifest"
for _ in 1 2 3 4 5; do
  [ -f "$work/launched" ] && break
  sleep 0.1
done
[ "$(cat "$work/launched")" = new ]
grep -q old "$backup/bin/hh"
[ "$(readlink "$work/home/.local/bin/hh")" = "$app/bin/hh" ]
[ "$(readlink "$work/home/.local/share/applications/com.harnessharlot.desktop.desktop")" = "$app/share/applications/com.harnessharlot.desktop.desktop" ]
[ "$(readlink "$work/home/.local/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png")" = "$app/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png" ]

broken_artifact=$(make_package broken broken broken)
broken_manifest="$work/broken.json"
make_manifest "$broken_artifact" 0.3.0 2 "$broken_manifest"
rm -f "$work/launched"
if run_install "$broken_artifact" "$broken_manifest" >"$work/broken.out" 2>&1; then
  echo "Linux updater accepted an application that could not relaunch" >&2
  exit 1
fi
grep -q new "$app/bin/hh"
[ "$(readlink "$work/home/.local/bin/hh")" = "$app/bin/hh" ]
for _ in 1 2 3 4 5; do
  [ -f "$work/launched" ] && break
  sleep 0.1
done
[ "$(cat "$work/launched")" = new ]


clean_home="$work/clean-home"
mkdir -p "$clean_home/.local/lib" "$clean_home/.local/bin"
if INSTALL_HOME="$clean_home" run_install "$broken_artifact" "$broken_manifest" >"$work/clean-broken.out" 2>&1; then
  echo "Linux updater accepted a broken clean installation" >&2
  exit 1
fi
[ ! -e "$clean_home/.local/lib/harness-harlot" ]
[ ! -e "$clean_home/.local/bin/hh" ]
[ ! -e "$clean_home/.local/share/applications/com.harnessharlot.desktop.desktop" ]
[ ! -e "$clean_home/.local/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png" ]
symlink_source="$work/source-symlink"
write_install "$symlink_source/Harness-Harlot" normal malicious
rm "$symlink_source/Harness-Harlot/bin/hh-service"
ln -s hh "$symlink_source/Harness-Harlot/bin/hh-service"
symlink_artifact="$work/Harness-Harlot-symlink-linux-${architecture}.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$symlink_artifact" -C "$symlink_source" Harness-Harlot
symlink_manifest="$work/symlink.json"
make_manifest "$symlink_artifact" 0.3.0 3 "$symlink_manifest"
if run_install "$symlink_artifact" "$symlink_manifest" >"$work/symlink.out" 2>&1; then
  echo "Linux updater accepted an archive symlink" >&2
  exit 1
fi

tampered_artifact=$(make_package tampered normal tampered)
tampered_manifest="$work/tampered.json"
make_manifest "$tampered_artifact" 0.3.0 4 "$tampered_manifest"
printf 'tamper' >> "$tampered_artifact"
if run_install "$tampered_artifact" "$tampered_manifest" >"$work/tampered.out" 2>&1; then
  echo "Linux updater accepted a tampered archive" >&2
  exit 1
fi

if HOME="$work/home" HH_SOCKET="$work/traversal-session.sock" "$tool" install \
  --fixture --platform linux --architecture "$architecture" \
  --current-version 0.1.0 --current-build 0 \
  --prefix "$work/home/../escaped" \
  --key-id test-only-v1 --public-key "$public_key" --host updates.example.invalid \
  --manifest "$manifest" --signature "$manifest.sig" --artifact "$artifact" \
  >"$work/traversal.out" 2>&1; then
  echo "Linux updater accepted a parent-directory install prefix" >&2
  exit 1
fi
[ ! -e "$work/escaped" ]

mkdir -p "$work/foreign-home/.local/lib" "$work/foreign-home/.local/bin"
write_install "$work/foreign-home/.local/lib/harness-harlot" normal foreign
ln -s /tmp/not-harness-harlot "$work/foreign-home/.local/bin/hh"
if HOME="$work/foreign-home" HH_SOCKET="$work/foreign-session.sock" "$tool" install \
  --fixture --platform linux --architecture "$architecture" \
  --current-version 0.1.0 --current-build 0 \
  --prefix "$work/foreign-home/.local/lib" \
  --key-id test-only-v1 --public-key "$public_key" --host updates.example.invalid \
  --manifest "$manifest" --signature "$manifest.sig" --artifact "$artifact" \
  >"$work/foreign.out" 2>&1; then
  echo "Linux updater replaced an unrelated command link" >&2
  exit 1
fi
[ "$(readlink "$work/foreign-home/.local/bin/hh")" = /tmp/not-harness-harlot ]

echo "Linux updater fixture installs atomically and rejects rollback, archive, integrity, and ownership hazards"
