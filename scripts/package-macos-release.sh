#!/bin/sh
set -eu

usage() {
  echo "usage: $0 VERSION BUILD [--community]" >&2
  echo "test fixtures require HH_RELEASE_TEST_MODE=1 plus HH_UPDATE_SIGNING_KEY_FILE and HH_UPDATE_PUBLIC_KEY" >&2
  echo "community production requires CEF_PATH, HH_UPDATE_KEY_ID, HH_UPDATE_BASE_URL, HH_RELEASE_TAG, and the update signing inputs" >&2
  echo "Developer ID production additionally requires HH_CODESIGN_IDENTITY, HH_EXPECTED_TEAM_ID, and HH_NOTARY_PROFILE" >&2
  exit 2
}

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  usage
fi
community=0
if [ "$#" -eq 3 ]; then
  [ "$3" = "--community" ] || usage
  community=1
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
export HH_RELEASE_BUILD="$build"

workspace_version=$(cargo metadata --locked --format-version 1 --no-deps | plutil -extract packages.0.version raw -o - -)
if [ "$workspace_version" != "$version" ]; then
  echo "VERSION $version does not match workspace version $workspace_version" >&2
  exit 2
fi
protocol_version=$(sed -nE 's/^pub const PROTOCOL_VERSION: u16 = ([0-9]+);$/\1/p' crates/protocol/src/lib.rs)
case "$protocol_version" in
  '' | *[!0-9]*) echo "could not read PROTOCOL_VERSION from crates/protocol/src/lib.rs" >&2; exit 2 ;;
esac

case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported macOS architecture: $(uname -m)" >&2; exit 2 ;;
esac

