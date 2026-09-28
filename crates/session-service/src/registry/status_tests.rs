use super::tests::status_state;
use super::*;
use crate::layout::first_pane_id;
use hh_protocol::ProgressSource;

fn pane(state: &RegistryState, pane_id: Uuid) -> &Pane {
    find_pane_in_snapshot(&state.snapshot, pane_id).unwrap()
}

fn progress(done: u32, total: u32) -> PaneProgress {
    PaneProgress {
        done,
        total,
        current: Some("Write tests".to_owned()),
        phase: None,
        source: ProgressSource::Omp,
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(10));
    }
}

fn state_directory(label: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("hh-{label}-{}", Uuid::new_v4()));
    create_owner_only_directory(&directory);
    directory
}

#[test]
fn entering_done_or_a_needs_you_status_marks_unseen_and_stores_one_notification() {
    let cases = [
        (PaneStatus::Done, NotificationKind::Completed, None),
        (
            PaneStatus::NeedsInput,
            NotificationKind::Attention,
            Some("Needs input"),
        ),
        (
            PaneStatus::NeedsApproval,
            NotificationKind::Attention,
            Some("Needs approval"),
        ),
        (PaneStatus::Attention, NotificationKind::Attention, None),
    ];
    for (status, kind, message) in cases {
        let (mut state, pane_id) = status_state(TerminalProfile::Claude);
        state.set_pane_status(pane_id, PaneStatus::Working);
        assert!(!pane(&state, pane_id).unseen);
        assert!(state.notifications.is_empty());

        let revision = state.snapshot.revision;
        state.set_pane_status(pane_id, status);
        assert!(pane(&state, pane_id).unseen, "{status:?}");
        assert!(state.snapshot.revision > revision);
        assert_eq!(state.notifications.len(), 1, "{status:?}");
        assert_eq!(state.notifications[0].kind, kind);
        assert_eq!(state.notifications[0].message.as_deref(), message);

        // Staying in the status is not a new transition.
        state.set_pane_status(pane_id, status);
        assert_eq!(state.notifications.len(), 1);
        // Leaving it keeps the pane unseen until the user opens it.
        state.set_pane_status(pane_id, PaneStatus::Working);
        assert!(pane(&state, pane_id).unseen);
    }

    let (mut state, pane_id) = status_state(TerminalProfile::Claude);
    state.set_pane_status(pane_id, PaneStatus::Working);
    state.set_pane_status(pane_id, PaneStatus::Idle);
    assert!(!pane(&state, pane_id).unseen);
    assert!(state.notifications.is_empty());
}

#[test]
fn a_bell_and_an_osc_status_each_store_exactly_one_notification() {
    let (mut state, pane_id) = status_state(TerminalProfile::Omp);
    state.set_pane_status(pane_id, PaneStatus::Working);
    state.apply_pane_event(
        pane_id,
        RawPaneEvent {
            kind: NotificationKind::Attention,
            message: None,
            at_ms: 1_000,
        },
    );
    assert_eq!(pane(&state, pane_id).status, PaneStatus::Done);
    assert!(pane(&state, pane_id).unseen);
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications[0].kind, NotificationKind::Completed);

    let (mut state, pane_id) = status_state(TerminalProfile::Codex);
    state.apply_pane_event(
        pane_id,
        RawPaneEvent {
            kind: NotificationKind::Message,
            message: Some("hh-status: needs-input".to_owned()),
            at_ms: 1_000,
        },
    );
    assert!(pane(&state, pane_id).unseen);
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(
        state.notifications[0].message.as_deref(),
        Some("Needs input")
    );
}

