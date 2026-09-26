use super::*;
use hh_protocol::{DropPlacement, WorkspaceConnectionStatus};

#[test]
fn retired_history_archive_is_removed_without_following_a_symlink() {
    let root = std::env::temp_dir().join(format!("hh-retired-history-{}", Uuid::new_v4()));
    let state = root.join("state");
    let archive = state.join("history");
    std::fs::create_dir_all(archive.join("sessions")).unwrap();
    std::fs::write(archive.join("sessions/chunk"), b"raw output").unwrap();
    remove_retired_history_archive(&state);
    assert!(!archive.exists());

    let target = root.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("keep"), b"unrelated").unwrap();
    std::os::unix::fs::symlink(&target, &archive).unwrap();
    remove_retired_history_archive(&state);
    assert!(std::fs::symlink_metadata(&archive).is_err());
    assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"unrelated");

    remove_retired_history_archive(&state);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_changed_at_ms_advances_only_on_real_transitions() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let changed_at = || {
        find_pane_in_snapshot(&registry.snapshot().unwrap(), pane_id)
            .unwrap()
            .status_changed_at_ms
    };
    assert_eq!(changed_at(), 0);

    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::Working);
    let working_since = changed_at();
    assert!(working_since > 0);
    thread::sleep(Duration::from_millis(5));
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::Working);
    assert_eq!(changed_at(), working_since);
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::NeedsInput);
    assert!(changed_at() > working_since);
}

#[test]
fn service_shutdown_requires_zero_live_terminals() {
    let registry = SessionRegistry::new().unwrap();
    assert!(registry.request_shutdown().is_err());
    assert!(!registry.shutdown_requested());

    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry.close_pane(pane_id).unwrap();
    registry.request_shutdown().unwrap();
    assert!(registry.shutdown_requested());
}

fn status_state(profile: TerminalProfile) -> (RegistryState, Uuid) {
    let mut snapshot = SessionSnapshot::seeded();
    let pane_id = first_pane_id(&snapshot).unwrap();
    let pane = find_pane_mut_in_snapshot(&mut snapshot, pane_id).unwrap();
    pane.identity.profile = profile;
    (
        RegistryState {
            snapshot,
            panes: HashMap::new(),
            tmux: None,
            tmux_clients: HashMap::new(),
            tmux_sinks: HashMap::new(),
            notifications: VecDeque::new(),
            next_notification_id: 1,
            next_terminal_number: 2,
            next_group_number: 1,
            last_identity_refresh: None,
        },
        pane_id,
    )
}

#[test]
fn contract_event_is_swallowed_and_synthesizes_attention() {
    let (mut state, pane_id) = status_state(TerminalProfile::Omp);
    state.apply_pane_event(
        pane_id,
        RawPaneEvent {
            kind: NotificationKind::Message,
            message: Some("hh-status: needs-approval".to_owned()),
            at_ms: 7,
        },
    );

    assert_eq!(
        find_pane_in_snapshot(&state.snapshot, pane_id)
            .unwrap()
            .status,
        PaneStatus::NeedsApproval
    );
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications[0].kind, NotificationKind::Attention);
    assert_eq!(
        state.notifications[0].message.as_deref(),
        Some("needs approval")
    );
}

#[test]
fn heuristic_event_sets_status_and_preserves_message() {
    let (mut state, pane_id) = status_state(TerminalProfile::Codex);
    state.apply_pane_event(
        pane_id,
        RawPaneEvent {
            kind: NotificationKind::Message,
            message: Some("Approval requested: edit src/lib.rs".to_owned()),
            at_ms: 8,
        },
    );

    assert_eq!(
        find_pane_in_snapshot(&state.snapshot, pane_id)
            .unwrap()
            .status,
        PaneStatus::NeedsApproval
    );
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications[0].kind, NotificationKind::Message);
    assert_eq!(
        state.notifications[0].message.as_deref(),
        Some("Approval requested: edit src/lib.rs")
    );
}

