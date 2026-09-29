#!/bin/sh
# Build the upgrade-smoke fixture pair: a PREVIOUS_TAG community DMG + manifest
# and a current-HEAD community DMG + manifest, both TEST-ONLY artifacts.
#
#   scripts/build-upgrade-fixtures.sh PREVIOUS_TAG [--next-version X.Y.Z]
#
# How the pair fits together (see scripts/package-macos-release.sh):
#   * HH_RELEASE_TEST_MODE=1 packages with an ad-hoc codesign identity, the key id
#     test-only-v1 and the host updates.example.invalid.
#   * Each build embeds the update public key, key id and host it was built with.
#     An old build therefore trusts a new manifest only if the new manifest is
#     signed by the SAME key, for the same key id and host. Both fixtures are
#     built with one fixed test key (stored in the cache directory, so cached old
#     fixtures stay compatible with later runs) and identical key id / host.
#   * The updater only offers a strictly newer version AND a strictly higher
#     build number, so each fixture's build is `git rev-list --count` of its
#     commit (as in the release workflow); HEAD must descend from PREVIOUS_TAG.
#     HEAD's workspace version must be greater than PREVIOUS_TAG's. Before the
#     release bump it is equal; pass --next-version X.Y.Z to build HEAD with that
#     version applied (Cargo.toml + hh-* entries in Cargo.lock) in a temporary
#     worktree.
#   * Each output directory keeps the fixture updater of that build as
#     hh-update-tool-fixture (the DMG's own updater is the production one).
#   * The script then proves the pair works: PREVIOUS_TAG's fixture updater must
#     report the HEAD fixture as an available update and install it over the old
#     app in a scratch prefix (real hdiutil and codesign; `open` is stubbed so
#     nothing launches). Use the printed commands to repeat that against your
#     own install when doing the manual smoke.
#
# Old fixtures are cached under
#   ${HH_FIXTURE_CACHE:-$HOME/Library/Caches/harness-harlot/upgrade-fixtures}/<tag>/<arch>/
# and reused if present and built with the expected build number. Both builds
# run in temporary detached git worktrees (removed afterwards); their target/ is
# seeded by a copy-on-write clone from this checkout's target/release, so
# dependency compilation is reused. macOS only.
set -eu

if [ "$(uname -s)" != Darwin ]; then
  echo "build-upgrade-fixtures.sh only runs on macOS (the fixtures are macOS DMGs)" >&2
  exit 1
fi

usage() {
  echo "usage: $0 PREVIOUS_TAG [--next-version X.Y.Z]" >&2
  exit 2
}
[ "$#" -ge 1 ] || usage
previous_tag=$1
shift
next_version=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --next-version) [ "$#" -ge 2 ] || usage; next_version=$2; shift 2 ;;
    *) usage ;;
  esac
done
case "$previous_tag" in
  '' | *[!0-9A-Za-z._-]*) echo "invalid tag name: $previous_tag" >&2; exit 2 ;;
esac

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"
git rev-parse --verify --quiet "refs/tags/$previous_tag^{commit}" >/dev/null || {
  echo "tag $previous_tag not found locally (git fetch --tags)" >&2
  exit 1
}
case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
esac

cache_root=${HH_FIXTURE_CACHE:-$HOME/Library/Caches/harness-harlot/upgrade-fixtures}
mkdir -p "$cache_root"
key="$cache_root/test-update-key"
if [ ! -f "$key" ]; then
  ( umask 077; printf '********************************' | base64 > "$key" )
fi
chmod 600 "$key"

cargo build --locked --release -p hh-release-signer --bin hh-release-sign
public_key=$("$repository_root/target/release/hh-release-sign" public-key --private-key "$key")

scratch=$(mktemp -d "${TMPDIR:-/tmp}/hh-upgrade-fixtures.XXXXXX")
worktrees=''
cleanup() {
  for tree in $worktrees; do
    git -C "$repository_root" worktree remove --force "$tree" >/dev/null 2>&1 || true
  done
  rm -rf "$scratch"
  git -C "$repository_root" worktree prune
}
trap cleanup EXIT HUP INT TERM

# build_number REV: the release workflow's build number for REV.
build_number() {
  git -C "$repository_root" rev-list --count "$1"
}

# build_in_worktree REV OUTPUT_DIR BUILD [NEXT_VERSION]: builds a community test
# fixture at REV in a temporary worktree and copies its distribution directory
# plus its fixture updater (as hh-update-tool-fixture) into OUTPUT_DIR.
build_in_worktree() {
  rev=$1
  output=$2
  build=$3
  bump=${4:-}
  tree="$scratch/tree-$build"
  git -C "$repository_root" worktree add --detach --quiet "$tree" "$rev"
  worktrees="$worktrees $tree"
  if [ -d "$repository_root/target/release" ]; then
    mkdir -p "$tree/target"
    /bin/cp -cR "$repository_root/target/release" "$tree/target/release"
    if [ -d "$repository_root/target/fixture-updater" ]; then
      /bin/cp -cR "$repository_root/target/fixture-updater" "$tree/target/fixture-updater"
    fi
  fi
  (
    cd "$tree"
    version=$(cargo metadata --locked --format-version 1 --no-deps |
      python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "hh-desktop"))')
    if [ -n "$bump" ]; then
      BUMP_FROM=$version BUMP_TO=$bump python3 - <<'PY'
import os, re
old, new = os.environ["BUMP_FROM"], os.environ["BUMP_TO"]
text = open("Cargo.toml").read()
text, count = re.subn(r'(?m)^version = "%s"$' % re.escape(old), 'version = "%s"' % new, text, count=1)
assert count == 1, "workspace version not found in Cargo.toml"
open("Cargo.toml", "w").write(text)
lock = open("Cargo.lock").read()
lock = re.sub(r'(name = "hh-[a-z0-9-]+"\nversion = ")%s(")' % re.escape(old), r'\g<1>%s\g<2>' % new, lock)
open("Cargo.lock", "w").write(lock)
PY
      version=$bump
    fi
    distribution=$(
      HH_RELEASE_TEST_MODE=1 \
      HH_ALLOW_DIRTY_TEST_PACKAGE=1 \
      HH_UPDATE_SIGNING_KEY_FILE="$key" \
      HH_UPDATE_PUBLIC_KEY="$public_key" \
      ./scripts/package-macos-release.sh "$version" "$build" --community | sed -n '$p'
    )
    rm -rf "$output.partial"
    mkdir -p "$output.partial"
    cp -R "$distribution"/. "$output.partial/"
    cp "$tree/target/fixture-updater/release/hh-update-tool" "$output.partial/hh-update-tool-fixture"
    rm -rf "$output"
    mv "$output.partial" "$output"
  )
}

