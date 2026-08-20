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
    /// The notification ring was reset by a stale id; refetch the full list.
    pub notifications_need_refresh: bool,
    /// New notifications arrived; auto-read them (window active) or refresh
    /// the dock badge.
    pub auto_read_or_badge: bool,
}

/// The `Updates` payload fields the reducer consumes.
pub(crate) struct UpdatePayload {
    pub session_revision: u64,
    pub snapshot: Option<SessionSnapshot>,
    pub screens: Vec<TerminalScreen>,
    pub pane_states: Vec<PaneStreamState>,
    pub notification_deltas: Vec<SessionNotification>,
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
                .find(|workspace| workspace_is_selectable(workspace))
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
            .collapsed_groups
            .retain(|tab_id| live_tab_ids.contains(tab_id));
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
    let notifications_changed = if notification_deltas
        .iter()
        .any(|notification| notification.id <= session.notifications_latest_id)
    {
        session.notifications.clear();
        session.notifications_latest_id = 0;
        outcome.notifications_need_refresh = true;
        true
    } else if notification_deltas.is_empty() {
        false
    } else {
        session.notifications_latest_id = notification_deltas
            .iter()
            .map(|notification| notification.id)
            .max()
            .unwrap_or(session.notifications_latest_id);
        session.notifications.extend(notification_deltas);
        let overflow = session
            .notifications
            .len()
            .saturating_sub(NOTIFICATION_RING_CAPACITY);
        if overflow > 0 {
            session.notifications.drain(..overflow);
        }
        outcome.auto_read_or_badge = true;
        true
    };
    let connection_changed = session.connection_error.take().is_some();
    session.connection_error = None;
    outcome.state_changed = pane_update_requires_repaint(snapshot_changed, screens_applied)
        || connection_changed
        || notifications_changed;
    outcome
}
#[cfg(test)]
mod tests {
    use super::*;

    use crate::view_models::{DragHoverState, SidebarResizeLifecycle};
    use gpui::ScrollHandle;
    use hh_protocol::{NotificationKind, StreamDiagnostics, TerminalModes, TerminalProfile};
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn session_state() -> SessionState {
        SessionState {
            stream_client: Arc::new(Mutex::new(None)),
            control_client: Arc::new(Mutex::new(None)),
            stream_tx: futures::channel::mpsc::unbounded().0,
            control_tx: futures::channel::mpsc::unbounded().0,
            poll_wake_tx: futures::channel::mpsc::channel(1).0,
            snapshot: None,
            screens: HashMap::new(),
            pane_states: HashMap::new(),
            notifications: Vec::new(),
            notifications_latest_id: 0,
            last_delivery: HashMap::new(),
            window_active: true,
            stream_diagnostics: StreamDiagnostics::default(),
            connection_error: None,
            history_status: None,
        }
    }

    fn sidebar() -> SidebarUi {
        SidebarUi {
            active_workspace: None,
            workspace_tab_scope: crate::helpers::WorkspaceTabScope::Workstation,
            expanded_workspaces: HashSet::new(),
            collapsed_groups: HashSet::new(),
            collapsed_pinned_sections: HashSet::new(),
            collapsed_project_sections: HashSet::new(),
            dismissed_workspace_tabs: HashSet::new(),
            workstation_tab_scroll: ScrollHandle::new(),
            dragging_workspace: None,
            workspace_drop_preview: None,
            suppress_workspace_click_until: None,
            tab_drop_preview: None,
            suppress_tab_click_until: None,
            sidebar_resize: SidebarResizeLifecycle::default(),
            preferred_sidebar_width: 200.0,
            sidebar_visible: true,
            sidebar_activity: false,
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
            columns: 80,
            rows: 24,
            lines: Vec::new(),
            cursor: None,
            selection: None,
            display_offset: 0,
            history_size: 0,
            modes: TerminalModes::default(),
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
                }],
                notification_deltas: Vec::new(),
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
                }],
                notification_deltas: Vec::new(),
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

    #[test]
    fn stale_notification_delta_resets_the_ring_and_requests_refetch() {
        let mut session = session_state();
        session.notifications_latest_id = 30;
        session.notifications = vec![notification(30)];
        let outcome = reconcile_updates(
            &mut session,
            &mut sidebar(),
            &mut layout(),
            &mut HashMap::new(),
            UpdatePayload {
                session_revision: 0,
                snapshot: None,
                screens: Vec::new(),
                pane_states: Vec::new(),
                notification_deltas: vec![notification(12)],
            },
            Instant::now(),
        );
        assert!(session.notifications.is_empty());
        assert_eq!(session.notifications_latest_id, 0);
        assert!(outcome.notifications_need_refresh);
        assert!(!outcome.auto_read_or_badge);
        assert!(outcome.state_changed);
    }
}
