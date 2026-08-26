//! Session registry core: state, recovery, history, and spawn plumbing.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::history::HistoryArchive;
use crate::layout::{
    find_pane_in_snapshot, find_pane_mut_in_snapshot, first_pane_id, pane_ids_in_snapshot,
    retain_persistable_panes, workspace_id_for_pane,
};
use crate::persistence::{MAX_TITLE_CHARS, SnapshotStore, default_snapshot_path};
use anyhow::{Context, Result, bail, ensure};
use hh_protocol::{
    HistoryArchiveStatus, HistoryClearScope, HistoryCursor, HistoryPageDirection, HistorySettings,
    NotificationKind, Pane, PaneKind, PaneStatus, PaneStreamState, SessionNotification,
    SessionSnapshot, StreamDiagnostics, TerminalHistoryPage, TerminalIdentity, TerminalProfile,
    TerminalScreen, TmuxSessionId, WorkspaceConnection,
};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use uuid::Uuid;

use crate::process::{fallback_cwd, shell_title, valid_local_cwd};
use crate::pty::{PtySession, RawPaneEvent};
use crate::registry::identity::{
    refresh_process_metadata, refresh_runtime_metadata, set_pane_runtime_label,
};
use crate::registry::remote::{RemoteLsGate, TmuxScanGate};
use crate::registry::status::{contract_status, heuristic_status};
use crate::registry::streaming::DiagnosticsSampler;
pub use remote::{TmuxAttachmentResult, TmuxScanResult};

mod identity;
mod panes;
mod remote;
mod status;
mod streaming;
mod tabs;
mod workspaces;

pub(crate) const MAX_NOTIFICATIONS: usize = 200;

#[derive(Debug)]
pub(crate) struct RuntimePane {
    backend: RuntimePaneBackend,
}

#[derive(Debug)]
pub(crate) enum RuntimePaneBackend {
    Terminal(TerminalRuntimePane),
    Browser,
    Assistant,
}

#[derive(Debug)]
pub(crate) struct TerminalRuntimePane {
    session: Arc<PtySession>,
    last_valid_cwd: PathBuf,
    kind: RuntimePaneKind,
    recovered: bool,
    exit_status: Option<String>,
    detected_command_profile: Option<TerminalProfile>,
    omp_title_status: Option<PaneStatus>,
}

impl RuntimePane {
    pub(crate) fn terminal(&self) -> Option<&TerminalRuntimePane> {
        match &self.backend {
            RuntimePaneBackend::Terminal(terminal) => Some(terminal),
            RuntimePaneBackend::Browser | RuntimePaneBackend::Assistant => None,
        }
    }

