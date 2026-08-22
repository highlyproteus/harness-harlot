#!/bin/sh
set -eu
unset HH_SOCKET HH_STATE_DIR HH_CONFIG HH_PANE_ID HH_DEVELOPMENT_BUILD \
  HH_INSTALLER_APPLICATIONS_DIR || true
unset TAR_OPTIONS GZIP BZIP2 XZ_OPT || true

RELEASE_INDEX_URL='https://harnessharlot.com/releases/stable-linux.json'

usage() {
  cat <<'EOF'
Harness Harlot installer for Linux

Usage:
  curl -fsS https://harnessharlot.com/install | sh

Options:
  --verify-only   Verify the release without installing it
  --verbose       Show download and verification command output
  -h, --help      Show this help
EOF
}

[ "$(uname -s)" = Linux ] || { echo "Linux is required" >&2; exit 1; }
[ "$(id -u)" -ne 0 ] || { echo "refusing to install as root" >&2; exit 1; }
for command in curl python3 sha256sum tar wc; do
  command -v "$command" >/dev/null 2>&1 || { echo "$command is required" >&2; exit 1; }
done

verify_only=0
verbose=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --verify-only) verify_only=1; shift ;;
    --verbose) verbose=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

case "$(uname -m)" in
  aarch64 | arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported Linux architecture: $(uname -m)" >&2; exit 1 ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/hh-linux-install.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

run_quiet() {
  label=$1
  shift
  printf '→ %s\n' "$label"
  if [ "$verbose" -eq 1 ]; then
    "$@"
  else
    "$@" >/dev/null
  fi
  printf '✓ %s\n' "$label"
}

index="$work/stable-linux.json"
run_quiet "Fetch the release index" \
  curl --max-filesize 1048576 --proto '=https' --tlsv1.2 -fsS "$RELEASE_INDEX_URL" -o "$index"

selection="$work/selection"
python3 -I - "$index" "$architecture" > "$selection" <<'PY'
import json, re, sys
from pathlib import PurePosixPath
from urllib.parse import urlparse

index_path, architecture = sys.argv[1:]
with open(index_path, encoding="utf-8") as source:
    index = json.load(source)
if index.get("schema") != "hh-web-release-index-v1":
    raise SystemExit("unsupported release index schema")
tag = index.get("tag")
version = index.get("version")
build = index.get("build")
if not isinstance(tag, str) or not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
    raise SystemExit("invalid release tag")
if version != tag[1:] or not isinstance(build, int) or isinstance(build, bool) or build <= 0:
    raise SystemExit("inconsistent release identity")
entry = index.get("linux", {}).get(architecture)
if not isinstance(entry, dict):
    raise SystemExit(f"release has no Linux {architecture} package")
expected = {
    "archive": f"Harness-Harlot-{version}-b{build}-linux-{architecture}.tar.gz",
    "manifest": f"manifest-linux-{architecture}.update.json",
    "signature": f"manifest-linux-{architecture}.update.json.sig",
}
values = [tag, version, str(build)]
maximum_sizes = {
    "archive": 2 * 1024 * 1024 * 1024,
    "manifest": 1024 * 1024,
    "signature": 4 * 1024,
}
for kind in ("archive", "manifest", "signature"):
    asset = entry.get(kind)
    if not isinstance(asset, dict):
        raise SystemExit(f"missing {kind} metadata")
    url = asset.get("url")
    digest = asset.get("sha256")
    size = asset.get("size")
    if not isinstance(url, str):
        raise SystemExit(f"invalid {kind} URL")
    parsed = urlparse(url)
    expected_prefix = f"/highlyproteus/harness-harlot/releases/download/{tag}/"
    if parsed.scheme != "https" or parsed.netloc != "github.com" or not parsed.path.startswith(expected_prefix):
        raise SystemExit(f"untrusted {kind} URL")
    if PurePosixPath(parsed.path).name != expected[kind]:
        raise SystemExit(f"unexpected {kind} filename")
    if not isinstance(digest, str) or not re.fullmatch(r"[a-f0-9]{64}", digest):
        raise SystemExit(f"invalid {kind} digest")
    if not isinstance(size, int) or isinstance(size, bool) or size <= 0 or size > maximum_sizes[kind]:
        raise SystemExit(f"invalid {kind} size")
    values.extend((url, digest, str(size)))
