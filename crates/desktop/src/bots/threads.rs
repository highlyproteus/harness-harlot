//! Bot threads: the saved omp conversations of a bot, listed under its card
//! in the Bots sidebar. The desktop caches each bot's list and refreshes it
//! while Bots mode is shown.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{Context, Pixels, Point};
use hh_protocol::{BotThread, ClientRequest, ServiceResponse, Tab, TerminalProfile};
use uuid::Uuid;

use super::bot_pane;
use crate::HhApp;
use crate::helpers::find_pane;
use crate::view_models::{BotThreadMenu, Modal, SidebarMode};

/// How often the expanded thread lists refresh while Bots mode is shown.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub(crate) struct BotThreadsState {
    /// Each omp bot's threads as last listed by the service.
    pub(crate) lists: HashMap<Uuid, Vec<BotThread>>,
    /// Chevron choices; bots without one are expanded only while selected.
    pub(crate) expanded: HashMap<Uuid, bool>,
    in_flight: HashSet<Uuid>,
    polling: bool,
    /// Bot tabs with an `OpenBotThread` whose pane switch the main area has
    /// not followed yet.
    opening: HashMap<Uuid, PendingOpen>,
}

/// An `OpenBotThread` the desktop waits to see in a snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingOpen {
    /// The bot's current pane when the request was sent.
    pub(crate) from_pane: Option<Uuid>,
    /// The snapshot revision the desktop held when the service acknowledged.
    pub(crate) acked_at: Option<u64>,
}

impl PendingOpen {
    /// Whether a snapshot at `revision` whose bot shows `active` reflects the
    /// open: the pane changed, or the snapshot is newer than the ack.
    pub(crate) fn settled(self, active: Option<Uuid>, revision: u64) -> bool {
        active != self.from_pane || self.acked_at.is_some_and(|acked| revision > acked)
    }
}

/// Threads exist only for omp bots.
pub(crate) fn has_threads(tab: &Tab) -> bool {
    tab.bot
        .as_ref()
        .is_some_and(|bot| bot.agent == TerminalProfile::Omp)
}

/// Whether a bot's thread list is shown: the chevron choice, else expanded
/// while the bot is selected.
pub(crate) fn threads_expanded(choice: Option<bool>, selected: bool) -> bool {
    choice.unwrap_or(selected)
}

