//! Session registry core: state, recovery, and spawn plumbing.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::bots::discover_coding_agents;
use crate::layout::{
    find_pane_in_snapshot, find_pane_mut_in_snapshot, first_pane_id, pane_ids_in_snapshot,
    retain_persistable_panes, workspace_id_for_pane,
};
use crate::notifications::NotificationStore;
use crate::persistence::{MAX_TITLE_CHARS, SnapshotStore, default_snapshot_path};
use anyhow::{Context, Result, bail, ensure};
use hh_protocol::{
    BrowserAction, BrowserCommandOutcome, BrowserCommandRequest, CodingAgent, NotificationKind,
    Pane, PaneAuthority, PaneKind, PaneProgress, PaneRevisionCursor, PaneStatus, PaneStreamState,
    SessionNotification, SessionSnapshot, StreamDiagnostics, TerminalIdentity, TerminalProfile,
    TerminalScreen, TerminalTransport, TmuxSessionId, WorkspaceConnection,
};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use uuid::Uuid;

use crate::process::{fallback_cwd, shell_title, valid_local_cwd};
use crate::pty::{PtySession, RawPaneEvent};
use crate::registry::bots::{bot_for_pane, bot_spawn_dir};
use crate::registry::identity::{
    refresh_process_metadata, refresh_runtime_metadata, set_pane_runtime_label,
};
use crate::registry::recovery::{
    LocalPaneRecovery, LocalRecovery, RecoveryLog, clear_offline_custom_titles,
    heal_local_tmux_clients, release_runtime_panes, sweep_closed_tab_windows,
};
use crate::registry::remote::{RemoteLsGate, TmuxScanGate};
use crate::registry::status::{contract_status, heuristic_status, omp_title_status};
use crate::registry::streaming::DiagnosticsSampler;
use crate::tmux_control::{PaneSinks, TmuxControlClient, TmuxServer};
pub use remote::{TmuxAttachmentResult, TmuxScanResult};
// Detached (not exited) pane ends, reported by transports outside the
// registry; `identity::PaneEnd::classify` recognises them.
pub(crate) use identity::{PANE_CONNECTION_LOST, PANE_NOT_REATTACHED_PREFIX};

mod bot_threads;
mod bots;
mod identity;
mod panes;
mod recovery;
mod remote;
mod remote_tmux;
mod status;
mod streaming;
mod tabs;
mod workspaces;

pub(crate) const MAX_NOTIFICATIONS: usize = 200;
pub(crate) const BROWSER_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct RuntimePane {
    backend: RuntimePaneBackend,
}

#[derive(Debug)]
pub(crate) enum RuntimePaneBackend {
    Terminal(TerminalRuntimePane),
    Browser,
    Gallery,
}

#[derive(Debug)]
pub(crate) struct TerminalRuntimePane {
    session: Arc<PtySession>,
    last_valid_cwd: PathBuf,
    kind: RuntimePaneKind,
    recovered: bool,
    exit_status: Option<String>,
    process_scan: ProcessScan,
    omp_title_status: Option<PaneStatus>,
    /// The next omp title status seen is a baseline, not news: the pane was
    /// reattached to a program that kept running while HH was away, so its
    /// first title only restates where it already was. Every path that
    /// reattaches a still-running program must set it.
    title_baseline_pending: bool,
}

impl TerminalRuntimePane {
    /// Where the terminal runs, which names it when no program identifies it.
    pub(crate) fn location(&self) -> PaneLocation {
        match self.kind.ssh_host() {
            Some(host) => PaneLocation::Remote(host.to_owned()),
            None => PaneLocation::Local(self.last_valid_cwd.clone()),
        }
    }
}

/// What the last process scan found running under a local pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessScan {
    /// Not scanned (a remote pane, or none yet), or too many processes to tell.
    Unknown,
    /// The scan finished and found no known agent: the pane runs its shell.
    NoAgent,
    Agent(TerminalProfile),
}

impl ProcessScan {
    pub(crate) fn agent(self) -> Option<TerminalProfile> {
        match self {
            Self::Agent(profile) => Some(profile),
            Self::Unknown | Self::NoAgent => None,
        }
    }
}

/// Where a terminal runs: a local directory, or an SSH host. A remote pane's
/// `last_valid_cwd` is only the local fallback directory, so it never names it.
#[derive(Clone, Debug)]
pub(crate) enum PaneLocation {
    Local(PathBuf),
    Remote(String),
}

impl RuntimePane {
    pub(crate) fn terminal(&self) -> Option<&TerminalRuntimePane> {
        match &self.backend {
            RuntimePaneBackend::Terminal(terminal) => Some(terminal),
            RuntimePaneBackend::Browser | RuntimePaneBackend::Gallery => None,
        }
    }