print("\t".join(values))
PY

tab=$(printf '	')
IFS=$tab read -r tag version build \
  archive_url archive_sha256 archive_size \
  manifest_url manifest_sha256 manifest_size \
  signature_url signature_sha256 signature_size < "$selection"
[ -n "$signature_size" ] || { echo "release index is incomplete" >&2; exit 1; }
printf '\nHarness Harlot %s build %s for Linux %s\n\n' "$version" "$build" "$architecture"

archive="$work/$(basename "$archive_url")"
manifest="$work/$(basename "$manifest_url")"
signature="$work/$(basename "$signature_url")"
run_quiet "Download Harness Harlot $version" \
  curl --max-filesize "$archive_size" --proto '=https' --proto-redir '=https' --tlsv1.2 -fL "$archive_url" -o "$archive"
run_quiet "Download the update manifest" \
  curl --max-filesize "$manifest_size" --proto '=https' --proto-redir '=https' --tlsv1.2 -fL "$manifest_url" -o "$manifest"
run_quiet "Download the update signature" \
  curl --max-filesize "$signature_size" --proto '=https' --proto-redir '=https' --tlsv1.2 -fL "$signature_url" -o "$signature"

verify_file_checksum() {
  label=$1
  file=$2
  expected_sha256=$3
  expected_size=$4
  actual=$(sha256sum "$file" | cut -d' ' -f1)
  [ "$actual" = "$expected_sha256" ] || { echo "$label checksum mismatch" >&2; return 1; }
  actual_size=$(wc -c < "$file" | tr -d ' ')
  [ "$actual_size" = "$expected_size" ] || { echo "$label size mismatch" >&2; return 1; }
}

verify_release_checksums() {
  verify_file_checksum archive "$archive" "$archive_sha256" "$archive_size"
  verify_file_checksum manifest "$manifest" "$manifest_sha256" "$manifest_size"
  verify_file_checksum signature "$signature" "$signature_sha256" "$signature_size"
}
run_quiet "Verify release checksums" verify_release_checksums

python3 -I - "$archive" <<'PY'
import posixpath
import sys
import tarfile

path = sys.argv[1]
seen = set()
unpacked_bytes = 0
entry_count = 0
with tarfile.open(path, mode="r:gz") as release:
    for member in release:
        entry_count += 1
        if entry_count > 100000:
            raise SystemExit("release archive contains too many entries")
        name = member.name.rstrip("/")
        if not name or any(ord(character) < 32 or ord(character) == 127 for character in name):
            raise SystemExit("release archive contains an invalid path")
        if name != "Harness-Harlot" and not name.startswith("Harness-Harlot/"):
            raise SystemExit(f"release archive contains a path outside Harness-Harlot: {name}")
        if posixpath.normpath(name) != name or name.startswith("/"):
            raise SystemExit(f"release archive contains an unsafe path: {name}")
        if name in seen:
            raise SystemExit(f"release archive contains a duplicate path: {name}")
        seen.add(name)
        if member.isfile():
            unpacked_bytes += member.size
            if unpacked_bytes > 1024 * 1024 * 1024:
                raise SystemExit("release archive exceeds unpacked size limit")
        elif not member.isdir():
            raise SystemExit(f"release archive contains a link or special file: {name}")
PY

extract="$work/extract"
mkdir -p "$extract"
tar -xzf "$archive" -C "$extract"
package="$extract/Harness-Harlot"
update_tool="$package/bin/hh-update-tool"
[ -x "$update_tool" ] || { echo "release package has no updater verifier" >&2; exit 1; }
[ -x "$package/install.sh" ] || { echo "release package has no installer" >&2; exit 1; }
run_quiet "Verify signed update manifest" \
  "$update_tool" verify-trusted --manifest "$manifest" --signature "$signature" --artifact "$archive"

if [ "$verify_only" -eq 1 ]; then
  echo "verified Harness Harlot Linux release $tag for $architecture"
  exit 0
fi
"$package/install.sh"
echo "installed Harness Harlot for Linux $architecture"
