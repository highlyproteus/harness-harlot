//! Live pane activity (Needs you, Running), the service's stored
//! notifications (Recent), the unread badges, and marking panes seen.
use hh_protocol::{
    ClientRequest, NotificationKind, Pane, PaneLayout, PaneStatus, PaneStreamState,
    ServiceResponse, SessionNotification, SessionSnapshot, Tab, Workspace,
};
use std::cmp::Reverse;
use std::collections::HashMap;
use uuid::Uuid;

use crate::helpers::{collect_terminal_tabs, find_pane, find_pane_mut};
use crate::{HhApp, THEME};

#[cfg(target_os = "macos")]
pub(crate) fn set_macos_dock_badge(label: Option<&str>) {
    hh_macos_icon::set_dock_badge(label);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_macos_dock_badge(_: Option<&str>) {}

/// Live Notifications groups, in display order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ActivitySection {
    NeedsYou,
    Running,
}

impl ActivitySection {
    pub(crate) const ALL: [Self; 2] = [Self::NeedsYou, Self::Running];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Running => "Running",
        }
    }
}

/// The live section a pane belongs in; an exited pane is in none.
pub(crate) const fn activity_section(status: PaneStatus, exited: bool) -> Option<ActivitySection> {
    if exited {
        return None;
    }
    match status {
        PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention => {
            Some(ActivitySection::NeedsYou)
        }
        PaneStatus::Working => Some(ActivitySection::Running),
        PaneStatus::Done | PaneStatus::Idle => None,
    }
}

pub(crate) const fn activity_badge(status: PaneStatus, exited: bool) -> &'static str {
    if exited {
        return "Exited";
    }
    match status {
        PaneStatus::NeedsApproval => "Needs approval",
        PaneStatus::NeedsInput => "Needs input",
        PaneStatus::Attention => "Attention",
        PaneStatus::Working => "Running",
        PaneStatus::Done => "Done",
        PaneStatus::Idle => "Idle",
    }
}

/// One live pane shown in Notifications.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActivityEntry<'a> {
    pub(crate) section: ActivitySection,
    pub(crate) workspace: &'a Workspace,
    pub(crate) tab: &'a Tab,
    pub(crate) pane: &'a Pane,
}

/// Every pane that needs the user or is running, across workstation tabs
/// and bots, grouped by section and newest status change first.
pub(crate) fn activity_entries<'a>(
    snapshot: &'a SessionSnapshot,
    pane_states: &HashMap<Uuid, PaneStreamState>,
) -> Vec<ActivityEntry<'a>> {
    let mut entries = Vec::new();
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            let mut panes = Vec::new();
            collect_terminal_tabs(&tab.layout, &mut panes);
            for pane in panes {
                let exited = pane_states.get(&pane.id).is_some_and(|state| state.exited);
                if let Some(section) = activity_section(pane.status, exited) {
                    entries.push(ActivityEntry {
                        section,
                        workspace,
                        tab,
                        pane,
                    });
                }
            }
        }
    }
    entries.sort_by_key(|entry| (entry.section, Reverse(entry.pane.status_changed_at_ms)));
    entries
}

fn needs_you_in(layout: &PaneLayout, pane_states: &HashMap<Uuid, PaneStreamState>) -> usize {
    let needs_you = |pane: &Pane| {
        let exited = pane_states.get(&pane.id).is_some_and(|state| state.exited);
        usize::from(activity_section(pane.status, exited) == Some(ActivitySection::NeedsYou))
    };
    match layout {
        PaneLayout::Leaf { pane } => needs_you(pane),
        PaneLayout::Stack { panes, .. } => panes.iter().map(needs_you).sum(),
        PaneLayout::Split { first, second, .. } => {
            needs_you_in(first, pane_states) + needs_you_in(second, pane_states)
        }
    }
}