#[test]
fn agent_bell_upgrades_approval_to_input() {
    let (mut state, pane_id) = status_state(TerminalProfile::Omp);
    state.set_pane_status(pane_id, PaneStatus::NeedsApproval);
    state.apply_pane_event(
        pane_id,
        RawPaneEvent {
            kind: NotificationKind::Attention,
            message: None,
            at_ms: 9,
        },
    );

    assert_eq!(
        find_pane_in_snapshot(&state.snapshot, pane_id)
            .unwrap()
            .status,
        PaneStatus::NeedsInput
    );
    assert_eq!(state.notifications[0].kind, NotificationKind::Attention);
}

#[test]
fn omp_bell_after_a_turn_is_done_but_other_agents_still_need_you() {
    let bell = || RawPaneEvent {
        kind: NotificationKind::Attention,
        message: None,
        at_ms: 10,
    };
    let status = |state: &RegistryState, pane_id| {
        find_pane_in_snapshot(&state.snapshot, pane_id)
            .unwrap()
            .status
    };

    let (mut omp, pane_id) = status_state(TerminalProfile::Omp);
    omp.set_pane_status(pane_id, PaneStatus::Working);
    omp.apply_pane_event(pane_id, bell());
    assert_eq!(status(&omp, pane_id), PaneStatus::Done);
    assert_eq!(omp.notifications[0].kind, NotificationKind::Completed);

    let (mut claude, pane_id) = status_state(TerminalProfile::Claude);
    claude.set_pane_status(pane_id, PaneStatus::Working);
    claude.apply_pane_event(pane_id, bell());
    assert_eq!(status(&claude, pane_id), PaneStatus::Attention);
    assert_eq!(claude.notifications[0].kind, NotificationKind::Attention);
}

#[test]
fn pane_input_clears_stale_prompt_status() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::NeedsInput);

    registry.write_input(pane_id, b"x").unwrap();

    assert_eq!(
        find_pane_in_snapshot(&registry.snapshot().unwrap(), pane_id)
            .unwrap()
            .status,
        PaneStatus::Working
    );
}

#[test]
fn local_runtime_replacement_inside_ssh_workstation_projects_local_transport() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    {
        let mut state = registry.state.write();
        state.snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
            destination: "developer@build-node".to_owned(),
            status: WorkspaceConnectionStatus::Connected,
        };
    }

    registry
        .move_pane_to_split(pane_id, pane_id, DropPlacement::Right)
        .unwrap();
    let snapshot = registry.snapshot().unwrap();
    let pane_ids = pane_ids_in_snapshot(&snapshot);

    assert_eq!(pane_ids.len(), 2);
    assert!(pane_ids.iter().all(|pane_id| {
        snapshot.terminal_transports.get(pane_id) == Some(&TerminalTransport::Local)
    }));
}

#[test]
fn browser_command_times_out_when_no_executor_is_polling() {
    let registry = SessionRegistry::new().unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let browser = registry.create_browser_tab(workspace_id, None).unwrap();

    let outcome = registry
        .browser_command_with_timeout(browser, BrowserAction::Reload, Duration::from_millis(50))
        .unwrap();

    assert!(matches!(
        outcome,
        BrowserCommandOutcome::Error { message } if message.contains("did not answer")
    ));
}

#[test]
fn browser_command_result_resolves_the_waiter() {
    let registry = SessionRegistry::new().unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let browser = registry.create_browser_tab(workspace_id, None).unwrap();
    let executor = registry.clone();
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        let commands = executor.take_browser_commands();
        assert_eq!(commands.len(), 1);
        executor.resolve_browser_command(
            commands[0].request_id,
            BrowserCommandOutcome::Ok {
                result: serde_json::json!({"value": 2}),
            },
        );
    });

    let outcome = registry
        .browser_command(browser, BrowserAction::Reload)
        .unwrap();
    handle.join().unwrap();

    assert_eq!(
        outcome,
        BrowserCommandOutcome::Ok {
            result: serde_json::json!({"value": 2}),
        }
    );
}
