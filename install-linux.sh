#!/bin/sh
set -eu

REPOSITORY='highlyproteus/harness-harlot'

usage() {
  echo "usage: $0 [--tag vVERSION] [--verify-only]" >&2
  exit 2
}

[ "$(uname -s)" = Linux ] || { echo "Linux is required" >&2; exit 1; }
[ "$(id -u)" -ne 0 ] || { echo "refusing to install as root" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || {
  echo "GitHub CLI (gh) is required to verify release provenance" >&2
  exit 1
}

tag=
verify_only=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --tag) [ "$#" -ge 2 ] || usage; tag=$2; shift 2 ;;
    --verify-only) verify_only=1; shift ;;
    *) usage ;;
  esac
done

case "$(uname -m)" in
  aarch64 | arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported Linux architecture: $(uname -m)" >&2; exit 1 ;;
esac
if [ -z "$tag" ]; then
  tag=$(gh release view --repo "$REPOSITORY" --json tagName --jq .tagName)
fi
case "$tag" in
  v[0-9A-Za-z.-]*) ;;
  *) echo "invalid release tag: $tag" >&2; exit 2 ;;
esac
case "$tag" in *..* | *. | *[!0-9A-Za-z.v-]*) echo "invalid release tag: $tag" >&2; exit 2 ;; esac
version=${tag#v}

work=$(mktemp -d "${TMPDIR:-/tmp}/hh-linux-install.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM
artifact_pattern="Harness-Harlot-${version}-b*-linux-${architecture}.tar.gz"
gh release download "$tag" --repo "$REPOSITORY" --dir "$work" --pattern "$artifact_pattern"
set -- "$work"/$artifact_pattern
[ "$#" -eq 1 ] && [ -f "$1" ] || {
  echo "release must contain exactly one Linux package for $architecture" >&2
  exit 1
}
artifact=$1
gh attestation verify "$artifact" --repo "$REPOSITORY"

archive_list="$work/archive-list"
archive_details="$work/archive-details"
tar -tzf "$artifact" > "$archive_list"
tar -tvzf "$artifact" > "$archive_details"
while IFS= read -r entry; do
  normalized=${entry%/}
  case "$normalized" in
    Harness-Harlot | Harness-Harlot/*) ;;
    *) echo "release archive contains a path outside Harness-Harlot: $entry" >&2; exit 1 ;;
  esac
  case "/$normalized/" in
    */../* | */./*) echo "release archive contains an unsafe path: $entry" >&2; exit 1 ;;
  esac
done < "$archive_list"
if grep -E '^[^d-]' "$archive_details" >/dev/null; then
  echo "release archive contains a link or special file" >&2
  exit 1
fi

extract="$work/extract"
mkdir -p "$extract"
tar -xzf "$artifact" -C "$extract"
package="$extract/Harness-Harlot"
[ -x "$package/install.sh" ] || { echo "release package has no installer" >&2; exit 1; }
if [ "$verify_only" -eq 1 ]; then
  echo "verified Harness Harlot Linux release $tag for $architecture"
  exit 0
fi
"$package/install.sh"
echo "installed Harness Harlot for Linux $architecture"
