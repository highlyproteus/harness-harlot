use super::*;
use crate::layout::{
    find_pane_in_snapshot, first_pane_id, first_pane_in_layout, pane_ids_for_workspace,
    pane_in_layout,
};
use crate::pty::{TEST_LOCAL_SSH_SEAM_ENABLED, validate_terminal_dimensions};
use crate::registry::{SessionRegistry, create_owner_only_directory, refresh_runtime_metadata};
use hh_protocol::DropPlacement;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[test]
#[allow(clippy::too_many_lines)]
fn ssh_test_seam_honors_workspace_directory_and_keeps_direct_tabs_offline() {
    let directory = std::env::temp_dir().join(format!("hh-ssh-working-dir-{}", Uuid::new_v4()));
    create_owner_only_directory(&directory);
    TEST_LOCAL_SSH_SEAM_ENABLED.store(true, Ordering::Relaxed);

    let snapshot_path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
    let local_pane = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let direct_ssh_pane = registry
        .connect_ssh(local_pane, "admin@second-host")
        .unwrap();
    let long_host = "a".repeat(hh_protocol::MAX_SSH_HOST_LEN);
    let long_ssh_pane = registry.connect_ssh(local_pane, &long_host).unwrap();
    let (workspace_id, _) = registry
        .create_ssh_workspace(Some("SSH"), "admin@test-host")
        .unwrap();
    registry
        .set_workspace_working_dir(workspace_id, Some(directory.to_string_lossy().into_owned()))
        .unwrap();
    let pane_id = registry.create_workspace_tab(workspace_id).unwrap();
    let state = registry.state.read();
    assert_eq!(
        state.terminal_pane(pane_id).unwrap().last_valid_cwd,
        directory
    );
    drop(state);

    // A remote tab is named for its host, not this machine's folder, and an
    // omp there is tracked from its title alone: working, then a finished
    // turn that stays Done when the title tracker later re-reads `π >`.
    let pane = || {
        find_pane_in_snapshot(&registry.snapshot().unwrap(), pane_id)
            .unwrap()
            .clone()
    };
    assert_eq!(pane().title, "SSH admin@test-host");
    registry
        .write_input(
            pane_id,
            "exec sh -c \"printf '\\033]0;π ⠋ fixing\\007'; sleep 1; printf '\\033]0;π > fixing\\007'; sleep 30\"\r".as_bytes(),
        )
        .unwrap();
    let observe = |title: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while registry
            .state
            .read()
            .terminal_pane(pane_id)
            .unwrap()
            .session
            .terminal_title()
            .as_deref()
            != Some(title)
        {
            assert!(Instant::now() < deadline, "title {title:?} never arrived");
            thread::sleep(Duration::from_millis(10));
        }
        refresh_runtime_metadata(&mut registry.state.write());
    };
    observe("π ⠋ fixing");
    assert_eq!(pane().identity.profile, TerminalProfile::Omp);
    assert_eq!(pane().status, PaneStatus::Working);
    observe("π > fixing");
    assert_eq!(pane().status, PaneStatus::Done);
    registry
        .state
        .write()
        .terminal_pane_mut(pane_id)
        .unwrap()
        .omp_title_status = None;
    refresh_runtime_metadata(&mut registry.state.write());
    assert_eq!(pane().status, PaneStatus::Done);
    drop(registry);

    let recovered = SessionRegistry::persistent(snapshot_path).unwrap();
    let recovered_snapshot = recovered.snapshot().unwrap();
    let recovered_pane = find_pane_in_snapshot(&recovered_snapshot, direct_ssh_pane).unwrap();
    assert_eq!(
        recovered_pane.title,
        "SSH admin@second-host — Offline; reconnect required"
    );
    assert!(recovered.pane_process_id(direct_ssh_pane).is_err());
    recovered.close_pane(direct_ssh_pane).unwrap();
    assert!(find_pane_in_snapshot(&recovered.snapshot().unwrap(), direct_ssh_pane).is_none());
    let long_pane = find_pane_in_snapshot(&recovered_snapshot, long_ssh_pane).unwrap();
    assert!(long_pane.title.ends_with(" — Offline; reconnect required"));
    assert!(long_pane.title.chars().count() <= MAX_TITLE_CHARS);
    recovered.close_pane(long_ssh_pane).unwrap();
    drop(recovered);

    TEST_LOCAL_SSH_SEAM_ENABLED.store(false, Ordering::Relaxed);
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn reattach_respawns_an_exited_pane_in_place_and_refuses_a_live_one() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let pane_id = first_pane_id(&snapshot).unwrap();
    let tab_ids = snapshot.workspaces[0]
        .tabs
        .iter()
        .map(|tab| tab.id)
        .collect::<Vec<_>>();

    let error = registry.reattach_pane(pane_id).unwrap_err();
    assert!(error.to_string().contains("still live"));

    let dead_session = {
        let mut state = registry.state.write();
        let dead_session = {
            let terminal = state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap();
            terminal.exit_status = Some("Exited with code 255".to_owned());
            terminal.omp_title_status = Some(PaneStatus::Done);
            Arc::clone(&terminal.session)
        };
        state.set_pane_status(pane_id, PaneStatus::Done);
        dead_session
    };
    dead_session.terminate_and_wait().unwrap();

    registry.reattach_pane(pane_id).unwrap();

    let snapshot = registry.snapshot().unwrap();
    assert_eq!(
        snapshot.workspaces[0]
            .tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<Vec<_>>(),
        tab_ids
    );
    assert!(pane_ids_for_workspace(&snapshot.workspaces[0]).contains(&pane_id));
    assert_eq!(
        registry.state.read().panes.get(&pane_id).map(|runtime| {
            runtime
                .terminal()
                .and_then(|terminal| terminal.exit_status.clone())
        }),
        Some(None)
    );
    let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
    assert!(!pane.shell.contains("exited"), "shell: {}", pane.shell);
    assert_eq!(pane.status, PaneStatus::Idle);
    assert_eq!(
        registry
            .state
            .read()
            .panes
            .get(&pane_id)
            .and_then(RuntimePane::terminal)
            .and_then(|terminal| terminal.omp_title_status),
        None
    );
    registry
        .write_input(pane_id, b"printf 'REATTACHED\\n'\r")
        .unwrap();
}