    pub(crate) fn terminal_mut(&mut self) -> Option<&mut TerminalRuntimePane> {
        match &mut self.backend {
            RuntimePaneBackend::Terminal(terminal) => Some(terminal),
            RuntimePaneBackend::Browser | RuntimePaneBackend::Gallery => None,
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

    /// The SSH destination this pane runs on, if it is remote.
    pub(crate) fn ssh_host(&self) -> Option<&str> {
        match self {
            Self::SystemSsh { host } | Self::TmuxSystemSsh { host, .. } => Some(host),
            Self::Local | Self::TmuxLocal { .. } => None,
        }
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

/// Default name of a terminal on SSH workstation `host`, e.g. `SSH devbox`.
pub(crate) fn ssh_pane_title(host: &str) -> String {
    format!("SSH {host}")
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
    pub(crate) snapshot: SessionSnapshot,
    panes: HashMap<Uuid, RuntimePane>,
    tmux: Option<TmuxServer>,
    tmux_clients: HashMap<Uuid, Arc<TmuxControlClient>>,
    tmux_sinks: HashMap<Uuid, PaneSinks>,
    /// Control connections to HH's tmux on SSH hosts, per workstation and
    /// destination (a local workstation can hold direct SSH tabs).
    remote_clients: HashMap<(Uuid, String), Arc<TmuxControlClient>>,
    /// `hh`, `hh-dev`, or `hh-<hash>`: names HH's tmux server locally and on
    /// every SSH host, so builds and tests never share remote sessions.
    tmux_socket_name: String,
    /// Where `recovery.log` goes; `None` for an in-memory registry.
    state_dir: Option<PathBuf>,
    notifications: VecDeque<SessionNotification>,
    next_notification_id: u64,
    /// The ring changed since it was last written to disk.
    notifications_dirty: bool,
    /// Random per service start; tells clients their cursor is stale.
    notifications_epoch: Uuid,
    next_terminal_number: u32,
    last_identity_refresh: Option<Instant>,
}

/// Which stored notification a status change creates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StatusNotice {
    /// Only an actual transition into a done or needs-you status notifies.
    OnTransition,
    /// An explicit signal (bell, OSC status, process exit): notifies even when
    /// the status is unchanged, with this message or the status default.
    Always(Option<String>),
    /// The caller stores its own notification for this event.
    Suppressed,
    /// A baseline, not an event: the status changes without marking the
    /// pane unseen or storing a notification.
    Silent,
}

/// Stored notification kind for a status that needs the user's eyes.
fn notification_kind_for(status: PaneStatus) -> Option<NotificationKind> {
    match status {
        PaneStatus::Done => Some(NotificationKind::Completed),
        PaneStatus::NeedsInput | PaneStatus::NeedsApproval | PaneStatus::Attention => {
            Some(NotificationKind::Attention)
        }
        PaneStatus::Idle | PaneStatus::Working => None,
    }
}

fn default_status_message(status: PaneStatus) -> Option<String> {
    match status {
        PaneStatus::NeedsInput => Some("Needs input".to_owned()),
        PaneStatus::NeedsApproval => Some("Needs approval".to_owned()),
        PaneStatus::Idle | PaneStatus::Working | PaneStatus::Attention | PaneStatus::Done => None,
    }
}

impl RegistryState {
    pub(crate) fn authorized_terminal(
        &self,
        authority: &PaneAuthority,
    ) -> Result<&TerminalRuntimePane> {
        ensure!(
            !matches!(authority.transport, TerminalTransport::Unknown),
            "pane authority transport must be exact"
        );
        let workspace = self
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == authority.workspace_id)
            .context("pane authority workspace changed or no longer exists")?;
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| tab.id == authority.tab_id)
            .context("pane authority tab changed or no longer exists")?;
        let pane = crate::layout::find_pane(&tab.layout, authority.pane_id)
            .context("pane authority mapping changed or no longer exists")?;
        ensure!(pane.kind == authority.kind, "pane authority kind changed");
        let terminal = self.terminal_pane(authority.pane_id)?;
        let transport = match &terminal.kind {
            RuntimePaneKind::Local | RuntimePaneKind::TmuxLocal { .. } => TerminalTransport::Local,
            RuntimePaneKind::SystemSsh { host } | RuntimePaneKind::TmuxSystemSsh { host, .. } => {
                TerminalTransport::SystemSsh {
                    destination: host.clone(),
                }
            }
        };
        ensure!(
            transport == authority.transport,
            "pane authority transport changed"
        );
        Ok(terminal)
    }

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
            status_changed_at_ms: 0,
            custom_title: None,
            profile_override: None,
            custom_icon: None,
            unseen: false,
            progress: None,
        }
    }

    /// A new terminal pane spawned as `kind`. Local panes are named from
    /// `cwd`; SSH panes after their host, because there `cwd` is only the
    /// local fallback directory and would name a remote tab after this Mac.
    pub(crate) fn new_runtime_pane(
        &mut self,
        id: Uuid,
        cwd: &Path,
        kind: &RuntimePaneKind,
    ) -> Pane {
        let mut pane = self.new_pane(id, Some(cwd));
        if let Some(host) = kind.ssh_host() {
            pane.title = ssh_pane_title(host);
            "ssh".clone_into(&mut pane.shell);
        }
        pane
    }

    pub(crate) fn set_pane_status(&mut self, pane_id: Uuid, status: PaneStatus) {
        self.update_pane_status(pane_id, status, StatusNotice::OnTransition, crate::now_ms());
    }

    /// Sets the pane's status. Entering done or a needs-you status marks the
    /// pane unseen and stores one notification, as `notice` directs.
    pub(crate) fn update_pane_status(
        &mut self,
        pane_id: Uuid,
        status: PaneStatus,
        notice: StatusNotice,
        at_ms: u64,
    ) {
        let Some(pane) = find_pane_mut_in_snapshot(&mut self.snapshot, pane_id) else {
            return;
        };
        let changed = pane.status != status;
        if changed {
            pane.status = status;
            pane.status_changed_at_ms = crate::now_ms();
        }
        let kind = notification_kind_for(status);
        let signalled = kind.is_some()
            && notice != StatusNotice::Silent
            && (changed || matches!(notice, StatusNotice::Always(_)));
        let newly_unseen = signalled && !pane.unseen;
        if newly_unseen {
            pane.unseen = true;
        }
        if changed || newly_unseen {
            self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        }
        let Some(kind) = kind.filter(|_| signalled) else {
            return;
        };
        let message = match notice {
            StatusNotice::OnTransition => default_status_message(status),
            StatusNotice::Always(message) => message.or_else(|| default_status_message(status)),
            StatusNotice::Suppressed | StatusNotice::Silent => return,
        };
        self.append_notification(pane_id, kind, message, at_ms);
    }

    /// Clears the pane's unseen flag and marks its notifications read.
    /// Returns whether the unseen flag changed, which `sessions.json` saves.
    pub(crate) fn mark_pane_seen(&mut self, pane_id: Uuid) -> Result<bool> {
        let pane = find_pane_mut_in_snapshot(&mut self.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        let was_unseen = pane.unseen;
        if was_unseen {
            pane.unseen = false;
            self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        }
        for notification in &mut self.notifications {
            if notification.pane_id == pane_id && !notification.read {
                notification.read = true;
                self.notifications_dirty = true;
            }
        }
        Ok(was_unseen)
    }

    /// Undoes a bot restart's `PANE_RESTARTING` mark after the restart
    /// failed, so the old shell's real exit is observed again.
    pub(crate) fn abandon_restart(&mut self, pane_id: Uuid) {
        if let Ok(terminal) = self.terminal_pane_mut(pane_id)
            && terminal.exit_status.as_deref() == Some(identity::PANE_RESTARTING)
        {
            terminal.exit_status = None;
        }
    }

    /// Drops the pane's task progress: the process that reported it is gone.
    pub(crate) fn clear_pane_progress(&mut self, pane_id: Uuid) {
        if let Some(pane) = find_pane_mut_in_snapshot(&mut self.snapshot, pane_id)
            && pane.progress.take().is_some()
        {
            self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        }
    }

    /// Replaces the task progress of a live terminal pane.
    pub(crate) fn set_pane_progress(
        &mut self,
        pane_id: Uuid,
        progress: Option<PaneProgress>,
    ) -> Result<()> {
        let terminal = self.terminal_pane(pane_id)?;
        ensure!(
            terminal.exit_status.is_none(),
            "pane {pane_id} has no running process"
        );
        if let Some(progress) = &progress {
            progress.validate().map_err(anyhow::Error::msg)?;
        }
        let pane = find_pane_mut_in_snapshot(&mut self.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.progress != progress {
            pane.progress = progress;
            self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        }
        Ok(())
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
                backend: RuntimePaneBackend::Gallery,
            }) => bail!("gallery panes cannot host terminals"),
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
                    self.update_pane_status(
                        pane_id,
                        status,
                        StatusNotice::Always(None),
                        event.at_ms,
                    );
                    return;
                }
                // The message itself is the notification of this event; one
                // that asks for the user is stored as attention, keeping its text.
                let status = heuristic_status(profile, &message);
                if let Some(status) = status {
                    self.update_pane_status(pane_id, status, StatusNotice::Suppressed, event.at_ms);
                }
                let kind = if matches!(
                    status,
                    Some(PaneStatus::NeedsInput | PaneStatus::NeedsApproval)
                ) {
                    NotificationKind::Attention
                } else {
                    NotificationKind::Message
                };
                self.append_notification(pane_id, kind, Some(message), event.at_ms);
            }
            (NotificationKind::Attention, message) => {
                // Under HH's tmux, omp announces everything with a bare bell. It
                // needs the user only while its title says so (an ask or an
                // approval); any other bell is its end-of-turn "Complete". The
                // stored status follows the title only every refresh, so the
                // title is read now: an approval bell never records Completed.
                let omp_title = (profile == TerminalProfile::Omp)
                    .then(|| self.current_omp_title_status(pane_id))
                    .flatten();
                let status = if let Some(title) = omp_title {
                    match title {
                        PaneStatus::NeedsApproval | PaneStatus::NeedsInput => title,
                        _ => PaneStatus::Done,
                    }
                } else if profile == TerminalProfile::Omp
                    && !matches!(
                        current_status,
                        PaneStatus::NeedsApproval | PaneStatus::NeedsInput
                    )
                {
                    PaneStatus::Done
                } else if matches!(profile, TerminalProfile::Terminal | TerminalProfile::Tmux) {
                    // A plain shell's bell carries no agent status.
                    self.append_notification(
                        pane_id,
                        NotificationKind::Attention,
                        message,
                        event.at_ms,
                    );
                    return;
                } else if current_status == PaneStatus::NeedsApproval {
                    PaneStatus::NeedsInput
                } else {
                    PaneStatus::Attention
                };
                self.update_pane_status(
                    pane_id,
                    status,
                    StatusNotice::Always(message),
                    event.at_ms,
                );
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
        // A repeated status signal replaces its recent unread predecessor
        // with a new id, so it moves to the top instead of piling up. The
        // rule is shared with the desktop mirror.
        self.notifications
            .retain(|existing| !existing.replaced_by(&notification));
        if self.notifications.len() == MAX_NOTIFICATIONS {
            self.notifications.pop_front();
        }
        self.notifications.push_back(notification);
        self.notifications_dirty = true;
    }

    /// The status omp's terminal title shows right now, if the pane's title
    /// is an omp state title.
    fn current_omp_title_status(&self, pane_id: Uuid) -> Option<PaneStatus> {
        self.panes
            .get(&pane_id)?
            .terminal()?
            .session
            .terminal_title()
            .as_deref()
            .and_then(omp_title_status)
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

/// Owns the background identity-refresh thread, which also re-establishes
/// lost local tmux connections. Refreshing runtime metadata
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
                let mut heal_attempts = HashMap::new();
                while !thread_stop.load(Ordering::Acquire) {
                    thread::sleep(IDENTITY_REFRESH_INTERVAL);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    heal_local_tmux_clients(&state, &mut heal_attempts);
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

#[derive(Debug)]
struct BrowserCommandQueue {
    next_id: u64,
    pending: VecDeque<BrowserCommandRequest>,
    waiters: HashMap<u64, std::sync::mpsc::SyncSender<BrowserCommandOutcome>>,
}

impl Default for BrowserCommandQueue {
    fn default() -> Self {
        Self {
            next_id: 1,
            pending: VecDeque::new(),
            waiters: HashMap::new(),
        }
    }
}

/// A persistent registry's state files. The only constructor derives both
/// from the recovery snapshot path, so a registry that saves `sessions.json`
/// always saves `notifications.json` beside it: there is no way to build a
/// persistent registry that silently drops notifications.
#[derive(Clone, Debug)]
pub(crate) struct StateFiles {
    snapshot: SnapshotStore,
    notifications: NotificationStore,
    directory: PathBuf,
}

impl StateFiles {
    fn at(snapshot_path: PathBuf) -> Result<Self> {
        if !snapshot_path.is_absolute() {
            bail!("recovery snapshot path must be absolute");
        }
        let directory = snapshot_path
            .parent()
            .context("recovery snapshot path has no parent")?
            .to_path_buf();
        Ok(Self {
            snapshot: SnapshotStore::new(snapshot_path),
            notifications: NotificationStore::in_state_directory(&directory),
            directory,
        })
    }

    /// The service's state directory, which holds both files.
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
}

#[derive(Clone, Debug)]
pub struct SessionRegistry {
    state: Arc<RwLock<RegistryState>>,
    _identity_worker: Arc<IdentityWorker>,
    diagnostics_sampler: Arc<Mutex<DiagnosticsSampler>>,
    shutdown_requested: Arc<AtomicBool>,
    files: Option<StateFiles>,
    /// Serializes notification-file writes so an older ring never lands last.
    notification_flush: Arc<Mutex<()>>,
    tmux_scan_gate: Arc<Mutex<TmuxScanGate>>,
    remote_ls_gate: Arc<Mutex<RemoteLsGate>>,
    coding_agents: Arc<Mutex<Option<Vec<CodingAgent>>>>,
    browser_commands: Arc<Mutex<BrowserCommandQueue>>,
}

#[derive(Debug)]
pub struct PaneUpdateBatch {
    pub session_revision: u64,
    pub snapshot: Option<SessionSnapshot>,
    pub screens: Vec<TerminalScreen>,
    pub pane_states: Vec<PaneStreamState>,
    pub notifications: Vec<SessionNotification>,
    pub notifications_epoch: Uuid,
    pub diagnostics: StreamDiagnostics,
    pub browser_commands: Vec<BrowserCommandRequest>,
}
#[derive(Clone, Copy)]
pub(crate) struct PaneUpdateRequest<'a> {
    pub(crate) snapshot_revision: Option<u64>,
    pub(crate) pane_revisions: &'a [PaneRevisionCursor],
    pub(crate) subscribed_panes: &'a [Uuid],
    pub(crate) browser_executor: bool,
    pub(crate) measure_bytes: bool,
    pub(crate) notifications_after: u64,
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
    snapshot.terminal_transports.clear();
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
    let mut tmux_by_pane = HashMap::new();
    for (pane_id, runtime) in &state.panes {
        let Some(terminal) = runtime.terminal() else {
            continue;
        };
        match &terminal.kind {
            RuntimePaneKind::SystemSsh { host } => {
                offline_panes.insert(*pane_id);
                if let Some(pane) = find_pane_mut_in_snapshot(&mut snapshot, *pane_id) {
                    pane.title = offline_ssh_title(host);
                }
            }
            RuntimePaneKind::Local => {
                cwd_by_pane.insert(*pane_id, terminal.last_valid_cwd.clone());
                if let Some((window_id, tmux_pane_id)) = terminal.session.tmux_ids() {
                    tmux_by_pane.insert(*pane_id, (window_id.to_owned(), tmux_pane_id.to_owned()));
                }
            }
            RuntimePaneKind::TmuxLocal { .. } | RuntimePaneKind::TmuxSystemSsh { .. } => {}
        }
    }
    // Only a custom title survives on disk for a terminal, so the offline
    // label rides there; loading moves it back to the automatic title
    // (`clear_offline_custom_titles`) so a reconnect can replace it.
    for pane_id in pane_ids_in_snapshot(&snapshot) {
        if let Some(pane) = find_pane_mut_in_snapshot(&mut snapshot, pane_id)
            && pane.custom_title.is_none()
            && is_offline_ssh_title(&pane.title)
        {
            pane.custom_title = Some(pane.title.clone());
        }
    }
    SnapshotStore::encode_with_offline(&snapshot, &cwd_by_pane, &tmux_by_pane, &offline_panes)
}

pub(crate) fn snapshot_with_runtime_transports(state: &RegistryState) -> SessionSnapshot {
    let mut snapshot = state.snapshot.clone();
    snapshot.terminal_transports.clear();
    for (pane_id, runtime) in &state.panes {
        let Some(terminal) = runtime.terminal() else {
            continue;
        };
        let transport = match &terminal.kind {
            RuntimePaneKind::Local | RuntimePaneKind::TmuxLocal { .. } => TerminalTransport::Local,
            RuntimePaneKind::SystemSsh { host } | RuntimePaneKind::TmuxSystemSsh { host, .. } => {
                TerminalTransport::SystemSsh {
                    destination: host.clone(),
                }
            }
        };
        snapshot.terminal_transports.insert(*pane_id, transport);
    }
    snapshot
}

const OFFLINE_SUFFIX: &str = " — Offline; reconnect required";

/// `SSH <host> — Offline; reconnect required`, within the title limit.
pub(crate) fn offline_ssh_title(host: &str) -> String {
    let host_chars =
        MAX_TITLE_CHARS.saturating_sub("SSH ".chars().count() + OFFLINE_SUFFIX.chars().count());
    let host: String = host.chars().take(host_chars).collect();
    format!("SSH {host}{OFFLINE_SUFFIX}")
}

pub(crate) fn is_offline_ssh_title(title: &str) -> bool {
    title.starts_with("SSH ") && title.ends_with(OFFLINE_SUFFIX)
}

fn discover_managed_tmux(state_dir: &Path) -> (Option<TmuxServer>, Option<String>) {
    #[cfg(test)]
    {
        let _ = state_dir;
        let _ = TmuxServer::discover as fn(&Path) -> Result<Option<TmuxServer>>;
        (None, None)
    }
    #[cfg(not(test))]
    {
        match TmuxServer::discover(state_dir) {
            Ok(server) => (server, None),
            Err(error) => (None, Some(format!("{error:#}"))),
        }
    }
}

/// The managed tmux session holding the windows of workspace `workspace_id`.
pub(crate) fn tmux_session_name(workspace_id: Uuid) -> String {
    format!("hh-{workspace_id}")
}

fn ensure_tmux_client(
    server: &TmuxServer,
    workspace_id: Uuid,
    clients: &mut HashMap<Uuid, Arc<TmuxControlClient>>,
    sinks_by_workspace: &mut HashMap<Uuid, PaneSinks>,
) -> Result<Arc<TmuxControlClient>> {
    if let Some(client) = clients.get(&workspace_id)
        && client.is_alive()
    {
        return Ok(Arc::clone(client));
    }
    let sinks = Arc::clone(
        sinks_by_workspace
            .entry(workspace_id)
            .or_insert_with(|| Arc::new(Mutex::new(HashMap::new()))),
    );
    let client = TmuxControlClient::spawn(server, &tmux_session_name(workspace_id), sinks)?;
    clients.insert(workspace_id, Arc::clone(&client));
    Ok(client)
}

fn append_tmux_notification(state: &mut RegistryState, message: String) {
    let pane_id = pane_ids_in_snapshot(&state.snapshot)
        .into_iter()
        .find(|pane_id| {
            find_pane_in_snapshot(&state.snapshot, *pane_id)
                .is_some_and(|pane| matches!(pane.kind, PaneKind::Terminal))
        });
    if let Some(pane_id) = pane_id {
        state.append_notification(
            pane_id,
            NotificationKind::Message,
            Some(message),
            crate::now_ms(),
        );
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

impl SessionRegistry {
    pub fn new() -> Result<Self> {
        let tmux_socket_name = hh_protocol::state_directory().map_or_else(
            || "hh-unavailable".to_owned(),
            |directory| hh_protocol::managed_tmux_socket_name(&directory),
        );
        Self::seeded_with_tmux(None, None, None, tmux_socket_name, None)
    }

    pub fn load_default() -> Result<Self> {
        Self::persistent(default_snapshot_path()?)
    }

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

    fn from_state(state: Arc<RwLock<RegistryState>>, files: Option<StateFiles>) -> Self {
        Self {
            state: Arc::clone(&state),
            _identity_worker: Arc::new(IdentityWorker::spawn(state)),
            diagnostics_sampler: Arc::new(Mutex::new(DiagnosticsSampler::default())),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            tmux_scan_gate: Arc::new(Mutex::new(TmuxScanGate::default())),
            remote_ls_gate: Arc::new(Mutex::new(RemoteLsGate::default())),
            coding_agents: Arc::new(Mutex::new(None)),
            files,
            notification_flush: Arc::new(Mutex::new(())),
            browser_commands: Arc::new(Mutex::new(BrowserCommandQueue::default())),
        }
    }

    fn seeded_with_tmux(
        files: Option<StateFiles>,
        tmux: Option<TmuxServer>,
        tmux_unavailable_reason: Option<String>,
        tmux_socket_name: String,
        state_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let persistent = files.is_some();
        let stored_notifications = files
            .as_ref()
            .map(|files| files.notifications.load_or_quarantine())
            .unwrap_or_default();
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).context("seeded snapshot has no pane")?;
        let workspace_id = snapshot.workspaces[0].id;
        if let Some(pane) = find_pane_mut_in_snapshot(&mut snapshot, pane_id) {
            pane.shell = shell_title();
        }
        let cwd = fallback_cwd()?;
        let mut tmux_clients = HashMap::new();
        let mut tmux_sinks = HashMap::new();
        let managed = tmux.as_ref().map(|server| {
            ensure_tmux_client(server, workspace_id, &mut tmux_clients, &mut tmux_sinks).and_then(
                |client| PtySession::spawn_tmux(pane_id, workspace_id, None, &cwd, &client),
            )
        });
        let (session, tmux_failure) = match managed {
            Some(Ok(session)) => (session, None),
            Some(Err(error)) => (
                PtySession::spawn_local(pane_id, workspace_id, None, &cwd)?,
                Some(format!("{error:#}")),
            ),
            None => (
                PtySession::spawn_local(pane_id, workspace_id, None, &cwd)?,
                None,
            ),
        };
        let tmux_unavailable = tmux.is_none();
        let state = Arc::new(RwLock::new(RegistryState {
            snapshot,
            notifications: stored_notifications.items,
            next_notification_id: stored_notifications.next_id.max(1),
            notifications_dirty: false,
            notifications_epoch: Uuid::new_v4(),
            panes: HashMap::from([(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session,
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::Local,
                        recovered: false,
                        exit_status: None,
                        process_scan: ProcessScan::Unknown,
                        omp_title_status: None,
                        title_baseline_pending: false,
                    }),
                },
            )]),
            tmux,
            tmux_clients,
            tmux_sinks,
            remote_clients: HashMap::new(),
            tmux_socket_name,
            state_dir,
            next_terminal_number: 2,
            last_identity_refresh: None,
        }));
        {
            let mut state = state.write();
            if persistent && tmux_unavailable && cfg!(not(test)) {
                append_tmux_notification(
                    &mut state,
                    match tmux_unavailable_reason {
                        Some(reason) => format!(
                            "managed tmux is unavailable ({reason}); terminals will not survive a service restart"
                        ),
                        None => {
                            "tmux 3.2+ was not found; terminals will not survive a service restart"
                                .to_owned()
                        }
                    },
                );
            }
            if let Some(error) = tmux_failure {
                append_tmux_notification(
                    &mut state,
                    format!("tmux window could not be created; using a plain shell: {error}"),
                );
            }
        }
        Ok(Self::from_state(state, files))
    }
    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        Ok(snapshot_with_runtime_transports(&self.state.read()))
    }

    pub(crate) fn browser_command(
        &self,
        pane_id: Uuid,
        action: BrowserAction,
    ) -> Result<BrowserCommandOutcome> {
        self.browser_command_with_timeout(pane_id, action, BROWSER_COMMAND_TIMEOUT)
    }

    pub(crate) fn browser_command_with_timeout(
        &self,
        pane_id: Uuid,
        action: BrowserAction,
        timeout: Duration,
    ) -> Result<BrowserCommandOutcome> {
        {
            let state = self.state.read();
            let pane = find_pane_in_snapshot(&state.snapshot, pane_id)
                .with_context(|| format!("pane {pane_id} does not exist"))?;
            if !pane.kind.is_browser() {
                bail!("pane {pane_id} is not a browser");
            }
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let request_id = {
            let mut queue = self.browser_commands.lock();
            let request_id = queue.next_id;
            queue.next_id = queue.next_id.checked_add(1).unwrap_or(1);
            queue.waiters.insert(request_id, sender);
            queue.pending.push_back(BrowserCommandRequest {
                request_id,
                pane_id,
                action,
            });
            request_id
        };
        match receiver.recv_timeout(timeout) {
            Ok(outcome) => Ok(outcome),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let mut queue = self.browser_commands.lock();
                queue.waiters.remove(&request_id);
                queue
                    .pending
                    .retain(|request| request.request_id != request_id);
                Ok(BrowserCommandOutcome::Error {
                    message:
                        "the desktop did not answer within 30 seconds; is Harness Harlot open?"
                            .to_owned(),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let mut queue = self.browser_commands.lock();
                queue.waiters.remove(&request_id);
                queue
                    .pending
                    .retain(|request| request.request_id != request_id);
                Ok(BrowserCommandOutcome::Error {
                    message: "the desktop browser command channel disconnected".to_owned(),
                })
            }
        }
    }

    pub(crate) fn take_browser_commands(&self) -> Vec<BrowserCommandRequest> {
        self.browser_commands.lock().pending.drain(..).collect()
    }

    pub(crate) fn resolve_browser_command(&self, request_id: u64, outcome: BrowserCommandOutcome) {
        if let Some(waiter) = self.browser_commands.lock().waiters.remove(&request_id) {
            let _ = waiter.send(outcome);
        }
    }

    /// Cached scan for installed coding agent CLIs; `refresh` rescans.
    pub(crate) fn coding_agents(&self, refresh: bool) -> Vec<CodingAgent> {
        if !refresh && let Some(cached) = self.coding_agents.lock().clone() {
            return cached;
        }
        let agents = discover_coding_agents();
        *self.coding_agents.lock() = Some(agents.clone());
        agents
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

    /// Copies each tmux-backed terminal's input modes into its tmux pane when
    /// they changed, so a restarted service can restore them on reattach.
    pub fn save_terminal_input_modes(&self) {
        let sessions = self
            .state
            .read()
            .panes
            .values()
            .filter_map(|runtime| {
                runtime
                    .terminal()
                    .map(|terminal| Arc::clone(&terminal.session))
            })
            .collect::<Vec<_>>();
        for session in sessions {
            if let Err(error) = session.save_input_modes() {
                eprintln!("failed to save terminal input modes: {error:#}");
            }
        }
    }

    /// Saves the recovery snapshot and the notification ring. The service
    /// calls it every 2 s, so queued bells and OSC notifications are recorded
    /// even while no desktop is polling.
    pub fn persist(&self) -> Result<()> {
        refresh_process_metadata(&self.state, true);
        let bytes = {
            let mut state = self.state.write();
            state.drain_pane_events();
            refresh_runtime_metadata(&mut state);
            encode_desired_state(&state)?
        };
        self.write_snapshot(&bytes)?;
        self.flush_notifications()
    }

    /// Writes the notification ring to disk if it changed since the last
    /// write. In-memory registries keep it in memory only.
    pub(crate) fn flush_notifications(&self) -> Result<()> {
        let Some(files) = &self.files else {
            return Ok(());
        };
        let _flush = self.notification_flush.lock();
        let (items, next_id) = {
            let mut state = self.state.write();
            if !state.notifications_dirty {
                return Ok(());
            }
            state.notifications_dirty = false;
            (state.notifications.clone(), state.next_notification_id)
        };
        let written = files.notifications.write(&items, next_id);
        if written.is_err() {
            self.state.write().notifications_dirty = true;
        }
        written
    }

    pub(crate) fn notifications_epoch(&self) -> Uuid {
        self.state.read().notifications_epoch
    }

    /// Handles `ClientRequest::MarkPaneSeen`. A cleared dot is saved right
    /// away, like any other user action, so a crash cannot bring it back.
    pub(crate) fn mark_pane_seen(&self, pane_id: Uuid) -> Result<()> {
        let bytes = {
            let mut state = self.state.write();
            state.drain_pane_events();
            state
                .mark_pane_seen(pane_id)?
                .then(|| encode_desired_state(&state))
                .transpose()?
        };
        if let Some(bytes) = bytes {
            self.write_snapshot(&bytes)?;
        }
        self.flush_notifications()
    }

    /// Handles `ClientRequest::ReportPaneProgress`.
    pub(crate) fn report_pane_progress(
        &self,
        pane_id: Uuid,
        progress: Option<PaneProgress>,
    ) -> Result<()> {
        self.state.write().set_pane_progress(pane_id, progress)
    }

    pub(crate) fn write_snapshot(&self, bytes: &[u8]) -> Result<()> {
        self.files
            .as_ref()
            .map_or(Ok(()), |files| files.snapshot.write_snapshot(bytes))
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

    pub(crate) fn client_for_workspace(
        &self,
        workspace_id: Uuid,
    ) -> Result<Arc<TmuxControlClient>> {
        let mut state = self.state.write();
        let server = state
            .tmux
            .clone()
            .context("managed tmux server is unavailable")?;
        let RegistryState {
            tmux_clients,
            tmux_sinks,
            ..
        } = &mut *state;
        ensure_tmux_client(&server, workspace_id, tmux_clients, tmux_sinks)
    }
    pub(crate) fn spawn_local_transport(
        &self,
        pane_id: Uuid,
        workspace_id: Uuid,
        bot_id: Option<Uuid>,
        cwd: &Path,
    ) -> Result<Arc<PtySession>> {
        if self.state.read().tmux.is_some() {
            match self.client_for_workspace(workspace_id).and_then(|client| {
                PtySession::spawn_tmux(pane_id, workspace_id, bot_id, cwd, &client)
            }) {
                Ok(session) => return Ok(session),
                Err(error) => {
                    let session = PtySession::spawn_local(pane_id, workspace_id, bot_id, cwd)?;
                    append_tmux_notification(
                        &mut self.state.write(),
                        format!("tmux window could not be created; using a plain shell: {error:#}"),
                    );
                    return Ok(session);
                }
            }
        }
        PtySession::spawn_local(pane_id, workspace_id, bot_id, cwd)
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
                self.spawn_local_transport(pane_id, workspace_id, None, cwd)?
            }
            RuntimePaneKind::SystemSsh { host } => {
                self.spawn_ssh_transport(pane_id, workspace_id, host, remote_dir)?
            }
            RuntimePaneKind::TmuxLocal { .. } | RuntimePaneKind::TmuxSystemSsh { .. } => {
                unreachable!("workspace connection cannot resolve to a runtime-only tmux pane")
            }
        };
        Ok((session, kind))
    }
}

/// Deletes `<state>/history`, the retired terminal history archive of raw PTY
/// output, removing a symlink itself rather than its target. Failures are
/// logged and never block startup.
fn remove_retired_history_archive(state_directory: &Path) {
    let archive = state_directory.join("history");
    let removal = match std::fs::symlink_metadata(&archive) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&archive),
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::remove_file(&archive),
        Ok(_) => return,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => Err(error),
    };
    if let Err(error) = removal {
        eprintln!(
            "failed to remove retired terminal history archive {}: {error}",
            archive.display()
        );
    }
}

/// Turns the first workstation of a test registry into an SSH workstation.
/// The home workstation must stay local, so an empty local home is appended.
#[cfg(test)]
pub(crate) fn make_first_workstation_remote(
    state: &mut RegistryState,
    destination: &str,
    status: hh_protocol::WorkspaceConnectionStatus,
) {
    let workspace = &mut state.snapshot.workspaces[0];
    workspace.connection = WorkspaceConnection::SystemSsh {
        destination: destination.to_owned(),
        status,
    };
    if std::mem::take(&mut workspace.home) {
        let mut home = workspace.clone();
        home.id = Uuid::new_v4();
        home.connection = WorkspaceConnection::Local;
        home.home = true;
        home.tabs.clear();
        home.active_terminal_count = 0;
        home.order = home.order.saturating_add(1);
        state.snapshot.workspaces.push(home);
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
#[path = "registry_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "status_tests.rs"]
mod status_tests;