/// Compact age of a thread: `now`, `5m`, `3h`, `2d`, `3w`.
pub(crate) fn relative_time(now_ms: u64, then_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(then_ms) / 1000;
    match seconds {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        86_400..604_800 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

impl HhApp {
    /// The bot is shown in the main area.
    pub(crate) fn bot_is_selected(&self, tab: &Tab) -> bool {
        self.bots_workspace()
            .is_some_and(|workspace| self.sidebar.active_workspace == Some(workspace.id))
            && self
                .layout
                .focused_pane
                .is_some_and(|pane_id| find_pane(&tab.layout, pane_id).is_some())
    }

    pub(crate) fn bot_threads_expanded(&self, tab: &Tab) -> bool {
        has_threads(tab)
            && threads_expanded(
                self.bot_threads.expanded.get(&tab.id).copied(),
                self.bot_is_selected(tab),
            )
    }

    pub(crate) fn toggle_bot_threads(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let Some(expanded) = self
            .bot_tab(tab_id)
            .map(|tab| self.bot_threads_expanded(tab))
        else {
            return;
        };
        self.bot_threads.expanded.insert(tab_id, !expanded);
        if !expanded {
            self.refresh_bot_threads(tab_id);
        }
        cx.notify();
    }

    /// Asks the service for one bot's threads unless a request is pending.
    pub(crate) fn refresh_bot_threads(&mut self, tab_id: Uuid) {
        if !self.bot_threads.in_flight.insert(tab_id) {
            return;
        }
        self.dispatch_with(
            ClientRequest::ListBotThreads { tab_id },
            Box::new(move |this, cx, result| {
                this.bot_threads.in_flight.remove(&tab_id);
                let exists = this.bot_tab(tab_id).is_some();
                match result {
                    Ok(ServiceResponse::BotThreads { threads }) if exists => {
                        this.bot_threads.lists.insert(tab_id, threads);
                    }
                    Ok(ServiceResponse::BotThreads { .. }) => {}
                    Ok(response) => this.report_unexpected(&response),
                    // The bot was deleted while the request was queued.
                    Err(_) if !exists => {}
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    /// Drops state of deleted bots and refreshes every expanded omp bot.
    fn refresh_expanded_bot_threads(&mut self) {
        let Some(workspace) = self.bots_workspace() else {
            return;
        };
        let live = workspace
            .tabs
            .iter()
            .filter(|tab| has_threads(tab))
            .map(|tab| tab.id)
            .collect::<HashSet<_>>();
        let expanded = workspace
            .tabs
            .iter()
            .filter(|tab| self.bot_threads_expanded(tab))
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        let state = &mut self.bot_threads;
        state.lists.retain(|tab_id, _| live.contains(tab_id));
        state.expanded.retain(|tab_id, _| live.contains(tab_id));
        state.opening.retain(|tab_id, _| live.contains(tab_id));
        for tab_id in expanded {
            self.refresh_bot_threads(tab_id);
        }
    }

    /// Refreshes now and every few seconds until Bots mode is left.
    pub(crate) fn start_bot_threads_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_expanded_bot_threads();
        if self.bot_threads.polling {
            return;
        }
        self.bot_threads.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(REFRESH_INTERVAL).await;
                let Ok(true) = this.update(cx, |this, _| {
                    if this.sidebar.sidebar_mode != SidebarMode::Bots {
                        this.bot_threads.polling = false;
                        return false;
                    }
                    this.refresh_expanded_bot_threads();
                    true
                }) else {
                    break;
                };
            }
        })
        .detach();
    }

    /// Shows a bot's thread: `None` starts a new one. Non-omp bots have no
    /// threads; a new thread restarts them fresh.
    pub(crate) fn open_bot_thread(
        &mut self,
        tab_id: Uuid,
        thread_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.bot_tab(tab_id) else {
            return;
        };
        if !has_threads(tab) {
            self.restart_bot(tab_id, cx);
            self.open_bot(tab_id, cx);
            return;
        }
        let from_pane = bot_pane(tab).map(|pane| pane.id);
        let live_pane = thread_id.as_deref().and_then(|thread_id| {
            self.bot_threads
                .lists
                .get(&tab_id)?
                .iter()
                .find(|thread| thread.id == thread_id)?
                .pane_id
                .filter(|pane_id| find_pane(&tab.layout, *pane_id).is_some())
        });
        match (
            live_pane,
            self.bots_workspace().map(|workspace| workspace.id),
        ) {
            (Some(pane_id), Some(workspace_id)) => {
                self.remember_return_workstation();
                self.editor.modal = Modal::None;
                self.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
            }
            _ => self.open_bot(tab_id, cx),
        }
        self.bot_threads.opening.insert(
            tab_id,
            PendingOpen {
                from_pane,
                acked_at: None,
            },
        );
        self.dispatch_with(
            ClientRequest::OpenBotThread { tab_id, thread_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        let revision = this
                            .session
                            .snapshot
                            .as_ref()
                            .map_or(0, |snapshot| snapshot.revision);
                        if let Some(pending) = this.bot_threads.opening.get_mut(&tab_id) {
                            pending.acked_at = Some(revision);
                        }
                        this.layout.last_sizes.clear();
                        this.refresh_bot_threads(tab_id);
                    }
                    Ok(response) => {
                        this.bot_threads.opening.remove(&tab_id);
                        this.report_unexpected(&response);
                    }
                    Err(error) => {
                        this.bot_threads.opening.remove(&tab_id);
                        this.report(&error);
                    }
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn set_bot_thread_pinned(
        &mut self,
        tab_id: Uuid,
        thread_id: String,
        pinned: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::SetBotThreadPinned {
                tab_id,
                thread_id,
                pinned,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => this.refresh_bot_threads(tab_id),
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn open_bot_thread_menu(
        &mut self,
        tab_id: Uuid,
        thread: &BotThread,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor.color_picker = None;
        self.editor.modal = Modal::BotThreadMenu(BotThreadMenu {
            tab_id,
            thread_id: thread.id.clone(),
            pinned: thread.pinned,
            position,
        });
        cx.notify();
    }

    /// Follows the service's pane switch after `OpenBotThread`. Returns the
    /// stack pane the update round should still reassert (a pending open
    /// suppresses reasserting the bot's previous pane) and the pane to focus.
    pub(crate) fn settle_bot_thread_open(
        &mut self,
        reassert: Option<Uuid>,
    ) -> (Option<Uuid>, Option<Uuid>) {
        if self.bot_threads.opening.is_empty() {
            return (reassert, None);
        }
        let Some(workspace) = self.bots_workspace() else {
            self.bot_threads.opening.clear();
            return (reassert, None);
        };
        let showing_bots = self.sidebar.active_workspace == Some(workspace.id);
        let revision = self
            .session
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.revision);
        let focused = self.layout.focused_pane;
        let mut reassert = reassert;
        let mut focus = None;
        let mut settled = Vec::new();
        for (&tab_id, &pending) in &self.bot_threads.opening {
            let Some(tab) = workspace.tabs.iter().find(|tab| tab.id == tab_id) else {
                settled.push(tab_id);
                continue;
            };
            if reassert.is_some_and(|pane_id| find_pane(&tab.layout, pane_id).is_some()) {
                reassert = None;
            }
            let active = bot_pane(tab).map(|pane| pane.id);
            if !pending.settled(active, revision) {
                continue;
            }
            settled.push(tab_id);
            let viewing = focused.is_none_or(|pane_id| find_pane(&tab.layout, pane_id).is_some());
            if showing_bots && viewing && active != focused {
                focus = active;
            }
        }
        for tab_id in settled {
            self.bot_threads.opening.remove(&tab_id);
        }
        (reassert, focus)
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingOpen, relative_time, threads_expanded};
    use uuid::Uuid;

    #[test]
    fn relative_time_uses_the_largest_whole_unit() {
        let now = 10_000_000_000;
        let ago = |seconds: u64| relative_time(now, now - seconds * 1000);
        assert_eq!(ago(0), "now");
        assert_eq!(ago(59), "now");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(3_599), "59m");
        assert_eq!(ago(3_600), "1h");
        assert_eq!(ago(86_399), "23h");
        assert_eq!(ago(2 * 86_400), "2d");
        assert_eq!(ago(604_799), "6d");
        assert_eq!(ago(3 * 604_800), "3w");
        assert_eq!(
            relative_time(now, now + 5_000),
            "now",
            "a clock skewed into the future reads as now"
        );
    }

    #[test]
    fn thread_lists_follow_selection_until_the_chevron_is_used() {
        assert!(threads_expanded(None, true));
        assert!(!threads_expanded(None, false));
        assert!(threads_expanded(Some(true), false));
        assert!(!threads_expanded(Some(false), true));
    }

    #[test]
    fn a_pending_open_settles_on_a_pane_switch_or_a_newer_snapshot() {
        let (old, new) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let in_flight = PendingOpen {
            from_pane: Some(old),
            acked_at: None,
        };
        assert!(
            !in_flight.settled(Some(old), 99),
            "a stale snapshot before the ack does not settle"
        );
        assert!(in_flight.settled(Some(new), 1));
        let acked = PendingOpen {
            acked_at: Some(7),
            ..in_flight
        };
        assert!(!acked.settled(Some(old), 7));
        assert!(
            acked.settled(Some(old), 8),
            "reopening the current thread settles once a newer snapshot lands"
        );
    }
}
