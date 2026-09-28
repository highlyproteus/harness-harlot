//! Bot threads: an omp bot's tabs hold its live thread panes, each one omp
//! process showing one saved conversation of the bot.
//!
//! Every bot pane's launch command ends in an exit hook
//! ([`crate::bots::with_exit_hook`]). When the agent quits cleanly (a double
//! Ctrl+C, `/exit`, Ctrl+D) the pane starts a fresh conversation in place and
//! the old one stays saved; a failed exit leaves the shell and notifies.
use super::bots::{BotTarget, bot_for_pane, local_terminal_runtime, thread_tab};
use super::{RegistryState, SessionRegistry, encode_desired_state};
use crate::bots::{
    SavedThread, delete_saved_thread, saved_threads, threads_directory, valid_session_id,
};
use crate::layout::{activate_tab, find_pane_in_snapshot, layout_contains};
use crate::persistence::MAX_TABS_PER_WORKSPACE;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    AgentLaunch, BotThread, BotThreadPane, MAX_PANES, NotificationKind, PaneStatus, TerminalProfile,
};
use std::sync::Arc;
use std::thread;
use uuid::Uuid;

/// A clean agent exit sooner than this after launch leaves the shell instead
/// of starting a new thread, so an agent that quits at once cannot loop.
const MIN_AGENT_RUN_MS: u64 = 5_000;

/// Live thread panes a bot keeps before idle ones are closed.
pub(crate) const MAX_LIVE_THREADS: usize = 5;
/// Prefix of the synthetic thread id of a live pane whose session is unknown.
const PANE_THREAD_PREFIX: &str = "pane:";
/// Most pinned threads a bot remembers.
const MAX_PINNED_THREADS: usize = 500;

/// The synthetic thread id of live pane `pane_id` before its session is known.
pub(crate) fn pane_thread_id(pane_id: Uuid) -> String {
    format!("{PANE_THREAD_PREFIX}{pane_id}")
}

