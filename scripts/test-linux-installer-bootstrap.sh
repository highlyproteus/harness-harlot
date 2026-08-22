#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-linux-bootstrap-test.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

mkdir -p "$work/package/Harness-Harlot/bin" "$work/mock-bin"
cat > "$work/package/Harness-Harlot/bin/hh-update-tool" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" > "${HH_TEST_VERIFY_LOG:?}"
EOF
cat > "$work/package/Harness-Harlot/install.sh" <<'EOF'
#!/bin/sh
set -eu
: > "${HH_TEST_INSTALL_LOG:?}"
EOF
chmod 755 "$work/package/Harness-Harlot/bin/hh-update-tool" "$work/package/Harness-Harlot/install.sh"
archive_name='Harness-Harlot-9.8.7-b42-linux-x86_64.tar.gz'
archive="$work/$archive_name"
tar -czf "$archive" -C "$work/package" Harness-Harlot
manifest_name='manifest-linux-x86_64.update.json'
signature_name="$manifest_name.sig"
printf '{"fixture":true}\n' > "$work/$manifest_name"
printf 'fixture-signature\n' > "$work/$signature_name"

sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
size() { wc -c < "$1" | tr -d ' '; }
write_index() {
  archive_digest=$1
  cat > "$work/stable-linux.json" <<EOF
{
  "schema": "hh-web-release-index-v1",
  "tag": "v9.8.7",
  "version": "9.8.7",
  "build": 42,
  "linux": {
    "x86_64": {
      "archive": {"url":"https://github.com/highlyproteus/harness-harlot/releases/download/v9.8.7/$archive_name","sha256":"$archive_digest","size":$(size "$archive")},
      "manifest": {"url":"https://github.com/highlyproteus/harness-harlot/releases/download/v9.8.7/$manifest_name","sha256":"$(sha256 "$work/$manifest_name")","size":$(size "$work/$manifest_name")},
      "signature": {"url":"https://github.com/highlyproteus/harness-harlot/releases/download/v9.8.7/$signature_name","sha256":"$(sha256 "$work/$signature_name")","size":$(size "$work/$signature_name")},
      "manifest_published_at":"2026-08-22T00:00:00Z",
      "manifest_valid_until":"2026-08-29T00:00:00Z"
    }
  }
}
EOF
}
write_index "$(sha256 "$archive")"

cat > "$work/mock-bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s|'') echo Linux ;;
  -m) echo x86_64 ;;
  *) exit 2 ;;
esac
EOF
cat > "$work/mock-bin/id" <<'EOF'
#!/bin/sh
[ "${1:-}" = -u ] && { echo 501; exit 0; }
exec /usr/bin/id "$@"
EOF
cat > "$work/mock-bin/sha256sum" <<'EOF'
#!/bin/sh
shasum -a 256 "$1"
EOF
cat > "$work/mock-bin/curl" <<'EOF'
#!/bin/sh
set -eu
output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) output=$2; shift 2 ;;
    --proto|--proto-redir|--tlsv1.2) if [ "$1" = --tlsv1.2 ]; then shift; else shift 2; fi ;;
    -*) shift ;;
    http*) url=$1; shift ;;
    *) shift ;;
  esac
done
[ -n "$output" ] && [ -n "$url" ]
case "$url" in
  https://harnessharlot.com/releases/stable-linux.json) source=$HH_TEST_INDEX ;;
  https://github.com/*) source="$HH_TEST_ASSET_DIR/${url##*/}" ;;
  *) exit 22 ;;
esac
cp "$source" "$output"
EOF
chmod 755 "$work/mock-bin"/*

HH_TEST_INDEX="$work/stable-linux.json" \
HH_TEST_ASSET_DIR="$work" \
HH_TEST_VERIFY_LOG="$work/verified" \
HH_TEST_INSTALL_LOG="$work/installed" \
PATH="$work/mock-bin:$PATH" \
  "$repository_root/install-linux.sh" --verify-only > "$work/output"
grep -F 'verified Harness Harlot Linux release v9.8.7 for x86_64' "$work/output" >/dev/null
grep -F 'verify-trusted --manifest' "$work/verified" >/dev/null
[ ! -e "$work/installed" ]

write_index "$(printf '0%.0s' $(seq 1 64))"
if HH_TEST_INDEX="$work/stable-linux.json" \
   HH_TEST_ASSET_DIR="$work" \
   HH_TEST_VERIFY_LOG="$work/unexpected-verified" \
   HH_TEST_INSTALL_LOG="$work/unexpected-installed" \
   PATH="$work/mock-bin:$PATH" \
     "$repository_root/install-linux.sh" --verify-only > "$work/bad.out" 2>&1; then
  echo "Linux installer accepted a bad website checksum" >&2
  exit 1
fi
grep -F 'archive checksum mismatch' "$work/bad.out" >/dev/null
[ ! -e "$work/unexpected-verified" ]

echo "Linux bootstrap verifies the website index and refuses checksum mismatches"
