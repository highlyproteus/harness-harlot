//! Pure reconciliation of service update payloads into desktop state.
//!
//! [`reconcile_updates`] is the entire merge of one `Updates` response into
//! session/sidebar/layout state with every side effect (tab reassertion,
//! focus resync, notification refetch, dock badge) returned as data. Keeping
//! it pure makes snapshot reconciliation testable without a service.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

use hh_protocol::{PaneStreamState, SessionNotification, SessionSnapshot, TerminalScreen};
use uuid::Uuid;

use crate::helpers::{
    FocusResync, find_pane, focus_resync_for, pane_update_requires_repaint,
    workspace_is_selectable, workspace_visible_panes,
};
use crate::view_models::SplitControlId;
use crate::{LayoutUi, SessionState, SidebarUi};

/// Side effects requested by one reconciliation pass. The caller performs
/// them after the pure merge.
#[derive(Debug, Default)]
pub(crate) struct ReconcileOutcome {
    pub state_changed: bool,
    /// A snapshot shrank the visible stack under a still-focused pane;
    /// reassert its tab on the service.
    pub reassert_tab: Option<Uuid>,
    /// Focus moved to a pane the desktop has not focused yet; resync its
    /// snapshot.
    pub focus_resync: Option<Uuid>,
    /// The notification mirror belongs to a replaced ring, or skipped items;
    /// refetch the full list.
    pub notifications_need_refresh: bool,
}

/// The `Updates` payload fields the reducer consumes.
pub(crate) struct UpdatePayload {
    pub session_revision: u64,
    pub snapshot: Option<SessionSnapshot>,
    pub screens: Vec<TerminalScreen>,
    pub pane_states: Vec<PaneStreamState>,
    pub notification_deltas: Vec<SessionNotification>,
    pub notifications_epoch: Uuid,
}

const NOTIFICATION_RING_CAPACITY: usize = 200;

/// Merges one update payload, returning which side effects to perform.
pub(crate) fn reconcile_updates(
    session: &mut SessionState,
    sidebar: &mut SidebarUi,
    layout: &mut LayoutUi,
    zoom_levels: &mut HashMap<Uuid, i8>,
    payload: UpdatePayload,
    delivered_at: Instant,
) -> ReconcileOutcome {
    let UpdatePayload {
        session_revision,
        snapshot,
        screens,
        pane_states,
        notification_deltas,
        notifications_epoch,
    } = payload;
    let mut outcome = ReconcileOutcome::default();
    let current_session_revision = session.snapshot.as_ref().map(|snapshot| snapshot.revision);
    let topology_is_current =
        current_session_revision.is_none_or(|current| session_revision >= current);
    let mut snapshot_changed = false;
    let mut screens_applied = 0_usize;
    if let Some(snapshot) = snapshot
        && current_session_revision.is_none_or(|current| snapshot.revision >= current)
    {
        snapshot_changed = session.snapshot.as_ref() != Some(&snapshot);
        if sidebar.active_workspace.is_none()
            || !snapshot.workspaces.iter().any(|workspace| {
                Some(workspace.id) == sidebar.active_workspace && workspace_is_selectable(workspace)
            })
        {
            sidebar.active_workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| !workspace.is_bot() && workspace_is_selectable(workspace))
                .map(|workspace| workspace.id);
        }
        let visible = sidebar
            .active_workspace
            .and_then(|active| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == active)
            })
            .map(workspace_visible_panes)
            .unwrap_or_default();
        if layout
            .zoomed_pane
            .is_some_and(|pane| !visible.contains(&pane))
        {
            layout.zoomed_pane = None;
            layout.last_sizes.clear();
        }
        let focused_exists = layout.focused_pane.is_some_and(|pane_id| {
            sidebar.active_workspace.is_some_and(|active| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == active)
                    .is_some_and(|workspace| {
                        workspace
                            .tabs
                            .iter()
                            .any(|tab| find_pane(&tab.layout, pane_id).is_some())
                    })
            })
        });
        match focus_resync_for(&visible, layout.focused_pane, focused_exists) {
            FocusResync::Keep => {}
            FocusResync::Reassert(pane_id) => outcome.reassert_tab = Some(pane_id),
            FocusResync::Switch(pane_id) => outcome.focus_resync = Some(pane_id),
            FocusResync::Clear => layout.focused_pane = None,
        }
        let live_tab_ids = snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter().map(|tab| tab.id))
            .collect::<HashSet<_>>();
        sidebar
            .dismissed_workspace_tabs
            .retain(|tab_id| live_tab_ids.contains(tab_id));
        session.snapshot = Some(snapshot);
    }
    for screen in screens {
        let is_newer = session
            .screens
            .get(&screen.pane_id)
            .is_none_or(|current| screen.revision > current.revision);
        if is_newer {
            session.last_delivery.insert(screen.pane_id, delivered_at);
            session.screens.insert(screen.pane_id, screen);
            screens_applied += 1;
        }
    }
    if topology_is_current {
        let live_panes = pane_states
            .iter()
            .map(|state| state.pane_id)
            .collect::<HashSet<_>>();
        session
            .screens
            .retain(|pane_id, _| live_panes.contains(pane_id));
        session
            .last_delivery
            .retain(|pane_id, _| live_panes.contains(pane_id));
        layout.split_ratios.retain(|id: &SplitControlId, _| {
            live_panes.contains(&id.first) && live_panes.contains(&id.second)
        });
        zoom_levels.retain(|pane_id, _| live_panes.contains(pane_id));
        session.pane_states = pane_states
            .into_iter()
            .map(|state| (state.pane_id, state))
            .collect();
    }
    let notifications = sync_notifications(
        &mut session.notifications,
        &mut session.notifications_latest_id,
        session.notifications_epoch,
        &mut session.notifications_reloading,
        notifications_epoch,
        notification_deltas,
    );
    outcome.notifications_need_refresh = notifications.refresh;
    let notifications_changed = notifications.changed;
    let connection_changed = session.connection_error.take().is_some();
    session.connection_error = None;
    outcome.state_changed = pane_update_requires_repaint(snapshot_changed, screens_applied)
        || connection_changed
        || notifications_changed;
    outcome
}

