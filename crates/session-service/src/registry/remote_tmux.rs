//! SSH terminals as windows of HH's tmux server on the remote host.
//!
//! One control connection per workstation and destination drives the
//! host's `hh-<workstation>` session. Each tab is a window tagged with its
//! pane id, so a reconnect finds it again and rebuilds its full scrollback.
//! A lost connection, an app quit, an update, or a service restart leaves the
//! windows running; closing a tab kills its window (at the next connection if
//! the host was unreachable when it closed).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hh_protocol::{NotificationKind, PaneStatus, WorkspaceConnection};
use parking_lot::Mutex;
use uuid::Uuid;

use super::identity::{refresh_workspace_activity, set_pane_runtime_label};
use super::panes::Reattached;
use super::recovery::{RecoveryLog, sweep_closed_tab_windows};
use super::{
    ProcessScan, RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry,
    TerminalRuntimePane, append_tmux_notification, encode_desired_state, ssh_pane_title,
    tmux_session_name,
};
use crate::layout::{find_pane_mut_in_snapshot, pane_ids_for_workspace};
use crate::process::fallback_cwd;
use crate::pty::PtySession;
use crate::tmux_control::TmuxControlClient;
use crate::tmux_remote::{RemoteConnectError, RemoteTmux};

/// How often a sign-in terminal is checked for completion.
const SIGN_IN_POLL: Duration = Duration::from_millis(250);
/// A sign-in left open longer than this is no longer watched; Reconnect
/// starts a new one.
const SIGN_IN_WATCH_LIMIT: Duration = Duration::from_hours(1);
/// Bound for killing an offline workstation's remote session.
const OFFLINE_KILL_TIMEOUT: Duration = Duration::from_secs(30);

/// Title of a tab while it asks the user to sign in to `host`.
pub(crate) fn sign_in_title(host: &str) -> String {
    format!("Sign in to {host}")
}

impl SessionRegistry {
    fn remote_tmux(&self, host: &str) -> Result<RemoteTmux> {
        RemoteTmux::new(host, &self.state.read().tmux_socket_name)
    }

    /// The live control connection to `host` for `workspace_id`, connecting
    /// (without prompting) when there is none. A new connection first kills
    /// windows of tabs closed while the host was unreachable.
    pub(crate) fn remote_client(
        &self,
        workspace_id: Uuid,
        host: &str,
    ) -> Result<Arc<TmuxControlClient>> {
        let key = (workspace_id, host.to_owned());
        if let Some(client) = self.state.read().remote_clients.get(&key)
            && client.is_alive()
        {
            return Ok(Arc::clone(client));
        }
        let remote = self.remote_tmux(host)?;
        let client = TmuxControlClient::spawn_remote(
            &remote,
            &tmux_session_name(workspace_id),
            Arc::new(Mutex::new(HashMap::new())),
        )?;
        let (existing_panes, state_dir) = {
            let mut state = self.state.write();
            state.remote_clients.insert(key, Arc::clone(&client));
            let existing = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(pane_ids_for_workspace)
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>();
            (existing, state.state_dir.clone())
        };
        let mut log = RecoveryLog::default();
        log.note(format!(
            "connected to tmux on {host} for workstation {workspace_id}"
        ));
        sweep_closed_tab_windows(&client, &HashSet::new(), &existing_panes, &mut log);
        log.flush(state_dir.as_deref());
        Ok(client)
    }

