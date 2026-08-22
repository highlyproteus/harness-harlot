use hh_protocol::ClientRequest;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{ACTIVE_TERMINAL_POLL_MS, IDLE_TERMINAL_POLL_MS};

pub(crate) fn next_terminal_poll_delay_ms(current: u64, state_changed: bool) -> u64 {
    if state_changed {
        ACTIVE_TERMINAL_POLL_MS
    } else {
        current.saturating_mul(2).min(IDLE_TERMINAL_POLL_MS)
    }
}

pub(crate) fn pane_update_requires_repaint(
    snapshot_delivered: bool,
    screens_delivered: usize,
) -> bool {
    snapshot_delivered || screens_delivered > 0
}

pub(crate) const fn terminal_poll_wake_requested(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::WriteInput { .. }
            | ClientRequest::MouseInput { .. }
            | ClientRequest::BeginSelection { .. }
            | ClientRequest::UpdateSelection { .. }
            | ClientRequest::ClearSelection { .. }
            | ClientRequest::ScrollPane { .. }
    )
}

/// The focused pane streams every poll. Other on-screen panes are paced so a
/// four-way split cannot multiply one pane's payload by four every 33 ms.
pub(crate) fn paced_subscriptions(
    now: Instant,
    on_screen: &[Uuid],
    focused: Option<Uuid>,
    last_delivery: &HashMap<Uuid, Instant>,
    interval: Duration,
) -> Vec<Uuid> {
    on_screen
        .iter()
        .copied()
        .filter(|pane_id| {
            Some(*pane_id) == focused
                || last_delivery
                    .get(pane_id)
                    .is_none_or(|last| now.saturating_duration_since(*last) >= interval)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ACTIVE_TERMINAL_POLL_MS, IDLE_TERMINAL_POLL_MS, next_terminal_poll_delay_ms,
        paced_subscriptions, pane_update_requires_repaint, terminal_poll_wake_requested,
    };
    use hh_protocol::{
        ClientRequest, TerminalModifiers, TerminalMouseAction, TerminalMouseButton, TerminalPoint,
        TerminalSelectionKind,
    };
    use std::collections::HashMap;
    use uuid::Uuid;

    use crate::SECONDARY_PANE_INTERVAL;
    use std::time::{Duration, Instant};

    #[test]
    fn terminal_polling_is_fast_while_output_changes_and_backs_off_when_idle() {
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, true),
            ACTIVE_TERMINAL_POLL_MS
        );
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, true),
            ACTIVE_TERMINAL_POLL_MS
        );
        assert_eq!(
            next_terminal_poll_delay_ms(ACTIVE_TERMINAL_POLL_MS, false),
            ACTIVE_TERMINAL_POLL_MS * 2
        );
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, false),
            IDLE_TERMINAL_POLL_MS
        );
    }

    #[test]
    fn visible_terminal_never_waits_multiple_seconds_after_long_idle() {
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, false),
            IDLE_TERMINAL_POLL_MS,
            "a visible terminal must remain responsive no matter how long it has been idle"
        );
    }

    #[test]
    fn terminal_input_wakes_an_idle_poll_immediately() {
        let pane_id = Uuid::from_u128(1);
        assert!(terminal_poll_wake_requested(&ClientRequest::WriteInput {
            pane_id,
            bytes: b"a".to_vec(),
        }));
        assert!(terminal_poll_wake_requested(&ClientRequest::MouseInput {
            pane_id,
            point: TerminalPoint { row: 0, column: 0 },
            button: TerminalMouseButton::Left,
            action: TerminalMouseAction::Press,
            modifiers: TerminalModifiers::default(),
        }));
        assert!(terminal_poll_wake_requested(
            &ClientRequest::BeginSelection {
                pane_id,
                point: TerminalPoint { row: 0, column: 0 },
                kind: TerminalSelectionKind::Simple,
            }
        ));
        assert!(terminal_poll_wake_requested(
            &ClientRequest::UpdateSelection {
                pane_id,
                point: TerminalPoint { row: 1, column: 2 },
            }
        ));
        assert!(terminal_poll_wake_requested(
            &ClientRequest::ClearSelection { pane_id }
        ));
        assert!(terminal_poll_wake_requested(&ClientRequest::ScrollPane {
            pane_id,
            lines: -3,
        }));
        assert!(!terminal_poll_wake_requested(&ClientRequest::RenameTab {
            tab_id: Uuid::from_u128(2),
            title: "work".to_string(),
        }));
    }

    #[test]
    fn on_screen_panes_stream_with_the_focused_pane_always_and_siblings_paced() {
        let now = Instant::now();
        let focused = Uuid::from_u128(1);
        let sibling = Uuid::from_u128(2);
        let fresh = Uuid::from_u128(3);
        let on_screen = [focused, sibling];
        let recent = HashMap::from([(focused, now), (sibling, now)]);

        assert_eq!(
            paced_subscriptions(
                now,
                &on_screen,
                Some(focused),
                &recent,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused],
            "a sibling delivered this instant waits for its pacing interval"
        );

        let stale = HashMap::from([
            (focused, now),
            (
                sibling,
                now.checked_sub(Duration::from_millis(200)).unwrap(),
            ),
        ]);
        assert_eq!(
            paced_subscriptions(
                now,
                &on_screen,
                Some(focused),
                &stale,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused, sibling]
        );

        assert_eq!(
            paced_subscriptions(
                now,
                &[focused, fresh],
                Some(focused),
                &recent,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused, fresh],
            "a pane never delivered before is always subscribed"
        );

        let untouched_for_an_hour = HashMap::from([
            (focused, now),
            (sibling, now.checked_sub(Duration::from_hours(1)).unwrap()),
        ]);
        assert_eq!(
            paced_subscriptions(
                now,
                &on_screen,
                Some(focused),
                &untouched_for_an_hour,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused, sibling],
            "a visible pane never cools: subscription follows what is on screen, not attention"
        );

        assert!(
            paced_subscriptions(now, &[], Some(focused), &recent, SECONDARY_PANE_INTERVAL)
                .is_empty(),
            "nothing on screen streams nothing"
        );
    }

    #[test]
    fn revision_metadata_alone_does_not_repaint_inactive_panes() {
        assert!(!pane_update_requires_repaint(false, 0));
        assert!(pane_update_requires_repaint(false, 1));
        assert!(pane_update_requires_repaint(true, 0));
    }
}