#[test]
fn resize_propagates_the_exact_requested_grid_to_the_terminal_model() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();

    registry.resize_pane(pane_id, 13, 3).unwrap();

    let (_, screens) = registry.state().unwrap();
    let screen = screens
        .iter()
        .find(|screen| screen.pane_id == pane_id)
        .unwrap();
    assert_eq!((screen.columns, screen.rows), (13, 3));
}

#[test]
fn split_creates_a_second_live_shell_without_replacing_the_first() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let first_pid = registry.pane_process_id(first).unwrap();
    let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();

    assert_ne!(first, second);
    assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
    assert!(registry.pane_process_id(second).unwrap().is_some());
    assert_eq!(registry.state().unwrap().1.len(), 2);
}

#[test]
fn rearrange_swaps_layout_positions_without_restarting_shells() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
    let first_pid = registry.pane_process_id(first).unwrap();
    let second_pid = registry.pane_process_id(second).unwrap();

    registry.swap_panes(first, second).unwrap();

    let snapshot = registry.snapshot().unwrap();
    let layout = &snapshot.workspaces[0].tabs[0].layout;
    let PaneLayout::Split {
        first: left,
        second: right,
        ..
    } = layout
    else {
        panic!("expected split layout");
    };

    assert_eq!(first_pane_in_layout(left), second);
    assert_eq!(first_pane_in_layout(right), first);
    assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
    assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
}

#[test]
fn resize_bounds_reject_oom_dimensions_without_killing_sessions() {
    assert!(validate_terminal_dimensions(1_200, 500).is_ok());
    assert!(validate_terminal_dimensions(2_000, 301).is_err());
    assert!(validate_terminal_dimensions(1, 30).is_err());

    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    assert!(registry.resize_pane(pane_id, u16::MAX, u16::MAX).is_err());
    assert!(registry.pane_process_id(pane_id).unwrap().is_some());
}

#[test]
fn terminals_receive_human_names_and_can_be_renamed() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let second = registry.create_group_terminal(first).unwrap();
    registry.rename_pane(second, "Build logs").unwrap();

    // Panes spawn at the fallback cwd ($HOME), so their default titles
    // are that directory's folder name rather than "Terminal N".
    let home_folder = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .and_then(|home| home.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Terminal 1".to_owned());
    let snapshot = registry.snapshot().unwrap();
    let PaneLayout::Stack { panes, .. } = &snapshot.workspaces[0].tabs[0].layout else {
        panic!("expected a pane-local tab stack");
    };
    assert_eq!(panes[0].title, home_folder);
    assert_eq!(panes[1].title, "Build logs");
    assert_eq!(panes[1].shell, shell_title());
}

