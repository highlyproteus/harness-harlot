//! Bot threads: each omp bot's live and saved conversations. Live threads are
//! the panes of the bot workspace's tabs; saved ones are listed under them in
//! the bot's sidebar card. The desktop caches each bot's list and refreshes it
//! while Bots mode is shown.
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{AnyElement, Context, IntoElement, ParentElement, Pixels, Point, Styled, div, rgb};
use hh_protocol::{BotSpec, BotThread, ClientRequest, Pane, ServiceResponse, Tab, TerminalProfile};
use uuid::Uuid;

use crate::HhApp;
use crate::THEME;
use crate::helpers::{
    WorkspaceTabScope, find_pane, identity_label, visible_panes, workspace_tab_standalone_pane,
};
use crate::view_models::{
    BotThreadDeleteConfirmation, BotThreadMenu, DialogAction, DialogSpec, DialogTone, Modal,
    SidebarMode,
};

/// How often the thread lists refresh while Bots mode is shown.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Title of a thread whose omp session has no title yet.
pub(crate) const NEW_THREAD_TITLE: &str = "New thread";

#[derive(Debug, Default)]
pub(crate) struct BotThreadsState {
    /// Each omp bot's threads as last listed by the service, by bot id.
    pub(crate) lists: HashMap<Uuid, Vec<BotThread>>,
    in_flight: HashSet<Uuid>,
    polling: bool,
    /// Each bot's live thread panes when its list was last requested; a
    /// change (thread opened, closed, or evicted) refreshes the list.
    listed_panes: HashMap<Uuid, Vec<Uuid>>,
    /// The bot pane whose activation the service last recorded.
    pub(crate) activated: Option<Uuid>,
}

/// Threads exist only for omp bots.
pub(crate) fn has_threads(bot: &BotSpec) -> bool {
    bot.agent == TerminalProfile::Omp
}

