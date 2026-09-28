//! Reattaching local terminals after a service restart, and keeping their
//! tmux connections alive afterwards.
//!
//! The rule everything here follows: only closing a tab kills its tmux
//! window. A restart, an update, a lost connection, or a failed reattach
//! leaves the window and its program running.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use hh_protocol::SessionSnapshot;
use parking_lot::RwLock;
use uuid::Uuid;

use super::{RegistryState, RuntimePane, ensure_tmux_client, tmux_session_name};
use crate::layout::{find_pane_mut_in_snapshot, pane_ids_in_snapshot, workspace_id_for_pane};
use crate::pty::PtySession;
use crate::tmux_control::{ListedPane, PaneSinks, TmuxControlClient, TmuxServer};

/// The recovery log is truncated past this size so it never grows unbounded.
const MAX_RECOVERY_LOG_BYTES: u64 = 512 * 1024;
/// Minimum gap between attempts to re-establish one lost local connection.
const HEAL_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Appends timestamped lines to `<state>/recovery.log` (owner-only). It
/// records what happened to each terminal on restart and when a lost tmux
/// connection was re-established, so a lost session leaves evidence.
/// Contains ids and error text only, never terminal output.
#[derive(Debug, Default)]
pub(crate) struct RecoveryLog {
    lines: Vec<String>,
}

impl RecoveryLog {
    pub(crate) fn note(&mut self, line: impl Into<String>) {
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown-time".to_owned());
        self.lines.push(format!("{timestamp} {}", line.into()));
    }

    pub(crate) fn flush(&mut self, state_dir: Option<&Path>) {
        let Some(state_dir) = state_dir else {
            self.lines.clear();
            return;
        };
        if self.lines.is_empty() {
            return;
        }
        let path = state_dir.join("recovery.log");
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            if file.metadata()?.len() > MAX_RECOVERY_LOG_BYTES {
                file.set_len(0)?;
            }
            for line in &self.lines {
                writeln!(file, "{line}")?;
            }
            Ok(())
        };
        if let Err(error) = write() {
            eprintln!("failed to write {}: {error}", path.display());
        }
        self.lines.clear();
    }
}

/// How one saved local terminal came back.
pub(crate) enum LocalRecovery {
    /// Its tmux window was found and attached with its scrollback.
    Reattached(Arc<PtySession>),
    /// It had no live window any more; a new one was opened.
    Fresh(Arc<PtySession>),
    /// Its window still runs but could not be attached; kept as is.
    Unattached(Arc<PtySession>),
}

pub(crate) struct LocalPaneRecovery<'a> {
    pub server: &'a TmuxServer,
    pub workspace_id: Uuid,
    pub pane_id: Uuid,
    pub bot_id: Option<Uuid>,
    pub cwd: &'a Path,
    pub saved: Option<(String, String)>,
    /// Set for a bot thread or project tab migrated into another
    /// workstation, whose window still lives in the old session.
    pub legacy_workspace: Option<Uuid>,
}