test_mode=${HH_RELEASE_TEST_MODE:-0}
: "${HH_UPDATE_SIGNING_KEY_FILE:?set HH_UPDATE_SIGNING_KEY_FILE to an owner-only base64 Ed25519 seed file}"
: "${HH_UPDATE_PUBLIC_KEY:?set HH_UPDATE_PUBLIC_KEY to the matching base64 Ed25519 public key}"
if [ "$test_mode" = 1 ]; then
  key_id=${HH_UPDATE_KEY_ID:-test-only-v1}
  base_url=${HH_UPDATE_BASE_URL:-https://updates.example.invalid/stable}
  codesign_identity=-
else
  : "${HH_UPDATE_KEY_ID:?set HH_UPDATE_KEY_ID for a publishable package}"
  : "${HH_UPDATE_BASE_URL:?set HH_UPDATE_BASE_URL for a publishable package}"
  : "${HH_RELEASE_TAG:?set HH_RELEASE_TAG to the signed annotated tag for this release}"
  : "${CEF_PATH:?set CEF_PATH to the pinned CEF distribution for production browser tabs}"
  key_id=$HH_UPDATE_KEY_ID
  base_url=$HH_UPDATE_BASE_URL
  if [ "$community" -eq 1 ]; then
    codesign_identity=-
  else
    : "${HH_CODESIGN_IDENTITY:?set HH_CODESIGN_IDENTITY for a publishable Developer ID package}"
    : "${HH_EXPECTED_TEAM_ID:?set HH_EXPECTED_TEAM_ID for verification}"
    : "${HH_NOTARY_PROFILE:?set HH_NOTARY_PROFILE created by notarytool store-credentials}"
    codesign_identity=$HH_CODESIGN_IDENTITY
  fi
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
  git verify-tag "$HH_RELEASE_TAG"
  tag_commit=$(git rev-parse "$HH_RELEASE_TAG^{commit}")
  head_commit=$(git rev-parse HEAD)
  if [ "$tag_commit" != "$head_commit" ]; then
    echo "signed release tag $HH_RELEASE_TAG does not resolve to HEAD" >&2
    exit 2
  fi
fi

if [ "$test_mode" = 1 ]; then
  if [ "$community" -eq 1 ]; then
    cargo build --locked --release -p hh-desktop --features community-macos --bin hh
  else
    cargo build --locked --release -p hh-desktop --bin hh
  fi
else
  if [ "$community" -eq 1 ]; then
    cargo build --locked --release -p hh-desktop --features browser,community-macos --bin hh
  else
    cargo build --locked --release -p hh-desktop --features browser --bin hh
  fi
fi
cargo build --locked --release -p hh-session-service --bin hh-service
updater_features=fetch
if [ "$community" -eq 1 ]; then
  updater_features="$updater_features,community-macos"
fi
cargo build --locked --release -p hh-updater --features "$updater_features" --bin hh-update-tool
fixture_update_tool=
if [ "$test_mode" = 1 ]; then
  fixture_target_directory="$repository_root/target/fixture-updater"
  fixture_updater_features=fetch,fixture
  if [ "$community" -eq 1 ]; then
    fixture_updater_features="$fixture_updater_features,community-macos"
  fi
  CARGO_TARGET_DIR="$fixture_target_directory" \
    cargo build --locked --release -p hh-updater --features "$fixture_updater_features" --bin hh-update-tool
  fixture_update_tool="$fixture_target_directory/release/hh-update-tool"
fi
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
derived_public_key=$("$repository_root/target/release/hh-release-sign" public-key --private-key "$HH_UPDATE_SIGNING_KEY_FILE")
if [ "$derived_public_key" != "$HH_UPDATE_PUBLIC_KEY" ]; then
  echo "HH_UPDATE_PUBLIC_KEY does not match HH_UPDATE_SIGNING_KEY_FILE" >&2
  exit 2
fi
if [ "$test_mode" = 1 ]; then
  if [ "$community" -eq 1 ]; then
    HH_RELEASE_TEST_MODE=0 "$repository_root/scripts/build-macos-app.sh" release --community
  else
    HH_RELEASE_TEST_MODE=0 "$repository_root/scripts/build-macos-app.sh" release
  fi
elif [ "$community" -eq 1 ]; then
  "$repository_root/scripts/build-macos-app.sh" release --browser --community
else
  "$repository_root/scripts/build-macos-app.sh" release --browser
fi

app_directory="$repository_root/target/release/Harness Harlot.app"
plist="$app_directory/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$plist"
plutil -replace CFBundleVersion -string "$build" "$plist"
"$repository_root/scripts/sign-macos-app.sh" "$codesign_identity" "$app_directory"
codesign --verify --deep --strict --verbose=2 "$app_directory"
update_tool="$repository_root/target/release/hh-update-tool"
verification_tool=$update_tool
if [ "$test_mode" = 1 ]; then
  verification_tool=$fixture_update_tool
  if "$update_tool" verify --fixture >/dev/null 2>&1; then
    echo "release updater unexpectedly contains fixture verification support" >&2
    exit 1
  fi
fi
if [ "$codesign_identity" = "-" ]; then
  codesign --force --options runtime --identifier com.harnessharlot.update-tool --sign - "$update_tool"
else
  codesign --force --options runtime --timestamp --identifier com.harnessharlot.update-tool \
    --sign "$codesign_identity" "$update_tool"
fi
codesign --verify --strict --verbose=2 "$update_tool"

artifact_prefix=
if [ "$test_mode" = 1 ]; then
  artifact_prefix=TESTONLY-
fi
community_suffix=
if [ "$community" -eq 1 ]; then
  community_suffix=-community
fi
artifact_stem="${artifact_prefix}Harness-Harlot-${version}-b${build}-macos-${architecture}${community_suffix}"
distribution_directory="$repository_root/target/release-dist/$artifact_stem"
rm -rf "$distribution_directory"
mkdir -p "$distribution_directory"
dmg="$distribution_directory/$artifact_stem.dmg"
dmg_root="$repository_root/target/release-dmg-root"
rm -rf "$dmg_root"
mkdir -p "$dmg_root"
ditto "$app_directory" "$dmg_root/Harness Harlot.app"
cp "$update_tool" "$dmg_root/hh-update-tool"
hdiutil create -quiet -volname "Harness Harlot" -srcfolder "$dmg_root" -format UDZO -ov "$dmg"
rm -rf "$dmg_root"
if [ "$test_mode" = 1 ] || [ "$community" -eq 1 ]; then
  codesign --force --sign - "$dmg"
else
  codesign --force --timestamp --sign "$HH_CODESIGN_IDENTITY" "$dmg"
  xcrun notarytool submit "$dmg" --keychain-profile "$HH_NOTARY_PROFILE" --wait
  xcrun stapler staple "$dmg"
  xcrun stapler validate "$dmg"
fi

artifact_name=$(basename "$dmg")
artifact_size=$(stat -f %z "$dmg")
artifact_sha256=$(shasum -a 256 "$dmg" | sed 's/[[:space:]].*$//')
manifest="$distribution_directory/$artifact_stem.update.json"
signature="$manifest.sig"
published_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
valid_until=$(date -u -v+7d '+%Y-%m-%dT%H:%M:%SZ')
cat > "$manifest" <<EOF
{
  "schema": "hh-update-manifest-v2",
  "product": "Harness Harlot",
  "channel": "stable",
  "key_id": "$key_id",
  "version": "$version",
  "build": $build,
  "published_at": "$published_at",
  "valid_until": "$valid_until",
  "platform": "macos",
  "minimum_macos": "13.0",
  "session_service": {
    "protocol_version": $protocol_version,
    "requires_quiescent_service": true
  },
  "artifacts": [
    {
      "platform": "macos",
      "architecture": "$architecture",
      "format": "dmg",
      "file_name": "$artifact_name",
      "url": "$base_url/$artifact_name",
      "sha256": "$artifact_sha256",
      "size": $artifact_size
    }
  ]
}
EOF

"$repository_root/target/release/hh-release-sign" sign \
  --manifest "$manifest" --signature "$signature" \
  --private-key "$HH_UPDATE_SIGNING_KEY_FILE"
if [ "$community" -eq 1 ]; then
  stable_manifest="$distribution_directory/manifest-macos-community-${architecture}.update.json"
else
  stable_manifest="$distribution_directory/manifest-macos-${architecture}.update.json"
fi
cp "$manifest" "$stable_manifest"
cp "$signature" "$stable_manifest.sig"
if [ "$test_mode" = 1 ]; then
  "$verification_tool" verify \
    --key-id "$key_id" --public-key "$HH_UPDATE_PUBLIC_KEY" --host "$update_host" \
    --manifest "$manifest" --signature "$signature" --artifact "$dmg" --fixture
else
  "$repository_root/target/release/hh-update-tool" verify-trusted \
    --manifest "$manifest" --signature "$signature" --artifact "$dmg"
fi
plutil -lint "$plist"

if [ "$test_mode" = 1 ]; then
  HH_FIXTURE_UPDATE_TOOL="$fixture_update_tool" \
    "$repository_root/scripts/verify-macos-release.sh" --fixture \
      com.harnessharlot.desktop "$update_host" "$key_id" "$HH_UPDATE_PUBLIC_KEY" "$manifest" "$signature"
elif [ "$community" -eq 1 ]; then
  "$repository_root/scripts/verify-macos-release.sh" --community \
    com.harnessharlot.desktop "$manifest" "$signature"
else
  "$repository_root/scripts/verify-macos-release.sh" \
    "$HH_EXPECTED_TEAM_ID" com.harnessharlot.desktop "$manifest" "$signature"
fi

printf '%s\n' "$distribution_directory"
