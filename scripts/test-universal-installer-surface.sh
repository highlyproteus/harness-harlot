#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
readme="$repository_root/README.md"
macos_installer="$repository_root/install-community-macos.sh"
linux_installer="$repository_root/install-linux.sh"
command='curl -fsS https://harnessharlot.com/install | sh'

install_line=$(grep -n '^## Install$' "$readme" | cut -d: -f1)
workstations_line=$(grep -n '^## Workstations$' "$readme" | cut -d: -f1)
if ! { [ -n "$install_line" ] && [ -n "$workstations_line" ] && [ "$install_line" -lt "$workstations_line" ]; }; then
  echo "README Install section must appear above Workstations" >&2
  exit 1
fi
[ "$(grep -Fxc "$command" "$readme")" -eq 1 ] || {
  echo "README must present exactly one universal install command" >&2
  exit 1
}
if grep -Eq "curl --proto|### macOS|### Linux$|GitHub CLI \(gh\) is required" "$readme"; then
  echo "README still advertises platform-specific or verbose bootstrap instructions" >&2
  exit 1
fi

grep -F "$command" "$macos_installer" >/dev/null
if grep -Eq 'command -v gh|gh release|gh attestation|--tag' "$linux_installer"; then
  echo "Linux installer still depends on GitHub CLI or historical tag selection" >&2
  exit 1
fi
grep -F "RELEASE_INDEX_URL='https://harnessharlot.com/releases/stable-linux.json'" "$linux_installer" >/dev/null
grep -F 'https://harnessharlot.com/releases/stable-v2/' "$linux_installer" >/dev/null
grep -F 'https://harnessharlot.com/releases/stable-v2/' "$macos_installer" >/dev/null
grep -F 'https://github.com/highlyproteus/harness-harlot/releases/download/' "$linux_installer" >/dev/null
grep -F "https://github.com/\$REPOSITORY/releases/download/" "$macos_installer" >/dev/null
grep -F "python3 -I - \"\$index\" \"\$architecture\"" "$linux_installer" >/dev/null
grep -F 'unset TAR_OPTIONS GZIP BZIP2 XZ_OPT' "$linux_installer" >/dev/null
grep -F "\"\$update_tool\" verify-trusted" "$linux_installer" >/dev/null
grep -F -- "--manifest \"\$manifest\" --signature \"\$signature\" --artifact \"\$archive\"" "$linux_installer" >/dev/null
grep -F "actual=\$(sha256sum \"\$file\"" "$linux_installer" >/dev/null
sh -n "$macos_installer"
sh -n "$linux_installer"

echo "universal macOS/Linux installer surface is consistent"
