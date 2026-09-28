mod support;

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use hh_protocol::{AppearanceColor, PaneKind, PaneLayout, SplitAxis};
use hh_session_service::SessionRegistry;
use support::TestStateDir;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use uuid::Uuid;

#[test]
fn daemon_restart_restores_layout_and_working_directories() {
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
    assert!(recovered.pane_process_id(first).unwrap().is_some());
    assert!(recovered.pane_process_id(second).unwrap().is_some());
    wait_for_process_cwd(&recovered, first, &expected_cwd);

    drop(recovered);
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
}

#[test]
fn gallery_tabs_round_trip_without_a_pty() {
    let directory = test_directory("gallery");
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let gallery_id = registry.create_gallery_tab(workspace_id).unwrap();

    let split_error = registry
        .create_pane(gallery_id, SplitAxis::Horizontal)
        .unwrap_err();
    assert!(
        split_error
            .to_string()
            .contains("gallery panes cannot host terminals")
    );
    assert!(
        registry
            .write_input(gallery_id, b"ignored")
            .unwrap_err()
            .to_string()
            .contains("not a terminal")
    );
    let updates = registry.pane_updates(None, &[], &[], false, 0).unwrap();
    assert!(
        updates
            .screens
            .iter()
            .all(|screen| screen.pane_id != gallery_id)
    );
    assert!(updates.pane_states.iter().any(|state| {
        state.pane_id == gallery_id && state.revision == 0 && !state.subscribed && !state.exited
    }));
    registry.persist().unwrap();
    drop(registry);

    let recovered = SessionRegistry::persistent(&path).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    let pane = snapshot.workspaces[0]
        .tabs
        .iter()
        .find_map(|tab| match &tab.layout {
            PaneLayout::Leaf { pane } if pane.id == gallery_id => Some(pane),
            _ => None,
        })
        .expect("recovered gallery tab");
    assert_eq!(pane.title, "Gallery");
    assert_eq!(pane.kind, PaneKind::Gallery);
    assert!(
        recovered
            .write_input(gallery_id, b"ignored")
            .unwrap_err()
            .to_string()
            .contains("not a terminal")
    );

    drop(recovered);
}

#[test]
fn grouped_browser_panes_round_trip_inside_the_group_stack() {
    let directory = test_directory("group-browser");
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let group_terminal = registry.create_workspace_tab(workspace_id).unwrap();
    let browser_id = registry
        .create_tab_browser(group_terminal, Some("https://example.com"))
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
}

#[test]
fn nested_workstations_open_terminals_in_inherited_root_folders_and_round_trip() {
    let directory = test_directory("nested-roots");
    let path = directory.join("sessions.json");
    let workspace_dir = directory.join("workspace");
    let project_dir = directory.join("project");
    create_owner_only_directory(&directory);
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&project_dir).unwrap();

    let registry = SessionRegistry::persistent(&path).unwrap();
    let home = registry.snapshot().unwrap().workspaces[0].id;
    registry
        .set_workspace_working_dir(home, Some(workspace_dir.to_string_lossy().into_owned()))
        .unwrap();
    let home_pane = registry.create_workspace_tab(home).unwrap();
    wait_for_process_cwd(&registry, home_pane, &workspace_dir);

    let (project, project_pane) = registry
        .create_workspace(
            None,
            Some(home),
            Some(project_dir.to_string_lossy().into_owned()),
        )
        .unwrap();
    wait_for_process_cwd(&registry, project_pane, &project_dir);
    let (inner, inner_pane) = registry
        .create_workspace(None, Some(project), None)
        .unwrap();
    wait_for_process_cwd(&registry, inner_pane, &project_dir);
    let inner_tab_pane = registry.create_workspace_tab(inner).unwrap();
    wait_for_process_cwd(&registry, inner_tab_pane, &project_dir);

    let snapshot = registry.snapshot().unwrap();
    let find = |id| {
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(find(project).title, "project");
    assert_eq!(find(project).parent_workstation, Some(home));
    assert_eq!(find(inner).parent_workstation, Some(project));
    assert_eq!(find(inner).working_dir, None);

    registry.persist().unwrap();
    drop(registry);

    let recovered = SessionRegistry::persistent(&path).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    let recovered_project = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == project)
        .unwrap();
    assert_eq!(recovered_project.parent_workstation, Some(home));
    assert_eq!(
        recovered_project.working_dir.as_deref(),
        Some(project_dir.to_string_lossy().as_ref())
    );
    assert!(
        snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.id == inner && workspace.parent_workstation == Some(project))
    );
    drop(recovered);
}

#[test]
fn deleting_a_workstation_removes_its_nested_workstations_but_home_is_kept() {
    let directory = test_directory("delete-nested");
    let path = directory.join("sessions.json");
    create_owner_only_directory(&directory);
    let registry = SessionRegistry::persistent(&path).unwrap();
    let home = registry.snapshot().unwrap().workspaces[0].id;
    let (outer, outer_pane) = registry
        .create_workspace(Some("Outer"), None, None)
        .unwrap();
    let (middle, middle_pane) = registry.create_workspace(None, Some(outer), None).unwrap();
    let (inner, inner_pane) = registry.create_workspace(None, Some(middle), None).unwrap();
    let (sibling, _) = registry.create_workspace(None, Some(home), None).unwrap();

    let error = registry.delete_workspace(home).unwrap_err();
    assert_eq!(error.to_string(), "the home workstation cannot be deleted");

    registry.delete_workspace(outer).unwrap();
    let remaining = registry
        .snapshot()
        .unwrap()
        .workspaces
        .iter()
        .map(|workspace| workspace.id)
        .collect::<Vec<_>>();
    assert_eq!(remaining, vec![home, sibling]);
    for (id, pane) in [
        (outer, outer_pane),
        (middle, middle_pane),
        (inner, inner_pane),
    ] {
        assert!(!remaining.contains(&id));
        assert!(registry.pane_process_id(pane).is_err());
    }
    drop(registry);

    let recovered = SessionRegistry::persistent(&path).unwrap();
    assert_eq!(recovered.snapshot().unwrap().workspaces.len(), 2);
    drop(recovered);
}

#[test]
fn tab_color_and_icon_round_trip() {
    let directory = test_directory("tab-appearance");
    let path = directory.join("sessions.json");
    create_owner_only_directory(&directory);
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
}

#[test]
fn list_remote_directory_lists_local_subdirectories() {
    let directory = test_directory("local-listing");
    let path = directory.join("sessions.json");
    let root = directory.join("root");
    create_owner_only_directory(&root.join("a"));
    create_owner_only_directory(&root.join("b"));
    create_owner_only_directory(&root.join(".hidden"));
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

fn test_directory(label: &str) -> TestStateDir {
    TestStateDir::new(label)
}

fn create_owner_only_directory(path: &std::path::Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}
