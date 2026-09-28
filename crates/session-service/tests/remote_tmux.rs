//! SSH terminals are windows of HH's tmux on the remote host, driven over
//! ssh in control mode. A stand-in `ssh` runs the remote command locally, so
//! this exercises the real bootstrap, control connection, reattach with
//! scrollback, dropped connections, and the close rule without a network.
//! Its own test binary: the stand-in is selected through the environment.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use hh_protocol::{WorkspaceConnection, WorkspaceConnectionStatus};
use hh_session_service::SessionRegistry;
use support::{TestStateDir, tmux_binary};
use uuid::Uuid;

const FAKE_SSH: &str = r#"#!/bin/sh
# Stand-in for ssh: ignore the options and run the remote command here.
# `needs-sign-in` refuses non-interactive logins until a sign-in (the
# ControlMaster=yes terminal) has run once.
for last; do :; done
marker="$(dirname "$0")/signed-in"
case "$*" in
  *needs-sign-in*ControlMaster=yes*|*ControlMaster=yes*needs-sign-in*)
    : > "$marker"; echo "Signed in."; exit 0 ;;
  *needs-sign-in*)
    if [ ! -e "$marker" ]; then
      echo "tester@needs-sign-in: Permission denied (publickey,password)." >&2
      exit 255
    fi ;;
esac
exec /bin/sh -c "$last"
"#;

#[test]
#[allow(
    unsafe_code,
    reason = "sets the stand-in ssh before any thread reads it"
)]
fn remote_terminals_survive_restarts_and_dropped_connections() {
    let tmux_usable = Command::new(tmux_binary())
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !tmux_usable {
        return;
    }
    let directory = TestStateDir::new("remote");
    let fake_ssh = directory.join("fake-ssh");
    std::fs::write(&fake_ssh, FAKE_SSH).unwrap();
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: this test binary runs this single test; no other thread exists yet.
    unsafe { std::env::set_var("HH_TEST_SSH_BINARY", &fake_ssh) };
    let path = directory.join("sessions.json");

    let registry = SessionRegistry::persistent(&path).unwrap();
    let (workspace_id, pane_id) = registry
        .create_ssh_workspace(Some("Remote"), "tester@fake-host")
        .unwrap();
    let agent = start_agent(&registry, pane_id);

    // The service restarts: the workstation comes back offline until the
    // user reconnects, and the program never stopped.
    drop(registry);
    assert!(
        process_alive(agent),
        "a service restart ended the remote program"
    );
    let registry = SessionRegistry::persistent(&path).unwrap();
    assert_eq!(
        status(&registry, workspace_id),
        WorkspaceConnectionStatus::Offline
    );
    assert_eq!(
        pane_title(&registry, pane_id),
        "SSH tester@fake-host — Offline; reconnect required"
    );
    registry.reconnect_workspace(workspace_id).unwrap();
    assert_eq!(registry.pane_process_id(pane_id).unwrap(), Some(agent));
    wait_for_screen(&registry, pane_id, "REMOTE-AGENT");
    assert_eq!(
        status(&registry, workspace_id),
        WorkspaceConnectionStatus::Connected
    );
    // The offline label is gone once connected, including after another
    // restart (it used to stick as a custom title).
    assert_eq!(pane_title(&registry, pane_id), "SSH tester@fake-host");
    registry.persist().unwrap();
    drop(registry);
    let registry = SessionRegistry::persistent(&path).unwrap();
    registry.reconnect_workspace(workspace_id).unwrap();
    assert_eq!(pane_title(&registry, pane_id), "SSH tester@fake-host");
    assert_eq!(registry.pane_process_id(pane_id).unwrap(), Some(agent));

    // The connection drops: the tab goes offline, the program keeps running,
    // and Reconnect brings the same program back.
    let pattern = format!("tmux -L .* -C new-session -A -s hh-{workspace_id}");
    let clients = child_processes(&pattern);
    assert!(!clients.is_empty(), "no control connection was running");
    for pid in clients {
        Command::new("kill").arg(pid.to_string()).status().unwrap();
    }
    wait_until(|| status(&registry, workspace_id) == WorkspaceConnectionStatus::Offline);
    assert!(
        process_alive(agent),
        "a dropped connection ended the remote program"
    );
    registry.reconnect_workspace(workspace_id).unwrap();
    assert_eq!(registry.pane_process_id(pane_id).unwrap(), Some(agent));
    wait_for_screen(&registry, pane_id, "REMOTE-AGENT");

    // Disconnecting leaves it running too.
    registry.disconnect_workspace(workspace_id).unwrap();
    assert!(process_alive(agent));
    registry.reconnect_workspace(workspace_id).unwrap();
    assert_eq!(registry.pane_process_id(pane_id).unwrap(), Some(agent));

    // Closing the tab ends it.
    let second = registry.create_workspace_tab(workspace_id).unwrap();
    registry.close_pane(pane_id).unwrap();
    wait_until(|| !process_alive(agent));
    assert!(registry.pane_process_id(second).unwrap().is_some());

    // A host that needs a prompt opens a sign-in terminal; once it finishes,
    // the tab becomes a tmux window on the host.
    let (_, signed_in_pane) = registry
        .create_ssh_workspace(Some("Needs sign-in"), "tester@needs-sign-in")
        .unwrap();
    wait_until(|| {
        registry.write_input(signed_in_pane, b" ").is_ok()
            && pane_title(&registry, signed_in_pane) == "SSH tester@needs-sign-in"
    });
    let agent = start_agent(&registry, signed_in_pane);
    drop(registry);
    assert!(process_alive(agent));
}

fn pane_title(registry: &SessionRegistry, pane_id: Uuid) -> String {
    let snapshot = registry.snapshot().unwrap();
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find_map(|tab| find_title(&tab.layout, pane_id))
        .unwrap_or_default()
}

fn find_title(layout: &hh_protocol::PaneLayout, pane_id: Uuid) -> Option<String> {
    match layout {
        hh_protocol::PaneLayout::Leaf { pane } => (pane.id == pane_id).then(|| pane.title.clone()),
        hh_protocol::PaneLayout::Stack { panes, .. } => panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .map(|pane| pane.title.clone()),
        hh_protocol::PaneLayout::Split { first, second, .. } => {
            find_title(first, pane_id).or_else(|| find_title(second, pane_id))
        }
    }
}

fn start_agent(registry: &SessionRegistry, pane_id: Uuid) -> u32 {
    wait_until(|| registry.pane_process_id(pane_id).unwrap().is_some());
    registry
        .write_input(
            pane_id,
            b"exec /bin/sh -c 'echo REMOTE-AGENT; while :; do sleep 1; done'\r",
        )
        .unwrap();
    wait_for_screen(registry, pane_id, "REMOTE-AGENT");
    let agent = registry.pane_process_id(pane_id).unwrap().unwrap();
    assert!(process_alive(agent));
    agent
}

fn status(registry: &SessionRegistry, workspace_id: Uuid) -> WorkspaceConnectionStatus {
    let snapshot = registry.snapshot().unwrap();
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .unwrap();
    match &workspace.connection {
        WorkspaceConnection::SystemSsh { status, .. } => *status,
        WorkspaceConnection::Local => panic!("not an SSH workstation"),
    }
}

fn wait_for_screen(registry: &SessionRegistry, pane_id: Uuid, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (screen, _) = registry.pane_snapshot(pane_id).unwrap();
        let text = screen
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .map(|run| run.text.as_str())
            .collect::<String>();
        if text.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared in:\n{text}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(50));
    }
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// This test process's children whose command line matches `pattern`.
fn child_processes(pattern: &str) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string(), "-f", pattern])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}