/// A bot's threads: saved sessions, then live panes whose session is not
/// saved yet. Pinned threads come first, then the most recently updated.
pub(crate) fn merge_threads(
    target: &BotTarget,
    saved: Vec<SavedThread>,
    now_ms: u64,
) -> Vec<BotThread> {
    let pinned = |id: &str| target.spec.pinned_threads.iter().any(|pin| pin == id);
    // The active pane wins when two live panes show the same session.
    let live_pane = |id: &str| {
        let mut showing = target
            .panes
            .iter()
            .copied()
            .filter(|pane_id| target.session_of(*pane_id) == Some(id));
        let first = showing.next()?;
        Some(if Some(first) == target.active_pane {
            first
        } else {
            showing
                .find(|pane_id| Some(*pane_id) == target.active_pane)
                .unwrap_or(first)
        })
    };
    let tab_of =
        |pane_id: Option<Uuid>| pane_id.and_then(|pane| target.tab_by_pane.get(&pane).copied());
    let mut threads = saved
        .into_iter()
        .map(|thread| {
            let pane_id = live_pane(&thread.id);
            BotThread {
                pinned: pinned(&thread.id),
                pane_id,
                tab_id: tab_of(pane_id),
                id: thread.id,
                title: thread.title,
                updated_ms: thread.updated_ms,
            }
        })
        .collect::<Vec<_>>();
    for pane_id in &target.panes {
        let id = match target.session_of(*pane_id) {
            Some(session) if threads.iter().any(|thread| thread.id == session) => continue,
            Some(session) if live_pane(session) != Some(*pane_id) => continue,
            Some(session) => session.to_owned(),
            None => pane_thread_id(*pane_id),
        };
        threads.push(BotThread {
            pinned: pinned(&id),
            id,
            title: None,
            updated_ms: now_ms,
            pane_id: Some(*pane_id),
            tab_id: tab_of(Some(*pane_id)),
        });
    }
    threads.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then(right.updated_ms.cmp(&left.updated_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    threads
}

fn require_omp(target: &BotTarget, bot_id: Uuid) -> Result<()> {
    if target.spec.agent != TerminalProfile::Omp {
        bail!("bot {bot_id} has no saved threads; only omp bots keep threads");
    }
    Ok(())
}

impl SessionRegistry {
    /// The bot's threads; bots of other agents have none.
    pub fn list_bot_threads(&self, bot_id: Uuid) -> Result<Vec<BotThread>> {
        let target = self.state.read().bot_target(bot_id)?;
        if target.spec.agent != TerminalProfile::Omp {
            return Ok(Vec::new());
        }
        let saved = saved_threads(&threads_directory(&self.bots_dir()?, bot_id));
        Ok(merge_threads(&target, saved, crate::now_ms()))
    }

    /// Shows thread `thread_id` of the bot: focuses the live pane showing
    /// it, or resumes the saved session in a new thread tab. `None` starts a
    /// new thread tab. Returns the tab and pane showing the thread.
    pub fn open_bot_thread(&self, bot_id: Uuid, thread_id: Option<&str>) -> Result<(Uuid, Uuid)> {
        let target = self.state.read().bot_target(bot_id)?;
        let Some(thread_id) = thread_id else {
            return self.add_bot_thread_tab(bot_id, None);
        };
        if let Some(pane) = thread_id.strip_prefix(PANE_THREAD_PREFIX) {
            let pane_id = Uuid::parse_str(pane)
                .ok()
                .filter(|pane_id| target.panes.contains(pane_id))
                .with_context(|| format!("thread {thread_id} is no longer open"))?;
            return self.activate_bot_pane(bot_id, pane_id);
        }
        require_omp(&target, bot_id)?;
        if !valid_session_id(thread_id) {
            bail!("invalid thread id {thread_id:?}");
        }
        let live = target
            .panes
            .iter()
            .copied()
            .filter(|pane_id| target.session_of(*pane_id) == Some(thread_id))
            .min_by_key(|pane_id| Some(*pane_id) != target.active_pane);
        if let Some(pane_id) = live {
            return self.activate_bot_pane(bot_id, pane_id);
        }
        let saved = saved_threads(&threads_directory(&self.bots_dir()?, bot_id));
        if !saved.iter().any(|thread| thread.id == thread_id) {
            bail!("bot {bot_id} has no thread {thread_id}");
        }
        self.add_bot_thread_tab(bot_id, Some(thread_id))
    }

    /// Pins or unpins a saved thread of the bot.
    pub fn set_bot_thread_pinned(&self, bot_id: Uuid, thread_id: &str, pinned: bool) -> Result<()> {
        if !valid_session_id(thread_id) {
            bail!("only a thread with a saved conversation can be pinned");
        }
        let mut state = self.state.write();
        let previous = state.snapshot.clone();
        let spec = state.bot_spec_mut(bot_id)?;
        let listed = spec.pinned_threads.iter().any(|pin| pin == thread_id);
        match (pinned, listed) {
            (true, false) => {
                if spec.pinned_threads.len() >= MAX_PINNED_THREADS {
                    bail!("a bot can pin at most {MAX_PINNED_THREADS} threads");
                }
                spec.pinned_threads.push(thread_id.to_owned());
            }
            (false, true) => spec.pinned_threads.retain(|pin| pin != thread_id),
            _ => return Ok(()),
        }
        self.commit_or_restore(&mut state, previous, &[])
    }

    /// Deletes thread `thread_id` of the bot: closes every live pane showing
    /// it (tabs left empty go too), deletes its saved conversation and drops
    /// its pin. A fresh thread without a session (`pane:<id>`) only closes
    /// its pane.
    pub fn delete_bot_thread(&self, bot_id: Uuid, thread_id: &str) -> Result<()> {
        let target = self.state.read().bot_target(bot_id)?;
        if let Some(pane) = thread_id.strip_prefix(PANE_THREAD_PREFIX) {
            let pane_id = Uuid::parse_str(pane)
                .ok()
                .filter(|pane_id| target.panes.contains(pane_id))
                .with_context(|| format!("thread {thread_id} is no longer open"))?;
            return self.close_pane(pane_id);
        }
        require_omp(&target, bot_id)?;
        if !valid_session_id(thread_id) {
            bail!("invalid thread id {thread_id:?}");
        }
        let live = target
            .panes
            .iter()
            .copied()
            .filter(|pane_id| target.session_of(*pane_id) == Some(thread_id))
            .collect::<Vec<_>>();
        for pane_id in &live {
            self.close_pane(*pane_id)?;
        }
        let deleted =
            delete_saved_thread(&threads_directory(&self.bots_dir()?, bot_id), thread_id)?;
        let mut state = self.state.write();
        let previous = state.snapshot.clone();
        let spec = state.bot_spec_mut(bot_id)?;
        let pinned = spec.pinned_threads.iter().any(|pin| pin == thread_id);
        if !pinned {
            if live.is_empty() && deleted == 0 {
                bail!("bot {bot_id} has no thread {thread_id}");
            }
            return Ok(());
        }
        spec.pinned_threads.retain(|pin| pin != thread_id);
        self.commit_or_restore(&mut state, previous, &[])
    }

    /// Records the agent session bot pane `pane_id` now shows, reported by
    /// the agent itself when it starts or switches sessions.
    pub fn report_bot_session(&self, pane_id: Uuid, session_id: &str) -> Result<()> {
        if !valid_session_id(session_id) {
            bail!("invalid session id {session_id:?}");
        }
        let mut state = self.state.write();
        let bot_id = bot_for_pane(&state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} is not a bot terminal"))?;
        let previous = state.snapshot.clone();
        let thread = state
            .bot_spec_mut(bot_id)?
            .thread_panes
            .entry(pane_id)
            .or_default();
        if thread.session.as_deref() == Some(session_id) {
            return Ok(());
        }
        thread.session = Some(session_id.to_owned());
        self.commit_or_restore(&mut state, previous, &[])
    }

    /// Handles the exit hook of bot pane `pane_id`'s agent launch `launch`.
    /// A clean exit starts a fresh conversation in the same pane off the
    /// caller's thread (the caller runs in the shell being replaced); a
    /// failed or immediate one only notifies. Reports from an earlier or
    /// already reported launch, or from a closed pane, are ignored.
    pub fn bot_agent_exited(&self, pane_id: Uuid, launch: Uuid, clean: bool) -> Result<()> {
        let (bot_id, started_ms) = {
            let mut state = self.state.write();
            let Some(bot_id) = bot_for_pane(&state.snapshot, pane_id) else {
                return Ok(());
            };
            let Some(thread) = state.bot_spec_mut(bot_id)?.thread_panes.get_mut(&pane_id) else {
                return Ok(());
            };
            match thread.launch.take() {
                Some(current) if current.id == launch => (bot_id, current.started_ms),
                current => {
                    thread.launch = current;
                    return Ok(());
                }
            }
        };
        if !clean {
            self.notify_bot_pane(
                bot_id,
                pane_id,
                "agent exited with an error; its shell is left open.",
            );
        } else if crate::now_ms().saturating_sub(started_ms) < MIN_AGENT_RUN_MS {
            self.notify_bot_pane(
                bot_id,
                pane_id,
                "agent quit right after starting, so no new thread was opened.",
            );
        } else {
            let registry = self.clone();
            thread::Builder::new()
                .name("hh-bot-new-thread".to_owned())
                .spawn(move || {
                    if let Err(error) = registry.renew_bot_thread(bot_id, pane_id) {
                        registry.notify_bot_pane(
                            bot_id,
                            pane_id,
                            &format!("could not start a new thread: {error:#}"),
                        );
                    }
                })
                .context("start the new thread")?;
        }
        Ok(())
    }

    /// Starts a fresh conversation in bot pane `pane_id`, in place: the pane
    /// forgets its session, which stays listed as a saved thread, and its
    /// agent is launched again without resuming. Workers the old
    /// conversation opened stay with the pane.
    pub(super) fn renew_bot_thread(&self, bot_id: Uuid, pane_id: Uuid) -> Result<()> {
        let target = self.state.read().bot_target(bot_id)?;
        if !target.panes.contains(&pane_id) {
            bail!("pane {pane_id} is not a thread of bot {bot_id}");
        }
        let launch = self.prepare_bot_launch(
            bot_id,
            &target.name,
            target.project_dir.as_deref(),
            &target.spec,
            None,
        )?;
        {
            let mut state = self.state.write();
            let previous = state.snapshot.clone();
            if let Some(thread) = state.bot_spec_mut(bot_id)?.thread_panes.get_mut(&pane_id) {
                thread.session = None;
            }
            self.commit_or_restore(&mut state, previous, &[])?;
        }
        self.relaunch_bot(bot_id, pane_id, launch)
    }

    /// Records a new agent launch in bot pane `pane_id` and returns its id.
    pub(super) fn record_bot_launch(&self, bot_id: Uuid, pane_id: Uuid) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let mut state = self.state.write();
        if !state.bot_target(bot_id)?.panes.contains(&pane_id) {
            bail!("pane {pane_id} is not a thread of bot {bot_id}");
        }
        state
            .bot_spec_mut(bot_id)?
            .thread_panes
            .entry(pane_id)
            .or_default()
            .launch = Some(AgentLaunch {
            id,
            started_ms: crate::now_ms(),
        });
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(id)
    }

    /// Posts a message notification on bot pane `pane_id`, prefixed with the
    /// bot's name.
    fn notify_bot_pane(&self, bot_id: Uuid, pane_id: Uuid, message: &str) {
        let mut state = self.state.write();
        if let Ok(target) = state.bot_target(bot_id) {
            state.append_notification(
                pane_id,
                NotificationKind::Message,
                Some(format!("{}'s {message}", target.name)),
                crate::now_ms(),
            );
        }
    }

    /// Shows live thread pane `pane_id` in its tab and records when it was
    /// activated. Returns its tab.
    fn activate_bot_pane(&self, bot_id: Uuid, pane_id: Uuid) -> Result<(Uuid, Uuid)> {
        let mut state = self.state.write();
        let previous = state.snapshot.clone();
        let workspace = state.bot_workspace_mut(bot_id)?;
        let tab = workspace
            .tabs
            .iter_mut()
            .find(|tab| layout_contains(&tab.layout, pane_id))
            .with_context(|| format!("pane {pane_id} is not a thread of bot {bot_id}"))?;
        activate_tab(&mut tab.layout, pane_id);
        let tab_id = tab.id;
        if let Some(spec) = &mut workspace.bot {
            spec.thread_panes.entry(pane_id).or_default().activated_ms = crate::now_ms();
        }
        self.commit_or_restore(&mut state, previous, &[])?;
        Ok((tab_id, pane_id))
    }

    /// Opens a new thread tab in the bot, resuming `resume` or starting a new
    /// conversation, then closes idle thread panes over the limit.
    fn add_bot_thread_tab(&self, bot_id: Uuid, resume: Option<&str>) -> Result<(Uuid, Uuid)> {
        let target = self.state.read().bot_target(bot_id)?;
        let launch = self.prepare_bot_launch(
            bot_id,
            &target.name,
            target.project_dir.as_deref(),
            &target.spec,
            resume,
        )?;
        if self.state.read().panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let pane_id = Uuid::new_v4();
        let tab_id = Uuid::new_v4();
        let cwd = launch.home.clone();
        let session = self.spawn_local_transport(pane_id, bot_id, Some(bot_id), &cwd)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let previous = state.snapshot.clone();
            let mut pane = state.new_pane(pane_id, Some(cwd.as_path()));
            pane.profile_override = Some(target.spec.agent);
            let workspace = state.bot_workspace_mut(bot_id)?;
            if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
                bail!("a bot keeps at most {MAX_TABS_PER_WORKSPACE} thread tabs");
            }
            workspace.tabs.push(thread_tab(tab_id, pane));
            if let Some(spec) = &mut workspace.bot {
                spec.thread_panes.insert(
                    pane_id,
                    BotThreadPane {
                        session: resume.map(str::to_owned),
                        activated_ms: crate::now_ms(),
                        launch: None,
                    },
                );
            }
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state
                .panes
                .insert(pane_id, local_terminal_runtime(Arc::clone(&session), cwd));
            self.commit_or_restore(&mut state, previous, &[pane_id])
        })();
        if let Err(error) = result {
            let _ = session.terminate_and_wait();
            return Err(error);
        }
        self.start_bot_agent(bot_id, pane_id, session, launch);
        let evictable = self.state.read().evictable_bot_panes(bot_id)?;
        for pane_id in evictable {
            if let Err(error) = self.close_pane(pane_id) {
                eprintln!("could not close idle thread pane {pane_id}: {error:#}");
            }
        }
        Ok((tab_id, pane_id))
    }
}

