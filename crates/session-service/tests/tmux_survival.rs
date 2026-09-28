//! Terminals run in HH's private tmux server, so programs in them outlive the
//! session service: a restart (an app update) must reattach them, never kill
//! them. Uses a private tmux server per test; skipped where tmux is missing.

mod support;

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use hh_protocol::PaneLayout;
use hh_session_service::SessionRegistry;
use support::{TestStateDir, tmux_binary};
use uuid::Uuid;

#[test]
fn a_running_program_survives_a_service_restart_and_reattaches() {
    let Some(directory) = tmux_state_dir("restart") else {
        return;
    };
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let pane_id = first_pane(&registry);
    let agent = start_agent(&registry, pane_id, "RESTART");
    drop(registry);
    assert!(
        process_alive(agent),
        "stopping the service ended the program"
    );

    let registry = SessionRegistry::persistent(&path).unwrap();
    assert_eq!(first_pane(&registry), pane_id);
    assert_eq!(registry.pane_process_id(pane_id).unwrap(), Some(agent));
    wait_for_screen(&registry, pane_id, "AGENT-RESTART");
    assert!(process_alive(agent));
    let log = std::fs::read_to_string(directory.join("recovery.log")).unwrap();
    assert!(log.contains(&format!("pane {pane_id} reattached")), "{log}");
}

#[test]
fn a_saved_window_is_found_by_its_tag_and_never_killed() {
    let Some(directory) = tmux_state_dir("tag") else {
        return;
    };
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let pane_id = first_pane(&registry);
    let agent = start_agent(&registry, pane_id, "TAGGED");
    drop(registry);

    // The saved window ids are lost (as if the pane had been saved without
    // them). Recovery used to open a new window and then kill every window
    // the new layout did not reference, ending the running program.
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(remove_tmux_ids(&mut saved), 1);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let registry = SessionRegistry::persistent(&path).unwrap();
    assert!(process_alive(agent), "recovery killed the running program");
    assert_eq!(registry.pane_process_id(pane_id).unwrap(), Some(agent));
    wait_for_screen(&registry, pane_id, "AGENT-TAGGED");
}

#[test]
fn a_lost_tmux_connection_is_reestablished_without_touching_the_program() {
    let Some(directory) = tmux_state_dir("heal") else {
        return;
    };
    let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
    let pane_id = first_pane(&registry);
    let agent = start_agent(&registry, pane_id, "HEAL");

    let pattern = format!("tmux -L {} -f .* -C new-session", directory.socket_name());
    let before = control_clients(&pattern);
    assert!(!before.is_empty(), "no control client was running");
    // Only the service's own client: the tmux server shares its command line.
    for pid in &before {
        let killed = Command::new("kill").arg(pid.to_string()).status().unwrap();
        assert!(killed.success());
    }

    // The service reconnects on its own and the pane keeps accepting input;
    // nothing reports it as exited.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reconnected = control_clients(&pattern)
            .iter()
            .any(|pid| !before.contains(pid));
        if reconnected && registry.write_input(pane_id, b" ").is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the connection never came back");
        thread::sleep(Duration::from_millis(200));
    }
    assert!(process_alive(agent));
    assert_eq!(registry.pane_process_id(pane_id).unwrap(), Some(agent));
}

/// Control clients this test process started (the server is not a child).
fn control_clients(pattern: &str) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string(), "-f", pattern])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[test]
fn closing_a_tab_kills_its_window_and_program() {
    let Some(directory) = tmux_state_dir("close") else {
        return;
    };
    let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
    let first = first_pane(&registry);
    let second = registry
        .create_pane(first, hh_protocol::SplitAxis::Horizontal)
        .unwrap();
    let agent = start_agent(&registry, second, "CLOSE");
    registry.close_pane(second).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(agent) {
        assert!(
            Instant::now() < deadline,
            "closing the tab left its program running"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// A private state directory, or `None` (skip) when tmux 3.2+ is missing.
fn tmux_state_dir(label: &str) -> Option<TestStateDir> {
    let usable = Command::new(tmux_binary())
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    usable.then(|| TestStateDir::new(label))
}

/// Replaces the pane's shell with a long-running program that prints a
/// marker, and returns its pid (the tmux pane's pid, since it `exec`s).
fn start_agent(registry: &SessionRegistry, pane_id: Uuid, label: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while registry.pane_process_id(pane_id).unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    registry
        .write_input(
            pane_id,
            format!("exec /bin/sh -c 'echo AGENT-{label}; while :; do sleep 1; done'\r").as_bytes(),
        )
        .unwrap();
    wait_for_screen(registry, pane_id, &format!("AGENT-{label}"));
    let agent = registry.pane_process_id(pane_id).unwrap().unwrap();
    assert!(process_alive(agent));
    agent
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

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn first_pane(registry: &SessionRegistry) -> Uuid {
    let snapshot = registry.snapshot().unwrap();
    match &snapshot.workspaces[0].tabs[0].layout {
        PaneLayout::Leaf { pane } => pane.id,
        layout => panic!("expected a single pane, got {layout:?}"),
    }
}

/// Drops every saved `tmux_window`/`tmux_pane` pair; returns how many.
fn remove_tmux_ids(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            let removed = usize::from(map.remove("tmux_window").is_some());
            map.remove("tmux_pane");
            removed + map.values_mut().map(remove_tmux_ids).sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter_mut().map(remove_tmux_ids).sum(),
        _ => 0,
    }
}
