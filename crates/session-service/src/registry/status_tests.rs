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
