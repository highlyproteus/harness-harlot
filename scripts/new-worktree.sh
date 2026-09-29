#!/bin/sh
# Create a git worktree next to the main checkout and seed its target/ from the
# main checkout's target/ with a copy-on-write clone, so the first build in the
# new worktree reuses compiled dependencies instead of starting cold.
#
#   scripts/new-worktree.sh NAME BRANCH [BASE]
#
# Creates <main checkout>/../worktrees/NAME on new branch BRANCH from BASE
# (default: main). Each worktree keeps its own target/ (no shared CARGO_TARGET_DIR).
# target/release-dist is not copied.
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  echo "usage: $0 NAME BRANCH [BASE]" >&2
  exit 2
fi
name=$1
branch=$2
base=${3:-main}
case "$name" in
  '' | */* | .*) echo "NAME must be a plain directory name" >&2; exit 2 ;;
esac

start=$(date +%s)
main_checkout=$(git worktree list --porcelain | sed -n '1s/^worktree //p')
[ -n "$main_checkout" ] || { echo "cannot locate the main worktree" >&2; exit 1; }
destination="$main_checkout/../worktrees/$name"
mkdir -p "$main_checkout/../worktrees"

git -C "$main_checkout" worktree add "$destination" -b "$branch" "$base"
destination=$(CDPATH='' cd -- "$destination" && pwd)

source_target="$main_checkout/target"
if [ -d "$source_target" ]; then
  mkdir -p "$destination/target"
  case "$(uname -s)" in
    Darwin) clone='/bin/cp -cR' ;; # APFS clonefile; GNU cp in PATH lacks -c
    *) clone='cp -R --reflink=auto' ;;
  esac
  for entry in "$source_target"/* "$source_target"/.[!.]*; do
    [ -e "$entry" ] || continue
    [ "$(basename "$entry")" = release-dist ] && continue
    # shellcheck disable=SC2086 # $clone is a fixed command with flags
    $clone "$entry" "$destination/target/"
  done
  echo "seeded $destination/target from $source_target"
else
  echo "no $source_target to seed from; first build will be cold"
fi

echo "worktree ready: $destination (branch $branch)"
echo "elapsed: $(($(date +%s) - start))s"