#[test]
fn a_repeated_signal_replaces_its_recent_unread_notification_with_a_new_id() {
    let (mut state, pane_id) = status_state(TerminalProfile::Claude);
    let attention = |state: &mut RegistryState, message: &str, at_ms: u64| {
        state.update_pane_status(
            pane_id,
            PaneStatus::Attention,
            StatusNotice::Always(Some(message.to_owned())),
            at_ms,
        );
    };
    attention(&mut state, "first", 10_000);
    attention(&mut state, "second", 12_000);
    let items = state.notifications.iter().collect::<Vec<_>>();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, 2, "the replacement gets a new id");
    assert_eq!(items[0].message.as_deref(), Some("second"));
    assert_eq!(items[0].at_ms, 12_000);

    // Past the window the older one stays.
    attention(&mut state, "third", 17_001);
    assert_eq!(
        state
            .notifications
            .iter()
            .map(|notification| notification.id)
            .collect::<Vec<_>>(),
        [2, 3]
    );

    // A read notification is history and is never replaced.
    state.notifications.back_mut().unwrap().read = true;
    attention(&mut state, "fourth", 17_500);
    assert_eq!(state.notifications.len(), 3);

    // Another kind is not a duplicate.
    state.update_pane_status(
        pane_id,
        PaneStatus::Done,
        StatusNotice::Always(None),
        17_600,
    );
    assert_eq!(state.notifications.len(), 4);
    assert_eq!(
        state.notifications.back().unwrap().kind,
        NotificationKind::Completed
    );
}

#[test]
fn mark_pane_seen_clears_unseen_and_reads_that_panes_notifications() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::NeedsInput);
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::Done);
    let revision = registry.snapshot().unwrap().revision;

    registry.mark_pane_seen(pane_id).unwrap();

    let snapshot = registry.snapshot().unwrap();
    assert!(!find_pane_in_snapshot(&snapshot, pane_id).unwrap().unseen);
    assert!(snapshot.revision > revision);
    let notifications = registry.notifications().unwrap();
    assert_eq!(notifications.len(), 2);
    assert!(notifications.iter().all(|notification| notification.read));
    // Seeing a seen pane changes nothing.
    registry.mark_pane_seen(pane_id).unwrap();
    assert_eq!(registry.snapshot().unwrap().revision, snapshot.revision);
    assert!(registry.mark_pane_seen(Uuid::new_v4()).is_err());
}

#[test]
fn a_title_driven_omp_turn_marks_done_unseen_with_a_completed_notification() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .write_input(
            pane_id,
            "exec sh -c \"printf '\\033]0;π ⠋ fixing\\007'; sleep 1; printf '\\033]0;π > fixing\\007'; sleep 30\"\r".as_bytes(),
        )
        .unwrap();
    let observe = |title: &str| {
        wait_until(
            || {
                registry
                    .state
                    .read()
                    .terminal_pane(pane_id)
                    .unwrap()
                    .session
                    .terminal_title()
                    .as_deref()
                    == Some(title)
            },
            title,
        );
        refresh_runtime_metadata(&mut registry.state.write());
    };
    observe("π ⠋ fixing");
    let snapshot = registry.snapshot().unwrap();
    assert_eq!(
        find_pane_in_snapshot(&snapshot, pane_id).unwrap().status,
        PaneStatus::Working
    );
    observe("π > fixing");
    let snapshot = registry.snapshot().unwrap();
    let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
    assert_eq!(pane.status, PaneStatus::Done);
    assert!(pane.unseen);
    let completed = registry
        .notifications()
        .unwrap()
        .into_iter()
        .filter(|notification| {
            notification.pane_id == pane_id && notification.kind == NotificationKind::Completed
        })
        .count();
    assert_eq!(completed, 1);
}