#[test]
fn moving_a_live_tab_to_a_directional_split_preserves_its_process() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let second = registry.create_group_terminal(first).unwrap();
    let first_pid = registry.pane_process_id(first).unwrap();
    let second_pid = registry.pane_process_id(second).unwrap();

    registry
        .move_pane_to_split(second, first, DropPlacement::Left)
        .unwrap();

    let snapshot = registry.snapshot().unwrap();
    let PaneLayout::Split {
        axis,
        first: left,
        second: right,
        ..
    } = &snapshot.workspaces[0].tabs[0].layout
    else {
        panic!("expected moved tab to become a split");
    };
    assert_eq!(*axis, SplitAxis::Horizontal);
    assert_eq!(first_pane_in_layout(left), second);
    assert_eq!(first_pane_in_layout(right), first);
    assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
    assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
}

#[test]
fn browser_moves_from_a_top_level_tab_into_a_terminal_split() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let terminal = first_pane_id(&snapshot).unwrap();
    let terminal_pid = registry.pane_process_id(terminal).unwrap();
    let browser = registry
        .create_browser_tab(workspace_id, Some("https://example.com"))
        .unwrap();

    registry
        .move_pane_to_split(browser, terminal, DropPlacement::Right)
        .unwrap();

    let snapshot = registry.snapshot().unwrap();
    assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
    let PaneLayout::Split {
        axis,
        first: left,
        second: right,
        ..
    } = &snapshot.workspaces[0].tabs[0].layout
    else {
        panic!("expected the browser to join the terminal split");
    };
    assert_eq!(*axis, SplitAxis::Horizontal);
    assert_eq!(first_pane_in_layout(left), terminal);
    assert!(matches!(
        &**right,
        PaneLayout::Leaf { pane }
            if pane.id == browser && matches!(pane.kind, PaneKind::Browser { .. })
    ));
    assert_eq!(registry.pane_process_id(terminal).unwrap(), terminal_pid);
}

#[test]
fn browser_moves_from_a_top_level_tab_into_a_terminal_tab_strip() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let terminal = first_pane_id(&snapshot).unwrap();
    let browser = registry
        .create_browser_tab(workspace_id, Some("https://example.com"))
        .unwrap();

    registry.move_pane_to_tab(browser, terminal).unwrap();

    let snapshot = registry.snapshot().unwrap();
    assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
    let PaneLayout::Stack { panes, active } = &snapshot.workspaces[0].tabs[0].layout else {
        panic!("expected the browser to join the terminal tab strip");
    };
    assert_eq!(
        panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
        [terminal, browser]
    );
    assert_eq!(*active, browser);
}

#[test]
fn directional_drop_of_a_lone_tab_keeps_it_live_and_fills_the_vacated_half() {
    let registry = SessionRegistry::new().unwrap();
    let moved = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let moved_pid = registry.pane_process_id(moved).unwrap();

    registry
        .move_pane_to_split(moved, moved, DropPlacement::Bottom)
        .unwrap();

    let snapshot = registry.snapshot().unwrap();
    let PaneLayout::Split {
        axis,
        first: top,
        second: bottom,
        ..
    } = &snapshot.workspaces[0].tabs[0].layout
    else {
        panic!("expected the lone tab drop to create a filled split");
    };
    let replacement = first_pane_in_layout(top);
    assert_eq!(*axis, SplitAxis::Vertical);
    assert_ne!(replacement, moved);
    assert_eq!(first_pane_in_layout(bottom), moved);
    assert_eq!(registry.pane_process_id(moved).unwrap(), moved_pid);
    assert!(registry.pane_process_id(replacement).unwrap().is_some());
    assert_eq!(registry.state().unwrap().1.len(), 2);
}

#[test]
fn closing_the_last_terminal_leaves_a_saved_empty_workspace_until_explicit_reopen() {
    let registry = SessionRegistry::new().unwrap();
    let initial = registry.snapshot().unwrap();
    let workspace_id = initial.workspaces[0].id;
    let first = first_pane_id(&initial).unwrap();
    let second = registry.create_pane(first, SplitAxis::Vertical).unwrap();
    let second_pid = registry.pane_process_id(second).unwrap();

    registry.close_pane(first).unwrap();

    let snapshot = registry.snapshot().unwrap();
    assert_eq!(first_pane_id(&snapshot), Some(second));
    assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
    assert!(registry.pane_process_id(first).is_err());

    registry.close_pane(second).unwrap();

    let empty = registry.snapshot().unwrap();
    assert_eq!(empty.workspaces.len(), 1);
    assert_eq!(empty.workspaces[0].id, workspace_id);
    assert!(empty.workspaces[0].tabs.is_empty());
    assert_eq!(empty.workspaces[0].active_terminal_count, 0);
    assert!(registry.state().unwrap().1.is_empty());

    let reopened = registry.create_workspace_terminal(workspace_id).unwrap();
    let reopened_snapshot = registry.snapshot().unwrap();
    assert_eq!(first_pane_id(&reopened_snapshot), Some(reopened));
    assert_eq!(reopened_snapshot.workspaces[0].active_terminal_count, 1);
    assert!(registry.create_workspace_terminal(workspace_id).is_err());
}