# manifest_field DIR FIELD: a top-level field of DIR's alias manifest.
manifest_field() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' \
    "$1/manifest-macos-community-$architecture-v2.update.json" "$2"
}

previous_build=$(build_number "refs/tags/$previous_tag")
head_build=$(build_number HEAD)
if ! git -C "$repository_root" merge-base --is-ancestor "refs/tags/$previous_tag" HEAD ||
  [ "$head_build" -le "$previous_build" ]; then
  echo "HEAD must descend from $previous_tag (builds $head_build vs $previous_build)" >&2
  exit 2
fi

# 1. previous version (cached; rebuilt if the cache lacks the fixture updater or
#    was built with another build number)
previous_out="$cache_root/$previous_tag/$architecture"
if [ -x "$previous_out/hh-update-tool-fixture" ] &&
  [ "$(manifest_field "$previous_out" build 2>/dev/null)" = "$previous_build" ]; then
  echo "using cached $previous_tag fixture (build $previous_build): $previous_out"
else
  echo "building $previous_tag fixture as build $previous_build"
  build_in_worktree "refs/tags/$previous_tag" "$previous_out" "$previous_build"
fi

# 2. current HEAD
echo "building HEAD fixture as build $head_build${next_version:+, version $next_version}"
head_out="$repository_root/target/upgrade-fixtures/head/$architecture"
build_in_worktree HEAD "$head_out" "$head_build" "$next_version"

previous_version=$(manifest_field "$previous_out" version)
head_version=$(manifest_field "$head_out" version)
new_manifest="$head_out/manifest-macos-community-$architecture-v2.update.json"
new_dmg=$(ls "$head_out"/*.dmg)
old_dmg=$(ls "$previous_out"/*.dmg)
set -- --fixture --key-id test-only-v1 --public-key "$public_key" \
  --host updates.example.invalid --manifest "$new_manifest" \
  --signature "$new_manifest.sig" --artifact "$new_dmg" \
  --current-version "$previous_version" --current-build "$previous_build"

# 3. prove the old build accepts and installs the new one
check_output=$("$previous_out/hh-update-tool-fixture" check "$@")
expected="update available: $head_version build $head_build"
if [ "$check_output" != "$expected" ]; then
  echo "self-check failed: '$check_output' (expected '$expected')" >&2
  exit 1
fi
echo "check: $check_output"

smoke="$scratch/install"
mkdir -p "$smoke/home/Applications" "$smoke/mock-bin" "$smoke/mount"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$smoke/mount" "$old_dmg"
ditto "$smoke/mount/Harness Harlot.app" "$smoke/home/Applications/Harness Harlot.app"
hdiutil detach -quiet "$smoke/mount"
printf '#!/bin/sh\necho "$@" >> "%s"\n' "$smoke/open.log" > "$smoke/mock-bin/open"
chmod +x "$smoke/mock-bin/open"
HOME="$smoke/home" HH_SOCKET="$smoke/session.sock" PATH="$smoke/mock-bin:$PATH" \
  "$previous_out/hh-update-tool-fixture" install --community "$@" \
  --prefix "$smoke/home/Applications"
installed=$(plutil -extract CFBundleShortVersionString raw -o - \
  "$smoke/home/Applications/Harness Harlot.app/Contents/Info.plist")
installed_build=$(plutil -extract CFBundleVersion raw -o - \
  "$smoke/home/Applications/Harness Harlot.app/Contents/Info.plist")
if [ "$installed" != "$head_version" ] || [ "$installed_build" != "$head_build" ]; then
  echo "self-check failed: installed $installed build $installed_build" >&2
  exit 1
fi
echo "install: $previous_version build $previous_build -> $installed build $installed_build in a scratch prefix"

echo
echo "update public key: $public_key   (key id test-only-v1, host updates.example.invalid)"
for dir in "$previous_out" "$head_out"; do
  echo "$(basename "$(dirname "$dir")") fixture:"
  for f in "$dir"/*.dmg "$dir"/manifest-macos-community-*-v2.update.json \
    "$dir"/manifest-macos-community-*-v2.update.json.sig "$dir"/hh-update-tool-fixture; do
    [ -e "$f" ] && echo "  $f"
  done
done
echo
echo "to install the new fixture over a real old-fixture install:"
echo "  $previous_out/hh-update-tool-fixture install --community --fixture --key-id test-only-v1 \\"
echo "    --public-key $public_key --host updates.example.invalid \\"
echo "    --manifest $new_manifest --signature $new_manifest.sig --artifact $new_dmg \\"
echo "    --current-version $previous_version --current-build $previous_build"