/// One live thread pane considered for eviction.
pub(crate) struct LiveThread {
    pub(crate) pane_id: Uuid,
    pub(crate) activated_ms: u64,
    pub(crate) status: PaneStatus,
}

/// The panes to close so at most [`MAX_LIVE_THREADS`] stay live: least
/// recently activated first, never `active` nor a pane that is busy or
/// waiting for the user. The conversations stay saved by the agent.
pub(crate) fn select_evictions(threads: &[LiveThread], active: Option<Uuid>) -> Vec<Uuid> {
    let excess = threads.len().saturating_sub(MAX_LIVE_THREADS);
    let mut candidates = threads
        .iter()
        .filter(|thread| Some(thread.pane_id) != active)
        .filter(|thread| {
            !matches!(
                thread.status,
                PaneStatus::Working
                    | PaneStatus::NeedsApproval
                    | PaneStatus::NeedsInput
                    | PaneStatus::Attention
            )
        })
        .map(|thread| (thread.activated_ms, thread.pane_id))
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates
        .into_iter()
        .take(excess)
        .map(|(_, pane_id)| pane_id)
        .collect()
}

impl RegistryState {
    /// Forgets bot pane `pane_id`'s agent launch so its exit hook is ignored;
    /// called before the pane's terminal is closed or replaced.
    pub(crate) fn forget_bot_launch(&mut self, pane_id: Uuid) {
        if let Some(bot_id) = bot_for_pane(&self.snapshot, pane_id)
            && let Ok(spec) = self.bot_spec_mut(bot_id)
            && let Some(thread) = spec.thread_panes.get_mut(&pane_id)
        {
            thread.launch = None;
        }
    }

    /// Live panes of bot `bot_id` to close; see [`select_evictions`].
    pub(crate) fn evictable_bot_panes(&self, bot_id: Uuid) -> Result<Vec<Uuid>> {
        let target = self.bot_target(bot_id)?;
        let threads = target
            .panes
            .iter()
            .map(|pane_id| LiveThread {
                pane_id: *pane_id,
                activated_ms: target
                    .spec
                    .thread_panes
                    .get(pane_id)
                    .map_or(0, |thread| thread.activated_ms),
                status: find_pane_in_snapshot(&self.snapshot, *pane_id)
                    .map_or(PaneStatus::Idle, |pane| pane.status),
            })
            .collect::<Vec<_>>();
        Ok(select_evictions(&threads, target.active_pane))
    }
}

#[cfg(test)]
#[path = "bot_threads_tests.rs"]
mod tests;