    /// Starts the terminals `pane_ids` of `workspace_id` on `host`: each
    /// reattaches to its window when one is still running there
    /// (`Reattached::RunningProgram`), else opens one in `remote_dir`
    /// (`Reattached::FreshShell`).
    ///
    /// A host that needs a prompt gets one sign-in terminal (for the first
    /// pane only); the others connect once the sign-in completes. A host
    /// without tmux 3.2+ gets plain SSH shells, as before.
    pub(crate) fn spawn_ssh_sessions(
        &self,
        workspace_id: Uuid,
        host: &str,
        remote_dir: Option<&str>,
        pane_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Arc<PtySession>, Reattached)>> {
        if pane_ids.is_empty() || ssh_test_seam_enabled() {
            return spawn_plain_ssh_sessions(workspace_id, host, remote_dir, pane_ids);
        }
        let client = match self.remote_client(workspace_id, host) {
            Ok(client) => client,
            Err(error) => {
                return match error.downcast_ref::<RemoteConnectError>() {
                    Some(RemoteConnectError::NoTmux(reason)) => {
                        append_tmux_notification(
                            &mut self.state.write(),
                            format!(
                                "{host} has no usable tmux ({reason}); its terminals are plain SSH shells that end when the connection drops. Install tmux 3.2 or newer there to keep them running"
                            ),
                        );
                        spawn_plain_ssh_sessions(workspace_id, host, remote_dir, pane_ids)
                    }
                    Some(RemoteConnectError::SignInRequired(_)) => {
                        let pane_id = pane_ids[0];
                        let remote = self.remote_tmux(host)?;
                        let session = PtySession::spawn_sign_in(pane_id, &remote)?;
                        self.watch_sign_in(workspace_id, host, pane_id, Arc::clone(&session));
                        Ok(vec![(pane_id, session, Reattached::FreshShell)])
                    }
                    Some(RemoteConnectError::Unreachable(_)) | None => Err(error),
                };
            }
        };
        let listed = client.list_panes()?;
        let mut sessions: Vec<(Uuid, Arc<PtySession>, Reattached)> =
            Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let result = match listed.iter().find(|pane| pane.tag == Some(*pane_id)) {
                Some(existing) => PtySession::attach_tmux(
                    *pane_id,
                    Arc::clone(&client),
                    existing.window_id.clone(),
                    existing.pane_id.clone(),
                    existing.pane_pid,
                )
                .map(|session| (session, Reattached::RunningProgram)),
                None => PtySession::spawn_remote_tmux(*pane_id, remote_dir, &client)
                    .map(|session| (session, Reattached::FreshShell)),
            };
            match result {
                Ok((session, behind)) => sessions.push((*pane_id, session, behind)),
                Err(error) => {
                    for (_, session, _) in sessions {
                        let _ = session.detach("reconnect failed");
                    }
                    return Err(error).with_context(|| format!("open terminal on {host}"));
                }
            }
        }
        Ok(sessions)
    }

    /// One new terminal on `host`; see `spawn_ssh_sessions`.
    pub(crate) fn spawn_ssh_transport(
        &self,
        pane_id: Uuid,
        workspace_id: Uuid,
        host: &str,
        remote_dir: Option<&str>,
    ) -> Result<Arc<PtySession>> {
        self.spawn_ssh_sessions(workspace_id, host, remote_dir, &[pane_id])?
            .into_iter()
            .next()
            .map(|(_, session, _)| session)
            .context("no SSH terminal was started")
    }

    /// Closes the control connections of `workspace_id` without touching the
    /// windows on the host (disconnect).
    pub(crate) fn close_remote_clients(&self, workspace_id: Uuid) {
        let clients = {
            let mut state = self.state.write();
            let keys = state
                .remote_clients
                .keys()
                .filter(|(workspace, _)| *workspace == workspace_id)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.remote_clients.remove(&key))
                .collect::<Vec<_>>()
        };
        for client in clients {
            client.close();
        }
    }

    /// Deleting a workstation ends its sessions on every host it used: over
    /// the live connection, or with a one-off non-interactive ssh when the
    /// host is not connected (best effort; reported if it fails).
    pub(crate) fn kill_remote_sessions(&self, workspace_id: Uuid, hosts: &HashSet<String>) {
        for host in hosts {
            let key = (workspace_id, host.clone());
            let client = self.state.write().remote_clients.remove(&key);
            if let Some(client) = client.filter(|client| client.is_alive())
                && client.kill_session().is_ok()
            {
                client.close();
                continue;
            }
            let Ok(remote) = self.remote_tmux(host) else {
                continue;
            };
            let registry = self.clone();
            let host = host.clone();
            let _ = thread::Builder::new()
                .name("hh-remote-kill".to_owned())
                .spawn(move || {
                    let killed = remote
                        .kill_session_command(&tmux_session_name(workspace_id))
                        .and_then(|command| run_with_timeout(command, OFFLINE_KILL_TIMEOUT));
                    if let Err(error) = killed {
                        append_tmux_notification(
                            &mut registry.state.write(),
                            format!(
                                "could not end the deleted workstation's terminals on {host} ({error:#}); they keep running in tmux there (`tmux -L {} ls`)",
                                remote.socket_name
                            ),
                        );
                    }
                });
        }
    }

    /// Watches a sign-in terminal: names its tab, and once ssh exits with
    /// HH's shared connection open, connects every waiting terminal of the
    /// workstation on `host`.
    fn watch_sign_in(
        &self,
        workspace_id: Uuid,
        host: &str,
        pane_id: Uuid,
        session: Arc<PtySession>,
    ) {
        let registry = self.clone();
        let host = host.to_owned();
        let _ = thread::Builder::new()
            .name("hh-ssh-sign-in".to_owned())
            .spawn(move || {
                let started = Instant::now();
                let mut titled = false;
                loop {
                    thread::sleep(SIGN_IN_POLL);
                    if !registry.pane_runs_session(pane_id, &session) {
                        if titled || started.elapsed() > Duration::from_secs(10) {
                            return;
                        }
                        continue;
                    }
                    if !titled {
                        titled = registry.set_pane_title(pane_id, &sign_in_title(&host));
                    }
                    if matches!(session.exit_status(), Ok(Some(_))) {
                        break;
                    }
                    if started.elapsed() > SIGN_IN_WATCH_LIMIT {
                        return;
                    }
                }
                if let Err(error) = registry.finish_sign_in(workspace_id, &host) {
                    append_tmux_notification(
                        &mut registry.state.write(),
                        format!("signing in to {host} did not finish ({error:#}); use Reconnect to try again"),
                    );
                }
            });
    }

    fn pane_runs_session(&self, pane_id: Uuid, session: &Arc<PtySession>) -> bool {
        self.state
            .read()
            .panes
            .get(&pane_id)
            .and_then(RuntimePane::terminal)
            .is_some_and(|terminal| Arc::ptr_eq(&terminal.session, session))
    }

    fn set_pane_title(&self, pane_id: Uuid, title: &str) -> bool {
        let mut state = self.state.write();
        let Some(pane) = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id) else {
            return false;
        };
        title.clone_into(&mut pane.title);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        true
    }

    /// Connects every terminal of `workspace_id` on `host` that is waiting
    /// (not started, exited, or the finished sign-in) now that the shared
    /// connection lets ssh run without prompting.
    fn finish_sign_in(&self, workspace_id: Uuid, host: &str) -> Result<()> {
        let (pane_ids, remote_dir) = {
            let state = self.state.read();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .context("the workstation was deleted during sign-in")?;
            let whole_workstation = matches!(
                &workspace.connection,
                WorkspaceConnection::SystemSsh { destination, .. } if destination == host
            );
            let pane_ids = pane_ids_for_workspace(workspace)
                .into_iter()
                .filter(|pane_id| match state.panes.get(pane_id) {
                    None => whole_workstation,
                    Some(runtime) => runtime.terminal().is_some_and(|terminal| {
                        terminal.kind.ssh_host() == Some(host)
                            && matches!(terminal.kind, RuntimePaneKind::SystemSsh { .. })
                            && terminal
                                .session
                                .exit_status()
                                .is_ok_and(|status| status.is_some())
                    }),
                })
                .collect::<Vec<_>>();
            let remote_dir = whole_workstation
                .then(|| workspace.working_dir.clone())
                .flatten();
            (pane_ids, remote_dir)
        };
        if pane_ids.is_empty() {
            return Ok(());
        }
        // Connect first: a sign-in that was cancelled or failed must not
        // immediately open another one.
        self.remote_client(workspace_id, host)?;
        let sessions =
            self.spawn_ssh_sessions(workspace_id, host, remote_dir.as_deref(), &pane_ids)?;
        self.install_ssh_sessions(workspace_id, host, sessions)
    }

    /// Puts started SSH sessions into their panes, replacing whatever ran
    /// there, and marks the workstation connected. A reattached window keeps
    /// its status and progress; see `install_reattached_session`.
    fn install_ssh_sessions(
        &self,
        workspace_id: Uuid,
        host: &str,
        sessions: Vec<(Uuid, Arc<PtySession>, Reattached)>,
    ) -> Result<()> {
        let cwd = fallback_cwd()?;
        let mut state = self.state.write();
        let mut replaced = Vec::new();
        for (pane_id, session, behind) in sessions {
            if find_pane_mut_in_snapshot(&mut state.snapshot, pane_id).is_none() {
                let _ = session.detach("tab closed");
                continue;
            }
            if state.terminal_pane(pane_id).is_ok() {
                replaced.push(state.install_reattached_session(pane_id, session, behind)?);
            } else {
                state.panes.insert(
                    pane_id,
                    RuntimePane {
                        backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                            session,
                            last_valid_cwd: cwd.clone(),
                            kind: RuntimePaneKind::SystemSsh {
                                host: host.to_owned(),
                            },
                            recovered: false,
                            exit_status: None,
                            process_scan: ProcessScan::Unknown,
                            omp_title_status: None,
                            title_baseline_pending: behind == Reattached::RunningProgram,
                        }),
                    },
                );
                set_pane_runtime_label(&mut state.snapshot, pane_id, false, None, "system OpenSSH");
                if behind == Reattached::FreshShell {
                    state.set_pane_status(pane_id, PaneStatus::Idle);
                }
            }
            if let Some(pane) = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id) {
                pane.title = ssh_pane_title(host);
                "ssh".clone_into(&mut pane.shell);
            }
        }
        refresh_workspace_activity(&mut state);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        let first_pane = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .and_then(|workspace| pane_ids_for_workspace(workspace).first().copied());
        if let Some(pane_id) = first_pane {
            state.append_notification(
                pane_id,
                NotificationKind::Message,
                Some(format!("connected to {host}")),
                crate::now_ms(),
            );
        }
        drop(state);
        drop(replaced);
        self.write_snapshot(&bytes)
    }
}

