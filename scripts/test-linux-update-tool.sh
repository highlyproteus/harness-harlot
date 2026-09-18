#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
target_directory=${CARGO_TARGET_DIR:-"$repository_root/target"}
case "$target_directory" in
  /*) ;;
  *) target_directory="$repository_root/$target_directory" ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/hh-linux-update-test.XXXXXX")
service_pid=
cleanup() {
  if [ -n "$service_pid" ]; then
    kill "$service_pid" 2>/dev/null || :
    wait "$service_pid" 2>/dev/null || :
  fi
  rm -rf "$work"
}
trap cleanup EXIT HUP INT TERM
mkdir -p "$work/home/.local/lib" "$work/home/.local/bin"

key="$work/update-key"
printf '********************************' | base64 > "$key"
chmod 600 "$key"
cargo build --locked --release -p hh-release-signer --bin hh-release-sign
cargo build --locked --release -p hh-updater --features fetch,fixture --bin hh-update-tool
signer="$target_directory/release/hh-release-sign"
tool="$target_directory/release/hh-update-tool"
public_key=$("$signer" public-key --private-key "$key")
current_protocol_version=$(sed -nE 's/^pub const PROTOCOL_VERSION: u16 = ([0-9]+);$/\1/p' "$repository_root/crates/protocol/src/lib.rs")
[ -n "$current_protocol_version" ]
case "$(uname -m)" in
  arm64 | aarch64) architecture=arm64 ;;
  x86_64) architecture=x86_64 ;;
  *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
case "$(uname -s)" in
  Darwin)
    file_size() { stat -f %z "$1"; }
    published_at=$(date -u -v-1H '+%Y-%m-%dT%H:%M:%SZ')
    valid_until=$(date -u -v+1d '+%Y-%m-%dT%H:%M:%SZ')
    ;;
  *)
    file_size() { stat -c %s "$1"; }
    published_at=$(date -u -d '-1 hour' '+%Y-%m-%dT%H:%M:%SZ')
    valid_until=$(date -u -d '+1 day' '+%Y-%m-%dT%H:%M:%SZ')
    ;;
esac

write_install() {
  root=$1
  hh_mode=$2
  marker=$3
  mkdir -p \
    "$root/bin" \
    "$root/share/applications" \
    "$root/share/icons/hicolor/512x512/apps" \
    "$root/share/licenses/harness-harlot" \
    "$root/share/harness-harlot"
  printf '#!/bin/sh\nexit 0\n' > "$root/install.sh"
  if [ "$hh_mode" = broken ]; then
    printf '#!/no/such/interpreter\n# %s\n' "$marker" > "$root/bin/hh"
  else
    cat > "$root/bin/hh" <<EOF
#!/bin/sh
printf '%s\n' '$marker' > "\${HH_LINUX_UPDATE_LAUNCH_LOG:-/dev/null}"
EOF
  fi
  printf '#!/bin/sh\nexit 0\n' > "$root/bin/hh-service"
  printf '#!/bin/sh\nexit 0\n' > "$root/bin/hh-update-tool"
  chmod 0755 "$root/install.sh" "$root/bin/hh" "$root/bin/hh-service" "$root/bin/hh-update-tool"
  printf '[Desktop Entry]\nName=Harness Harlot\n' > "$root/share/applications/com.harnessharlot.desktop.desktop"
  printf 'fixture icon\n' > "$root/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png"
  printf 'license\n' > "$root/share/licenses/harness-harlot/LICENSE"
  printf 'privacy\n' > "$root/share/licenses/harness-harlot/PRIVACY.md"
  printf 'notices\n' > "$root/share/licenses/harness-harlot/THIRD_PARTY_NOTICES.md"
  printf 'asset notices\n' > "$root/share/licenses/harness-harlot/ASSET_NOTICES.md"
  printf 'com.harnessharlot.desktop\n' > "$root/share/harness-harlot/install-id"
  chmod 0644 \
    "$root/share/applications/com.harnessharlot.desktop.desktop" \
    "$root/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png" \
    "$root/share/licenses/harness-harlot/LICENSE" \
    "$root/share/licenses/harness-harlot/PRIVACY.md" \
    "$root/share/licenses/harness-harlot/THIRD_PARTY_NOTICES.md" \
    "$root/share/licenses/harness-harlot/ASSET_NOTICES.md" \
    "$root/share/harness-harlot/install-id"
}

make_package() {
  name=$1
  mode=$2
  marker=$3
  source="$work/source-$name"
  rm -rf "$source"
  write_install "$source/Harness-Harlot" "$mode" "$marker"
  artifact="$work/Harness-Harlot-${name}-linux-${architecture}.tar.gz"
  COPYFILE_DISABLE=1 tar -czf "$artifact" -C "$source" Harness-Harlot
  printf '%s\n' "$artifact"
}

make_manifest() {
  artifact=$1
  version=$2
  build=$3
  manifest=$4
  sha256=$(shasum -a 256 "$artifact" | sed 's/[[:space:]].*$//')
  size=$(file_size "$artifact")
  protocol_version=${5:-"$current_protocol_version"}
  cat > "$manifest" <<EOF
{
  "schema": "hh-update-manifest-v2",
  "product": "Harness Harlot",
  "channel": "stable",
  "key_id": "test-only-v1",
  "version": "$version",
  "build": $build,
  "published_at": "$published_at",
  "valid_until": "$valid_until",
  "platform": "linux",
  "minimum_glibc": "2.35",
  "session_service": {
    "protocol_version": $protocol_version,
    "requires_quiescent_service": true
  },
  "artifacts": [
    {
      "platform": "linux",
      "architecture": "$architecture",
      "format": "tar.gz",
      "file_name": "$(basename "$artifact")",
      "url": "https://updates.example.invalid/$(basename "$artifact")",
      "sha256": "$sha256",
      "size": $size
    }
  ]
}
EOF
  $signer sign --manifest "$manifest" --signature "$manifest.sig" --private-key "$key"
}

run_install() {
  artifact=$1
  manifest=$2
  install_home=${3:-"$work/home"}
  restart_service=${4:-false}
  set -- install \
    --fixture \
    --platform linux \
    --architecture "$architecture" \
    --current-version 0.1.0 \
    --current-build 0 \
    --prefix "$install_home/.local/lib" \
    --key-id test-only-v1 \
    --public-key "$public_key" \
    --host updates.example.invalid \
    --manifest "$manifest" \
    --signature "$manifest.sig" \
    --artifact "$artifact"
  if [ "$restart_service" = true ]; then
    set -- "$@" --restart-service
  fi
  HOME="$install_home" HH_SOCKET="$work/session.sock" \
    HH_TEST_SERVICE_STOP_LOG="$work/service-stop.log" \
    HH_LINUX_UPDATE_LAUNCH_LOG="$work/launched" "$tool" "$@"
}
start_test_service() {
  rm -f "$work/session.sock"
  python3 - "$work/session.sock" <<'PY' >"$work/service.log" 2>&1 &
import socket
import sys

listener = socket.socket(socket.AF_UNIX)
listener.bind(sys.argv[1])
listener.listen()
while True:
    connection, _ = listener.accept()
    connection.close()
PY
  service_pid=$!
  printf '%s\n' "$service_pid" >"$work/service.pid"
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -S "$work/session.sock" ] && break
    sleep 0.1
  done
  [ -S "$work/session.sock" ]
}


app="$work/home/.local/lib/harness-harlot"
backup="$work/home/.local/lib/harness-harlot.previous"
write_install "$app" normal old
ln -s "$app/bin/hh" "$work/home/.local/bin/hh"
start_test_service

artifact=$(make_package good normal new)
manifest="$work/good.json"
make_manifest "$artifact" 0.2.0 1 "$manifest"
# The desktop keeps running until readiness, then closes its pipes and exits.
# Exercise that handoff with an actual process, including the cached-PID exit case.
HOME="$work/home" HH_SOCKET="$work/session.sock" HH_LINUX_UPDATE_LAUNCH_LOG="$work/launched" \
python3 - "$tool" "$artifact" "$manifest" "$public_key" "$work" "$architecture" <<'PY'
import datetime
import os
import select
import shutil
import subprocess
import sys
import time
from pathlib import Path

tool, artifact, manifest, public_key, work, architecture = sys.argv[1:]
desktop = subprocess.Popen(["sleep", "30"])
installer = None
try:
    started = subprocess.check_output(
        ["ps", "-p", str(desktop.pid), "-o", "lstart="],
        text=True, env=dict(os.environ, LC_ALL="C"),
    ).strip()
    start_time = int(datetime.datetime.strptime(started, "%a %b %d %H:%M:%S %Y").timestamp())
    command = [
        tool, "install", "--fixture", "--platform", "linux", "--architecture", architecture,
        "--current-version", "0.1.0", "--current-build", "0",
        "--prefix", f"{work}/home/.local/lib", "--key-id", "test-only-v1",
        "--public-key", public_key, "--host", "updates.example.invalid",
        "--manifest", manifest, "--signature", manifest + ".sig", "--artifact", artifact,
        "--wait-pid", str(desktop.pid), "--wait-start-time", str(start_time),
    ]
    # A download failure occurs while the original desktop is still alive.
    # Put the tool in a bundle-shaped path to exercise macOS recovery handling.
    bundled_tool = Path(work, "probe", "Harness Harlot.app", "Contents", "MacOS", "hh-update-tool")
    bundled_tool.parent.mkdir(parents=True)
    shutil.copy2(tool, bundled_tool)
    original = Path(artifact).read_bytes()
    try:
        Path(artifact).write_bytes(bytes([original[0] ^ 1]) + original[1:])
        failed = subprocess.run(
            [str(bundled_tool), *command[1:]], capture_output=True, timeout=5,
        )
        assert failed.returncode != 0 and b"SHA-256 mismatch" in failed.stderr
        assert b"download-complete" not in failed.stdout
        assert desktop.poll() is None, "download failure stopped the desktop"
    finally:
        Path(artifact).write_bytes(original)
    installer = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    assert select.select([installer.stdout], [], [], 5)[0], "readiness was not published"
    assert installer.stdout.readline() == b"download-complete\n"
    time.sleep(0.2)
    assert desktop.poll() is None and installer.poll() is None
    assert not Path(work, "launched").exists(), "installed before the desktop exited"
    installer.stdout.close()
    installer.stderr.close()
    desktop.terminate()
    desktop.wait()
    assert installer.wait(timeout=10) == 0, "installer failed after desktop exit"
finally:
    for process in (desktop, installer):
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait()
PY
for _ in 1 2 3 4 5; do
  [ -f "$work/launched" ] && break
  sleep 0.1
done
[ "$(cat "$work/launched")" = new ]
grep -q old "$backup/bin/hh"
[ "$(readlink "$work/home/.local/bin/hh")" = "$app/bin/hh" ]
[ "$(readlink "$work/home/.local/share/applications/com.harnessharlot.desktop.desktop")" = "$app/share/applications/com.harnessharlot.desktop.desktop" ]
[ "$(readlink "$work/home/.local/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png")" = "$app/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png" ]
kill -0 "$service_pid"


broken_artifact=$(make_package broken broken broken)
broken_manifest="$work/broken.json"
make_manifest "$broken_artifact" 0.3.0 2 "$broken_manifest"
rm -f "$work/launched"
if run_install "$broken_artifact" "$broken_manifest" >"$work/broken.out" 2>&1; then
  echo "Linux updater accepted an application that could not relaunch" >&2
  exit 1
fi
grep -q new "$app/bin/hh"
[ "$(readlink "$work/home/.local/bin/hh")" = "$app/bin/hh" ]
for _ in 1 2 3 4 5; do
  [ -f "$work/launched" ] && break
  sleep 0.1
done
[ "$(cat "$work/launched")" = new ]


clean_home="$work/clean-home"
mkdir -p "$clean_home/.local/lib" "$clean_home/.local/bin"
if run_install "$broken_artifact" "$broken_manifest" "$clean_home" >"$work/clean-broken.out" 2>&1; then
  echo "Linux updater accepted a broken clean installation" >&2
  exit 1
fi
[ ! -e "$clean_home/.local/lib/harness-harlot" ]
[ ! -e "$clean_home/.local/bin/hh" ]
[ ! -e "$clean_home/.local/share/applications/com.harnessharlot.desktop.desktop" ]
[ ! -e "$clean_home/.local/share/icons/hicolor/512x512/apps/com.harnessharlot.desktop.png" ]
symlink_source="$work/source-symlink"
write_install "$symlink_source/Harness-Harlot" normal malicious
rm "$symlink_source/Harness-Harlot/bin/hh-service"
ln -s hh "$symlink_source/Harness-Harlot/bin/hh-service"
symlink_artifact="$work/Harness-Harlot-symlink-linux-${architecture}.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$symlink_artifact" -C "$symlink_source" Harness-Harlot
symlink_manifest="$work/symlink.json"
make_manifest "$symlink_artifact" 0.3.0 3 "$symlink_manifest"
if run_install "$symlink_artifact" "$symlink_manifest" >"$work/symlink.out" 2>&1; then
  echo "Linux updater accepted an archive symlink" >&2
  exit 1
fi

tampered_artifact=$(make_package tampered normal tampered)
tampered_manifest="$work/tampered.json"
make_manifest "$tampered_artifact" 0.3.0 4 "$tampered_manifest"
printf 'tamper' >> "$tampered_artifact"
if run_install "$tampered_artifact" "$tampered_manifest" >"$work/tampered.out" 2>&1; then
  echo "Linux updater accepted a tampered archive" >&2
  exit 1
fi

if HOME="$work/home" HH_SOCKET="$work/traversal-session.sock" "$tool" install \
  --fixture --platform linux --architecture "$architecture" \
  --current-version 0.1.0 --current-build 0 \
  --prefix "$work/home/../escaped" \
  --key-id test-only-v1 --public-key "$public_key" --host updates.example.invalid \
  --manifest "$manifest" --signature "$manifest.sig" --artifact "$artifact" \
  >"$work/traversal.out" 2>&1; then
  echo "Linux updater accepted a parent-directory install prefix" >&2
  exit 1
fi
[ ! -e "$work/escaped" ]

mkdir -p "$work/foreign-home/.local/lib" "$work/foreign-home/.local/bin"
write_install "$work/foreign-home/.local/lib/harness-harlot" normal foreign
ln -s /tmp/not-harness-harlot "$work/foreign-home/.local/bin/hh"
if HOME="$work/foreign-home" HH_SOCKET="$work/foreign-session.sock" "$tool" install \
  --fixture --platform linux --architecture "$architecture" \
  --current-version 0.1.0 --current-build 0 \
  --prefix "$work/foreign-home/.local/lib" \
  --key-id test-only-v1 --public-key "$public_key" --host updates.example.invalid \
  --manifest "$manifest" --signature "$manifest.sig" --artifact "$artifact" \
  >"$work/foreign.out" 2>&1; then
  echo "Linux updater replaced an unrelated command link" >&2
  exit 1
fi
[ "$(readlink "$work/foreign-home/.local/bin/hh")" = /tmp/not-harness-harlot ]

kill "$service_pid" 2>/dev/null || :
wait "$service_pid" 2>/dev/null || :
service_pid=
start_test_service

cat >"$app/bin/hh-service" <<'EOF'
#!/bin/sh
[ "${1:-}" = "--shutdown" ] || exit 1
touch "$HH_TEST_SERVICE_STOP_LOG"
rm -f "$HH_SOCKET"
EOF
chmod 0755 "$app/bin/hh-service"

restart_artifact=$(make_package restart normal restart)
restart_manifest="$work/restart.json"
make_manifest "$restart_artifact" 0.4.0 5 "$restart_manifest" "$((current_protocol_version + 1))"
run_install "$restart_artifact" "$restart_manifest"
[ -f "$work/service-stop.log" ]
kill "$service_pid" 2>/dev/null || :
wait "$service_pid" 2>/dev/null || :
service_pid=
[ ! -S "$work/session.sock" ]
for _ in 1 2 3 4 5; do
  [ "$(cat "$work/launched" 2>/dev/null || :)" = restart ] && break
  sleep 0.1
done
[ "$(cat "$work/launched")" = restart ]

forced_artifact=$(make_package forced normal forced)
forced_manifest="$work/forced.json"
make_manifest "$forced_artifact" 0.5.0 6 "$forced_manifest" "$((current_protocol_version + 1))"

cat >"$app/bin/hh-service" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod 0755 "$app/bin/hh-service"
start_test_service
if run_install "$forced_artifact" "$forced_manifest" "$work/home" true \
  >"$work/forced-not-found.out" 2>&1; then
  echo "Linux updater forced an unrelated process to stop" >&2
  exit 1
fi
grep -q "could not find the running session service to restart" "$work/forced-not-found.out"
kill -0 "$service_pid"
kill "$service_pid"
wait "$service_pid" 2>/dev/null || :
service_pid=
rm -f "$work/session.sock"

rm -f "$work/service-stop.log"
# Compile a real executable: copied Python launchers may exec a different binary
# on macOS, which must not satisfy the updater's exact-executable identity check.
cat >"$work/forced-service.c" <<'C'
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static volatile sig_atomic_t stopping = 0;
static void stop(int signal_number) {
  (void)signal_number;
  stopping = 1;
}

int main(int argc, char **argv) {
  if (argc != 3) return 1;
  struct sockaddr_un address = {0};
  address.sun_family = AF_UNIX;
  if (strlen(argv[1]) >= sizeof(address.sun_path)) return 2;
  strcpy(address.sun_path, argv[1]);
  struct sigaction action = {0};
  action.sa_handler = stop;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGTERM, &action, NULL) < 0) return 3;
  int listener = socket(AF_UNIX, SOCK_STREAM, 0);
  if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) < 0
      || listen(listener, 16) < 0) return 4;
  while (!stopping) {
    int client = accept(listener, NULL, NULL);
    if (client >= 0) close(client);
    else if (errno != EINTR) return 5;
  }
  close(listener);
  FILE *log = fopen(argv[2], "w");
  if (!log) return 6;
  fputs("SIGTERM\n", log);
  fclose(log);
  return unlink(argv[1]) == 0 ? 0 : 7;
}
C
${CC:-cc} "$work/forced-service.c" -o "$app/bin/hh-service"
"$app/bin/hh-service" "$work/session.sock" "$work/service-stop.log" \
  >"$work/forced-service.out" 2>&1 &
service_pid=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  [ -S "$work/session.sock" ] && break
  sleep 0.1
done
[ -S "$work/session.sock" ]


if run_install "$forced_artifact" "$forced_manifest" >"$work/forced-refused.out" 2>&1; then
  echo "Linux updater restarted a live service without --restart-service" >&2
  exit 1
fi
grep -q "re-run with --restart-service" "$work/forced-refused.out"
kill -0 "$service_pid"
[ -S "$work/session.sock" ]

run_install "$forced_artifact" "$forced_manifest" "$work/home" true >"$work/forced.out"
[ "$(grep -c '^download-complete$' "$work/forced.out")" -eq 1 ]
wait "$service_pid"
service_pid=
[ "$(cat "$work/service-stop.log")" = SIGTERM ]
[ ! -S "$work/session.sock" ]

echo "Linux updater fixture preserves compatible services, gracefully forces incompatible service restarts, installs atomically, and rejects rollback, archive, integrity, and ownership hazards"