impl LocalPaneRecovery<'_> {
    /// Finds the pane's window (by saved ids, then by its `@hh-pane` tag) and
    /// attaches it, retrying once on a fresh connection. Never kills or
    /// covers a saved window: if it cannot be attached, the pane stays
    /// `Unattached`. `Err` only when no tmux connection works and there is no
    /// saved window to protect, so a plain shell is the honest fallback.
    pub(crate) fn run(
        &self,
        clients: &mut HashMap<Uuid, Arc<TmuxControlClient>>,
        sinks: &mut HashMap<Uuid, PaneSinks>,
        log: &mut RecoveryLog,
    ) -> Result<LocalRecovery> {
        let mut last_error = anyhow!("tmux was not tried");
        for attempt in 1..=2 {
            let client = match ensure_tmux_client(self.server, self.workspace_id, clients, sinks) {
                Ok(client) => client,
                Err(error) => {
                    log.note(format!(
                        "pane {} attempt {attempt}: tmux connection failed: {error:#}",
                        self.pane_id
                    ));
                    clients.remove(&self.workspace_id);
                    last_error = error;
                    continue;
                }
            };
            match self.attach(&client) {
                Ok(Some(session)) => {
                    log.note(format!(
                        "pane {} reattached to {}",
                        self.pane_id,
                        session.tmux_ids().map_or("?", |(window, _)| window)
                    ));
                    return Ok(LocalRecovery::Reattached(session));
                }
                Ok(None) => {
                    if let Some((window, _)) = &self.saved {
                        log.note(format!(
                            "pane {} saved window {window} no longer exists; opening a new one",
                            self.pane_id
                        ));
                    }
                    return match PtySession::spawn_tmux(
                        self.pane_id,
                        self.workspace_id,
                        self.bot_id,
                        self.cwd,
                        &client,
                    ) {
                        Ok(session) => {
                            log.note(format!(
                                "pane {} opened new window {}",
                                self.pane_id,
                                session.tmux_ids().map_or("?", |(window, _)| window)
                            ));
                            Ok(LocalRecovery::Fresh(session))
                        }
                        Err(error) => {
                            log.note(format!(
                                "pane {} could not open a new window: {error:#}",
                                self.pane_id
                            ));
                            Err(error)
                        }
                    };
                }
                Err(error) => {
                    log.note(format!(
                        "pane {} attempt {attempt}: reattach failed: {error:#}",
                        self.pane_id
                    ));
                    if !client.is_alive() {
                        clients.remove(&self.workspace_id);
                    }
                    last_error = error;
                }
            }
        }
        match &self.saved {
            Some((window_id, tmux_pane_id)) => {
                let reason = format!("{last_error:#}");
                log.note(format!(
                    "pane {} left unattached; window {window_id} keeps running",
                    self.pane_id
                ));
                Ok(LocalRecovery::Unattached(PtySession::unattached_tmux(
                    self.pane_id,
                    window_id.clone(),
                    tmux_pane_id.clone(),
                    reason,
                )))
            }
            None => Err(last_error),
        }
    }

    /// `Ok(None)` when the pane has no window left to attach.
    fn attach(&self, client: &Arc<TmuxControlClient>) -> Result<Option<Arc<PtySession>>> {
        if self.legacy_workspace.is_some()
            && let Some((window_id, _)) = &self.saved
        {
            // A migrated bot thread or project tab: its window still lives
            // in the old session.
            let _ = client.move_window_to_session(window_id, &tmux_session_name(self.workspace_id));
        }
        let listed = client.list_panes()?;
        let Some(existing) = find_window(&listed, self.saved.as_ref(), self.pane_id) else {
            return Ok(None);
        };
        PtySession::attach_tmux(
            self.pane_id,
            Arc::clone(client),
            existing.window_id.clone(),
            existing.pane_id.clone(),
            existing.pane_pid,
        )
        .map(Some)
    }
}

/// The window saved for `pane_id`, else one tagged with it.
pub(crate) fn find_window<'a>(
    listed: &'a [ListedPane],
    saved: Option<&(String, String)>,
    pane_id: Uuid,
) -> Option<&'a ListedPane> {
    saved
        .and_then(|(window_id, tmux_pane_id)| {
            listed
                .iter()
                .find(|pane| &pane.window_id == window_id && &pane.pane_id == tmux_pane_id)
        })
        .or_else(|| listed.iter().find(|pane| pane.tag == Some(pane_id)))
}

/// Kills windows no saved or live tab refers to: tabs closed while the
/// service could not act on them. A window is kept if the saved layout
/// names it, a live pane uses it, or its `@hh-pane` tag is a pane that
/// still exists.
pub(crate) fn sweep_closed_tab_windows(
    client: &TmuxControlClient,
    keep_windows: &HashSet<String>,
    existing_panes: &HashSet<Uuid>,
    log: &mut RecoveryLog,
) {
    if !client.is_alive() {
        return;
    }
    let Ok(listed) = client.list_panes() else {
        return;
    };
    for pane in listed {
        if keep_windows.contains(&pane.window_id)
            || pane.tag.is_some_and(|tag| existing_panes.contains(&tag))
        {
            continue;
        }
        match client.kill_window(&pane.window_id) {
            Ok(()) => log.note(format!(
                "killed window {} of a tab that no longer exists",
                pane.window_id
            )),
            Err(error) => log.note(format!(
                "could not kill window {} of a closed tab: {error:#}",
                pane.window_id
            )),
        }
    }
}

