#!/bin/sh
set -eu

profile="${1:-debug}"
case "$profile" in
  debug)
    cargo build -p rust-mux-desktop --bin not-a-harness
    ;;
  release)
    cargo build --release -p rust-mux-desktop --bin not-a-harness
    ;;
  *)
    echo "usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app_directory="$repository_root/target/$profile/Not a Harness.app"
contents_directory="$app_directory/Contents"

mkdir -p "$contents_directory/MacOS"
cp "$repository_root/packaging/macos/Info.plist" "$contents_directory/Info.plist"
cp "$repository_root/target/$profile/not-a-harness" \
  "$contents_directory/MacOS/not-a-harness"
chmod 755 "$contents_directory/MacOS/not-a-harness"

echo "$app_directory"
