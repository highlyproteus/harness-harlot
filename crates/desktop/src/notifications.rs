//! Live pane activity (Needs you, Running, Done), the Dock badge, and the
//! service's message notifications.
use hh_protocol::{
    ClientRequest, NotificationKind, Pane, PaneLayout, PaneStatus, PaneStreamState,
    ServiceResponse, SessionSnapshot, Tab, Workspace,
};
use std::cmp::Reverse;
use std::collections::HashMap;
use uuid::Uuid;

use crate::HhApp;
use crate::helpers::collect_terminal_tabs;

#[cfg(target_os = "macos")]
pub(crate) fn set_macos_dock_badge(label: Option<&str>) {
    hh_macos_icon::set_dock_badge(label);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_macos_dock_badge(_: Option<&str>) {}

/// Notifications groups, in display order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ActivitySection {
    NeedsYou,
    Running,
    Done,
}

impl ActivitySection {
    pub(crate) const ALL: [Self; 3] = [Self::NeedsYou, Self::Running, Self::Done];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Running => "Running",
            Self::Done => "Done",
        }
    }
}

/// A pane whose process exited is done whatever its last status said.
pub(crate) const fn activity_section(status: PaneStatus, exited: bool) -> Option<ActivitySection> {
    if exited {
        return Some(ActivitySection::Done);
    }
    match status {
        PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention => {
            Some(ActivitySection::NeedsYou)
        }
        PaneStatus::Working => Some(ActivitySection::Running),
        PaneStatus::Done => Some(ActivitySection::Done),
        PaneStatus::Idle => None,
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

/// One pane shown in Notifications.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActivityEntry<'a> {
    pub(crate) section: ActivitySection,
    pub(crate) workspace: &'a Workspace,
    pub(crate) tab: &'a Tab,
    pub(crate) pane: &'a Pane,
    pub(crate) exited: bool,
}

/// Every pane with activity across workstation tabs and bots, grouped by
/// section and newest status change first within each section.
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
                        exited,
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

impl HhApp {
    pub(crate) fn refresh_notifications(&mut self) {
        self.dispatch_with(
            ClientRequest::GetNotifications,
            Box::new(|this, cx, result| {
                let previous = this.session.notifications.clone();
                match result {
                    Ok(ServiceResponse::Notifications { items }) => {
                        this.session.notifications_latest_id = items
                            .iter()
                            .map(|notification| notification.id)
                            .max()
                            .unwrap_or(0);
                        this.session.notifications = items
                            .into_iter()
                            .filter(|notification| notification.kind == NotificationKind::Message)
                            .collect();
                        this.session.connection_error = None;
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                if this.session.notifications != previous {
                    cx.notify();
                }
            }),
        );
    }

    /// Panes waiting on the user; drives the bell and Dock badges.
    pub(crate) fn needs_you_count(&self) -> usize {
        self.session.snapshot.as_ref().map_or(0, |snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .map(|tab| needs_you_in(&tab.layout, &self.session.pane_states))
                .sum()
        })
    }

    pub(crate) fn sync_dock_badge(&mut self) {
        let count = self.needs_you_count();
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

    pub(crate) fn clear_notifications(&mut self) {
        self.dispatch_with(
            ClientRequest::ClearNotifications,
            Box::new(|this, cx, result| match result {
                Ok(ServiceResponse::Ack) => {
                    this.session.notifications.clear();
                    this.session.connection_error = None;
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
    use super::{ActivitySection, activity_entries, bots_needing_you};
    use hh_protocol::{
        PaneLayout, PaneStatus, PaneStreamState, SessionSnapshot, Tab, Workspace, WorkspaceKind,
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

    #[test]
    fn bots_needing_you_counts_bots_not_panes_and_ignores_workstations_and_exited_panes() {
        let mut snapshot = SessionSnapshot::seeded();
        let workstation = snapshot.workspaces[0].clone();
        let template = workstation.tabs[0].clone();
        let (waiting, _) = tab_with(&template, PaneStatus::NeedsApproval);
        let (also_waiting, _) = tab_with(&template, PaneStatus::NeedsInput);
        let (working, _) = tab_with(&template, PaneStatus::Working);
        let (exited_tab, exited) = tab_with(&template, PaneStatus::NeedsInput);
        snapshot.workspaces[0].tabs = vec![tab_with(&template, PaneStatus::NeedsInput).0];
        snapshot.workspaces.extend([
            bot(&workstation, vec![waiting, also_waiting]),
            bot(&workstation, vec![working]),
            bot(&workstation, vec![exited_tab]),
        ]);
        let pane_states = HashMap::from([(
            exited,
            PaneStreamState {
                pane_id: exited,
                revision: 1,
                subscribed: false,
                dirty: false,
                exited: true,
            },
        )]);
        assert_eq!(bots_needing_you(&snapshot, &pane_states), 1);
    }

    #[test]
    fn activity_groups_needs_you_running_done_newest_first_and_treats_exited_as_done() {
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
                let mut tab = template.clone();
                tab.id = uuid::Uuid::new_v4();
                let PaneLayout::Leaf { pane } = &mut tab.layout else {
                    unreachable!("seeded tabs are single panes");
                };
                pane.id = uuid::Uuid::new_v4();
                pane.status = *status;
                pane.status_changed_at_ms = *changed_at;
                ids.push(pane.id);
                tab
            })
            .collect();
        let exited = ids[6];
        let pane_states = HashMap::from([(
            exited,
            PaneStreamState {
                pane_id: exited,
                revision: 1,
                subscribed: false,
                dirty: false,
                exited: true,
            },
        )]);

        let entries = activity_entries(&snapshot, &pane_states);
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
                (ActivitySection::Done, exited),
                (ActivitySection::Done, ids[2]),
            ]
        );
    }
}
