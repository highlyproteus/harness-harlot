use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use hh_protocol::{AppearanceColor, PaneKind, PaneLayout, SplitAxis};
use hh_session_service::SessionRegistry;
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

#[test]
fn browser_tabs_round_trip_without_a_pty_and_reject_terminal_operations() {
    let directory = test_directory("browser");
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let browser_id = registry.create_browser_tab(workspace_id, None).unwrap();

    let split_error = registry
        .create_pane(browser_id, SplitAxis::Horizontal)
        .unwrap_err();
    assert!(
        split_error
            .to_string()
            .contains("browser tabs cannot create terminal panes")
    );
    let input_error = registry.write_input(browser_id, b"ignored").unwrap_err();
    assert!(input_error.to_string().contains("not a terminal"));

    registry
        .set_browser_state(browser_id, "example.com/docs", Some("Example Docs"))
        .unwrap();
    let browser_revision = registry.snapshot().unwrap().revision;
    registry
        .set_browser_state(browser_id, "https://example.com/docs", Some("Example Docs"))
        .unwrap();
    assert_eq!(registry.snapshot().unwrap().revision, browser_revision);
    let updates = registry.pane_updates(None, &[], &[], false, 0).unwrap();
    assert!(
        updates
            .screens
            .iter()
            .all(|screen| screen.pane_id != browser_id)
    );
    assert!(updates.pane_states.iter().any(|state| {
        state.pane_id == browser_id
            && state.revision == 0
            && !state.subscribed
            && !state.dirty
            && !state.exited
    }));
    registry.persist().unwrap();
    drop(registry);

    let recovered = SessionRegistry::persistent(&path).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    let pane = snapshot.workspaces[0]
        .tabs
        .iter()
        .find_map(|tab| match &tab.layout {
            PaneLayout::Leaf { pane } if pane.id == browser_id => Some(pane),
            _ => None,
        })
        .expect("recovered browser tab");
    assert_eq!(pane.title, "Example Docs");
    assert_eq!(
        pane.kind,
        PaneKind::Browser {
            url: "https://example.com/docs".to_owned(),
        }
    );
    assert!(
        recovered
            .write_input(browser_id, b"ignored")
            .unwrap_err()
            .to_string()
            .contains("not a terminal")
    );

    drop(recovered);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn grouped_browser_panes_round_trip_inside_the_group_stack() {
    let directory = test_directory("group-browser");
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let group_terminal = registry.create_workspace_group(workspace_id, None).unwrap();
    let browser_id = registry
        .create_group_browser(group_terminal, Some("https://example.com"))
        .unwrap();

    let snapshot = registry.snapshot().unwrap();
    let group = snapshot.workspaces[0]
        .tabs
        .iter()
        .find_map(|tab| match &tab.layout {
            PaneLayout::Stack { panes, active }
                if panes.iter().any(|pane| pane.id == group_terminal) =>
            {
                Some((panes, active))
            }
            _ => None,
        })
        .expect("group stack containing its initial terminal");
    assert_eq!(*group.1, browser_id);
    assert!(group.0.iter().any(|pane| {
        pane.id == browser_id
            && matches!(
                &pane.kind,
                PaneKind::Browser { url } if url == "https://example.com/"
            )
    }));

    drop(registry);
    let recovered = SessionRegistry::persistent(&path).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    let (panes, active) = snapshot.workspaces[0]
        .tabs
        .iter()
        .find_map(|tab| match &tab.layout {
            PaneLayout::Stack { panes, active }
                if panes.iter().any(|pane| pane.id == group_terminal) =>
            {
                Some((panes, active))
            }
            _ => None,
        })
        .expect("recovered group stack");
    assert_eq!(*active, browser_id);
    assert!(panes.iter().any(|pane| {
        pane.id == browser_id
            && matches!(
                &pane.kind,
                PaneKind::Browser { url } if url == "https://example.com/"
            )
    }));

    drop(recovered);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn projects_and_working_dirs_round_trip() {
    let directory = test_directory("projects");
    let path = directory.join("sessions.json");
    let workspace_dir = directory.join("workspace");
    let project_dir = directory.join("project");
    fs::create_dir_all(&directory).unwrap();
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&project_dir).unwrap();

    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    registry
        .set_workspace_working_dir(
            workspace_id,
            Some(workspace_dir.to_string_lossy().into_owned()),
        )
        .unwrap();
    let workspace_pane = registry.create_workspace_tab(workspace_id).unwrap();
    wait_for_process_cwd(&registry, workspace_pane, &workspace_dir);

    let project_pane = registry
        .create_workspace_project(workspace_id, &project_dir.to_string_lossy(), None)
        .unwrap();
    wait_for_process_cwd(&registry, project_pane, &project_dir);
    let snapshot = registry.snapshot().unwrap();
    let project_tab = snapshot.workspaces[0]
        .tabs
        .iter()
        .find(|tab| matches!(&tab.layout, PaneLayout::Leaf { pane } if pane.id == project_pane))
        .unwrap();
    assert_eq!(project_tab.custom_title.as_deref(), Some("project"));
    assert_eq!(
        project_tab.project_dir.as_deref(),
        Some(project_dir.to_string_lossy().as_ref())
    );

    let grouped_pane = registry.create_group_terminal(project_pane).unwrap();
    wait_for_process_cwd(&registry, grouped_pane, &project_dir);
    let plain_tab = registry.snapshot().unwrap().workspaces[0].tabs[0].id;
    let error = registry
        .set_tab_working_dir(plain_tab, project_dir.to_string_lossy().into_owned())
        .unwrap_err();
    assert!(error.to_string().contains("not a project"));

    registry.persist().unwrap();
    drop(registry);

    let recovered = SessionRegistry::persistent(&path).unwrap();
    let workspace = &recovered.snapshot().unwrap().workspaces[0];
    assert_eq!(
        workspace.working_dir.as_deref(),
        Some(workspace_dir.to_string_lossy().as_ref())
    );
    assert!(
        workspace.tabs.iter().any(|tab| {
            tab.project_dir.as_deref() == Some(project_dir.to_string_lossy().as_ref())
        })
    );

    drop(recovered);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn project_group_inherits_project_directory() {
    let directory = test_directory("project-group");
    let path = directory.join("sessions.json");
    let project_dir = directory.join("project");
    fs::create_dir_all(&project_dir).unwrap();
    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let project_pane = registry
        .create_workspace_project(workspace_id, &project_dir.to_string_lossy(), None)
        .unwrap();
    let project_tab = registry.snapshot().unwrap().workspaces[0]
        .tabs
        .iter()
        .find(|tab| matches!(&tab.layout, PaneLayout::Leaf { pane } if pane.id == project_pane))
        .unwrap()
        .id;

    let child_pane = registry
        .create_workspace_group(workspace_id, Some(project_tab))
        .unwrap();
    wait_for_process_cwd(&registry, child_pane, &project_dir);
    let snapshot = registry.snapshot().unwrap();
    let child = snapshot.workspaces[0]
        .tabs
        .iter()
        .find(|tab| matches!(&tab.layout, PaneLayout::Leaf { pane } if pane.id == child_pane))
        .unwrap();
    assert_eq!(child.parent_tab, Some(project_tab));

    let grouped_pane = registry.create_group_terminal(child_pane).unwrap();
    wait_for_process_cwd(&registry, grouped_pane, &project_dir);
    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn close_tab_removes_tab_children_and_sessions() {
    let directory = test_directory("close-project");
    let path = directory.join("sessions.json");
    let project_dir = directory.join("project");
    fs::create_dir_all(&project_dir).unwrap();
    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let project_pane = registry
        .create_workspace_project(workspace_id, &project_dir.to_string_lossy(), None)
        .unwrap();
    let project_tab = registry.snapshot().unwrap().workspaces[0]
        .tabs
        .iter()
        .find(|tab| matches!(&tab.layout, PaneLayout::Leaf { pane } if pane.id == project_pane))
        .unwrap()
        .id;
    let child_pane = registry
        .create_workspace_group(workspace_id, Some(project_tab))
        .unwrap();

    registry.close_tab(project_tab).unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace = &snapshot.workspaces[0];
    assert!(
        !workspace
            .tabs
            .iter()
            .any(|tab| { tab.id == project_tab || tab.parent_tab == Some(project_tab) })
    );
    assert_eq!(workspace.active_terminal_count, 1);
    assert!(registry.pane_process_id(project_pane).is_err());
    assert!(registry.pane_process_id(child_pane).is_err());
    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn tab_color_and_icon_round_trip() {
    let directory = test_directory("tab-appearance");
    let path = directory.join("sessions.json");
    fs::create_dir_all(&directory).unwrap();
    let registry = SessionRegistry::persistent(&path).unwrap();
    let tab_id = registry.snapshot().unwrap().workspaces[0].tabs[0].id;
    let color = AppearanceColor::new(0x12, 0x34, 0x56);
    let icon = "00000000-0000-4000-8000-000000000001.png".to_owned();
    registry.set_tab_color(tab_id, Some(color)).unwrap();
    registry
        .set_tab_custom_icon(tab_id, Some(icon.clone()))
        .unwrap();
    drop(registry);

    let recovered = SessionRegistry::persistent(&path).unwrap();
    let tab = recovered.snapshot().unwrap().workspaces[0].tabs[0].clone();
    assert_eq!(tab.color, Some(color));
    assert_eq!(tab.custom_icon.as_deref(), Some(icon.as_str()));
    drop(recovered);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn list_remote_directory_lists_local_subdirectories() {
    let directory = test_directory("local-listing");
    let path = directory.join("sessions.json");
    let root = directory.join("root");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::create_dir_all(root.join(".hidden")).unwrap();
    fs::write(root.join("file.txt"), b"file").unwrap();
    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;

    assert_eq!(
        registry
            .list_remote_directory(workspace_id, &root.to_string_lossy())
            .unwrap(),
        ["a".to_owned(), "b".to_owned()]
    );
    drop(registry);
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

fn leaf(layout: &PaneLayout) -> &hh_protocol::Pane {
    match layout {
        PaneLayout::Leaf { pane } => pane,
        _ => panic!("expected leaf pane"),
    }
}

fn test_directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hh-integration-{label}-{}", Uuid::new_v4()))
}