#[test]
fn natural_shell_exit_stays_visible_until_explicit_layout_close() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let exiting = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
    registry.write_input(exiting, b"exit 7\r").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = registry.snapshot().unwrap();
        let pane = snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .find_map(|tab| pane_in_layout(&tab.layout, exiting))
            .expect("exited pane must remain in its layout");
        if pane.shell.contains("exited") {
            assert!(pane.shell.contains('7'));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "natural child exit was not reflected in pane metadata"
        );
        thread::sleep(Duration::from_millis(25));
    }

    assert!(registry.pane(exiting).is_ok());
    registry.close_pane(exiting).unwrap();
    assert!(registry.pane(exiting).is_err());
    assert_eq!(first_pane_id(&registry.snapshot().unwrap()), Some(first));
}

#[test]
fn pane_local_split_only_mutates_the_explicit_second_pane() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
    let nested = registry.create_pane(second, SplitAxis::Vertical).unwrap();

    let snapshot = registry.snapshot().unwrap();
    let PaneLayout::Split {
        axis,
        first: left,
        second: right,
        ..
    } = &snapshot.workspaces[0].tabs[0].layout
    else {
        panic!("expected outer two pane columns");
    };
    assert_eq!(*axis, SplitAxis::Horizontal);
    assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
    let PaneLayout::Split {
        axis,
        first: top,
        second: bottom,
        ..
    } = &**right
    else {
        panic!("split control must split the targeted second pane");
    };
    assert_eq!(*axis, SplitAxis::Vertical);
    assert_eq!(first_pane_in_layout(top), second);
    assert_eq!(first_pane_in_layout(bottom), nested);
}
#[test]
fn add_gallery_image_opens_a_gallery_beside_the_origin_pane_without_activating_it() {
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0x60, 0xf8, 0x5f, 0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let source = std::env::temp_dir().join(format!("hh-gallery-{}.png", Uuid::new_v4()));
    fs::write(&source, PNG).unwrap();
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let first_pane = first_pane_id(&snapshot).unwrap();

    let (destination, gallery_pane) = registry
        .add_gallery_image(workspace_id, Some(first_pane), source.to_str().unwrap())
        .unwrap();

    let snapshot = registry.snapshot().unwrap();
    let PaneLayout::Stack { panes, active } = &snapshot.workspaces[0].tabs[0].layout else {
        panic!("gallery should join the origin pane's tab");
    };
    assert_eq!(*active, first_pane);
    assert!(
        panes
            .iter()
            .any(|pane| { pane.id == gallery_pane && matches!(pane.kind, PaneKind::Gallery) })
    );
    assert!(destination.exists());
    assert!(destination.starts_with(hh_protocol::gallery_directory(workspace_id).unwrap()));
    assert_eq!(
        destination.extension().and_then(|ext| ext.to_str()),
        Some("png")
    );

    let (second_destination, reused_pane) = registry
        .add_gallery_image(workspace_id, Some(first_pane), source.to_str().unwrap())
        .unwrap();
    assert_eq!(reused_pane, gallery_pane);

    fs::remove_file(source).unwrap();
    fs::remove_file(destination).unwrap();
    if second_destination.exists() {
        fs::remove_file(second_destination).unwrap();
    }
}

#[test]
fn add_gallery_image_rejects_non_images() {
    let source = std::env::temp_dir().join(format!("hh-gallery-{}.txt", Uuid::new_v4()));
    fs::write(&source, b"not an image").unwrap();
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let first_pane = first_pane_id(&snapshot).unwrap();

    let error = registry
        .add_gallery_image(workspace_id, Some(first_pane), source.to_str().unwrap())
        .unwrap_err();

    assert!(error.to_string().contains("not a PNG"));
    fs::remove_file(source).unwrap();
}
