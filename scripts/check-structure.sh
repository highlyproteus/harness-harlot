#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

limit=1550
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-structure.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

files="$work/files"
stats="$work/stats"
violations="$work/violations"
git ls-files -- '*.rs' > "$files"
: > "$stats"
: > "$violations"

checked=0
while IFS= read -r file; do
  [ -n "$file" ] || continue
  [ -f "$file" ] || continue
  lines=$(wc -l < "$file" | tr -d ' ')
  checked=$((checked + 1))
  printf '%s\t%s\n' "$lines" "$file" >> "$stats"
  if [ "$lines" -gt "$limit" ]; then
    printf '%s: %s lines (limit %s)\n' "$file" "$lines" "$limit" >> "$violations"
  fi
done < "$files"

if [ "$checked" -eq 0 ]; then
  echo 'structure check found no tracked Rust files' >&2
  exit 1
fi

largest=$(sort -nr -k1,1 "$stats" | sed -n '1p')
largest_lines=$(printf '%s\n' "$largest" | cut -f1)
largest_file=$(printf '%s\n' "$largest" | cut -f2-)
printf 'structure: checked %s tracked Rust files; largest %s (%s lines); limit %s\n' \
  "$checked" "$largest_file" "$largest_lines" "$limit"

if [ -s "$violations" ]; then
  cat "$violations" >&2
  exit 1
fi