/// What one `Updates` payload did to the notification mirror.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct NotificationSync {
    pub changed: bool,
    pub refresh: bool,
}

/// Merges notification deltas into the mirror.
///
/// A new `epoch` means the service replaced its ring (restart), so ids from
/// the old ring mean nothing: the mirror keeps showing the previous list and
/// badge, ignores deltas, and asks once for the full reload that replaces
/// it (`reloading` stays set until that reload answers).
///
/// Within one epoch, deltas at or below the cursor are late duplicates and
/// ignored; newer ones are applied directly, replacing their unread
/// predecessors by the service's own rule. Only a gap in the ids (items the
/// service replaced or dropped before this poll saw them) asks for a reload.
pub(crate) fn sync_notifications(
    notifications: &mut Vec<SessionNotification>,
    latest_id: &mut u64,
    current_epoch: Option<Uuid>,
    reloading: &mut bool,
    epoch: Uuid,
    deltas: Vec<SessionNotification>,
) -> NotificationSync {
    if current_epoch != Some(epoch) {
        return NotificationSync {
            changed: false,
            refresh: !std::mem::replace(reloading, true),
        };
    }
    let mut fresh = deltas
        .into_iter()
        .filter(|notification| notification.id > *latest_id)
        .collect::<Vec<_>>();
    if fresh.is_empty() {
        return NotificationSync::default();
    }
    fresh.sort_by_key(|notification| notification.id);
    let gap = fresh
        .iter()
        .zip(latest_id.saturating_add(1)..)
        .any(|(notification, expected)| notification.id != expected);
    for incoming in fresh {
        notifications.retain(|existing| !existing.replaced_by(&incoming));
        *latest_id = incoming.id;
        notifications.push(incoming);
    }
    let overflow = notifications
        .len()
        .saturating_sub(NOTIFICATION_RING_CAPACITY);
    notifications.drain(..overflow);
    NotificationSync {
        changed: true,
        refresh: gap && !std::mem::replace(reloading, true),
    }
}

