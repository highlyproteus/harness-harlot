use std::thread;
use std::time::{Duration, Instant};

use hh_protocol::{NotificationKind, PaneLayout};
use hh_session_service::SessionRegistry;
use uuid::Uuid;

#[test]
fn pane_signals_reach_the_feed_and_read_state_is_shared() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane(&registry);
    registry
        .write_input(
            pane_id,
            b"exec sh -c \"printf '\\a'; printf '\\033]9;approval needed\\a'; sleep 30\"\r",
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let notifications = loop {
        let update = registry.pane_updates(None, &[], &[], false, 0).unwrap();
        let has_attention = update.notifications.iter().any(|notification| {
            notification.pane_id == pane_id && notification.kind == NotificationKind::Attention
        });
        let has_message = update.notifications.iter().any(|notification| {
            notification.pane_id == pane_id
                && notification.kind == NotificationKind::Message
                && notification.message.as_deref() == Some("approval needed")
        });
        if has_attention && has_message {
            break update.notifications;
        }
        assert!(
            Instant::now() < deadline,
            "notification signals were not delivered"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let ids = notifications
        .iter()
        .filter(|notification| notification.pane_id == pane_id)
        .map(|notification| notification.id)
        .collect::<Vec<_>>();
    registry.mark_notifications_read(&ids);

    let feed = registry.notifications().unwrap();
    assert!(
        feed.iter()
            .filter(|notification| ids.contains(&notification.id))
            .all(|notification| notification.read)
    );
}

#[test]
fn process_exit_reaches_updates_as_a_completion_notification() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane(&registry);
    registry
        .write_input(pane_id, b"exec sh -c 'exit 0'\r")
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let update = registry.pane_updates(None, &[], &[], false, 0).unwrap();
        if update.notifications.iter().any(|notification| {
            notification.pane_id == pane_id && notification.kind == NotificationKind::Completed
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "completion notification was not delivered"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn first_pane(registry: &SessionRegistry) -> Uuid {
    let snapshot = registry.snapshot().unwrap();
    match &snapshot.workspaces[0].tabs[0].layout {
        PaneLayout::Leaf { pane } => pane.id,
        other => panic!("unexpected initial layout: {other:?}"),
    }
}