    pub(crate) fn terminal_mut(&mut self) -> Option<&mut TerminalRuntimePane> {
        match &mut self.backend {
            RuntimePaneBackend::Terminal(terminal) => Some(terminal),
            RuntimePaneBackend::Browser | RuntimePaneBackend::Assistant => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SshWorkspaceIds {
    workspace: Uuid,
    tab: Uuid,
    pane: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RuntimePaneKind {
    Local,
    SystemSsh {
        host: String,
    },
    TmuxLocal {
        session_id: TmuxSessionId,
    },
    TmuxSystemSsh {
        host: String,
        session_id: TmuxSessionId,
    },
}

impl RuntimePaneKind {
    pub(crate) fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub(crate) fn is_runtime_only(&self) -> bool {
        matches!(self, Self::TmuxLocal { .. } | Self::TmuxSystemSsh { .. })
    }

    /// Runs over the workstation's SSH transport, so its liveness reflects
    /// whether that workstation is still reachable.
    pub(crate) fn is_remote(&self) -> bool {
        matches!(self, Self::SystemSsh { .. } | Self::TmuxSystemSsh { .. })
    }

    pub(crate) fn tmux_session_id(&self) -> Option<&TmuxSessionId> {
        match self {
            Self::TmuxLocal { session_id } | Self::TmuxSystemSsh { session_id, .. } => {
                Some(session_id)
            }
            Self::Local | Self::SystemSsh { .. } => None,
        }
    }

    pub(crate) fn shell_label(&self) -> String {
        match self {
            Self::Local => shell_title(),
            Self::SystemSsh { host } => format!("ssh {host}"),
            Self::TmuxLocal { .. } | Self::TmuxSystemSsh { .. } => "tmux".to_owned(),
        }
    }
}

pub(crate) fn runtime_kind_for_workspace(connection: &WorkspaceConnection) -> RuntimePaneKind {
    match connection {
        WorkspaceConnection::Local => RuntimePaneKind::Local,
        WorkspaceConnection::SystemSsh { destination, .. } => RuntimePaneKind::SystemSsh {
            host: destination.clone(),
        },
    }
}

#[derive(Debug)]
pub(crate) struct RegistryState {
    snapshot: SessionSnapshot,
    panes: HashMap<Uuid, RuntimePane>,
    notifications: VecDeque<SessionNotification>,
    next_notification_id: u64,
    next_terminal_number: u32,
    next_group_number: u32,
    last_identity_refresh: Option<Instant>,
}

impl RegistryState {
    pub(crate) fn new_pane(&mut self, id: Uuid, cwd: Option<&Path>) -> Pane {
        let title = cwd.and_then(Path::file_name).map_or_else(
            || {
                let fallback = format!("Terminal {}", self.next_terminal_number);
                self.next_terminal_number += 1;
                fallback
            },
            |name| name.to_string_lossy().into_owned(),
        );
        Pane {
            id,
            kind: PaneKind::Terminal,
            title,
            shell: shell_title(),
            color: None,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        }
    }

    pub(crate) fn set_pane_status(&mut self, pane_id: Uuid, status: PaneStatus) {
        let Some(pane) = find_pane_mut_in_snapshot(&mut self.snapshot, pane_id) else {
            return;
        };
        if pane.status == status {
            return;
        }
        pane.status = status;
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
    }

    pub(crate) fn terminal_pane(&self, pane_id: Uuid) -> Result<&TerminalRuntimePane> {
        self.panes
            .get(&pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?
            .terminal()
            .with_context(|| format!("pane {pane_id} is not a terminal"))
    }

    pub(crate) fn terminal_pane_mut(&mut self, pane_id: Uuid) -> Result<&mut TerminalRuntimePane> {
        self.panes
            .get_mut(&pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?
            .terminal_mut()
            .with_context(|| format!("pane {pane_id} is not a terminal"))
    }

    pub(crate) fn require_terminal_layout_pane(&self, pane_id: Uuid) -> Result<()> {
        match self.panes.get(&pane_id) {
            Some(RuntimePane {
                backend: RuntimePaneBackend::Terminal(_),
            }) => Ok(()),
            Some(RuntimePane {
                backend: RuntimePaneBackend::Browser,
            }) => bail!("browser tabs cannot create terminal panes"),
            Some(RuntimePane {
                backend: RuntimePaneBackend::Assistant,
            }) => bail!("assistant tabs cannot create terminal panes"),
            None => bail!("pane {pane_id} does not exist"),
        }
    }
}

impl RegistryState {
    pub(crate) fn drain_pane_events(&mut self) {
        let pending = self
            .panes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                let terminal = runtime.terminal()?;
                let events = terminal.session.try_drain_events()?;
                (!events.is_empty()).then_some((*pane_id, events))
            })
            .collect::<Vec<_>>();
        for (pane_id, events) in pending {
            for event in events {
                self.apply_pane_event(pane_id, event);
            }
        }
    }

    fn apply_pane_event(&mut self, pane_id: Uuid, event: RawPaneEvent) {
        let (profile, current_status) = find_pane_in_snapshot(&self.snapshot, pane_id)
            .map(|pane| (pane.identity.profile, pane.status))
            .unwrap_or_default();
        match (event.kind, event.message) {
            (NotificationKind::Message, Some(message)) => {
                if let Some(status) = contract_status(&message) {
                    self.set_pane_status(pane_id, status);
                    match status {
                        PaneStatus::NeedsApproval => self.append_notification(
                            pane_id,
                            NotificationKind::Attention,
                            Some("needs approval".to_owned()),
                            event.at_ms,
                        ),
                        PaneStatus::NeedsInput => self.append_notification(
                            pane_id,
                            NotificationKind::Attention,
                            Some("needs input".to_owned()),
                            event.at_ms,
                        ),
                        PaneStatus::Done => self.append_notification(
                            pane_id,
                            NotificationKind::Completed,
                            None,
                            event.at_ms,
                        ),
                        PaneStatus::Idle | PaneStatus::Working | PaneStatus::Attention => {}
                    }
                    return;
                }
                if let Some(status) = heuristic_status(profile, &message) {
                    self.set_pane_status(pane_id, status);
                }
                self.append_notification(
                    pane_id,
                    NotificationKind::Message,
                    Some(message),
                    event.at_ms,
                );
            }
            (NotificationKind::Attention, message) => {
                self.append_notification(
                    pane_id,
                    NotificationKind::Attention,
                    message,
                    event.at_ms,
                );
                if !matches!(profile, TerminalProfile::Terminal | TerminalProfile::Tmux) {
                    self.set_pane_status(
                        pane_id,
                        if current_status == PaneStatus::NeedsApproval {
                            PaneStatus::NeedsInput
                        } else {
                            PaneStatus::Attention
                        },
                    );
                }
            }
            (kind, message) => {
                self.append_notification(pane_id, kind, message, event.at_ms);
            }
        }
    }

    pub(crate) fn append_notification(
        &mut self,
        pane_id: Uuid,
        kind: NotificationKind,
        message: Option<String>,
        at_ms: u64,
    ) {
        if kind == NotificationKind::Attention
            && let Some(existing) = self.notifications.iter_mut().rev().find(|notification| {
                notification.pane_id == pane_id
                    && notification.kind == NotificationKind::Attention
                    && !notification.read
            })
        {
            existing.at_ms = at_ms;
            return;
        }
        let Some(workspace_id) = workspace_id_for_pane(&self.snapshot, pane_id) else {
            return;
        };
        let Some(workspace) = self
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        let Some(pane) = find_pane_in_snapshot(&self.snapshot, pane_id) else {
            return;
        };
        let notification = SessionNotification {
            id: self.next_notification_id,
            pane_id,
            workspace_id,
            kind,
            message,
            pane_title: pane.title.clone(),
            workspace_title: workspace.title.clone(),
            profile: pane.identity.profile,
            at_ms,
            read: false,
        };
        self.next_notification_id = self.next_notification_id.saturating_add(1);
        if self.notifications.len() == MAX_NOTIFICATIONS {
            self.notifications.pop_front();
        }
        self.notifications.push_back(notification);
    }
}

pub(crate) struct InitialTerminalSpawn {
    session: Arc<PtySession>,
    kind: RuntimePaneKind,
    pane_title: String,
    pane_shell: String,
    tab_title: String,
}
/// How often the background identity worker refreshes runtime metadata.
const IDENTITY_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Owns the background identity-refresh thread. Refreshing runtime metadata
/// enumerates processes, which is too expensive to run inline on every
/// desktop poll; the worker keeps exit/identity staleness bounded to this
/// interval instead. The thread is stopped and joined when the last
/// registry handle drops.
struct IdentityWorker {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for IdentityWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdentityWorker")
            .field("running", &self.handle.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for IdentityWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl IdentityWorker {
    fn spawn(state: Arc<RwLock<RegistryState>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("rmux-identity-refresh".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    thread::sleep(IDENTITY_REFRESH_INTERVAL);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    refresh_process_metadata(&state, false);
                }
            })
            .expect("spawn identity refresh worker");
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRegistry {
    state: Arc<RwLock<RegistryState>>,
    _identity_worker: Arc<IdentityWorker>,
    diagnostics_sampler: Arc<Mutex<DiagnosticsSampler>>,
    shutdown_requested: Arc<AtomicBool>,
    store: Option<SnapshotStore>,
    history: HistoryArchive,
    tmux_scan_gate: Arc<Mutex<TmuxScanGate>>,
    remote_ls_gate: Arc<Mutex<RemoteLsGate>>,
}

#[derive(Debug)]
pub struct PaneUpdateBatch {
    pub session_revision: u64,
    pub snapshot: Option<SessionSnapshot>,
    pub screens: Vec<TerminalScreen>,
    pub pane_states: Vec<PaneStreamState>,
    pub notifications: Vec<SessionNotification>,
    pub diagnostics: StreamDiagnostics,
}

pub(crate) struct CountingWriter(u64);

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .saturating_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn serialized_len(value: &impl Serialize) -> Result<u64> {
    let mut counter = CountingWriter(0);
    serde_json::to_writer(&mut counter, value).context("measure protocol payload")?;
    Ok(counter.0)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub(crate) fn encode_desired_state(state: &RegistryState) -> Result<Vec<u8>> {
    let mut snapshot = state.snapshot.clone();
    let runtime_only_panes = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| {
            runtime
                .terminal()
                .is_some_and(|terminal| terminal.kind.is_runtime_only())
                .then_some(*pane_id)
        })
        .collect::<HashSet<_>>();
    if !runtime_only_panes.is_empty() {
        for workspace in &mut snapshot.workspaces {
            workspace
                .tabs
                .retain_mut(|tab| retain_persistable_panes(&mut tab.layout, &runtime_only_panes));
        }
    }
    // Recovery never replays per-tab SSH authentication. Keep directly
    // connected SSH tabs in local workstations as explicit offline panes
    // instead of silently replacing their transport with a local shell.
    let mut offline_panes = pane_ids_in_snapshot(&snapshot)
        .into_iter()
        .filter(|pane_id| {
            !state.panes.contains_key(pane_id)
                && find_pane_in_snapshot(&snapshot, *pane_id)
                    .is_some_and(|pane| matches!(pane.kind, PaneKind::Terminal))
        })
        .collect::<HashSet<_>>();
    let mut cwd_by_pane = HashMap::new();
    for (pane_id, runtime) in &state.panes {
        let Some(terminal) = runtime.terminal() else {
            continue;
        };
        match &terminal.kind {
            RuntimePaneKind::SystemSsh { host } => {
                offline_panes.insert(*pane_id);
                if let Some(pane) = find_pane_mut_in_snapshot(&mut snapshot, *pane_id) {
                    const OFFLINE_SUFFIX: &str = " — Offline; reconnect required";
                    let host_chars = MAX_TITLE_CHARS
                        .saturating_sub("SSH ".chars().count() + OFFLINE_SUFFIX.chars().count());
                    let host: String = host.chars().take(host_chars).collect();
                    let offline_title = format!("SSH {host}{OFFLINE_SUFFIX}");
                    pane.title.clone_from(&offline_title);
                    pane.custom_title = Some(offline_title);
                }
            }
            RuntimePaneKind::Local => {
                cwd_by_pane.insert(*pane_id, terminal.last_valid_cwd.clone());
            }
            RuntimePaneKind::TmuxLocal { .. } | RuntimePaneKind::TmuxSystemSsh { .. } => {}
        }
    }
    SnapshotStore::encode_with_offline(&snapshot, &cwd_by_pane, &offline_panes)
}

pub(crate) fn terminate_runtime_panes(panes: &HashMap<Uuid, RuntimePane>) {
    for terminal in panes.values().filter_map(RuntimePane::terminal) {
        let _ = terminal.session.terminate_and_wait();
    }
}

impl SessionRegistry {
    pub fn new() -> Result<Self> {
        Self::seeded(None, HistoryArchive::disabled())
    }

    pub fn load_default() -> Result<Self> {
        Self::persistent(default_snapshot_path()?)
    }

    pub fn persistent(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.is_absolute() {
            bail!("recovery snapshot path must be absolute");
        }
        let history_root = path
            .parent()
            .context("recovery snapshot path has no parent")?
            .join("history");
        let history = HistoryArchive::open(history_root)?;
        let store = SnapshotStore::new(path);
        let Some(mut recovered) = store.load_or_quarantine()? else {
            let registry = Self::seeded(Some(store), history)?;
            registry.persist()?;
            return Ok(registry);
        };

        let fallback = fallback_cwd()?;
        let pane_ids = pane_ids_in_snapshot(&recovered.snapshot);
        let mut panes = HashMap::new();
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
            if matches!(pane_kind, PaneKind::Assistant) {
                panes.insert(
                    pane_id,
                    RuntimePane {
                        backend: RuntimePaneBackend::Assistant,
                    },
                );
                continue;
            }
            if recovered.offline_panes.contains(&pane_id) {
                continue;
            }
            let workspace_id = workspace_id_for_pane(&recovered.snapshot, pane_id)
                .context("recovered pane has no workspace")?;
            let cwd = recovered
                .cwd_by_pane
                .remove(&pane_id)
                .filter(|cwd| valid_local_cwd(cwd))
                .unwrap_or_else(|| fallback.clone());
            match PtySession::spawn_local(pane_id, workspace_id, &cwd, &history) {
                Ok(session) => {
                    panes.insert(
                        pane_id,
                        RuntimePane {
                            backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                                session,
                                last_valid_cwd: cwd,
                                kind: RuntimePaneKind::Local,
                                recovered: true,
                                exit_status: None,
                                detected_command_profile: None,
                                omp_title_status: None,
                            }),
                        },
                    );
                }
                Err(error) => {
                    terminate_runtime_panes(&panes);
                    return Err(error).context("recreate fresh shell for recovered pane");
                }
            }
        }
        for pane_id in panes
            .iter()
            .filter_map(|(pane_id, runtime)| runtime.terminal().is_some().then_some(*pane_id))
        {
            set_pane_runtime_label(&mut recovered.snapshot, pane_id, true, None, &shell_title());
        }
        let next_terminal_number = u32::try_from(
            panes
                .values()
                .filter(|runtime| runtime.terminal().is_some())
                .count(),
        )
        .unwrap_or(u32::MAX)
        .saturating_add(1);
        let state = Arc::new(RwLock::new(RegistryState {
            snapshot: recovered.snapshot,
            panes,
            notifications: VecDeque::new(),
            next_notification_id: 1,
            next_terminal_number,
            next_group_number: 1,
            last_identity_refresh: None,
        }));
        let registry = Self {
            state: Arc::clone(&state),
            _identity_worker: Arc::new(IdentityWorker::spawn(state)),
            diagnostics_sampler: Arc::new(Mutex::new(DiagnosticsSampler::default())),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            tmux_scan_gate: Arc::new(Mutex::new(TmuxScanGate::default())),
            remote_ls_gate: Arc::new(Mutex::new(RemoteLsGate::default())),
            store: Some(store),
            history,
        };
        registry.persist()?;
        Ok(registry)
    }

    pub(crate) fn seeded(store: Option<SnapshotStore>, history: HistoryArchive) -> Result<Self> {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).context("seeded snapshot has no pane")?;
        let workspace_id = snapshot.workspaces[0].id;
        if let Some(pane) = find_pane_mut_in_snapshot(&mut snapshot, pane_id) {
            pane.shell = shell_title();
        }
        let cwd = fallback_cwd()?;
        let session = PtySession::spawn_local(pane_id, workspace_id, &cwd, &history)?;
        let state = Arc::new(RwLock::new(RegistryState {
            snapshot,
            notifications: VecDeque::new(),
            next_notification_id: 1,
            panes: HashMap::from([(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session,
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::Local,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                        omp_title_status: None,
                    }),
                },
            )]),
            next_terminal_number: 2,
            next_group_number: 1,
            last_identity_refresh: None,
        }));
        Ok(Self {
            state: Arc::clone(&state),
            _identity_worker: Arc::new(IdentityWorker::spawn(state)),
            diagnostics_sampler: Arc::new(Mutex::new(DiagnosticsSampler::default())),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            tmux_scan_gate: Arc::new(Mutex::new(TmuxScanGate::default())),
            store,
            remote_ls_gate: Arc::new(Mutex::new(RemoteLsGate::default())),
            history,
        })
    }
    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        Ok(self.state.read().snapshot.clone())
    }

    pub fn request_shutdown(&self) -> Result<()> {
        let active_terminals = self
            .state
            .read()
            .snapshot
            .workspaces
            .iter()
            .map(|workspace| workspace.active_terminal_count)
            .sum::<u32>();
        ensure!(
            active_terminals == 0,
            "session service still owns {active_terminals} live terminal(s)"
        );
        self.shutdown_requested.store(true, Ordering::Release);
        Ok(())
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    pub fn persist(&self) -> Result<()> {
        refresh_process_metadata(&self.state, true);
        let bytes = {
            let mut state = self.state.write();
            refresh_runtime_metadata(&mut state);
            encode_desired_state(&state)?
        };
        self.write_snapshot(&bytes)
    }

    pub(crate) fn write_snapshot(&self, bytes: &[u8]) -> Result<()> {
        self.store
            .as_ref()
            .map_or(Ok(()), |store| store.write_snapshot(bytes))
    }

    pub fn history_status(&self) -> HistoryArchiveStatus {
        self.history.status()
    }

    pub fn set_history_settings(&self, settings: HistorySettings) -> Result<()> {
        self.history.update_settings(settings)
    }

    pub fn clear_history(&self, scope: HistoryClearScope) -> Result<()> {
        if let HistoryClearScope::Workspace { workspace_id } = scope {
            let state = self.state.read();
            if !state
                .snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.id == workspace_id)
            {
                bail!("workstation {workspace_id} does not exist");
            }
        }
        self.history.clear(scope)
    }

    pub fn load_history_page(
        &self,
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    ) -> Result<Option<TerminalHistoryPage>> {
        self.history.load_page(pane_id, cursor, direction)
    }

    pub fn search_archived_history(
        &self,
        pane_id: Uuid,
        query: &str,
        before: Option<HistoryCursor>,
    ) -> Result<Option<TerminalHistoryPage>> {
        self.history.search(pane_id, query, before)
    }

    pub(crate) fn pane(&self, pane_id: Uuid) -> Result<Arc<PtySession>> {
        let state = self.state.read();
        Ok(Arc::clone(&state.terminal_pane(pane_id)?.session))
    }

    pub(crate) fn cwd_for_pane(&self, pane_id: Uuid) -> Result<PathBuf> {
        let mut state = self.state.write();
        refresh_runtime_metadata(&mut state);
        let runtime = state.terminal_pane(pane_id)?;
        match &runtime.kind {
            RuntimePaneKind::Local => Ok(runtime.last_valid_cwd.clone()),
            RuntimePaneKind::SystemSsh { .. }
            | RuntimePaneKind::TmuxLocal { .. }
            | RuntimePaneKind::TmuxSystemSsh { .. } => fallback_cwd(),
        }
    }

    pub(crate) fn workspace_for_pane(&self, pane_id: Uuid) -> Result<Uuid> {
        let state = self.state.read();
        workspace_id_for_pane(&state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} has no workspace"))
    }

    pub(crate) fn workspace_connection(&self, workspace_id: Uuid) -> Result<WorkspaceConnection> {
        let state = self.state.read();
        state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.connection.clone())
            .with_context(|| format!("workstation {workspace_id} does not exist"))
    }

    pub(crate) fn spawn_pane_for_workspace(
        &self,
        pane_id: Uuid,
        workspace_id: Uuid,
        cwd: &Path,
        remote_dir: Option<&str>,
    ) -> Result<(Arc<PtySession>, RuntimePaneKind)> {
        let kind = runtime_kind_for_workspace(&self.workspace_connection(workspace_id)?);
        let session = match &kind {
            RuntimePaneKind::Local => {
                PtySession::spawn_local(pane_id, workspace_id, cwd, &self.history)?
            }
            RuntimePaneKind::SystemSsh { host } => {
                PtySession::spawn_ssh(pane_id, workspace_id, host, remote_dir, &self.history)?
            }
            RuntimePaneKind::TmuxLocal { .. } | RuntimePaneKind::TmuxSystemSsh { .. } => {
                unreachable!("workspace connection cannot resolve to a runtime-only tmux pane")
            }
        };
        Ok((session, kind))
    }
}