/// Applies a full notification reload: the mirror, its cursor, and its
/// epoch become the service's, and deltas apply again.
pub(crate) fn replace_notifications(
    session: &mut SessionState,
    items: Vec<SessionNotification>,
    epoch: Uuid,
) {
    session.notifications_latest_id = items
        .iter()
        .map(|notification| notification.id)
        .max()
        .unwrap_or(0);
    session.notifications = items;
    session.notifications_epoch = Some(epoch);
    session.notifications_reloading = false;
}
#[cfg(test)]
mod tests {
    use super::*;

    use crate::view_models::{DragHoverState, SidebarResizeLifecycle};
    use gpui::ScrollHandle;
    use hh_protocol::{NotificationKind, StreamDiagnostics, TerminalModes, TerminalProfile};

    const EPOCH: Uuid = Uuid::from_u128(0xe90c);
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn session_state() -> SessionState {
        SessionState {
            stream_client: Arc::new(Mutex::new(None)),
            control_client: Arc::new(Mutex::new(None)),
            input_client: Arc::new(Mutex::new(None)),
            stream_tx: crate::pipeline::bounded_lane(crate::pipeline::STREAM_PIPELINE_CAPACITY).0,
            control_tx: crate::pipeline::bounded_lane(crate::pipeline::CONTROL_PIPELINE_CAPACITY).0,
            terminal_input_tx: crate::pipeline::terminal_input_channel(
                crate::pipeline::TERMINAL_INPUT_CAPACITY_BYTES,
            )
            .0,
            poll_wake_tx: futures::channel::mpsc::channel(1).0,
            snapshot: None,
            screens: HashMap::new(),
            pane_states: HashMap::new(),
            notifications: Vec::new(),
            notifications_latest_id: 0,
            notifications_epoch: Some(EPOCH),
            notifications_reloading: false,
            dock_badge: None,
            last_delivery: HashMap::new(),
            window_active: true,
            stream_diagnostics: StreamDiagnostics::default(),
            connection_error: None,
        }
    }

    fn sidebar() -> SidebarUi {
        SidebarUi {
            active_workspace: None,
            expanded_workspaces: HashSet::new(),
            collapsed_pinned_sections: HashSet::new(),
            dismissed_workspace_tabs: HashSet::new(),
            workstation_tab_scroll: ScrollHandle::new(),
            dragging_workspace: None,
            workspace_drop_preview: None,
            suppress_workspace_click_until: None,
            tab_drop_preview: None,
            tab_drop_workspace: None,
            suppress_tab_click_until: None,
            sidebar_resize: SidebarResizeLifecycle::default(),
            preferred_sidebar_width: 200.0,
            sidebar_visible: true,
            sidebar_mode: crate::view_models::SidebarMode::Workstations,
            notifications_return: crate::view_models::SidebarMode::Workstations,
            return_workstation: None,
            sidebar_pixels: 200.0,
            workstation_banner: None,
            workstation_banner_hidden: false,
        }
    }

    fn layout() -> LayoutUi {
        LayoutUi {
            focused_pane: None,
            split_ratios: HashMap::new(),
            zoomed_pane: None,
            resizing: None,
            dragging_pane: None,
            drag_hover: DragHoverState::default(),
            selection_drag: None,
            selection_autoscroll: None,
            autoscroll_generation: 0,
            scroll_residual: HashMap::new(),
            last_sizes: HashMap::new(),
            resize_generation: 0,
            workspace_pixels: (0.0, 0.0),
        }
    }

    fn snapshot_with_revision(revision: u64) -> SessionSnapshot {
        let mut snapshot = SessionSnapshot::seeded();
        snapshot.revision = revision;
        snapshot
    }

    fn screen(pane_id: Uuid, revision: u64) -> TerminalScreen {
        TerminalScreen {
            pane_id,
            revision,
            content_revision: revision,
            columns: 80,
            rows: 24,
            lines: Vec::new(),
            cursor: None,
            selection: None,
            display_offset: 0,
            history_size: 0,
            modes: TerminalModes::default(),
            images: Vec::new(),
        }
    }

    fn notification(id: u64) -> SessionNotification {
        SessionNotification {
            id,
            pane_id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            kind: NotificationKind::Completed,
            message: None,
            pane_title: "t".to_owned(),
            workspace_title: "w".to_owned(),
            profile: TerminalProfile::default(),
            at_ms: 0,
            read: false,
        }
    }

