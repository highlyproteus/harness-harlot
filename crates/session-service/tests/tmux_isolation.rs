mod support;

use std::process::{Command, Stdio};

use hh_session_service::{SessionRegistry, managed_tmux_socket_name};
use support::{TestStateDir, tmux_binary};

#[test]
fn only_the_default_state_directory_uses_the_app_tmux_socket() {
    let default_name = if cfg!(debug_assertions) {
        "hh-dev"
    } else {
        "hh"
    };
    if std::env::var_os(hh_protocol::STATE_DIR_ENV).is_none()
        && let Some(default_dir) = hh_protocol::state_directory()
    {
        assert_eq!(managed_tmux_socket_name(&default_dir), default_name);
    }

    let first = TestStateDir::new("tmux-name-a");
    let second = TestStateDir::new("tmux-name-b");
    assert_ne!(first.socket_name(), second.socket_name());
    for directory in [&first, &second] {
        let name = directory.socket_name();
        assert!(name != "hh" && name != "hh-dev");
        assert!(name.starts_with("hh-"));
        // Stable: the same directory always addresses the same server.
        assert_eq!(managed_tmux_socket_name(directory), name);
    }
}

#[test]
fn registries_on_different_state_directories_use_separate_tmux_servers() {
    let first_dir = TestStateDir::new("tmux-isolation-a");
    let second_dir = TestStateDir::new("tmux-isolation-b");
    let first = SessionRegistry::persistent(first_dir.join("sessions.json")).unwrap();
    let second = SessionRegistry::persistent(second_dir.join("sessions.json")).unwrap();
    let first_workspace = first.snapshot().unwrap().workspaces[0].id;
    let second_workspace = second.snapshot().unwrap().workspaces[0].id;
    assert_ne!(first_workspace, second_workspace);

    let Some(first_sessions) = tmux_sessions(first_dir.socket_name()) else {
        // Without tmux the registries run plain PTYs; nothing is shared.
        return;
    };
    let second_sessions = tmux_sessions(second_dir.socket_name()).unwrap();
    assert!(first_sessions.contains(&format!("hh-{first_workspace}")));
    assert!(!first_sessions.contains(&format!("hh-{second_workspace}")));
    assert!(second_sessions.contains(&format!("hh-{second_workspace}")));
    assert!(!second_sessions.contains(&format!("hh-{first_workspace}")));

    drop(first);
    drop(second);
}

fn tmux_sessions(socket_name: &str) -> Option<Vec<String>> {
    let output = Command::new(tmux_binary())
        .args(["-L", socket_name, "list-sessions", "-F", "#{session_name}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}
