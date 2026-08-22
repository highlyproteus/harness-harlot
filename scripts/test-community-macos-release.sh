#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=$(
  cd "$repository_root"
  cargo metadata --locked --format-version 1 --no-deps |
    python3 -c 'import json,sys; print(next(package["version"] for package in json.load(sys.stdin)["packages"] if package["name"] == "hh-desktop"))'
)
case "$(uname -m)" in
  arm64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-community-release-fixture.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

sh -n "$repository_root/install-community-macos.sh"
grep -F 'unset HH_SOCKET HH_STATE_DIR' "$repository_root/install-community-macos.sh" >/dev/null
grep -F "RELEASE_INDEX_URL='https://harnessharlot.com/releases/stable-macos.json'" \
  "$repository_root/install-community-macos.sh" >/dev/null
if grep -F 'command -v gh' "$repository_root/install-community-macos.sh" >/dev/null || \
  grep -F 'gh release' "$repository_root/install-community-macos.sh" >/dev/null || \
  grep -F 'gh attestation' "$repository_root/install-community-macos.sh" >/dev/null; then
  echo "community installer still requires GitHub CLI" >&2
  exit 1
fi
grep -F "actual=\$(shasum -a 256 \"\$file\" | cut -d ' ' -f 1)" \
  "$repository_root/install-community-macos.sh" >/dev/null
grep -F "codesign --verify --deep --strict --verbose=2 \"\$candidate\" ||" \
  "$repository_root/install-community-macos.sh" >/dev/null
grep -F "ln -s \"\$previous_link_target\" \"\$link\"" \
  "$repository_root/install-community-macos.sh" >/dev/null
grep -F "if [ \"\$verify_only\" -eq 0 ]; then" \
  "$repository_root/install-community-macos.sh" >/dev/null
grep -F "preflight_app \"\$alternate_app\"" \
  "$repository_root/install-community-macos.sh" >/dev/null
"$repository_root/install-community-macos.sh" --help >"$work/help.out"
grep -F -- "curl --proto '=https' --tlsv1.2 -fsS https://harnessharlot.com/install | sh" \
  "$work/help.out" >/dev/null
grep -F -- "--verbose" "$work/help.out" >/dev/null
if grep -F -- "--tag" "$work/help.out" >/dev/null; then
  echo "community installer advertises unsupported historical release selection" >&2
  exit 1
fi
if grep -F -- "--acknowledge-unnotarized" "$work/help.out" >/dev/null; then
  echo "community installer still exposes the legacy acknowledgement flag" >&2
  exit 1
fi

mkdir -p "$work/home/Applications" "$work/system-apps" "$work/poisoned-apps"
mkdir -p "$work/home/Applications/Harness Harlot.app"
HOME="$work/home" \
HH_RELEASE_TEST_MODE=1 \
HH_INSTALLER_APPLICATIONS_DIR="$work/poisoned-apps" \
  "$repository_root/install-community-macos.sh" --plan >"$work/poisoned-plan.out"
if grep -F "$work/poisoned-apps" "$work/poisoned-plan.out" >/dev/null; then
  echo "inherited test variables redirected a normal installation" >&2
  exit 1
fi
HOME="$work/home" \
HH_RELEASE_TEST_MODE=1 \
  "$repository_root/install-community-macos.sh" \
    --test-applications-dir "$work/system-apps" --plan >"$work/plan.out"
grep -F "Install location: $work/system-apps/Harness Harlot.app" "$work/plan.out" >/dev/null
grep -F "Another installation exists: $work/home/Applications/Harness Harlot.app" \
  "$work/plan.out" >/dev/null

preflight_line=$(grep -n '^  preflight_existing_install$' \
  "$repository_root/install-community-macos.sh" | cut -d: -f1)
download_line=$(grep -n '^run_quiet "Download Harness Harlot' \
  "$repository_root/install-community-macos.sh" | cut -d: -f1)
[ -n "$preflight_line" ]
[ "$preflight_line" -lt "$download_line" ] || {
  echo "installer downloads the release before checking whether live terminals block replacement" >&2
  exit 1
}

key="$work/update-key"
printf '********************************' | base64 > "$key"
chmod 600 "$key"
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
public_key=$("$repository_root/target/release/hh-release-sign" public-key --private-key "$key")
distribution=$(
  HH_RELEASE_TEST_MODE=1 \
  HH_RELEASE_BUILD=1 \
  HH_ALLOW_DIRTY_TEST_PACKAGE=1 \
  HH_UPDATE_SIGNING_KEY_FILE="$key" \
  HH_UPDATE_PUBLIC_KEY="$public_key" \
  "$repository_root/scripts/package-macos-release.sh" "$version" 1 --community | sed -n '$p'
)

case "$(basename "$distribution")" in
  TESTONLY-Harness-Harlot-"$version"-b1-macos-"$architecture"-community) ;;
  *) echo "community fixture artifact name is not isolated" >&2; exit 1 ;;
esac
manifest="$distribution/manifest-macos-community-${architecture}.update.json"
[ -f "$manifest" ]
[ -f "$manifest.sig" ]
[ ! -e "$distribution/manifest-macos-${architecture}.update.json" ]
artifact=$(plutil -extract artifacts.0.file_name raw -o - "$manifest")
case "$artifact" in
  *-macos-"$architecture"-community.dmg) ;;
  *) echo "community manifest does not select a community-only DMG" >&2; exit 1 ;;
esac

app="$repository_root/target/release/Harness Harlot.app"
HH_SOCKET="$work/no-service.sock" \
  "$app/Contents/MacOS/hh-update-tool" prepare-community-install

echo "community fixture isolates its feed and validates explicit installation trust"
