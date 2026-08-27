#!/bin/sh
set -eu

usage() {
  echo "usage: $0 VERSION BUILD" >&2
  echo "test fixtures require HH_RELEASE_TEST_MODE=1 plus HH_UPDATE_SIGNING_KEY_FILE and HH_UPDATE_PUBLIC_KEY" >&2
  echo "production additionally requires HH_UPDATE_KEY_ID, HH_UPDATE_BASE_URL, and HH_RELEASE_TAG" >&2
  exit 2
}

if [ "$#" -ne 2 ]; then
  usage
fi

version=$1
build=$2
case "$version" in
  *[!0-9A-Za-z.-]* | '' | *..* | .* | *.) echo "VERSION must be a plain semantic version" >&2; exit 2 ;;
esac
case "$build" in
  *[!0-9]* | 0 | '') echo "BUILD must be a positive integer" >&2; exit 2 ;;
esac

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"
target_directory=${CARGO_TARGET_DIR:-"$repository_root/target"}
case "$target_directory" in
  /*) ;;
  *) target_directory="$repository_root/$target_directory" ;;
esac
release_binary_directory="$target_directory/release"
export HH_RELEASE_BUILD="$build"

workspace_version=$(cargo metadata --locked --format-version 1 --no-deps | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "hh-desktop"))')
if [ "$workspace_version" != "$version" ]; then
  echo "VERSION $version does not match workspace version $workspace_version" >&2
  exit 2
fi
protocol_version=$(sed -nE 's/^pub const PROTOCOL_VERSION: u16 = ([0-9]+);$/\1/p' crates/protocol/src/lib.rs)
case "$protocol_version" in
  '' | *[!0-9]*) echo "could not read PROTOCOL_VERSION from crates/protocol/src/lib.rs" >&2; exit 2 ;;
esac

case "$(uname -m)" in
  aarch64 | arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported Linux architecture: $(uname -m)" >&2; exit 2 ;;
esac

test_mode=${HH_RELEASE_TEST_MODE:-0}
channel=${HH_UPDATE_CHANNEL:-stable}
case "$channel" in
  stable | edge) ;;
  *) echo "HH_UPDATE_CHANNEL must be stable or edge" >&2; exit 2 ;;
esac
: "${HH_UPDATE_SIGNING_KEY_FILE:?set HH_UPDATE_SIGNING_KEY_FILE to an owner-only base64 Ed25519 seed file}"
: "${HH_UPDATE_PUBLIC_KEY:?set HH_UPDATE_PUBLIC_KEY to the matching base64 Ed25519 public key}"
if [ "$test_mode" = 1 ]; then
  key_id=${HH_UPDATE_KEY_ID:-test-only-v1}
  base_url=${HH_UPDATE_BASE_URL:-https://updates.example.invalid/stable}
else
  : "${HH_UPDATE_KEY_ID:?set HH_UPDATE_KEY_ID for a publishable package}"
  : "${HH_UPDATE_BASE_URL:?set HH_UPDATE_BASE_URL for a publishable package}"
  : "${CEF_PATH:?set CEF_PATH to the unpacked Linux CEF distribution}"
  if [ "$channel" = stable ]; then
    : "${HH_RELEASE_TAG:?set HH_RELEASE_TAG to the signed annotated tag for this release}"
  else
    : "${HH_RELEASE_COMMIT:?set HH_RELEASE_COMMIT to the edge source commit}"
  fi
  key_id=$HH_UPDATE_KEY_ID
  base_url=$HH_UPDATE_BASE_URL
  if [ "$key_id" = "test-only-v1" ]; then
    echo "test-only update key is forbidden in production" >&2
    exit 2
  fi
  case "$base_url" in
    *.invalid | *.invalid/*) echo ".invalid update hosts are forbidden in production" >&2; exit 2 ;;
  esac
fi
case "$base_url" in
  https://*) ;;
  *) echo "HH_UPDATE_BASE_URL must use HTTPS" >&2; exit 2 ;;
esac
base_url=${base_url%/}
update_host=${base_url#https://}
update_host=${update_host%%/*}
case "$update_host" in
  '' | *:* | *@*) echo "HH_UPDATE_BASE_URL must use a bare HTTPS host without credentials or port" >&2; exit 2 ;;
esac

if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
  if [ "$test_mode" = 1 ] && [ "${HH_ALLOW_DIRTY_TEST_PACKAGE:-0}" = 1 ]; then
    echo "warning: creating a test-only package from a dirty worktree" >&2
  else
    echo "refusing to package a dirty worktree, including untracked files" >&2
    exit 2
  fi
fi
if [ "$test_mode" != 1 ]; then
  head_commit=$(git rev-parse HEAD)
  if [ "$channel" = stable ]; then
    git verify-tag "$HH_RELEASE_TAG"
    tag_commit=$(git rev-parse "$HH_RELEASE_TAG^{commit}")
    if [ "$tag_commit" != "$head_commit" ]; then
      echo "signed release tag $HH_RELEASE_TAG does not resolve to HEAD" >&2
      exit 2
    fi
  elif [ "$HH_RELEASE_COMMIT" != "$head_commit" ]; then
    echo "edge release commit $HH_RELEASE_COMMIT does not resolve to HEAD" >&2
    exit 2
  fi
fi

if [ "$test_mode" = 1 ]; then
  cargo build --locked --release -p hh-desktop --bin hh
else
  cargo build --locked --release -p hh-desktop --features browser --bin hh
  cargo build --locked --release -p hh-cef-view --features cef --bin hh-cef-helper
fi
cargo build --locked --release -p hh-session-service --bin hh-service
updater_features=fetch
if [ "$test_mode" = 1 ]; then
  updater_features=fetch,fixture
fi
cargo build --locked --release -p hh-updater --features "$updater_features" --bin hh-update-tool
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
derived_public_key=$("$release_binary_directory/hh-release-sign" public-key --private-key "$HH_UPDATE_SIGNING_KEY_FILE")
if [ "$derived_public_key" != "$HH_UPDATE_PUBLIC_KEY" ]; then
  echo "HH_UPDATE_PUBLIC_KEY does not match HH_UPDATE_SIGNING_KEY_FILE" >&2
  exit 2
fi

distribution_directory="$target_directory/release-dist/linux-$architecture"
staging="$target_directory/linux-package-$architecture"
root="$staging/Harness-Harlot"
rm -rf "$distribution_directory" "$staging"
mkdir -p \
  "$distribution_directory" \
  "$root/bin" \
  "$root/share/applications" \
  "$root/share/icons/hicolor/512x512/apps" \
  "$root/share/licenses/harness-harlot" \
  "$root/share/harness-harlot"
install -m 0755 "$repository_root/packaging/linux/install.sh" "$root/install.sh"
install -m 0755 "$release_binary_directory/hh" "$root/bin/hh"
install -m 0755 "$release_binary_directory/hh-service" "$root/bin/hh-service"
install -m 0755 "$release_binary_directory/hh-update-tool" "$root/bin/hh-update-tool"
if [ "$test_mode" != 1 ]; then
  install -m 0755 "$release_binary_directory/hh-cef-helper" "$root/bin/hh-cef-helper"
  cef_release_directory=$CEF_PATH
  cef_resources_directory=$CEF_PATH
  if [ -f "$CEF_PATH/Release/libcef.so" ]; then
    cef_release_directory="$CEF_PATH/Release"
    cef_resources_directory="$CEF_PATH/Resources"
  elif [ ! -f "$CEF_PATH/libcef.so" ]; then
    echo "CEF_PATH contains neither libcef.so nor Release/libcef.so: $CEF_PATH" >&2
    exit 2
  fi
  for cef_directory in "$cef_release_directory" "$cef_resources_directory"; do
    [ -d "$cef_directory" ] || continue
    for cef_file in "$cef_directory"/*; do
      [ -f "$cef_file" ] || continue
      cef_name=$(basename "$cef_file")
      if [ "$cef_name" = chrome-sandbox ]; then
        install -m 0755 "$cef_file" "$root/bin/$cef_name"
      else
        install -m 0644 "$cef_file" "$root/bin/$cef_name"
      fi
    done
  done
  if [ -d "$cef_resources_directory/locales" ]; then
    mkdir -p "$root/bin/locales"
    for cef_locale in "$cef_resources_directory/locales"/*; do
      [ -f "$cef_locale" ] || continue
      install -m 0644 "$cef_locale" "$root/bin/locales/$(basename "$cef_locale")"
    done
  fi
  test -f "$root/bin/libcef.so"
  test -f "$root/bin/icudtl.dat"
  test -d "$root/bin/locales"
fi
install -m 0644 "$repository_root/packaging/linux/com.harnessharlot.desktop.desktop" "$root/share/applications/com.harnessharlot.desktop.desktop"
install -m 0644 "$repository_root/packaging/linux/com.harnessharlot.desktop.png" "$root/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png"
install -m 0644 "$repository_root/LICENSE" "$root/share/licenses/harness-harlot/LICENSE"
install -m 0644 "$repository_root/PRIVACY.md" "$root/share/licenses/harness-harlot/PRIVACY.md"
install -m 0644 "$repository_root/THIRD_PARTY_NOTICES.md" "$root/share/licenses/harness-harlot/THIRD_PARTY_NOTICES.md"
install -m 0644 "$repository_root/ASSET_NOTICES.md" "$root/share/licenses/harness-harlot/ASSET_NOTICES.md"
printf 'com.harnessharlot.desktop\n' > "$root/share/harness-harlot/install-id"
chmod 0644 "$root/share/harness-harlot/install-id"

artifact_name="Harness-Harlot-${version}-b${build}-linux-${architecture}.tar.gz"
artifact="$distribution_directory/$artifact_name"
source_date_epoch=$(git show -s --format=%ct HEAD)
TZ=UTC tar --sort=name --mtime="@$source_date_epoch" --owner=0 --group=0 --numeric-owner -czf "$artifact" -C "$staging" Harness-Harlot
artifact_sha256=$(sha256sum "$artifact" | sed 's/[[:space:]].*$//')
artifact_size=$(stat -c %s "$artifact")
published_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
valid_until=$(date -u -d '+7 days' '+%Y-%m-%dT%H:%M:%SZ')
manifest="$distribution_directory/Harness-Harlot-${version}-b${build}-linux-${architecture}.update.json"
signature="$manifest.sig"
cat > "$manifest" <<EOF
{
  "schema": "hh-update-manifest-v2",
  "product": "Harness Harlot",
  "channel": "$channel",
  "key_id": "$key_id",
  "version": "$version",
  "build": $build,
  "published_at": "$published_at",
  "valid_until": "$valid_until",
  "platform": "linux",
  "minimum_glibc": "2.35",
  "session_service": {
    "protocol_version": $protocol_version,
    "requires_quiescent_service": true
  },
  "artifacts": [
    {
      "platform": "linux",
      "architecture": "$architecture",
      "format": "tar.gz",
      "file_name": "$artifact_name",
      "url": "$base_url/$artifact_name",
      "sha256": "$artifact_sha256",
      "size": $artifact_size
    }
  ]
}
EOF
"$release_binary_directory/hh-release-sign" sign \
  --manifest "$manifest" \
  --signature "$signature" \
  --private-key "$HH_UPDATE_SIGNING_KEY_FILE"
stable_manifest="$distribution_directory/manifest-linux-${architecture}.update.json"
cp "$manifest" "$stable_manifest"
cp "$signature" "$stable_manifest.sig"
if [ "$test_mode" = 1 ]; then
  "$release_binary_directory/hh-update-tool" verify \
    --fixture \
    --key-id "$key_id" \
    --public-key "$HH_UPDATE_PUBLIC_KEY" \
    --host "$update_host" \
    --manifest "$manifest" \
    --signature "$signature" \
    --artifact "$artifact" \
    --channel "$channel"
else
  "$release_binary_directory/hh-update-tool" verify-trusted \
    --manifest "$manifest" \
    --signature "$signature" \
    --artifact "$artifact" \
    --channel "$channel"
fi

echo "$distribution_directory"
