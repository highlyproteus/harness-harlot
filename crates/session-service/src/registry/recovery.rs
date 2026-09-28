//! Reattaching local terminals after a service restart, and keeping their
//! tmux connections alive afterwards.
//!
//! The rule everything here follows: only closing a tab kills its tmux
//! window. A restart, an update, a lost connection, or a failed reattach
//! leaves the window and its program running.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use hh_protocol::{NotificationKind, PaneKind, SessionSnapshot};
use parking_lot::RwLock;
use uuid::Uuid;

use super::bots::{bot_for_pane, bot_spawn_dir};
use super::identity::set_pane_runtime_label;
use super::{
    ProcessScan, RegistryState, RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry,
    StateFiles, TerminalRuntimePane, append_tmux_notification, discover_managed_tmux,
    ensure_tmux_client, remove_retired_history_archive, tmux_session_name,
};
use crate::layout::{
    find_pane_in_snapshot, find_pane_mut_in_snapshot, pane_ids_in_snapshot, workspace_id_for_pane,
};
use crate::process::{fallback_cwd, shell_title, valid_local_cwd};
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

impl SessionRegistry {
    /// Loads the registry saved at `path` and brings back every terminal:
    /// managed tmux windows are reattached (or left running when they
    /// cannot be), SSH workstations are restored offline, and bots whose
    /// shells are new get their agent again.
    pub fn persistent(path: impl Into<PathBuf>) -> Result<Self> {
        let files = StateFiles::at(path.into())?;
        let state_dir = files.directory().to_path_buf();
        remove_retired_history_archive(&state_dir);
        let (tmux, tmux_unavailable_reason) = discover_managed_tmux(&state_dir);
        let tmux_socket_name = hh_protocol::managed_tmux_socket_name(&state_dir);
        let Some(mut recovered) = files.snapshot.load_or_quarantine()? else {
            let registry = Self::seeded_with_tmux(
                Some(files),
                tmux,
                tmux_unavailable_reason,
                tmux_socket_name,
                Some(state_dir),
            )?;
            registry.persist()?;
            return Ok(registry);
        };
        clear_offline_custom_titles(&mut recovered.snapshot);

        let started = Instant::now();
        let mut log = RecoveryLog::default();
        log.note(format!(
            "service {} (pid {}) recovering {} saved panes; tmux {}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            pane_ids_in_snapshot(&recovered.snapshot).len(),
            tmux.as_ref().map_or_else(
                || format!(
                    "unavailable ({})",
                    tmux_unavailable_reason.as_deref().unwrap_or("not found")
                ),
                |server| format!("-L {}", server.socket_name),
            )
        ));
        let fallback = fallback_cwd()?;
        let bots_dir = crate::bots::bots_directory(&state_dir);
        let pane_ids = pane_ids_in_snapshot(&recovered.snapshot);
        let existing_panes = pane_ids.iter().copied().collect::<HashSet<_>>();
        // Every window the saved layout names survives recovery, whether or
        // not it could be reattached.
        let mut keep_windows = recovered
            .tmux_by_pane
            .values()
            .map(|(window_id, _)| window_id.clone())
            .collect::<HashSet<_>>();
        let mut panes = HashMap::new();
        let mut tmux_clients = HashMap::new();
        let mut tmux_sinks = HashMap::new();
        let mut tmux_failures = Vec::new();
        let mut unattached = Vec::new();
        let mut fresh_bot_panes = Vec::new();
        let mut reattached_panes = HashSet::new();
        for pane_id in pane_ids {
            let pane_kind = find_pane_in_snapshot(&recovered.snapshot, pane_id)
                .with_context(|| format!("recovered pane {pane_id} is missing"))?
                .kind
                .clone();
            if matches!(pane_kind, PaneKind::Browser { .. }) {
                panes.insert(
                    pane_id,
                    RuntimePane {
                        backend: RuntimePaneBackend::Browser,
                    },
                );
                continue;
            }
            if matches!(pane_kind, PaneKind::Gallery) {
                panes.insert(
                    pane_id,
                    RuntimePane {
                        backend: RuntimePaneBackend::Gallery,
                    },
                );
                continue;
            }
            if recovered.offline_panes.contains(&pane_id) {
                continue;
            }
            let workspace_id = workspace_id_for_pane(&recovered.snapshot, pane_id)
                .context("recovered pane has no workspace")?;
            let bot_id = bot_for_pane(&recovered.snapshot, pane_id);
            let saved_cwd = recovered.cwd_by_pane.remove(&pane_id);
            let cwd = bot_id
                .and_then(|bot| bot_spawn_dir(&recovered.snapshot, Some(&bots_dir), bot))
                .or_else(|| saved_cwd.filter(|cwd| valid_local_cwd(cwd)))
                .unwrap_or_else(|| fallback.clone());
            let outcome = tmux.as_ref().map(|server| {
                LocalPaneRecovery {
                    server,
                    workspace_id,
                    pane_id,
                    bot_id,
                    cwd: &cwd,
                    saved: recovered.tmux_by_pane.remove(&pane_id),
                    legacy_workspace: recovered.legacy_tmux_workspace.get(&pane_id).copied(),
                }
                .run(&mut tmux_clients, &mut tmux_sinks, &mut log)
            });
            // `reattached`: the program is attached again, so its progress
            // stays and its next title is a baseline. `program_kept`: its
            // program still runs (attached or not), so a bot is not relaunched.
            let (session, reattached, program_kept) = match outcome {
                Some(Ok(LocalRecovery::Reattached(session))) => {
                    reattached_panes.insert(pane_id);
                    (Ok(session), true, true)
                }
                Some(Ok(LocalRecovery::Fresh(session))) => (Ok(session), false, false),
                Some(Ok(LocalRecovery::Unattached(session))) => {
                    unattached.push(pane_id);
                    (Ok(session), false, true)
                }
                Some(Err(error)) => {
                    tmux_failures.push((pane_id, format!("{error:#}")));
                    log.note(format!("pane {pane_id} fell back to a plain shell"));
                    (
                        PtySession::spawn_local(pane_id, workspace_id, bot_id, &cwd),
                        false,
                        false,
                    )
                }
                None => (
                    PtySession::spawn_local(pane_id, workspace_id, bot_id, &cwd),
                    false,
                    false,
                ),
            };
            match session {
                Ok(session) => {
                    if let Some((window_id, _)) = session.tmux_ids() {
                        keep_windows.insert(window_id.to_owned());
                    }
                    if let Some(bot_id) = bot_id.filter(|_| !program_kept) {
                        fresh_bot_panes.push((bot_id, pane_id));
                    }
                    panes.insert(
                        pane_id,
                        RuntimePane {
                            backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                                session,
                                last_valid_cwd: cwd,
                                kind: RuntimePaneKind::Local,
                                recovered: true,
                                exit_status: None,
                                process_scan: ProcessScan::Unknown,
                                omp_title_status: None,
                                // A reattached program restates its state in
                                // its title; that is not a new event.
                                title_baseline_pending: reattached,
                            }),
                        },
                    );
                }
                Err(error) => {
                    log.note(format!(
                        "recovery stopped: pane {pane_id} could not start a shell: {error:#}; tmux windows were left running"
                    ));
                    log.flush(Some(&state_dir));
                    release_runtime_panes(&panes);
                    return Err(error).context("recreate fresh shell for recovered pane");
                }
            }
        }
        // Sessions of retired workspaces (the shared Bots workspace) are gone;
        // a workstation whose projects moved out keeps its own session.
        let legacy_sessions = recovered
            .legacy_tmux_workspace
            .values()
            .copied()
            .filter(|workspace_id| {
                !recovered
                    .snapshot
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.id == *workspace_id)
            })
            .collect::<HashSet<_>>();
        if let Some(client) = tmux_clients.values().next() {
            for legacy in legacy_sessions {
                let _ = client.kill_named_session(&tmux_session_name(legacy));
            }
        }
        for (from, to) in &recovered.gallery_copies {
            if let Err(error) = crate::gallery::copy_gallery_contents(*from, *to) {
                eprintln!("failed to copy the gallery of a migrated project: {error:#}");
            }
        }
        for client in tmux_clients.values() {
            sweep_closed_tab_windows(client, &keep_windows, &existing_panes, &mut log);
        }
        // A pane attached before a retry replaced its workspace's connection
        // still points at the dead one; the shared sink map lets it move over.
        for (pane_id, runtime) in &panes {
            let Some(terminal) = runtime.terminal() else {
                continue;
            };
            if let Some(workspace_id) = workspace_id_for_pane(&recovered.snapshot, *pane_id)
                && let (Some(current), Some(live)) = (
                    terminal.session.tmux_client(),
                    tmux_clients.get(&workspace_id),
                )
                && !Arc::ptr_eq(&current, live)
            {
                terminal.session.replace_tmux_client(live);
            }
        }
        for pane_id in panes
            .iter()
            .filter_map(|(pane_id, runtime)| runtime.terminal().is_some().then_some(*pane_id))
        {
            set_pane_runtime_label(&mut recovered.snapshot, pane_id, true, None, &shell_title());
        }
        // Progress belongs to the process that reported it; only a truly
        // reattached tmux pane still runs that process. A pane whose window
        // could not be reattached keeps running too, but it is not attached
        // here, so it must never count as reattached for progress.
        for pane_id in pane_ids_in_snapshot(&recovered.snapshot) {
            if !reattached_panes.contains(&pane_id)
                && let Some(pane) = find_pane_mut_in_snapshot(&mut recovered.snapshot, pane_id)
            {
                pane.progress = None;
            }
        }
        let stored_notifications = files.notifications.load_or_quarantine();
        log.note(format!(
            "recovery finished in {} ms: {} unattached, {} plain-shell fallbacks",
            started.elapsed().as_millis(),
            unattached.len(),
            tmux_failures.len()
        ));
        log.flush(Some(&state_dir));
        let next_terminal_number = u32::try_from(
            panes
                .values()
                .filter(|runtime| runtime.terminal().is_some())
                .count(),
        )
        .unwrap_or(u32::MAX)
        .saturating_add(1);
        let tmux_unavailable = tmux.is_none();
        let state = Arc::new(RwLock::new(RegistryState {
            snapshot: recovered.snapshot,
            panes,
            tmux,
            tmux_clients,
            tmux_sinks,
            remote_clients: HashMap::new(),
            tmux_socket_name,
            state_dir: Some(state_dir),
            notifications: stored_notifications.items,
            next_notification_id: stored_notifications.next_id,
            notifications_dirty: false,
            notifications_epoch: Uuid::new_v4(),
            next_terminal_number,
            last_identity_refresh: None,
        }));
        append_recovery_notifications(
            &mut state.write(),
            tmux_unavailable,
            tmux_unavailable_reason,
            tmux_failures,
            unattached,
        );
        let registry = Self::from_state(state, Some(files));
        registry.persist()?;
        for (bot_id, pane_id) in fresh_bot_panes {
            registry.relaunch_recovered_bot(bot_id, pane_id);
        }
        Ok(registry)
    }
}

/// Tells the user what recovery could not do: tmux is unavailable (with its
/// reason, if known), windows fell back to plain shells, or saved windows
/// still run but could not be reattached.
fn append_recovery_notifications(
    state: &mut RegistryState,
    tmux_unavailable: bool,
    tmux_unavailable_reason: Option<String>,
    tmux_failures: Vec<(Uuid, String)>,
    unattached: Vec<Uuid>,
) {
    if tmux_unavailable && cfg!(not(test)) {
        append_tmux_notification(
            state,
            match tmux_unavailable_reason {
                Some(reason) => format!(
                    "managed tmux is unavailable ({reason}); terminals will not survive a service restart"
                ),
                None => "tmux 3.2+ was not found; terminals will not survive a service restart"
                    .to_owned(),
            },
        );
    }
    for (_, error) in tmux_failures {
        append_tmux_notification(
            state,
            format!("tmux window could not be created; using a plain shell: {error}"),
        );
    }
    for pane_id in unattached {
        state.append_notification(
            pane_id,
            NotificationKind::Message,
            Some(
                "couldn't reattach this terminal; its program is still running. Use Reattach Exited Terminal to try again"
                    .to_owned(),
            ),
            crate::now_ms(),
        );
    }
}