#[test]
fn progress_is_validated_replaced_and_cleared_when_the_process_exits() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let pane_id = first_pane_id(&snapshot).unwrap();
    let current = || {
        find_pane_in_snapshot(&registry.snapshot().unwrap(), pane_id)
            .unwrap()
            .progress
            .clone()
    };

    registry
        .report_pane_progress(pane_id, Some(progress(1, 3)))
        .unwrap();
    assert_eq!(current(), Some(progress(1, 3)));
    let revision = registry.snapshot().unwrap().revision;
    registry
        .report_pane_progress(pane_id, Some(progress(1, 3)))
        .unwrap();
    assert_eq!(
        registry.snapshot().unwrap().revision,
        revision,
        "an unchanged report does not bump the revision"
    );

    assert!(
        registry
            .report_pane_progress(pane_id, Some(progress(4, 3)))
            .is_err()
    );
    assert!(
        registry
            .report_pane_progress(Uuid::new_v4(), Some(progress(1, 3)))
            .is_err()
    );
    let browser = registry
        .create_browser_tab(snapshot.workspaces[0].id, None)
        .unwrap();
    assert!(
        registry
            .report_pane_progress(browser, Some(progress(1, 3)))
            .is_err()
    );
    assert_eq!(
        current(),
        Some(progress(1, 3)),
        "rejections keep the last report"
    );

    registry.report_pane_progress(pane_id, None).unwrap();
    assert_eq!(current(), None);
    registry
        .report_pane_progress(pane_id, Some(progress(2, 3)))
        .unwrap();

    registry
        .write_input(pane_id, b"exec sh -c 'exit 0'\r")
        .unwrap();
    // Wait for the exit itself: until then a report is accepted (and an
    // identity refresh may already drop progress the shell never owned).
    wait_until(
        || {
            refresh_runtime_metadata(&mut registry.state.write());
            registry
                .report_pane_progress(pane_id, Some(progress(2, 3)))
                .is_err()
        },
        "an exited pane accepts no progress",
    );
    assert_eq!(current(), None, "exit clears progress");
}

#[test]
fn notifications_unseen_and_ids_survive_a_restart_under_a_new_epoch() {
    let directory = state_directory("notification-ring");
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::NeedsApproval);
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::Done);
    registry
        .report_pane_progress(pane_id, Some(progress(1, 2)))
        .unwrap();
    let before = registry.pane_updates(None, &[], &[], false, 0).unwrap();
    let old_items = before.notifications.clone();
    assert_eq!(old_items.len(), 2);
    let cursor = old_items.last().unwrap().id;
    registry.persist().unwrap();

    let file = directory.join("notifications.json");
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(registry);

    let restarted = SessionRegistry::persistent(&path).unwrap();
    assert_eq!(restarted.notifications().unwrap(), old_items);
    let snapshot = restarted.snapshot().unwrap();
    let restored = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
    assert!(restored.unseen, "unseen survives a restart");
    assert_eq!(
        restored.progress, None,
        "a fresh shell does not inherit the old process's progress"
    );

    // A client polling with its old cursor sees the epoch change.
    let after = restarted
        .pane_updates(None, &[], &[], false, cursor)
        .unwrap();
    assert_ne!(after.notifications_epoch, before.notifications_epoch);
    assert!(after.notifications.is_empty());

    restarted
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::Attention);
    let newest = restarted
        .pane_updates(None, &[], &[], false, cursor)
        .unwrap()
        .notifications;
    assert_eq!(newest.len(), 1);
    assert!(newest[0].id > cursor, "ids keep increasing across restarts");
    drop(restarted);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_corrupt_notification_file_is_quarantined_and_the_ring_starts_empty() {
    let directory = state_directory("notification-corrupt");
    let file = directory.join("notifications.json");
    hh_protocol::atomic_write_private(&file, b"{not json").unwrap();

    let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
    assert!(registry.notifications().unwrap().is_empty());
    let quarantined = std::fs::read_dir(&directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("notifications.corrupt-")
        })
        .count();
    assert_eq!(quarantined, 1);

    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::Done);
    assert_eq!(registry.notifications().unwrap()[0].id, 1);
    drop(registry);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_needs_you_message_is_stored_as_attention_with_its_own_text() {
    let cases = [
        (
            TerminalProfile::Codex,
            "Approval requested: rm -rf target",
            PaneStatus::NeedsApproval,
            NotificationKind::Attention,
        ),
        (
            TerminalProfile::Omp,
            "Waiting for input",
            PaneStatus::NeedsInput,
            NotificationKind::Attention,
        ),
        (
            TerminalProfile::Codex,
            "Agent turn complete",
            PaneStatus::Done,
            NotificationKind::Message,
        ),
    ];
    for (profile, message, status, kind) in cases {
        let (mut state, pane_id) = status_state(profile);
        state.apply_pane_event(
            pane_id,
            RawPaneEvent {
                kind: NotificationKind::Message,
                message: Some(message.to_owned()),
                at_ms: 1_000,
            },
        );
        assert_eq!(pane(&state, pane_id).status, status, "{message}");
        assert_eq!(state.notifications.len(), 1, "{message}");
        assert_eq!(state.notifications[0].kind, kind, "{message}");
        assert_eq!(state.notifications[0].message.as_deref(), Some(message));
    }
}