/// Bot workspaces with at least one pane waiting on the user.
pub(crate) fn bots_needing_you(
    snapshot: &SessionSnapshot,
    pane_states: &HashMap<Uuid, PaneStreamState>,
) -> usize {
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| workspace.is_bot())
        .filter(|workspace| {
            workspace
                .tabs
                .iter()
                .any(|tab| needs_you_in(&tab.layout, pane_states) > 0)
        })
        .count()
}

/// The bell and Dock badge: how many stored notifications are unread, and
/// whether any of them asks for the user (orange) rather than reports (blue).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnreadBadge {
    pub(crate) count: usize,
    pub(crate) attention: bool,
}

impl UnreadBadge {
    pub(crate) const fn color(self) -> u32 {
        if self.attention {
            THEME.warning
        } else {
            THEME.accent
        }
    }
}

/// `None` when everything is read, so the badge hides.
pub(crate) fn unread_badge(notifications: &[SessionNotification]) -> Option<UnreadBadge> {
    let mut badge = UnreadBadge {
        count: 0,
        attention: false,
    };
    for notification in notifications
        .iter()
        .filter(|notification| !notification.read)
    {
        badge.count += 1;
        badge.attention |= notification.kind == NotificationKind::Attention;
    }
    (badge.count > 0).then_some(badge)
}