    #[test]
    fn older_revision_snapshot_is_ignored_but_screens_apply() {
        let pane_id = Uuid::new_v4();
        let mut session = session_state();
        session.snapshot = Some(snapshot_with_revision(10));
        let mut sidebar = sidebar();
        let mut layout = layout();
        let outcome = reconcile_updates(
            &mut session,
            &mut sidebar,
            &mut layout,
            &mut HashMap::new(),
            UpdatePayload {
                session_revision: 9,
                snapshot: Some(snapshot_with_revision(9)),
                screens: vec![screen(pane_id, 5)],
                pane_states: vec![PaneStreamState {
                    pane_id,
                    revision: 5,
                    subscribed: true,
                    dirty: false,
                    exited: false,
                    enhanced_paste: false,
                }],
                notification_deltas: Vec::new(),
                notifications_epoch: EPOCH,
            },
            Instant::now(),
        );
        assert_eq!(session.snapshot.as_ref().unwrap().revision, 10);
        assert!(session.screens.contains_key(&pane_id));
        assert!(outcome.state_changed, "screens alone must count as change");
    }

    #[test]
    fn topology_current_update_prunes_dead_panes() {
        let living = Uuid::new_v4();
        let dead = Uuid::new_v4();
        let mut session = session_state();
        session.snapshot = Some(snapshot_with_revision(7));
        session.screens.insert(living, screen(living, 1));
        session.screens.insert(dead, screen(dead, 1));
        session.last_delivery.insert(dead, Instant::now());
        let mut sidebar = sidebar();
        sidebar.active_workspace = session.snapshot.as_ref().map(|s| s.workspaces[0].id);
        let mut layout = layout();
        layout.split_ratios.insert(
            SplitControlId {
                first: dead,
                second: living,
            },
            0.5,
        );
        let mut zoom = HashMap::new();
        zoom.insert(dead, 2);
        let outcome = reconcile_updates(
            &mut session,
            &mut sidebar,
            &mut layout,
            &mut zoom,
            UpdatePayload {
                session_revision: 8,
                snapshot: None,
                screens: Vec::new(),
                pane_states: vec![PaneStreamState {
                    pane_id: living,
                    revision: 1,
                    subscribed: true,
                    dirty: false,
                    exited: false,
                    enhanced_paste: false,
                }],
                notification_deltas: Vec::new(),
                notifications_epoch: EPOCH,
            },
            Instant::now(),
        );
        assert!(!session.screens.contains_key(&dead));
        assert!(!session.last_delivery.contains_key(&dead));
        assert!(session.screens.contains_key(&living));
        assert!(layout.split_ratios.is_empty());
        assert!(!zoom.contains_key(&dead));
        assert!(session.pane_states.contains_key(&living));
        assert!(!outcome.state_changed, "nothing visible changed");
    }

    fn deltas(
        session: &mut SessionState,
        epoch: Uuid,
        deltas: Vec<SessionNotification>,
    ) -> ReconcileOutcome {
        reconcile_updates(
            session,
            &mut sidebar(),
            &mut layout(),
            &mut HashMap::new(),
            UpdatePayload {
                session_revision: 0,
                snapshot: None,
                screens: Vec::new(),
                pane_states: Vec::new(),
                notification_deltas: deltas,
                notifications_epoch: epoch,
            },
            Instant::now(),
        )
    }

