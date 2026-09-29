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
#   * The updater only offers a strictly newer version, so HEAD's workspace
#     version must be greater than PREVIOUS_TAG's. Before the release bump it is
#     equal; pass --next-version X.Y.Z to build HEAD with that version applied
#     (Cargo.toml + hh-* entries in Cargo.lock) in a temporary worktree.
#   * Fixture manifests are served from example.invalid, which never resolves. To
#     exercise the update, install the old DMG's app, then run the NEW build's
#     fixture updater (target/fixture-updater/release/hh-update-tool in the HEAD
#     build) with `install --fixture --community --key-id test-only-v1
#     --public-key <key> --host updates.example.invalid --manifest ... --signature
#     ... --artifact <new dmg> --current-version <old>` as scripts/test-update-tool.sh does.
#
# Old fixtures are cached under
#   ${HH_FIXTURE_CACHE:-$HOME/Library/Caches/harness-harlot/upgrade-fixtures}/<tag>/<arch>/
# and reused if present. Both builds run in temporary detached git worktrees
# (removed afterwards); their target/ is seeded by a copy-on-write clone from
# this checkout's target/release, so dependency compilation is reused.
# macOS only.
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

# build_in_worktree REV OUTPUT_VAR_FILE [NEXT_VERSION]: builds a community test
# fixture at REV in a temporary worktree and writes its distribution dir to $2.
build_in_worktree() {
  rev=$1
  result=$2
  bump=${3:-}
  tree="$scratch/$(basename "$result")-tree"
  git -C "$repository_root" worktree add --detach --quiet "$tree" "$rev"
  worktrees="$worktrees $tree"
  if [ -d "$repository_root/target/release" ]; then
    mkdir -p "$tree/target"
    /bin/cp -cR "$repository_root/target/release" "$tree/target/release"
    [ -d "$repository_root/target/fixture-updater" ] &&
      /bin/cp -cR "$repository_root/target/fixture-updater" "$tree/target/fixture-updater"
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
      HH_RELEASE_BUILD=1 \
      HH_ALLOW_DIRTY_TEST_PACKAGE=1 \
      HH_UPDATE_SIGNING_KEY_FILE="$key" \
      HH_UPDATE_PUBLIC_KEY="$public_key" \
      ./scripts/package-macos-release.sh "$version" 1 --community | sed -n '$p'
    )
    printf '%s\n' "$distribution" > "$result"
  )
}

# 1. previous version (cached)
previous_cache="$cache_root/$previous_tag/$architecture"
if ls "$previous_cache"/*.dmg >/dev/null 2>&1; then
  echo "using cached $previous_tag fixture: $previous_cache"
else
  echo "building $previous_tag fixture (not cached)"
  build_in_worktree "refs/tags/$previous_tag" "$scratch/previous.path"
  built=$(cat "$scratch/previous.path")
  mkdir -p "$previous_cache.partial"
  cp -R "$built"/. "$previous_cache.partial/"
  rm -rf "$previous_cache"
  mv "$previous_cache.partial" "$previous_cache"
fi

# 2. current HEAD
echo "building HEAD fixture${next_version:+ as version $next_version}"
build_in_worktree HEAD "$scratch/head.path" "$next_version"
head_built=$(cat "$scratch/head.path")
head_out="$repository_root/target/upgrade-fixtures/head/$architecture"
rm -rf "$head_out"
mkdir -p "$head_out"
cp -R "$head_built"/. "$head_out/"

echo
echo "update public key: $public_key   (key id test-only-v1, host updates.example.invalid)"
for dir in "$previous_cache" "$head_out"; do
  echo "$(basename "$(dirname "$dir")") fixture:"
  for f in "$dir"/*.dmg "$dir"/manifest-macos-community-*-v2.update.json "$dir"/manifest-macos-community-*-v2.update.json.sig; do
    [ -e "$f" ] && echo "  $f"
  done
done