/// The bell of an omp approval can arrive before the refresh reads omp's
/// `π !` title; the bell itself must read it.
#[test]
fn an_omp_bell_reads_the_current_title_and_never_records_an_approval_as_completed() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .write_input(
            pane_id,
            "exec sh -c \"printf '\\033]0;π ! approve edit\\007'; sleep 30\"\r".as_bytes(),
        )
        .unwrap();
    wait_until(
        || {
            registry
                .state
                .read()
                .terminal_pane(pane_id)
                .unwrap()
                .session
                .terminal_title()
                .as_deref()
                == Some("π ! approve edit")
        },
        "the approval title",
    );
    let mut state = registry.state.write();
    find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
        .unwrap()
        .identity
        .profile = TerminalProfile::Omp;
    // The stored status still says the turn is running.
    state.set_pane_status(pane_id, PaneStatus::Working);
    state.apply_pane_event(
        pane_id,
        RawPaneEvent {
            kind: NotificationKind::Attention,
            message: None,
            at_ms: crate::now_ms(),
        },
    );
    assert_eq!(pane(&state, pane_id).status, PaneStatus::NeedsApproval);
    assert!(
        state
            .notifications
            .iter()
            .all(|notification| notification.kind != NotificationKind::Completed),
        "{:?}",
        state.notifications
    );
    assert!(
        state
            .notifications
            .iter()
            .any(|notification| notification.kind == NotificationKind::Attention)
    );
}

