#!/bin/sh
set -eu

usage() {
  echo "usage: $0 BASE64_SEED_FILE EXPECTED_PUBLIC_KEY MANIFEST SIGNATURE" >&2
  exit 2
}

[ "$#" -eq 4 ] || usage
seed_file=$1
expected_public_key=$2
manifest=$3
signature=$4
openssl_command=${HH_OPENSSL:-openssl}
command -v "$openssl_command" >/dev/null 2>&1 || {
  echo "OpenSSL command is unavailable: $openssl_command" >&2
  exit 2
}

[ -f "$seed_file" ] && [ ! -L "$seed_file" ] || {
  echo "signing seed must be a regular non-symlink file" >&2
  exit 2
}
[ -f "$manifest" ] && [ ! -L "$manifest" ] || {
  echo "manifest must be a regular non-symlink file" >&2
  exit 2
}
[ -n "$expected_public_key" ] || {
  echo "expected public key is empty" >&2
  exit 2
}

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/hh-isolated-sign.XXXXXX")
cleanup() {
  rm -rf "$work"
}
trap cleanup EXIT HUP INT TERM
umask 077

"$openssl_command" base64 -d -A -in "$seed_file" -out "$work/seed.bin"
[ "$(wc -c < "$work/seed.bin" | tr -d ' ')" = 32 ] || {
  echo "Ed25519 seed must decode to exactly 32 bytes" >&2
  exit 2
}
printf '\060\056\002\001\000\060\005\006\003\053\145\160\004\042\004\040' > "$work/private.der"
cat "$work/seed.bin" >> "$work/private.der"
rm -f "$work/seed.bin"

"$openssl_command" pkey -inform DER -in "$work/private.der" -out "$work/private.pem" >/dev/null 2>&1
"$openssl_command" pkey -in "$work/private.pem" -pubout -out "$work/public.pem" >/dev/null 2>&1
derived_public_key=$(
  "$openssl_command" pkey -in "$work/private.pem" -pubout -outform DER 2>/dev/null |
    tail -c 32 |
    "$openssl_command" base64 -A
)
[ "$derived_public_key" = "$expected_public_key" ] || {
  echo "signing seed does not match the expected public key" >&2
  exit 2
}

"$openssl_command" pkeyutl -sign -inkey "$work/private.pem" -rawin \
  -in "$manifest" -out "$work/signature.bin"
"$openssl_command" pkeyutl -verify -pubin -inkey "$work/public.pem" -rawin \
  -in "$manifest" -sigfile "$work/signature.bin" >/dev/null
"$openssl_command" base64 -A -in "$work/signature.bin" -out "$signature"
printf '\n' >> "$signature"
chmod 0644 "$signature"
