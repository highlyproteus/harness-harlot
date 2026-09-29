#!/bin/sh
# Run the exact CI gate locally, in CI order, failing fast between stages:
#   1. scripts/check-structure.sh
#   2. cargo fmt --all --check
#   3. cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
#   4. cargo test  --locked --workspace --all-targets --all-features --no-fail-fast
#   5. shellcheck scripts/*.sh install*.sh   (Security workflow; skipped with a warning if missing)
# Prints per-stage wall time and the total.
#
# Limits: this only compiles what the current OS/arch compiles. Code behind a
# cfg(target_os = "linux") (or other non-host cfg) can still fail only in CI.
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

summary=''
total_start=$(date +%s)

record() { summary="$summary
$(printf '  %-12s %4ss  %s' "$1" "$2" "$3")"; }

print_summary() {
  echo
  echo "preflight summary:$summary"
  echo "  total        $(($(date +%s) - total_start))s"
}

stage() {
  name=$1
  shift
  echo
  echo "==> $name: $*"
  start=$(date +%s)
  if "$@"; then
    record "$name" "$(($(date +%s) - start))" ok
  else
    record "$name" "$(($(date +%s) - start))" FAILED
    print_summary
    echo "preflight FAILED at stage: $name" >&2
    exit 1
  fi
}

stage structure scripts/check-structure.sh
stage fmt cargo fmt --all --check
stage clippy cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
stage test cargo test --locked --workspace --all-targets --all-features --no-fail-fast

if command -v shellcheck >/dev/null 2>&1; then
  stage shellcheck sh -c 'shellcheck scripts/*.sh install*.sh'
else
  echo "warning: shellcheck not installed; skipping (CI's Security workflow runs it)" >&2
  record shellcheck 0 skipped
fi

print_summary
echo "preflight passed"
