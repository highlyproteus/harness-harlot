use super::*;
use crate::layout::{find_pane_mut_in_snapshot, first_pane_id};
use crate::registry::{SessionRegistry, create_owner_only_directory, runtime_kind_for_workspace};
use uuid::Uuid;

#[test]
fn extra_panes_in_saved_ssh_workstations_retain_the_saved_destination() {
    let destination = "admin@build-node";
    let ssh = WorkspaceConnection::SystemSsh {
        destination: destination.to_owned(),
        status: WorkspaceConnectionStatus::Connected,
    };
    assert_eq!(
        runtime_kind_for_workspace(&ssh),
        RuntimePaneKind::SystemSsh {
            host: destination.to_owned(),
        }
    );
    assert_eq!(
        runtime_kind_for_workspace(&WorkspaceConnection::Local),
        RuntimePaneKind::Local
    );
}

#[test]
fn failed_custom_icon_persistence_does_not_publish_live_state() {
    let directory =
        std::env::temp_dir().join(format!("hh-icon-persistence-test-{}", Uuid::new_v4()));
    create_owner_only_directory(&directory);
    let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
    let before = registry.snapshot().unwrap();
    let workspace_id = before.workspaces[0].id;
    registry
        .files
        .as_ref()
        .unwrap()
        .snapshot
        .inject_failure_before_replace(true);

    assert!(
        registry
            .set_workspace_custom_icon(
                workspace_id,
                Some("00000000-0000-4000-8000-000000000004.png".to_owned()),
            )
            .is_err()
    );
    assert_eq!(registry.snapshot().unwrap(), before);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn stable_ssh_workstation_creation_is_delivered_to_the_rail_and_survives_restart() {
    let directory =
        std::env::temp_dir().join(format!("hh-ssh-workstation-test-{}", Uuid::new_v4()));
    create_owner_only_directory(&directory);
    let snapshot_path = directory.join("sessions.json");

    let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
    let before = registry.snapshot().unwrap();
    let (workspace_id, _) = registry
        .create_simulated_ssh_workspace(Some("Safe local simulation"), "test@local-host")
        .unwrap();

    let update = registry
        .pane_updates(Some(before.revision), &[], &[], true, 0)
        .unwrap();
    let delivered = update
        .snapshot
        .expect("new workstation snapshot is delivered");
    assert!(
        delivered
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
    );
    let created = delivered
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .unwrap();
    assert_eq!(created.title, "Safe local simulation");
    assert!(matches!(
        created.connection,
        WorkspaceConnection::SystemSsh {
            ref destination,
            status: WorkspaceConnectionStatus::Connected,
        } if destination == "test@local-host"
    ));

    drop(registry);

    let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    let saved = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .expect("saved SSH workstation remains after restart");
    assert_eq!(saved.title, "Safe local simulation");
    assert!(matches!(
        saved.connection,
        WorkspaceConnection::SystemSsh {
            ref destination,
            status: WorkspaceConnectionStatus::Offline,
        } if destination == "test@local-host"
    ));

    drop(recovered);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn confirmed_ssh_workstation_is_durable_before_session_attachment() {
    let directory =
        std::env::temp_dir().join(format!("hh-ssh-workstation-intent-test-{}", Uuid::new_v4()));
    create_owner_only_directory(&directory);
    let snapshot_path = directory.join("sessions.json");
    let ids = SshWorkspaceIds {
        workspace: Uuid::new_v4(),
        tab: Uuid::new_v4(),
        pane: Uuid::new_v4(),
    };

    let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
    registry
        .persist_ssh_workspace_intent(
            Some("Durable before connection".to_owned()),
            "test@local-host",
            ids,
        )
        .unwrap();
    drop(registry);

    let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    let saved = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == ids.workspace)
        .expect("confirmed SSH workstation remains after a restart");
    assert_eq!(saved.title, "Durable before connection");
    assert_eq!(saved.active_terminal_count, 0);
    assert!(matches!(
        saved.connection,
        WorkspaceConnection::SystemSsh {
            ref destination,
            status: WorkspaceConnectionStatus::Offline,
        } if destination == "test@local-host"
    ));

    drop(recovered);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejected_ssh_intent_does_not_create_or_replace_a_terminal() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let first_pid = registry.pane_process_id(first).unwrap();

    assert!(registry.connect_ssh(first, "-A").is_err());

    assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
    assert_eq!(registry.state().unwrap().1.len(), 1);
}

#[test]
fn appearance_mutations_keep_global_defaults_and_entity_overrides_independent() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let pane_id = first_pane_id(&snapshot).unwrap();
    let terminal_default = AppearanceColor::new(0x95, 0xcc, 0x7f);
    let workspace_default = AppearanceColor::new(0xc9, 0x90, 0xe5);
    let terminal_override = AppearanceColor::new(0xef, 0x71, 0x7a);
    let workspace_override = AppearanceColor::new(0xe4, 0xbd, 0x72);

    registry
        .set_default_terminal_accent(terminal_default)
        .unwrap();
    registry
        .set_default_workspace_color(workspace_default)
        .unwrap();
    registry
        .set_pane_color(pane_id, Some(terminal_override))
        .unwrap();
    registry
        .set_workspace_color(workspace_id, Some(workspace_override))
        .unwrap();

    let snapshot = registry.snapshot().unwrap();
    assert_eq!(
        snapshot.appearance.default_terminal_accent,
        terminal_default
    );
    assert_eq!(
        snapshot.appearance.default_workspace_color,
        workspace_default
    );
    assert_eq!(snapshot.workspaces[0].color, Some(workspace_override));
    assert_eq!(
        find_pane_mut_in_snapshot(&mut snapshot.clone(), pane_id).and_then(|pane| pane.color),
        Some(terminal_override)
    );
    assert_eq!(snapshot.appearance.recent_colors[0], workspace_override);

    registry.set_pane_color(pane_id, None).unwrap();
    registry.set_workspace_color(workspace_id, None).unwrap();
    let reset = registry.snapshot().unwrap();
    assert_eq!(reset.workspaces[0].color, None);
    assert_eq!(
        find_pane_mut_in_snapshot(&mut reset.clone(), pane_id).and_then(|pane| pane.color),
        None
    );
    assert_eq!(reset.appearance.default_terminal_accent, terminal_default);
    assert_eq!(reset.appearance.default_workspace_color, workspace_default);
}