/// The debug-only local stand-ins for ssh keep their plain-PTY behavior.
fn ssh_test_seam_enabled() -> bool {
    #[cfg(test)]
    if crate::pty::TEST_LOCAL_SSH_SEAM_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    #[cfg(debug_assertions)]
    if std::env::var_os(crate::pty::LOCAL_SSH_TEST_SEAM_ENV).is_some() {
        return true;
    }
    false
}

fn spawn_plain_ssh_sessions(
    workspace_id: Uuid,
    host: &str,
    remote_dir: Option<&str>,
    pane_ids: &[Uuid],
) -> Result<Vec<(Uuid, Arc<PtySession>, Reattached)>> {
    let mut sessions = Vec::with_capacity(pane_ids.len());
    for pane_id in pane_ids {
        match PtySession::spawn_ssh(*pane_id, workspace_id, host, remote_dir) {
            Ok(session) => sessions.push((*pane_id, session, Reattached::FreshShell)),
            Err(error) => {
                for (_, session, _) in sessions {
                    let _ = session.terminate_and_wait();
                }
                return Err(error);
            }
        }
    }
    Ok(sessions)
}

fn run_with_timeout(mut command: std::process::Command, timeout: Duration) -> Result<()> {
    let mut child = command.spawn().context("start ssh")?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().context("wait for ssh")? {
            anyhow::ensure!(status.success(), "ssh exited with {status}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("ssh timed out");
        }
        thread::sleep(Duration::from_millis(100));
    }
}