#[test]
fn an_older_event_never_deletes_a_newer_unread_notification() {
    let (mut state, pane_id) = status_state(TerminalProfile::Claude);
    let attention = |state: &mut RegistryState, message: &str, at_ms: u64| {
        state.update_pane_status(
            pane_id,
            PaneStatus::Attention,
            StatusNotice::Always(Some(message.to_owned())),
            at_ms,
        );
    };
    attention(&mut state, "newer", 100_000);
    // A bell queued a minute earlier, processed late.
    attention(&mut state, "older", 40_000);
    let messages = state
        .notifications
        .iter()
        .map(|notification| notification.message.as_deref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(messages, ["newer", "older"]);
}

#[test]
fn the_periodic_save_records_queued_pane_events_without_a_polling_client() {
    let directory = state_directory("queued-events");
    let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry.write_input(pane_id, b"printf '\\a'\r").unwrap();
    wait_until(
        || registry.pane(pane_id).unwrap().has_pending_events(),
        "the bell event",
    );

    registry.persist().unwrap();

    let stored = NotificationStore::in_state_directory(&directory).load_or_quarantine();
    assert!(
        stored
            .items
            .iter()
            .any(|notification| notification.pane_id == pane_id
                && notification.kind == NotificationKind::Attention),
        "{:?}",
        stored.items
    );
    drop(registry);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_seen_pane_stays_seen_after_a_crash_before_the_next_save() {
    let directory = state_directory("seen-crash");
    let path = directory.join("sessions.json");
    let registry = SessionRegistry::persistent(&path).unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .state
        .write()
        .set_pane_status(pane_id, PaneStatus::Done);
    registry.persist().unwrap();

    registry.mark_pane_seen(pane_id).unwrap();
    // A crash: no further save runs.
    drop(registry);

    let restarted = SessionRegistry::persistent(&path).unwrap();
    let snapshot = restarted.snapshot().unwrap();
    assert!(!find_pane_in_snapshot(&snapshot, pane_id).unwrap().unseen);
    assert!(
        restarted
            .notifications()
            .unwrap()
            .iter()
            .all(|notification| notification.read)
    );
    drop(restarted);
    std::fs::remove_dir_all(directory).unwrap();
}

/// A pane HH detached from (a disconnect, a bot restart, a lost connection,
/// a failed reattach) still runs its program: the transport's own exit
/// afterwards is not news.
#[test]
fn a_detached_pane_only_changes_its_label() {
    for reason in [
        identity::PANE_DISCONNECTED,
        identity::PANE_RESTARTING,
        identity::PANE_CONNECTION_LOST,
        "not reattached: window @3 is gone",
    ] {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry
            .report_pane_progress(pane_id, Some(progress(1, 3)))
            .unwrap();
        let session = {
            let mut state = registry.state.write();
            state.set_pane_status(pane_id, PaneStatus::NeedsApproval);
            state.notifications.clear();
            let terminal = state.terminal_pane_mut(pane_id).unwrap();
            terminal.exit_status = Some(reason.to_owned());
            Arc::clone(&terminal.session)
        };
        // Marked first, then the transport really ends.
        session.terminate_and_wait().unwrap();
        wait_until(
            || session.exit_status().is_ok_and(|status| status.is_some()),
            "the transport exit",
        );
        let mut state = registry.state.write();
        refresh_runtime_metadata(&mut state);
        let pane = pane(&state, pane_id);
        assert_eq!(pane.status, PaneStatus::NeedsApproval, "{reason}");
        assert_eq!(pane.progress, Some(progress(1, 3)), "{reason}");
        assert!(!pane.shell.contains("exited"), "{}", pane.shell);
        assert!(state.notifications.is_empty(), "{reason}");
        assert_eq!(
            state.terminal_pane(pane_id).unwrap().exit_status.as_deref(),
            Some(reason)
        );
    }
}

/// After a reattach the program restates its state in its title: the first
/// omp title is a baseline, later changes notify as usual.
#[test]
fn the_first_omp_title_after_a_reattach_is_a_silent_baseline() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .state
        .write()
        .terminal_pane_mut(pane_id)
        .unwrap()
        .title_baseline_pending = true;
    registry
        .write_input(
            pane_id,
            "exec sh -c \"printf '\\033]0;π ! approve\\007'; sleep 1; printf '\\033]0;π ⠋ edit\\007'; sleep 1; printf '\\033]0;π ! again\\007'; sleep 30\"\r".as_bytes(),
        )
        .unwrap();
    let observe = |title: &str| {
        wait_until(
            || {
                registry
                    .state
                    .read()
                    .terminal_pane(pane_id)
                    .unwrap()
                    .session
                    .terminal_title()
                    .as_deref()
                    == Some(title)
            },
            title,
        );
        let mut state = registry.state.write();
        refresh_runtime_metadata(&mut state);
        let pane = pane(&state, pane_id);
        (pane.status, pane.unseen, state.notifications.len())
    };

    assert_eq!(
        observe("π ! approve"),
        (PaneStatus::NeedsApproval, false, 0),
        "the restated approval is where the pane already was"
    );
    assert_eq!(observe("π ⠋ edit"), (PaneStatus::Working, false, 0));
    assert_eq!(observe("π ! again"), (PaneStatus::NeedsApproval, true, 1));
}