#[test]
fn saved_workspace_management_renames_pins_reorders_and_deletes_deterministically() {
    let registry = SessionRegistry::new().unwrap();
    let first = registry.snapshot().unwrap().workspaces[0].id;
    let (second, _) = registry
        .create_workspace(Some("Second"), None, None)
        .unwrap();
    let (third, _) = registry
        .create_workspace(Some("Third"), None, None)
        .unwrap();

    registry.rename_workspace(second, "Build tools").unwrap();
    registry.set_workspace_pinned(second, true).unwrap();
    registry.set_workspace_pinned(third, true).unwrap();
    registry
        .move_pinned_workspace(third, WorkspacePinMove::Up)
        .unwrap();

    let snapshot = registry.snapshot().unwrap();
    let build = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == second)
        .unwrap();
    let third_workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == third)
        .unwrap();
    assert_eq!(build.title, "Build tools");
    assert!(build.pinned);
    assert!(third_workspace.pinned);
    assert_eq!(third_workspace.pin_order, 1);
    assert_eq!(build.pin_order, 2);

    registry.delete_workspace(second).unwrap();
    let snapshot = registry.snapshot().unwrap();
    assert!(
        snapshot
            .workspaces
            .iter()
            .all(|workspace| workspace.id != second)
    );
    assert_eq!(
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == third)
            .unwrap()
            .pin_order,
        1
    );
    assert!(registry.delete_workspace(first).is_err(), "home is kept");
    assert!(registry.delete_workspace(third).is_ok());
}

#[test]
fn workspace_reorder_stays_in_its_group_and_persists_explicit_order() {
    let registry = SessionRegistry::new().unwrap();
    let first = registry.snapshot().unwrap().workspaces[0].id;
    let (second, _) = registry
        .create_workspace(Some("Second"), None, None)
        .unwrap();
    let (third, _) = registry
        .create_workspace(Some("Third"), None, None)
        .unwrap();

    registry.reorder_workspace(third, first, false).unwrap();
    registry.set_workspace_pinned(second, true).unwrap();
    assert!(registry.reorder_workspace(third, second, false).is_err());

    let snapshot = registry.snapshot().unwrap();
    let mut regular = snapshot
        .workspaces
        .iter()
        .filter(|workspace| !workspace.pinned)
        .map(|workspace| (workspace.title.as_str(), workspace.order))
        .collect::<Vec<_>>();
    regular.sort_by_key(|(_, order)| *order);
    assert_eq!(
        regular,
        vec![("Third", 1), (hh_protocol::this_machine_title(), 2)]
    );
    assert!(
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == second)
            .is_some_and(|workspace| workspace.pinned)
    );
}