/// Saved (not live) threads: pinned first, then newest first.
pub(crate) fn saved_threads(threads: &[BotThread]) -> Vec<&BotThread> {
    let mut saved = threads
        .iter()
        .filter(|thread| thread.pane_id.is_none())
        .collect::<Vec<_>>();
    saved.sort_by_key(|thread| (!thread.pinned, Reverse(thread.updated_ms)));
    saved
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
    /// A pane's display name: a bot thread shows its omp thread title (or
    /// "New thread"); every other pane its own title.
    pub(crate) fn pane_label(&self, pane: &Pane) -> String {
        match self.bot_for_pane(pane.id) {
            Some(bot_id) => self
                .bot_threads
                .lists
                .get(&bot_id)
                .and_then(|threads| {
                    threads
                        .iter()
                        .find(|thread| thread.pane_id == Some(pane.id))
                })
                .and_then(|thread| thread.title.clone())
                .unwrap_or_else(|| NEW_THREAD_TITLE.to_owned()),
            None => identity_label(pane).to_owned(),
        }
    }

    /// A tab's display name: its rename, else its single pane's label, else
    /// (a bot tab holding several threads) its first visible thread's title,
    /// else the service-chosen title.
    pub(crate) fn tab_label(&self, tab: &Tab) -> String {
        if let Some(title) = &tab.custom_title {
            return title.clone();
        }
        if let Some(pane) = workspace_tab_standalone_pane(tab) {
            return self.pane_label(pane);
        }
        visible_panes(&tab.layout)
            .first()
            .and_then(|pane_id| find_pane(&tab.layout, *pane_id))
            .filter(|pane| self.pane_is_bot(pane.id))
            .map_or_else(|| tab.title.clone(), |pane| self.pane_label(pane))
    }

    /// Asks the service for one bot's threads unless a request is pending.
    pub(crate) fn refresh_bot_threads(&mut self, bot_id: Uuid) {
        let Some(bot) = self.bot_spec(bot_id) else {
            return;
        };
        if !has_threads(bot) {
            return;
        }
        let panes = bot.thread_panes.keys().copied().collect();
        self.bot_threads.listed_panes.insert(bot_id, panes);
        if !self.bot_threads.in_flight.insert(bot_id) {
            return;
        }
        self.dispatch_with(
            ClientRequest::ListBotThreads { bot_id },
            Box::new(move |this, cx, result| {
                this.bot_threads.in_flight.remove(&bot_id);
                let exists = this.bot_workspace(bot_id).is_some();
                match result {
                    Ok(ServiceResponse::BotThreads { threads }) if exists => {
                        this.bot_threads.lists.insert(bot_id, threads);
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

    /// Drops state of deleted bots and refreshes every omp bot's threads.
    fn refresh_all_bot_threads(&mut self) {
        let bots = self
            .bot_workspaces()
            .into_iter()
            .map(|workspace| workspace.id)
            .collect::<Vec<_>>();
        let state = &mut self.bot_threads;
        state.lists.retain(|bot_id, _| bots.contains(bot_id));
        state.listed_panes.retain(|bot_id, _| bots.contains(bot_id));
        for bot_id in bots {
            self.refresh_bot_threads(bot_id);
        }
    }

    /// After a snapshot in Bots mode: refreshes the bots whose live threads
    /// changed since their list was requested.
    pub(crate) fn refresh_changed_bot_threads(&mut self) {
        if self.sidebar.sidebar_mode != SidebarMode::Bots {
            return;
        }
        let changed = self
            .bot_workspaces()
            .into_iter()
            .filter_map(|workspace| {
                let bot = workspace.bot.as_ref().filter(|bot| has_threads(bot))?;
                let listed = self.bot_threads.listed_panes.get(&workspace.id)?;
                (!listed.iter().eq(bot.thread_panes.keys())).then_some(workspace.id)
            })
            .collect::<Vec<_>>();
        for bot_id in changed {
            self.refresh_bot_threads(bot_id);
        }
    }

    /// Refreshes now and every few seconds until Bots mode is left.
    pub(crate) fn start_bot_threads_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_all_bot_threads();
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
                    this.refresh_all_bot_threads();
                    true
                }) else {
                    break;
                };
            }
        })
        .detach();
    }

    /// Shows a bot's thread: a saved thread id resumes it in a new tab, a
    /// live one focuses its pane, and `None` starts a new thread tab.
    pub(crate) fn open_bot_thread(
        &mut self,
        bot_id: Uuid,
        thread_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::OpenBotThread { bot_id, thread_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::BotThreadOpened { tab_id, pane_id }) => {
                        this.bot_threads.activated = Some(pane_id);
                        this.editor.modal = Modal::None;
                        this.sidebar.dismissed_workspace_tabs.remove(&tab_id);
                        this.sidebar.workspace_tab_scope = WorkspaceTabScope::Workstation;
                        this.layout.last_sizes.clear();
                        this.focus_created_pane(bot_id, pane_id, cx);
                        this.mark_pane_viewed(pane_id);
                        this.refresh_bot_threads(bot_id);
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    /// Records that the user focused a bot's thread pane, once per change of
    /// the focused bot pane, so the service never evicts the thread in use.
    pub(crate) fn note_bot_pane_focus(&mut self, pane_id: Uuid) {
        if self.bot_threads.activated == Some(pane_id) {
            return;
        }
        let Some(bot_id) = self.bot_for_pane(pane_id) else {
            return;
        };
        self.bot_threads.activated = Some(pane_id);
        self.dispatch_with(
            ClientRequest::OpenBotThread {
                bot_id,
                thread_id: Some(format!("pane:{pane_id}")),
            },
            Box::new(|this, _, result| {
                if let Err(error) = result {
                    this.report(&error);
                }
            }),
        );
    }

    pub(crate) fn set_bot_thread_pinned(
        &mut self,
        bot_id: Uuid,
        thread_id: String,
        pinned: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::SetBotThreadPinned {
                bot_id,
                thread_id,
                pinned,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => this.refresh_bot_threads(bot_id),
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    /// The thread live bot pane `pane_id` shows: its agent session, else the
    /// `pane:<id>` id of a fresh thread with no saved conversation yet.
    pub(crate) fn live_thread_id(&self, bot_id: Uuid, pane_id: Uuid) -> String {
        self.bot_spec(bot_id)
            .and_then(|bot| bot.thread_panes.get(&pane_id))
            .and_then(|thread| thread.session.clone())
            .unwrap_or_else(|| format!("pane:{pane_id}"))
    }

    /// The × on a bot thread row: asks before deleting the thread.
    pub(crate) fn begin_bot_thread_delete(
        &mut self,
        bot_id: Uuid,
        thread_id: String,
        title: String,
        cx: &mut Context<Self>,
    ) {
        self.editor.color_picker = None;
        self.editor.modal = Modal::BotThreadDelete(BotThreadDeleteConfirmation {
            bot_id,
            thread_id,
            title,
        });
        cx.notify();
    }

    /// Deletes the confirmed thread: the service closes its panes and
    /// removes its saved conversation, so it leaves the list for good.
    pub(crate) fn confirm_bot_thread_delete(&mut self, cx: &mut Context<Self>) {
        let Modal::BotThreadDelete(confirmation) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        let BotThreadDeleteConfirmation {
            bot_id, thread_id, ..
        } = confirmation;
        self.dispatch_with(
            ClientRequest::DeleteBotThread {
                bot_id,
                thread_id: thread_id.clone(),
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        if let Some(threads) = this.bot_threads.lists.get_mut(&bot_id) {
                            threads.retain(|thread| thread.id != thread_id);
                        }
                        this.layout.last_sizes.clear();
                        this.refresh_bot_threads(bot_id);
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn render_bot_thread_delete_dialog(
        &self,
        confirmation: &BotThreadDeleteConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.confirm_dialog(
            div()
                .text_sm()
                .text_color(rgb(THEME.muted))
                .child("Its conversation is removed.")
                .into_any_element(),
            DialogSpec {
                title: format!("Delete thread '{}'?", confirmation.title),
                confirm_label: "Delete thread",
                confirm_tone: DialogTone::Danger,
                confirm_id: "confirm-bot-thread-delete",
                action: DialogAction::DeleteBotThread,
            },
            cx,
        )
    }

    pub(crate) fn open_bot_thread_menu(
        &mut self,
        bot_id: Uuid,
        thread: &BotThread,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor.color_picker = None;
        self.editor.modal = Modal::BotThreadMenu(BotThreadMenu {
            bot_id,
            thread_id: thread.id.clone(),
            pinned: thread.pinned,
            position,
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{relative_time, saved_threads};
    use hh_protocol::BotThread;
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

    fn thread(id: &str, updated_ms: u64, pinned: bool, live: bool) -> BotThread {
        BotThread {
            id: id.to_owned(),
            title: None,
            updated_ms,
            pinned,
            pane_id: live.then(Uuid::new_v4),
            tab_id: None,
        }
    }

    #[test]
    fn saved_threads_exclude_live_ones_and_put_pinned_then_newest_first() {
        let threads = [
            thread("old", 10, false, false),
            thread("live", 99, true, true),
            thread("new", 30, false, false),
            thread("pinned-old", 5, true, false),
            thread("pinned-new", 20, true, false),
        ];
        let order = saved_threads(&threads)
            .into_iter()
            .map(|thread| thread.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(order, ["pinned-new", "pinned-old", "new", "old"]);
    }
}