/// Ends the plain shells recovery started; tmux windows are released, not
/// killed, so a failed startup never costs a running program.
pub(crate) fn release_runtime_panes(panes: &HashMap<Uuid, RuntimePane>) {
    for terminal in panes.values().filter_map(RuntimePane::terminal) {
        if terminal.session.tmux_ids().is_none() {
            let _ = terminal.session.terminate_and_wait();
        }
    }
}

/// Earlier builds wrote the offline label of an SSH tab into its custom
/// title, where it stuck after a reconnect. Moves it back to the automatic
/// title, which the next connection replaces.
pub(crate) fn clear_offline_custom_titles(snapshot: &mut SessionSnapshot) {
    for pane_id in pane_ids_in_snapshot(snapshot) {
        if let Some(pane) = find_pane_mut_in_snapshot(snapshot, pane_id)
            && pane
                .custom_title
                .as_deref()
                .is_some_and(super::is_offline_ssh_title)
            && let Some(title) = pane.custom_title.take()
        {
            pane.title = title;
        }
    }
}

/// Re-establishes lost local tmux connections and points every pane at the
/// live one. Runs on the identity worker; the panes' programs never noticed.
pub(crate) fn heal_local_tmux_clients(
    shared: &Arc<RwLock<RegistryState>>,
    last_attempt: &mut HashMap<Uuid, Instant>,
) {
    let (server, state_dir, workspaces) = {
        let state = shared.read();
        let Some(server) = state.tmux.clone() else {
            return;
        };
        let workspaces = state
            .panes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                let client = runtime.terminal()?.session.tmux_client()?;
                if client.is_remote() {
                    return None;
                }
                let workspace_id = workspace_id_for_pane(&state.snapshot, *pane_id)?;
                let current = state.tmux_clients.get(&workspace_id);
                let needs_reconnect = !client.is_alive()
                    || current.is_none_or(|current| !Arc::ptr_eq(current, &client));
                needs_reconnect.then_some(workspace_id)
            })
            .collect::<HashSet<_>>();
        (server, state.state_dir.clone(), workspaces)
    };
    if workspaces.is_empty() {
        return;
    }
    let mut log = RecoveryLog::default();
    let now = Instant::now();
    for workspace_id in workspaces {
        if last_attempt
            .get(&workspace_id)
            .is_some_and(|last| now.saturating_duration_since(*last) < HEAL_RETRY_INTERVAL)
        {
            continue;
        }
        last_attempt.insert(workspace_id, now);
        let (existing, sinks) = {
            let state = shared.read();
            (
                state.tmux_clients.get(&workspace_id).cloned(),
                state.tmux_sinks.get(&workspace_id).cloned(),
            )
        };
        let Some(sinks) = sinks else {
            continue;
        };
        let client = match existing.filter(|client| client.is_alive()) {
            Some(client) => client,
            None => match TmuxControlClient::spawn(
                &server,
                &tmux_session_name(workspace_id),
                Arc::clone(&sinks),
            ) {
                Ok(client) => {
                    log.note(format!(
                        "workstation {workspace_id}: re-established the lost tmux connection"
                    ));
                    client
                }
                Err(error) => {
                    log.note(format!(
                        "workstation {workspace_id}: tmux connection could not be re-established: {error:#}"
                    ));
                    continue;
                }
            },
        };
        match client.list_panes() {
            Ok(listed) => client.mark_missing_panes_exited(&listed),
            Err(error) => {
                log.note(format!(
                    "workstation {workspace_id}: could not list windows after reconnecting: {error:#}"
                ));
                continue;
            }
        }
        let mut state = shared.write();
        state.tmux_clients.insert(workspace_id, Arc::clone(&client));
        let pane_ids = state
            .panes
            .keys()
            .copied()
            .filter(|pane_id| {
                workspace_id_for_pane(&state.snapshot, *pane_id) == Some(workspace_id)
            })
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            if let Some(session) = state
                .panes
                .get(&pane_id)
                .and_then(RuntimePane::terminal)
                .map(|terminal| Arc::clone(&terminal.session))
                && session
                    .tmux_client()
                    .is_some_and(|current| !current.is_remote() && !Arc::ptr_eq(&current, &client))
            {
                session.replace_tmux_client(&client);
            }
        }
    }
    log.flush(state_dir.as_deref());
}
