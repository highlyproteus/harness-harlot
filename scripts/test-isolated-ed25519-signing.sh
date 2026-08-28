#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
target_directory=${CARGO_TARGET_DIR:-"$repository_root/target"}
case "$target_directory" in
  /*) ;;
  *) target_directory="$repository_root/$target_directory" ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-openssl-signing-fixture.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

seed="$work/seed"
printf '********************************' | base64 > "$seed"
chmod 0600 "$seed"
artifact="$work/artifact.tar.gz"
printf 'isolated signer fixture\n' > "$artifact"
sha256=$(shasum -a 256 "$artifact" | sed 's/[[:space:]].*$//')
size=$(stat -f %z "$artifact" 2>/dev/null || stat -c %s "$artifact")
manifest="$work/manifest.update.json"
cat > "$manifest" <<EOF
{
  "schema": "hh-update-manifest-v2",
  "product": "Harness Harlot",
  "channel": "stable",
  "key_id": "test-only-v1",
  "version": "99.0.0",
  "build": 1,
  "published_at": "2026-08-28T00:00:00Z",
  "valid_until": "2026-08-29T00:00:00Z",
  "platform": "linux",
  "minimum_glibc": "2.35",
  "session_service": {
    "protocol_version": 34,
    "requires_quiescent_service": true
  },
  "artifacts": [
    {
      "platform": "linux",
      "architecture": "x86_64",
      "format": "tar.gz",
      "file_name": "artifact.tar.gz",
      "sha256": "$sha256",
      "size": $size,
      "url": "https://updates.example.invalid/stable/artifact.tar.gz"
    }
  ]
}
EOF
signature="$manifest.sig"

cd "$repository_root"
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
cargo build --locked --release -p hh-updater --features fetch,fixture --bin hh-update-tool
public_key=$("$target_directory/release/hh-release-sign" public-key --private-key "$seed")
openssl_command=openssl
if command -v brew >/dev/null 2>&1; then
  candidate=$(brew --prefix openssl@3 2>/dev/null || true)
  if [ -x "$candidate/bin/openssl" ]; then
    openssl_command="$candidate/bin/openssl"
  fi
fi
HH_OPENSSL="$openssl_command" "$repository_root/scripts/isolated-ed25519-sign.sh" \
  "$seed" "$public_key" "$manifest" "$signature"
"$target_directory/release/hh-update-tool" verify \
  --fixture \
  --key-id test-only-v1 \
  --public-key "$public_key" \
  --host updates.example.invalid \
  --manifest "$manifest" \
  --signature "$signature" \
  --artifact "$artifact" \
  --channel stable >/dev/null

echo "OpenSSL isolated signer interoperates with the Rust update verifier"