#[test]
fn disconnect_keeps_saved_workspace_tabs_and_layout_offline() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let pane_id = first_pane_id(&snapshot).unwrap();
    let expected_panes = pane_ids_for_workspace(&snapshot.workspaces[0]);
    {
        let mut state = registry.state.write();
        crate::registry::make_first_workstation_remote(
            &mut state,
            "build-node",
            WorkspaceConnectionStatus::Connected,
        );
        state
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .terminal_mut()
            .unwrap()
            .kind = RuntimePaneKind::SystemSsh {
            host: "build-node".to_owned(),
        };
        state.set_pane_status(pane_id, hh_protocol::PaneStatus::Working);
        state.notifications.clear();
    }

    registry.disconnect_workspace(workspace_id).unwrap();
    // The stopped SSH client's own exit is observable now; a disconnect
    // is not an exit: no Done, no notification, no dot.
    crate::registry::identity::refresh_runtime_metadata(&mut registry.state.write());
    let snapshot = registry.snapshot().unwrap();
    let workspace = &snapshot.workspaces[0];

    assert_eq!(pane_ids_for_workspace(workspace), expected_panes);
    assert!(matches!(workspace.tabs[0].layout, PaneLayout::Leaf { .. }));
    assert_eq!(workspace.active_terminal_count, 0);
    assert_eq!(
        workspace.connection,
        WorkspaceConnection::SystemSsh {
            destination: "build-node".to_owned(),
            status: WorkspaceConnectionStatus::Offline,
        }
    );
    assert!(registry.state.read().panes.contains_key(&pane_id));
    let pane = crate::layout::find_pane_in_snapshot(&snapshot, pane_id).unwrap();
    assert_eq!(pane.status, hh_protocol::PaneStatus::Working);
    assert!(!pane.unseen);
    assert_eq!(pane.shell, "system OpenSSH · disconnected");
    assert!(registry.notifications().unwrap().is_empty());
}

#[test]
fn closing_the_last_terminal_keeps_an_ssh_workstation_connected() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let pane_id = first_pane_id(&snapshot).unwrap();
    {
        let mut state = registry.state.write();
        crate::registry::make_first_workstation_remote(
            &mut state,
            "build-node",
            WorkspaceConnectionStatus::Connected,
        );
        state
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .terminal_mut()
            .unwrap()
            .kind = RuntimePaneKind::SystemSsh {
            host: "build-node".to_owned(),
        };
    }

    registry.close_pane(pane_id).unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace = &snapshot.workspaces[0];

    // Zero terminals is an empty workstation, not a disconnect: the next
    // terminal must open instead of demanding a reconnect.
    assert!(workspace.tabs.is_empty());
    assert_eq!(workspace.active_terminal_count, 0);
    assert_eq!(
        workspace.connection,
        WorkspaceConnection::SystemSsh {
            destination: "build-node".to_owned(),
            status: WorkspaceConnectionStatus::Connected,
        }
    );
}

#[test]
fn nested_workstations_run_on_their_parents_machine_up_to_the_depth_limit() {
    let registry = SessionRegistry::new().unwrap();
    let home = registry.snapshot().unwrap().workspaces[0].id;
    let (remote, _) = registry
        .create_simulated_ssh_workspace(None, "test@local-host")
        .unwrap();
    let mut parent = home;
    for _ in 1..MAX_WORKSTATION_DEPTH {
        parent = registry
            .create_workspace(None, Some(parent), None)
            .unwrap()
            .0;
    }
    let error = registry
        .create_workspace(None, Some(parent), None)
        .unwrap_err();
    assert!(error.to_string().contains("at most"), "{error:#}");

    let snapshot = registry.snapshot().unwrap();
    let deepest = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == parent)
        .unwrap();
    assert_eq!(deepest.connection, WorkspaceConnection::Local);
    assert_eq!(
        deepest.title,
        format!("Workstation {}", snapshot.workspaces.len())
    );
    assert_eq!(
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == remote)
            .unwrap()
            .title,
        "test@local-host",
        "an unnamed remote workstation is titled after its destination"
    );

    // Siblings reorder among themselves only.
    let (sibling, _) = registry.create_workspace(None, None, None).unwrap();
    assert!(registry.reorder_workspace(parent, sibling, false).is_err());
    assert!(registry.reorder_workspace(sibling, home, false).is_ok());
}