#[cfg(test)]
pub(crate) fn create_owner_only_directory(path: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_shutdown_requires_zero_live_terminals() {
        let registry = SessionRegistry::new().unwrap();
        assert!(registry.request_shutdown().is_err());
        assert!(!registry.shutdown_requested());

        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry.close_pane(pane_id).unwrap();
        registry.request_shutdown().unwrap();
        assert!(registry.shutdown_requested());
    }

    fn status_state(profile: TerminalProfile) -> (RegistryState, Uuid) {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let pane = find_pane_mut_in_snapshot(&mut snapshot, pane_id).unwrap();
        pane.identity.profile = profile;
        (
            RegistryState {
                snapshot,
                panes: HashMap::new(),
                notifications: VecDeque::new(),
                next_notification_id: 1,
                next_terminal_number: 2,
                next_group_number: 1,
                last_identity_refresh: None,
            },
            pane_id,
        )
    }

    #[test]
    fn contract_event_is_swallowed_and_synthesizes_attention() {
        let (mut state, pane_id) = status_state(TerminalProfile::Omp);
        state.apply_pane_event(
            pane_id,
            RawPaneEvent {
                kind: NotificationKind::Message,
                message: Some("hh-status: needs-approval".to_owned()),
                at_ms: 7,
            },
        );

        assert_eq!(
            find_pane_in_snapshot(&state.snapshot, pane_id)
                .unwrap()
                .status,
            PaneStatus::NeedsApproval
        );
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(state.notifications[0].kind, NotificationKind::Attention);
        assert_eq!(
            state.notifications[0].message.as_deref(),
            Some("needs approval")
        );
    }

    #[test]
    fn heuristic_event_sets_status_and_preserves_message() {
        let (mut state, pane_id) = status_state(TerminalProfile::Codex);
        state.apply_pane_event(
            pane_id,
            RawPaneEvent {
                kind: NotificationKind::Message,
                message: Some("Approval requested: edit src/lib.rs".to_owned()),
                at_ms: 8,
            },
        );

        assert_eq!(
            find_pane_in_snapshot(&state.snapshot, pane_id)
                .unwrap()
                .status,
            PaneStatus::NeedsApproval
        );
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(state.notifications[0].kind, NotificationKind::Message);
        assert_eq!(
            state.notifications[0].message.as_deref(),
            Some("Approval requested: edit src/lib.rs")
        );
    }

    #[test]
    fn agent_bell_upgrades_approval_to_input() {
        let (mut state, pane_id) = status_state(TerminalProfile::Omp);
        state.set_pane_status(pane_id, PaneStatus::NeedsApproval);
        state.apply_pane_event(
            pane_id,
            RawPaneEvent {
                kind: NotificationKind::Attention,
                message: None,
                at_ms: 9,
            },
        );

        assert_eq!(
            find_pane_in_snapshot(&state.snapshot, pane_id)
                .unwrap()
                .status,
            PaneStatus::NeedsInput
        );
        assert_eq!(state.notifications[0].kind, NotificationKind::Attention);
    }

    #[test]
    fn pane_input_clears_stale_prompt_status() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry
            .state
            .write()
            .set_pane_status(pane_id, PaneStatus::NeedsInput);

        registry.write_input(pane_id, b"x").unwrap();

        assert_eq!(
            find_pane_in_snapshot(&registry.snapshot().unwrap(), pane_id)
                .unwrap()
                .status,
            PaneStatus::Working
        );
    }
}