impl HhApp {
    /// Replaces the notification mirror with the service's full ring.
    pub(crate) fn refresh_notifications(&mut self) {
        self.dispatch_with(
            ClientRequest::GetNotifications,
            Box::new(|this, cx, result| {
                let previous = this.session.notifications.clone();
                match result {
                    Ok(ServiceResponse::Notifications { items, epoch }) => {
                        this.session.notifications_latest_id = items
                            .iter()
                            .map(|notification| notification.id)
                            .max()
                            .unwrap_or(0);
                        this.session.notifications = items;
                        this.session.notifications_epoch = Some(epoch);
                        this.session.connection_error = None;
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                this.sync_dock_badge();
                if this.session.notifications != previous {
                    cx.notify();
                }
            }),
        );
    }

    pub(crate) fn unread_badge(&self) -> Option<UnreadBadge> {
        unread_badge(&self.session.notifications)
    }

    pub(crate) fn sync_dock_badge(&mut self) {
        let count = self.unread_badge().map_or(0, |badge| badge.count);
        if self.session.dock_badge == Some(count) {
            return;
        }
        self.session.dock_badge = Some(count);
        if count == 0 {
            set_macos_dock_badge(None);
        } else {
            set_macos_dock_badge(Some(&count.to_string()));
        }
    }

    /// The user opened pane `pane_id` (clicked it, typed into it, or switched
    /// to its tab): clears its unseen dot and reads its notifications. Sends
    /// nothing unless the pane is unseen.
    pub(crate) fn mark_pane_seen(&mut self, pane_id: Uuid) {
        let Some(pane) = self.session.snapshot.as_mut().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter_mut()
                .flat_map(|workspace| workspace.tabs.iter_mut())
                .find_map(|tab| find_pane_mut(&mut tab.layout, pane_id))
        }) else {
            return;
        };
        if !pane.unseen {
            return;
        }
        // Optimistic: the next snapshot and notification refresh confirm it.
        pane.unseen = false;
        for notification in &mut self.session.notifications {
            if notification.pane_id == pane_id {
                notification.read = true;
            }
        }
        self.sync_dock_badge();
        self.dispatch_with(
            ClientRequest::MarkPaneSeen { pane_id },
            Box::new(|this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => this.refresh_notifications(),
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    /// Switching to a tab shows every pane in it.
    pub(crate) fn mark_tab_seen(&mut self, pane_id: Uuid) {
        let panes = self
            .session
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .flat_map(|workspace| &workspace.tabs)
                    .find(|tab| find_pane(&tab.layout, pane_id).is_some())
            })
            .map(|tab| {
                let mut panes = Vec::new();
                collect_terminal_tabs(&tab.layout, &mut panes);
                panes
                    .into_iter()
                    .filter(|pane| pane.unseen)
                    .map(|pane| pane.id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for pane_id in panes {
            self.mark_pane_seen(pane_id);
        }
    }

    /// Marks stored notifications read without touching their panes.
    pub(crate) fn mark_notifications_read(&mut self, ids: Vec<u64>) {
        if ids.is_empty() {
            return;
        }
        for notification in &mut self.session.notifications {
            if ids.contains(&notification.id) {
                notification.read = true;
            }
        }
        self.sync_dock_badge();
        self.dispatch_with(
            ClientRequest::MarkNotificationsRead { ids },
            Box::new(|this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => this.refresh_notifications(),
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    /// The Recent header's "Mark all read".
    pub(crate) fn mark_all_notifications_read(&mut self) {
        let unread = self
            .session
            .notifications
            .iter()
            .filter(|notification| !notification.read)
            .map(|notification| notification.id)
            .collect();
        self.mark_notifications_read(unread);
    }

    /// A Recent row was clicked: show its pane (which marks the pane seen)
    /// and read the notification itself.
    pub(crate) fn open_notification(&mut self, id: u64, cx: &mut gpui::Context<Self>) {
        let Some(notification) = self
            .session
            .notifications
            .iter()
            .find(|notification| notification.id == id)
        else {
            return;
        };
        let pane_id = notification.pane_id;
        let unread = !notification.read;
        let target = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot.workspaces.iter().find_map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                    .map(|tab| (workspace.id, workspace.is_bot(), tab.id))
            })
        });
        match target {
            Some((workspace_id, true, tab_id)) => {
                self.open_bot_pane(workspace_id, tab_id, pane_id, cx);
            }
            Some((workspace_id, false, tab_id)) => {
                self.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
            }
            None => {}
        }
        self.mark_pane_seen(pane_id);
        let still_unread = unread
            && self
                .session
                .notifications
                .iter()
                .any(|notification| notification.id == id && !notification.read);
        if still_unread {
            self.mark_notifications_read(vec![id]);
        }
        cx.notify();
    }

    pub(crate) fn clear_notifications(&mut self) {
        self.dispatch_with(
            ClientRequest::ClearNotifications,
            Box::new(|this, cx, result| match result {
                Ok(ServiceResponse::Ack) => {
                    this.session.notifications.clear();
                    this.session.connection_error = None;
                    this.sync_dock_badge();
                    cx.notify();
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{ActivitySection, UnreadBadge, activity_entries, bots_needing_you, unread_badge};
    use hh_protocol::{
        NotificationKind, PaneLayout, PaneStatus, PaneStreamState, SessionNotification,
        SessionSnapshot, Tab, TerminalProfile, Workspace, WorkspaceKind,
    };
    use std::collections::HashMap;
    use uuid::Uuid;

    fn tab_with(template: &Tab, status: PaneStatus) -> (Tab, Uuid) {
        let mut tab = template.clone();
        tab.id = Uuid::new_v4();
        let PaneLayout::Leaf { pane } = &mut tab.layout else {
            unreachable!("seeded tabs are single panes");
        };
        pane.id = Uuid::new_v4();
        pane.status = status;
        let pane_id = pane.id;
        (tab, pane_id)
    }

    fn bot(template: &Workspace, tabs: Vec<Tab>) -> Workspace {
        let mut bot = template.clone();
        bot.id = Uuid::new_v4();
        bot.kind = WorkspaceKind::Bot;
        bot.tabs = tabs;
        bot
    }

    fn exited(pane_id: Uuid) -> HashMap<Uuid, PaneStreamState> {
        HashMap::from([(
            pane_id,
            PaneStreamState {
                pane_id,
                revision: 1,
                subscribed: false,
                dirty: false,
                exited: true,
                enhanced_paste: false,
            },
        )])
    }

    fn stored(id: u64, kind: NotificationKind, read: bool) -> SessionNotification {
        SessionNotification {
            id,
            pane_id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            kind,
            message: None,
            pane_title: "t".to_owned(),
            workspace_title: "w".to_owned(),
            profile: TerminalProfile::default(),
            at_ms: 0,
            read,
        }
    }

    #[test]
    fn the_badge_counts_unread_and_turns_orange_for_unread_attention() {
        assert_eq!(unread_badge(&[]), None);
        assert_eq!(
            unread_badge(&[
                stored(1, NotificationKind::Attention, true),
                stored(2, NotificationKind::Completed, true),
            ]),
            None,
            "hidden when everything is read"
        );
        let blue = unread_badge(&[
            stored(1, NotificationKind::Attention, true),
            stored(2, NotificationKind::Completed, false),
            stored(3, NotificationKind::Message, false),
        ]);
        assert_eq!(
            blue,
            Some(UnreadBadge {
                count: 2,
                attention: false
            }),
            "read attention does not tint the badge"
        );
        let orange = unread_badge(&[
            stored(1, NotificationKind::Completed, false),
            stored(2, NotificationKind::Attention, false),
        ])
        .expect("unread");
        assert_eq!(orange.count, 2);
        assert_ne!(orange.color(), blue.expect("unread").color());
    }

    #[test]
    fn bots_needing_you_counts_bots_not_panes_and_ignores_workstations_and_exited_panes() {
        let mut snapshot = SessionSnapshot::seeded();
        let workstation = snapshot.workspaces[0].clone();
        let template = workstation.tabs[0].clone();
        let (waiting, _) = tab_with(&template, PaneStatus::NeedsApproval);
        let (also_waiting, _) = tab_with(&template, PaneStatus::NeedsInput);
        let (working, _) = tab_with(&template, PaneStatus::Working);
        let (exited_tab, exited_pane) = tab_with(&template, PaneStatus::NeedsInput);
        snapshot.workspaces[0].tabs = vec![tab_with(&template, PaneStatus::NeedsInput).0];
        snapshot.workspaces.extend([
            bot(&workstation, vec![waiting, also_waiting]),
            bot(&workstation, vec![working]),
            bot(&workstation, vec![exited_tab]),
        ]);
        assert_eq!(bots_needing_you(&snapshot, &exited(exited_pane)), 1);
    }

    #[test]
    fn live_activity_groups_needs_you_then_running_newest_first() {
        let mut snapshot = SessionSnapshot::seeded();
        let template = snapshot.workspaces[0].tabs[0].clone();
        let statuses = [
            (PaneStatus::Working, 10),
            (PaneStatus::NeedsInput, 20),
            (PaneStatus::Done, 30),
            (PaneStatus::NeedsApproval, 40),
            (PaneStatus::Idle, 50),
            (PaneStatus::Attention, 5),
            (PaneStatus::NeedsInput, 60),
        ];
        let mut ids = Vec::new();
        snapshot.workspaces[0].tabs = statuses
            .iter()
            .map(|(status, changed_at)| {
                let (mut tab, pane_id) = tab_with(&template, *status);
                let PaneLayout::Leaf { pane } = &mut tab.layout else {
                    unreachable!("seeded tabs are single panes");
                };
                pane.status_changed_at_ms = *changed_at;
                ids.push(pane_id);
                tab
            })
            .collect();

        let entries = activity_entries(&snapshot, &exited(ids[6]));
        let order = entries
            .iter()
            .map(|entry| (entry.section, entry.pane.id))
            .collect::<Vec<_>>();

        assert_eq!(
            order,
            vec![
                (ActivitySection::NeedsYou, ids[3]),
                (ActivitySection::NeedsYou, ids[1]),
                (ActivitySection::NeedsYou, ids[5]),
                (ActivitySection::Running, ids[0]),
            ],
            "finished, idle, and exited panes live in Recent, not here"
        );
    }
}
