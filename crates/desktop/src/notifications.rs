//! Live pane activity (Needs you, Running), the service's stored
//! notifications (Recent), the unread badges, and marking panes seen.
use hh_protocol::{
    ClientRequest, NotificationKind, Pane, PaneLayout, PaneStatus, PaneStreamState,
    ServiceResponse, SessionNotification, SessionSnapshot, Tab, Workspace,
};
use std::cmp::Reverse;
use std::collections::HashMap;
use uuid::Uuid;

use crate::helpers::{collect_terminal_tabs, find_pane, find_pane_mut, zoom_projection};
use crate::tab_chrome::{PaneIndicator, aggregate_indicators, pane_indicator, shows_running};
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

/// The live section a pane belongs in; an exited pane is in none. Running
/// follows [`shows_running`], the same rule as the tab indicator.
pub(crate) fn activity_section(pane: &Pane, exited: bool) -> Option<ActivitySection> {
    if exited {
        return None;
    }
    match pane.status {
        PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention => {
            Some(ActivitySection::NeedsYou)
        }
        status => shows_running(status, pane.unseen, pane.progress.as_ref())
            .then_some(ActivitySection::Running),
    }
}

/// A pane's status word; `None` for a plain idle pane, which shows none.
pub(crate) fn activity_badge(pane: &Pane, exited: bool) -> Option<&'static str> {
    if exited {
        return Some("Exited");
    }
    match activity_section(pane, exited) {
        Some(ActivitySection::Running) => return Some("Running"),
        Some(ActivitySection::NeedsYou) | None => {}
    }
    match pane.status {
        PaneStatus::NeedsApproval => Some("Needs approval"),
        PaneStatus::NeedsInput => Some("Needs input"),
        PaneStatus::Attention => Some("Attention"),
        PaneStatus::Working => Some("Running"),
        PaneStatus::Done => Some("Done"),
        PaneStatus::Idle => None,
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
                if let Some(section) = activity_section(pane, exited) {
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
        usize::from(activity_section(pane, exited) == Some(ActivitySection::NeedsYou))
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

/// The state border of bot workspace `workspace`'s icon: its threads'
/// states aggregated (needs you, then finished unseen, then working), so it
/// clears exactly when each thread's own border does. `None` for
/// workstations.
pub(crate) fn bot_indicator(
    workspace: &Workspace,
    pane_states: &HashMap<Uuid, PaneStreamState>,
) -> PaneIndicator {
    if !workspace.is_bot() {
        return PaneIndicator::None;
    }
    aggregate_indicators(workspace.tabs.iter().flat_map(|tab| {
        let mut panes = Vec::new();
        collect_terminal_tabs(&tab.layout, &mut panes);
        panes
            .into_iter()
            .map(|pane| {
                pane_indicator(
                    pane.status,
                    pane_states.get(&pane.id).is_some_and(|state| state.exited),
                    pane.unseen,
                    pane.progress.as_ref(),
                )
            })
            .collect::<Vec<_>>()
    }))
}

/// Every bot's icon state in one: the toolbar Bots button's border.
pub(crate) fn bots_indicator(
    snapshot: &SessionSnapshot,
    pane_states: &HashMap<Uuid, PaneStreamState>,
) -> PaneIndicator {
    aggregate_indicators(
        snapshot
            .workspaces
            .iter()
            .map(|workspace| bot_indicator(workspace, pane_states)),
    )
}

/// The unread count shared by the in-app bell and the Dock, and whether any
/// unread item asks for the user. Only the in-app bell is tinted (the
/// needs-you magenta for asks, blue for reports); macOS always draws the
/// Dock number in red.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnreadBadge {
    pub(crate) count: usize,
    pub(crate) attention: bool,
}

impl UnreadBadge {
    pub(crate) const fn color(self) -> u32 {
        if self.attention {
            crate::status_art::NEEDS_YOU_COLOR
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

/// Whether opening `pane` has anything to clear: its unseen dot, or any
/// unread stored notification (plain-shell bells and messages leave the dot
/// unset but still count as unread).
pub(crate) fn pane_has_unseen(pane: &Pane, notifications: &[SessionNotification]) -> bool {
    pane.unseen
        || notifications
            .iter()
            .any(|notification| notification.pane_id == pane.id && !notification.read)
}

/// What showing a pane counts as having looked at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SeenScope {
    /// Switching to its tab: every pane that tab puts on screen.
    Tab,
    /// A click on the pane itself (a chip, a Notifications row): that pane.
    Pane,
    /// Restoring a view the user did not pick (e.g. leaving a bot): nothing.
    Nothing,
}

/// Panes on screen once `pane_id`'s tab `layout` is shown with `pane_id`
/// active in its stack: each split side's active stack member, narrowed to
/// the zoomed slot when `zoomed` covers `pane_id`.
pub(crate) fn panes_shown_with(
    layout: &PaneLayout,
    pane_id: Uuid,
    zoomed: Option<Uuid>,
) -> Vec<Uuid> {
    fn collect(layout: &PaneLayout, pane_id: Uuid, shown: &mut Vec<Uuid>) {
        match layout {
            PaneLayout::Leaf { pane } => shown.push(pane.id),
            PaneLayout::Stack { panes, active } => {
                let holds_target = panes.iter().any(|pane| pane.id == pane_id);
                shown.push(if holds_target { pane_id } else { *active });
            }
            PaneLayout::Split { first, second, .. } => {
                collect(first, pane_id, shown);
                collect(second, pane_id, shown);
            }
        }
    }
    let zoomed_slot = zoomed
        .and_then(|zoomed| zoom_projection(layout, zoomed))
        .filter(|slot| find_pane(slot, pane_id).is_some());
    let mut shown = Vec::new();
    collect(zoomed_slot.as_ref().unwrap_or(layout), pane_id, &mut shown);
    shown
}

impl HhApp {
    /// Replaces the notification mirror with the service's full ring. Until
    /// it answers, the mirror keeps showing what it had.
    pub(crate) fn refresh_notifications(&mut self) {
        self.dispatch_with(
            ClientRequest::GetNotifications,
            Box::new(|this, cx, result| {
                let previous = this.session.notifications.clone();
                match result {
                    Ok(ServiceResponse::Notifications { items, epoch }) => {
                        crate::reconcile::replace_notifications(&mut this.session, items, epoch);
                        this.session.connection_error = None;
                    }
                    Ok(response) => {
                        this.session.notifications_reloading = false;
                        this.report_unexpected(&response);
                    }
                    Err(error) => {
                        // The next poll that still sees a replaced ring retries.
                        this.session.notifications_reloading = false;
                        this.report(&error);
                    }
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

    /// Mirrors the unread count onto the Dock icon, which macOS draws red.
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
    /// nothing when the pane has neither.
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
        if !pane_has_unseen(pane, &self.session.notifications) {
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

    /// Switching to `pane_id`'s tab (with `pane_id` active in its stack)
    /// shows only the panes on screen: not the stack's hidden members, and
    /// only the zoomed pane while zoom covers it.
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
            .map(|tab| panes_shown_with(&tab.layout, pane_id, self.layout.zoomed_pane))
            .unwrap_or_default();
        for pane_id in panes {
            self.mark_pane_seen(pane_id);
        }
    }

    /// Marks what showing `pane_id` in `scope` counts as having looked at.
    pub(crate) fn mark_seen(&mut self, pane_id: Uuid, scope: SeenScope) {
        match scope {
            SeenScope::Tab => self.mark_tab_seen(pane_id),
            SeenScope::Pane => self.mark_pane_seen(pane_id),
            SeenScope::Nothing => {}
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

    /// A Recent row was clicked: show its pane (which marks only that pane
    /// seen) and read the notification itself.
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
                self.select_sidebar_pane(workspace_id, tab_id, pane_id, SeenScope::Pane, cx);
            }
            None => {}
        }
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
    use super::{
        ActivitySection, UnreadBadge, activity_badge, activity_entries, bot_indicator,
        bots_indicator, bots_needing_you, pane_has_unseen, panes_shown_with, unread_badge,
    };
    use crate::tab_chrome::PaneIndicator;
    use hh_protocol::{
        NotificationKind, PaneLayout, PaneStatus, PaneStreamState, SessionNotification,
        SessionSnapshot, Tab, TerminalProfile, Workspace, WorkspaceKind,
    };
    use hh_protocol::{Pane, PaneProgress, ProgressSource, SplitAxis};
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
    fn the_badge_counts_unread_and_turns_magenta_for_unread_attention() {
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
        let magenta = unread_badge(&[
            stored(1, NotificationKind::Completed, false),
            stored(2, NotificationKind::Attention, false),
        ])
        .expect("unread");
        assert_eq!(magenta.count, 2);
        assert_ne!(magenta.color(), blue.expect("unread").color());
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

    fn unseen(mut tab: Tab) -> Tab {
        let PaneLayout::Leaf { pane } = &mut tab.layout else {
            unreachable!("seeded tabs are single panes");
        };
        pane.unseen = true;
        tab
    }

    /// A bot's icon border is its threads' states summarized: needs you
    /// (including a bell and a thread hidden in a stack), then finished
    /// unseen (an exited thread nobody opened too), then working. Seen,
    /// idle, and exited-while-waiting threads leave it bare.
    #[test]
    fn a_bot_icon_summarizes_its_threads_and_the_toolbar_summarizes_every_bot() {
        use PaneIndicator::{Done, NeedsYou, Running};
        let mut snapshot = SessionSnapshot::seeded();
        let workstation = snapshot.workspaces[0].clone();
        let template = workstation.tabs[0].clone();
        let quiet = |status| tab_with(&template, status).0;
        let (exited_tab, exited_pane) = tab_with(&template, PaneStatus::NeedsInput);
        let exited_unseen = unseen(tab_with(&template, PaneStatus::Done).0);
        let exited_unseen_id = match &exited_unseen.layout {
            PaneLayout::Leaf { pane } => pane.id,
            _ => unreachable!(),
        };
        let mut stacked = quiet(PaneStatus::Idle);
        let PaneLayout::Leaf { pane: front } = stacked.layout.clone() else {
            unreachable!();
        };
        let mut hidden = front.clone();
        hidden.id = Uuid::new_v4();
        hidden.status = PaneStatus::Attention;
        stacked.layout = PaneLayout::Stack {
            active: front.id,
            panes: vec![front, hidden],
        };

        let cases = [
            (vec![quiet(PaneStatus::NeedsInput)], NeedsYou),
            (vec![quiet(PaneStatus::NeedsApproval)], NeedsYou),
            (vec![quiet(PaneStatus::Attention)], NeedsYou),
            (vec![stacked], NeedsYou),
            (vec![unseen(quiet(PaneStatus::Done))], Done),
            (vec![exited_unseen], Done),
            (
                vec![quiet(PaneStatus::Working), quiet(PaneStatus::Idle)],
                Running(None),
            ),
            (
                vec![quiet(PaneStatus::Working), unseen(quiet(PaneStatus::Done))],
                Done,
            ),
            (
                vec![
                    unseen(quiet(PaneStatus::Done)),
                    quiet(PaneStatus::NeedsInput),
                ],
                NeedsYou,
            ),
            (vec![quiet(PaneStatus::Done)], PaneIndicator::None),
            (vec![exited_tab], PaneIndicator::None),
        ];
        let mut states = exited(exited_pane);
        states.extend(exited(exited_unseen_id));
        for (index, (tabs, expected)) in cases.into_iter().enumerate() {
            let one = bot(&workstation, tabs);
            assert_eq!(bot_indicator(&one, &states), expected, "case {index}");
        }

        // A workstation's panes never mark a bot; the toolbar takes the most
        // urgent state across every bot.
        snapshot.workspaces[0].tabs = vec![quiet(PaneStatus::NeedsInput)];
        assert_eq!(
            bot_indicator(&snapshot.workspaces[0], &states),
            PaneIndicator::None
        );
        assert_eq!(bots_indicator(&snapshot, &states), PaneIndicator::None);
        snapshot
            .workspaces
            .push(bot(&workstation, vec![quiet(PaneStatus::Working)]));
        assert_eq!(bots_indicator(&snapshot, &states), Running(None));
        snapshot
            .workspaces
            .push(bot(&workstation, vec![unseen(quiet(PaneStatus::Done))]));
        assert_eq!(bots_indicator(&snapshot, &states), Done);
        snapshot
            .workspaces
            .push(bot(&workstation, vec![quiet(PaneStatus::NeedsApproval)]));
        assert_eq!(bots_indicator(&snapshot, &states), NeedsYou);
    }

    /// Opening a finished thread (the seen rule clears `unseen`) takes the
    /// green border off its bot icon and off the toolbar.
    #[test]
    fn viewing_the_finished_thread_clears_the_bot_border() {
        let mut snapshot = SessionSnapshot::seeded();
        let workstation = snapshot.workspaces[0].clone();
        let template = workstation.tabs[0].clone();
        let finished = unseen(tab_with(&template, PaneStatus::Done).0);
        snapshot.workspaces.push(bot(&workstation, vec![finished]));
        let states = HashMap::new();
        assert_eq!(bots_indicator(&snapshot, &states), PaneIndicator::Done);

        let bot_index = snapshot.workspaces.len() - 1;
        let PaneLayout::Leaf { pane } = &mut snapshot.workspaces[bot_index].tabs[0].layout else {
            unreachable!();
        };
        pane.unseen = false;
        assert_eq!(
            bot_indicator(&snapshot.workspaces[bot_index], &states),
            PaneIndicator::None
        );
        assert_eq!(bots_indicator(&snapshot, &states), PaneIndicator::None);
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

    fn pane(id: u128) -> Pane {
        let snapshot = SessionSnapshot::seeded();
        let PaneLayout::Leaf { pane } = &snapshot.workspaces[0].tabs[0].layout else {
            unreachable!("seeded tabs are single panes");
        };
        let mut pane = pane.clone();
        pane.id = Uuid::from_u128(id);
        pane
    }

    #[test]
    fn an_idle_seen_pane_with_unfinished_tasks_is_listed_as_running() {
        let mut snapshot = SessionSnapshot::seeded();
        let template = snapshot.workspaces[0].tabs[0].clone();
        let (mut tab, pane_id) = tab_with(&template, PaneStatus::Idle);
        let PaneLayout::Leaf { pane } = &mut tab.layout else {
            unreachable!("seeded tabs are single panes");
        };
        pane.unseen = false;
        pane.progress = Some(PaneProgress {
            done: 1,
            total: 3,
            current: None,
            phase: None,
            source: ProgressSource::Codex,
        });
        assert_eq!(activity_badge(pane, false), Some("Running"));
        let finished = {
            let mut finished = pane.clone();
            finished.progress.as_mut().expect("progress").done = 3;
            finished
        };
        assert_eq!(activity_badge(&finished, false), None);
        snapshot.workspaces[0].tabs = vec![tab];

        let entries = activity_entries(&snapshot, &HashMap::new());
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.section, entry.pane.id))
                .collect::<Vec<_>>(),
            vec![(ActivitySection::Running, pane_id)]
        );
    }

    #[test]
    fn a_pane_with_unread_notifications_needs_marking_even_without_its_dot() {
        let mut shell = pane(1);
        shell.unseen = false;
        let mut bell = stored(1, NotificationKind::Attention, false);
        bell.pane_id = shell.id;
        let mut elsewhere = stored(2, NotificationKind::Attention, false);
        elsewhere.pane_id = Uuid::from_u128(2);
        assert!(
            pane_has_unseen(&shell, std::slice::from_ref(&bell)),
            "a plain-shell bell sets no dot but is unread"
        );
        assert!(!pane_has_unseen(&shell, std::slice::from_ref(&elsewhere)));
        bell.read = true;
        assert!(!pane_has_unseen(&shell, &[bell]));
        shell.unseen = true;
        assert!(pane_has_unseen(&shell, &[]));
    }

    #[test]
    fn switching_to_a_tab_marks_only_the_panes_it_puts_on_screen() {
        let stack = PaneLayout::Stack {
            panes: vec![pane(1), pane(2)],
            active: Uuid::from_u128(1),
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(stack),
            second: Box::new(PaneLayout::Leaf { pane: pane(3) }),
        };
        let ids = |raw: &[u128]| raw.iter().copied().map(Uuid::from_u128).collect::<Vec<_>>();

        assert_eq!(
            panes_shown_with(&layout, Uuid::from_u128(1), None),
            ids(&[1, 3]),
            "the stack's hidden member stays unseen"
        );
        assert_eq!(
            panes_shown_with(&layout, Uuid::from_u128(2), None),
            ids(&[2, 3]),
            "selecting the hidden member brings it to the front"
        );
        assert_eq!(
            panes_shown_with(&layout, Uuid::from_u128(3), Some(Uuid::from_u128(3))),
            ids(&[3]),
            "zoom hides the other side"
        );
        assert_eq!(
            panes_shown_with(&layout, Uuid::from_u128(3), Some(Uuid::from_u128(1))),
            ids(&[1, 3]),
            "a zoom that does not cover the target does not apply"
        );
    }
}
