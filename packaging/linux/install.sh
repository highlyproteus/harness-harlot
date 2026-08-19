#!/bin/sh
set -eu

package_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$package_root/bin/hh-update-tool" install-local --source "$package_root" "$@"
