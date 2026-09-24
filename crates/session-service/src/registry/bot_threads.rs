//! Bot threads: an omp bot's tab is a stack of live thread panes, each one
//! omp process showing one saved conversation of the bot.
use super::bots::{BotTarget, bot_tab_for_pane, local_terminal_runtime};
use super::{RegistryState, SessionRegistry};
use crate::bots::{SavedThread, saved_threads, threads_directory, valid_session_id};
use crate::layout::{activate_tab, add_tab, find_pane_in_snapshot};
use anyhow::{Context, Result, bail};
use hh_protocol::{BotThread, BotThreadPane, MAX_PANES, PaneStatus, Tab, TerminalProfile};
use std::sync::Arc;
use uuid::Uuid;

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
        Some(if first == target.active_pane {
            first
        } else {
            showing
                .find(|pane_id| *pane_id == target.active_pane)
                .unwrap_or(first)
        })
    };
    let mut threads = saved
        .into_iter()
        .map(|thread| BotThread {
            pinned: pinned(&thread.id),
            pane_id: live_pane(&thread.id),
            id: thread.id,
            title: thread.title,
            updated_ms: thread.updated_ms,
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

fn bot_tab_mut(state: &mut RegistryState, tab_id: Uuid) -> Result<&mut Tab> {
    state
        .snapshot
        .workspaces
        .iter_mut()
        .filter(|workspace| workspace.is_bots())
        .flat_map(|workspace| &mut workspace.tabs)
        .find(|tab| tab.id == tab_id && tab.bot.is_some())
        .with_context(|| format!("bot {tab_id} does not exist"))
}

fn require_omp(target: &BotTarget, tab_id: Uuid) -> Result<()> {
    if target.spec.agent != TerminalProfile::Omp {
        bail!("bot {tab_id} has no threads; only omp bots keep threads");
    }
    Ok(())
}

impl SessionRegistry {
    /// The bot's threads; bots of other agents have none.
    pub fn list_bot_threads(&self, tab_id: Uuid) -> Result<Vec<BotThread>> {
        let target = self.state.read().bot_target(tab_id)?;
        if target.spec.agent != TerminalProfile::Omp {
            return Ok(Vec::new());
        }
        let saved = saved_threads(&threads_directory(&self.bots_dir()?, tab_id));
        Ok(merge_threads(&target, saved, crate::now_ms()))
    }

    /// Shows thread `thread_id` of the bot: activates the live pane showing
    /// it, or resumes the saved session in a new pane. `None` starts a new
    /// thread (a fresh restart for bots of other agents). Returns the pane.
    pub fn open_bot_thread(&self, tab_id: Uuid, thread_id: Option<&str>) -> Result<Uuid> {
        let target = self.state.read().bot_target(tab_id)?;
        let Some(thread_id) = thread_id else {
            if target.spec.agent != TerminalProfile::Omp {
                self.restart_bot(tab_id)?;
                return Ok(target.active_pane);
            }
            return self.add_bot_thread_pane(tab_id, None);
        };
        require_omp(&target, tab_id)?;
        if let Some(pane) = thread_id.strip_prefix(PANE_THREAD_PREFIX) {
            let pane_id = Uuid::parse_str(pane)
                .ok()
                .filter(|pane_id| target.panes.contains(pane_id))
                .with_context(|| format!("thread {thread_id} is no longer open"))?;
            self.activate_bot_pane(tab_id, pane_id)?;
            return Ok(pane_id);
        }
        if !valid_session_id(thread_id) {
            bail!("invalid thread id {thread_id:?}");
        }
        let live = target
            .panes
            .iter()
            .copied()
            .filter(|pane_id| target.session_of(*pane_id) == Some(thread_id))
            .min_by_key(|pane_id| *pane_id != target.active_pane);
        if let Some(pane_id) = live {
            self.activate_bot_pane(tab_id, pane_id)?;
            return Ok(pane_id);
        }
        let saved = saved_threads(&threads_directory(&self.bots_dir()?, tab_id));
        if !saved.iter().any(|thread| thread.id == thread_id) {
            bail!("bot {tab_id} has no thread {thread_id}");
        }
        self.add_bot_thread_pane(tab_id, Some(thread_id))
    }

    /// Pins or unpins a saved thread of the bot.
    pub fn set_bot_thread_pinned(&self, tab_id: Uuid, thread_id: &str, pinned: bool) -> Result<()> {
        if !valid_session_id(thread_id) {
            bail!("only a thread with a saved conversation can be pinned");
        }
        let mut state = self.state.write();
        let previous = state.snapshot.clone();
        let spec = bot_tab_mut(&mut state, tab_id)?
            .bot
            .as_mut()
            .context("bot spec disappeared")?;
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

    /// Records the agent session bot pane `pane_id` now shows, reported by
    /// the agent itself when it starts or switches sessions.
    pub fn report_bot_session(&self, pane_id: Uuid, session_id: &str) -> Result<()> {
        if !valid_session_id(session_id) {
            bail!("invalid session id {session_id:?}");
        }
        let mut state = self.state.write();
        let tab_id = bot_tab_for_pane(&state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} is not a bot terminal"))?;
        let previous = state.snapshot.clone();
        let spec = bot_tab_mut(&mut state, tab_id)?
            .bot
            .as_mut()
            .context("bot spec disappeared")?;
        let thread = spec.thread_panes.entry(pane_id).or_default();
        if thread.session.as_deref() == Some(session_id) {
            return Ok(());
        }
        thread.session = Some(session_id.to_owned());
        self.commit_or_restore(&mut state, previous, &[])
    }

    /// Activates live bot pane `pane_id` and records when.
    fn activate_bot_pane(&self, tab_id: Uuid, pane_id: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let previous = state.snapshot.clone();
        let tab = bot_tab_mut(&mut state, tab_id)?;
        if !activate_tab(&mut tab.layout, pane_id) {
            bail!("pane {pane_id} is not a terminal of bot {tab_id}");
        }
        if let Some(spec) = &mut tab.bot {
            spec.thread_panes.entry(pane_id).or_default().activated_ms = crate::now_ms();
        }
        self.commit_or_restore(&mut state, previous, &[])
    }

    /// Opens a new active thread pane in the bot's stack, resuming `resume`
    /// or starting a new conversation, then closes idle panes over the limit.
    fn add_bot_thread_pane(&self, tab_id: Uuid, resume: Option<&str>) -> Result<Uuid> {
        let target = self.state.read().bot_target(tab_id)?;
        let launch = self.prepare_bot_launch(
            tab_id,
            &target.name,
            target.project_dir.as_deref(),
            &target.spec,
            resume,
        )?;
        if self.state.read().panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let pane_id = Uuid::new_v4();
        let cwd = launch.home.clone();
        let session =
            self.spawn_local_transport(pane_id, target.workspace_id, Some(tab_id), &cwd)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let previous = state.snapshot.clone();
            let mut pane = state.new_pane(pane_id, Some(cwd.as_path()));
            pane.title.clone_from(&target.name);
            pane.custom_title = Some(target.name.clone());
            pane.profile_override = Some(target.spec.agent);
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == target.workspace_id)
                .context("the Bots workspace disappeared")?;
            let tab = workspace
                .tabs
                .iter_mut()
                .find(|tab| tab.id == tab_id && tab.bot.is_some())
                .with_context(|| format!("bot {tab_id} does not exist"))?;
            if !add_tab(&mut tab.layout, target.active_pane, pane, true) {
                bail!("bot {tab_id} changed while opening a thread");
            }
            if let Some(spec) = &mut tab.bot {
                spec.thread_panes.insert(
                    pane_id,
                    BotThreadPane {
                        session: resume.map(str::to_owned),
                        activated_ms: crate::now_ms(),
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
        self.start_bot_agent(tab_id, session, launch);
        let evictable = self.state.read().evictable_bot_panes(tab_id)?;
        for pane_id in evictable {
            if let Err(error) = self.close_pane(pane_id) {
                eprintln!("could not close idle thread pane {pane_id}: {error:#}");
            }
        }
        Ok(pane_id)
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
pub(crate) fn select_evictions(threads: &[LiveThread], active: Uuid) -> Vec<Uuid> {
    let excess = threads.len().saturating_sub(MAX_LIVE_THREADS);
    let mut candidates = threads
        .iter()
        .filter(|thread| thread.pane_id != active)
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
    /// Live panes of bot `tab_id` to close; see [`select_evictions`].
    pub(crate) fn evictable_bot_panes(&self, tab_id: Uuid) -> Result<Vec<Uuid>> {
        let target = self.bot_target(tab_id)?;
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