    #[test]
    fn a_new_epoch_keeps_the_previous_list_until_the_full_reload_replaces_it() {
        let mut session = session_state();
        session.notifications_latest_id = 30;
        session.notifications = vec![notification(29), notification(30)];
        let shown = session.notifications.clone();
        let restarted = Uuid::from_u128(0xbeef);
        // The restarted service's ids mean nothing against the old cursor.
        let outcome = deltas(&mut session, restarted, vec![notification(2)]);
        assert_eq!(session.notifications, shown, "list and badge stay up");
        assert_eq!(session.notifications_latest_id, 30);
        assert!(outcome.notifications_need_refresh);
        assert!(!outcome.state_changed);

        // Polls before the reload answers neither apply deltas nor ask again.
        let outcome = deltas(&mut session, restarted, vec![notification(3)]);
        assert_eq!(session.notifications, shown);
        assert!(!outcome.notifications_need_refresh);

        replace_notifications(
            &mut session,
            vec![notification(2), notification(3)],
            restarted,
        );
        assert_eq!(
            session.notifications,
            vec![notification(2), notification(3)]
        );
        assert_eq!(session.notifications_latest_id, 3, "the new ring's cursor");
        let mut message = notification(4);
        message.kind = NotificationKind::Message;
        let outcome = deltas(&mut session, restarted, vec![message]);
        assert_eq!(session.notifications.len(), 3, "deltas apply again");
        assert!(!outcome.notifications_need_refresh);
    }

    #[test]
    fn new_deltas_apply_directly_and_only_a_gap_triggers_a_full_reload() {
        let mut session = session_state();
        session.notifications_latest_id = 6;
        let mut earlier = notification(6);
        earlier.at_ms = 1_000;
        let mut other_pane = notification(5);
        other_pane.pane_id = Uuid::from_u128(5);
        session.notifications = vec![other_pane.clone(), earlier];

        // The pane finishes again within the dedupe window: the service
        // replaced id 6 with id 7, and the mirror does the same.
        let mut repeat = notification(7);
        repeat.at_ms = 3_000;
        let outcome = deltas(&mut session, EPOCH, vec![repeat.clone()]);
        assert_eq!(
            session.notifications,
            vec![other_pane.clone(), repeat.clone()]
        );
        assert_eq!(session.notifications_latest_id, 7);
        assert!(outcome.state_changed);
        assert!(
            !outcome.notifications_need_refresh,
            "an ordinary delta needs no 200-item reload"
        );

        // Id 8 never arrived (replaced before this poll): reload once.
        let outcome = deltas(&mut session, EPOCH, vec![notification(9)]);
        assert_eq!(session.notifications_latest_id, 9);
        assert!(outcome.notifications_need_refresh);
        let outcome = deltas(&mut session, EPOCH, vec![notification(11)]);
        assert!(
            !outcome.notifications_need_refresh,
            "one reload in flight at a time"
        );
    }

    #[test]
    fn every_kind_is_kept_and_late_duplicates_are_ignored() {
        let mut session = session_state();
        session.notifications_latest_id = 6;
        let mut message = notification(8);
        message.kind = NotificationKind::Message;
        let mut attention = notification(9);
        attention.kind = NotificationKind::Attention;
        let outcome = deltas(
            &mut session,
            EPOCH,
            vec![notification(7), message.clone(), attention.clone()],
        );
        assert_eq!(
            session.notifications,
            vec![notification(7), message, attention]
        );
        assert_eq!(session.notifications_latest_id, 9);
        assert!(outcome.state_changed);

        // A response computed with an older cursor repeats items already held.
        let before = session.notifications.clone();
        let outcome = deltas(&mut session, EPOCH, vec![notification(8), notification(9)]);
        assert_eq!(session.notifications, before);
        assert!(!outcome.notifications_need_refresh);
        assert!(!outcome.state_changed);
    }

    #[test]
    fn a_missing_active_workspace_falls_back_to_a_workstation_never_a_bot() {
        let mut snapshot = snapshot_with_revision(3);
        let mut bot = snapshot.workspaces[0].clone();
        bot.id = Uuid::new_v4();
        bot.kind = hh_protocol::WorkspaceKind::Bot;
        let workstation = snapshot.workspaces[0].id;
        snapshot.workspaces.insert(0, bot);
        let mut session = session_state();
        let mut sidebar = sidebar();
        sidebar.active_workspace = Some(Uuid::new_v4());
        reconcile_updates(
            &mut session,
            &mut sidebar,
            &mut layout(),
            &mut HashMap::new(),
            UpdatePayload {
                session_revision: 3,
                snapshot: Some(snapshot),
                screens: Vec::new(),
                pane_states: Vec::new(),
                notification_deltas: Vec::new(),
                notifications_epoch: EPOCH,
            },
            Instant::now(),
        );
        assert_eq!(sidebar.active_workspace, Some(workstation));
    }
}
