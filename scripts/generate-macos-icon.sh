#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 SOURCE_SQUARE_PNG OUTPUT_ICNS" >&2
  exit 2
fi

source_image=$1
output_icns=$2

if [ ! -f "$source_image" ]; then
  echo "source image does not exist: $source_image" >&2
  exit 2
fi

width=$(sips -g pixelWidth "$source_image" | awk '/pixelWidth/ { print $2 }')
height=$(sips -g pixelHeight "$source_image" | awk '/pixelHeight/ { print $2 }')
if [ "$width" != "$height" ]; then
  echo "source image must be square, got ${width}x${height}" >&2
  exit 2
fi

output_directory=$(dirname "$output_icns")
iconset=$(mktemp -d "${TMPDIR:-/tmp}/nah-iconset.XXXXXX.iconset")
cleanup() {
  rm -rf "$iconset"
}
trap cleanup EXIT HUP INT TERM

make_icon() {
  size=$1
  name=$2
  sips -s format png -z "$size" "$size" "$source_image" --out "$iconset/$name" >/dev/null
}

make_icon 16 icon_16x16.png
make_icon 32 icon_16x16@2x.png
make_icon 32 icon_32x32.png
make_icon 64 icon_32x32@2x.png
make_icon 128 icon_128x128.png
make_icon 256 icon_128x128@2x.png
make_icon 256 icon_256x256.png
make_icon 512 icon_256x256@2x.png
make_icon 512 icon_512x512.png
make_icon 1024 icon_512x512@2x.png

mkdir -p "$output_directory"
iconutil --convert icns --output "$output_icns" "$iconset"
echo "$output_icns"
