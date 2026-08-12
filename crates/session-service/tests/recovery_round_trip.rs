use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rust_mux_protocol::{PaneLayout, SplitAxis};
use rust_mux_session_service::SessionRegistry;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use uuid::Uuid;

#[test]
fn daemon_restart_recreates_fresh_shells_from_safe_desired_state() {
    let directory = test_directory("restart");
    let path = directory.join("sessions.json");
    let expected_cwd = std::env::temp_dir();

    let registry = SessionRegistry::persistent(&path).unwrap();
    let first = first_pane(&registry);
    registry.rename_pane(first, "Recovered editor").unwrap();
    registry
        .write_input(
            first,
            format!("cd '{}'\r", expected_cwd.display()).as_bytes(),
        )
        .unwrap();
    wait_for_process_cwd(&registry, first, &expected_cwd);
    let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
    let old_first_pid = registry.pane_process_id(first).unwrap().unwrap();
    let old_second_pid = registry.pane_process_id(second).unwrap().unwrap();
    registry.persist().unwrap();
    drop(registry);

    let recovered = SessionRegistry::persistent(&path).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    let PaneLayout::Split {
        first: left,
        second: right,
        ..
    } = &snapshot.workspaces[0].tabs[0].layout
    else {
        panic!("persisted split layout was not recovered");
    };
    let left = leaf(left);
    let right = leaf(right);
    assert_eq!(left.id, first);
    assert_eq!(left.title, "Recovered editor");
    assert_eq!(right.id, second);
    assert!(left.shell.contains("recovered with a fresh shell"));
    assert!(right.shell.contains("recovered with a fresh shell"));
    assert_ne!(
        recovered.pane_process_id(first).unwrap(),
        Some(old_first_pid)
    );
    assert_ne!(
        recovered.pane_process_id(second).unwrap(),
        Some(old_second_pid)
    );
    wait_for_process_cwd(&recovered, first, &expected_cwd);

    drop(recovered);
    fs::remove_dir_all(directory).unwrap();
}

fn wait_for_process_cwd(registry: &SessionRegistry, pane_id: Uuid, expected: &Path) {
    let process_id = registry.pane_process_id(pane_id).unwrap().unwrap();
    let pid = Pid::from_u32(process_id);
    let expected = expected.canonicalize().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
        );
        if system
            .process(pid)
            .and_then(sysinfo::Process::cwd)
            .and_then(|cwd| cwd.canonicalize().ok())
            .is_some_and(|cwd| cwd == expected)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane process did not adopt expected CWD {}",
            expected.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn first_pane(registry: &SessionRegistry) -> Uuid {
    let snapshot = registry.snapshot().unwrap();
    leaf(&snapshot.workspaces[0].tabs[0].layout).id
}

fn leaf(layout: &PaneLayout) -> &rust_mux_protocol::Pane {
    match layout {
        PaneLayout::Leaf { pane } => pane,
        _ => panic!("expected leaf pane"),
    }
}

fn test_directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "not-a-harness-integration-{label}-{}",
        Uuid::new_v4()
    ))
}
