#![allow(clippy::missing_errors_doc)]

mod history;
mod persistence;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hh_protocol::{
    AppearanceColor, ClientRequest, DropPlacement, HistoryArchiveStatus, HistoryClearScope,
    HistoryCursor, HistoryPageDirection, HistorySettings, MAX_FRAME_SIZE, MAX_PANES,
    MAX_TERMINAL_CELLS, MAX_TERMINAL_COLUMNS, MAX_TERMINAL_ROWS, MIN_TERMINAL_COLUMNS,
    MIN_TERMINAL_ROWS, NotificationKind, PROTOCOL_VERSION, Pane, PaneKind, PaneLayout,
    PaneRevisionCursor, PaneStreamState, ServiceResponse, SessionNotification, SessionSnapshot,
    SplitAxis, StreamDiagnostics, Tab, TerminalHistoryPage, TerminalIdentity,
    TerminalIdentitySource, TerminalModes, TerminalModifiers, TerminalMouseAction,
    TerminalMouseButton, TerminalPoint, TerminalProfile, TerminalScreen, TerminalSelectionKind,
    TmuxScanScope, TmuxSession, TmuxSessionAttachIssue, TmuxSessionId, Workspace,
    WorkspaceConnection, WorkspaceConnectionStatus, WorkspacePinMove, normalize_browser_url,
    normalize_browser_url_or_default, terminal_profile_for_command,
    terminal_profile_for_executable, terminal_profile_for_title, validate_ssh_host,
    validate_workspace_dir,
};
use hh_terminal_model::TerminalModel;
use parking_lot::{Mutex, RwLock};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use uuid::Uuid;

use crate::history::{HistoryArchive, HistorySink};
use crate::persistence::{SnapshotStore, default_snapshot_path};

const INITIAL_COLUMNS: u16 = 100;
const INITIAL_ROWS: u16 = 30;
const MAX_INPUT_FRAME: usize = 64 * 1024;
const MAX_TABS_PER_WORKSPACE: usize = 32;
const MAX_WORKSPACES: usize = 16;
const MAX_WORKSPACE_TITLE_CHARS: usize = 80;
const MAX_RECENT_COLORS: usize = 8;
const DIAGNOSTICS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const IDENTITY_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_DISCOVERY_PROCESSES: usize = 4_096;
const MAX_DISCOVERY_DESCENDANTS_PER_PANE: usize = 64;
const MAX_DISCOVERY_DEPTH: usize = 4;
const TMUX_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const TMUX_PROBE_MAX_BYTES: usize = 64 * 1024;
const TMUX_PROBE_MAX_SESSIONS: usize = 64;
const TMUX_SCAN_MIN_INTERVAL: Duration = Duration::from_secs(2);
const REMOTE_LS_MIN_INTERVAL: Duration = Duration::from_millis(100);
const REMOTE_LS_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_REMOTE_DIRECTORY_ENTRIES: usize = 200;
const MAX_TMUX_ATTACH_SESSIONS: usize = 32;
const MAX_RAW_PANE_EVENTS: usize = 32;
const MAX_NOTIFICATIONS: usize = 200;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_mins(5);
const TMUX_ATTACH_STARTUP_GRACE: Duration = Duration::from_millis(75);
const TMUX_SESSION_LIST_FORMAT: &str =
    "S\t#{session_id}\t#{session_name}\t#{session_windows}\t#{session_attached}";
const TMUX_REMOTE_LIST_COMMAND: &str = "exec tmux list-sessions -F 'S\t#{session_id}\t#{session_name}\t#{session_windows}\t#{session_attached}'";
#[cfg(debug_assertions)]
const LOCAL_SSH_TEST_SEAM_ENV: &str = "HH_TEST_LOCAL_SSH_SEAM";

#[cfg(test)]
static TEST_LOCAL_SSH_SEAM_ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct RawPaneEvent {
    kind: NotificationKind,
    message: Option<String>,
    at_ms: u64,
}

struct PtySession {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
    terminal: Arc<Mutex<TerminalModel>>,
    revision: Arc<AtomicU64>,
    events: Arc<Mutex<VecDeque<RawPaneEvent>>>,
    _history: Arc<HistorySink>,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtySession")
            .field("revision", &self.revision.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let child = self.child.get_mut();
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        if let Some(reader) = self.reader.get_mut().take() {
            let _ = reader.join();
        }
    }
}

fn validate_terminal_dimensions(columns: u16, rows: u16) -> Result<()> {
    if !(MIN_TERMINAL_COLUMNS..=MAX_TERMINAL_COLUMNS).contains(&columns) {
        bail!("terminal columns must be between {MIN_TERMINAL_COLUMNS} and {MAX_TERMINAL_COLUMNS}");
    }
    if !(MIN_TERMINAL_ROWS..=MAX_TERMINAL_ROWS).contains(&rows) {
        bail!("terminal rows must be between {MIN_TERMINAL_ROWS} and {MAX_TERMINAL_ROWS}");
    }
    let cells = u32::from(columns) * u32::from(rows);
    if cells > MAX_TERMINAL_CELLS {
        bail!("terminal dimensions exceed the {MAX_TERMINAL_CELLS}-cell limit");
    }
    Ok(())
}
impl PtySession {
    fn spawn_local(
        pane_id: Uuid,
        workspace_id: Uuid,
        cwd: &Path,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        let shell = configured_shell();
        Self::spawn_command(
            pane_id,
            workspace_id,
            local_shell_command(pane_id, cwd),
            &format!("configured shell {shell}"),
            archive,
        )
    }

    fn spawn_ssh(
        pane_id: Uuid,
        workspace_id: Uuid,
        host: &str,
        remote_dir: Option<&str>,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        #[cfg(test)]
        if TEST_LOCAL_SSH_SEAM_ENABLED.load(Ordering::Relaxed) {
            return Self::spawn_local(
                pane_id,
                workspace_id,
                &local_spawn_dir(remote_dir)?,
                archive,
            );
        }
        if std::env::var_os(LOCAL_SSH_TEST_SEAM_ENV).is_some() {
            return Self::spawn_local(
                pane_id,
                workspace_id,
                &local_spawn_dir(remote_dir)?,
                archive,
            );
        }
        Self::spawn_command(
            pane_id,
            workspace_id,
            system_ssh_command(pane_id, host, remote_dir)?,
            "system OpenSSH",
            archive,
        )
    }

    fn spawn_tmux_local(
        pane_id: Uuid,
        workspace_id: Uuid,
        session_id: &TmuxSessionId,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            workspace_id,
            tmux_local_attach_command(pane_id, session_id),
            "tmux session attach",
            archive,
        )
    }

    fn spawn_tmux_ssh(
        pane_id: Uuid,
        workspace_id: Uuid,
        host: &str,
        session_id: &TmuxSessionId,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            workspace_id,
            tmux_ssh_attach_command(pane_id, host, session_id)?,
            "system OpenSSH tmux session attach",
            archive,
        )
    }

    fn spawn_command(
        pane_id: Uuid,
        workspace_id: Uuid,
        command: CommandBuilder,
        description: &str,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        // Session registration may wait behind prior disk work, so do it
        // before a child exists. Once the PTY is live, its reader only uses
        // the archive's bounded non-blocking append path.
        let history = Arc::new(archive.start_session(pane_id, workspace_id));
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: INITIAL_ROWS,
                cols: INITIAL_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open native PTY")?;

        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawn {description}"))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("take PTY writer")?;
        let terminal = Arc::new(Mutex::new(TerminalModel::new(
            usize::from(INITIAL_COLUMNS),
            usize::from(INITIAL_ROWS),
        )));
        let revision = Arc::new(AtomicU64::new(0));
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let reader_terminal = Arc::clone(&terminal);
        let reader_revision = Arc::clone(&revision);
        let reader_events = Arc::clone(&events);
        let reader_history = Arc::clone(&history);
        let reader = thread::Builder::new()
            .name(format!("rmux-pty-{pane_id}"))
            .spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                let mut previous_bell_count = 0;
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut terminal = reader_terminal.lock();
                            terminal.process_output(&buffer[..read]);
                            let bell_count = terminal.bell_count();
                            let messages = terminal.take_notification_messages();
                            drop(terminal);
                            if (bell_count > previous_bell_count || !messages.is_empty())
                                && let Some(mut events) = reader_events.try_lock()
                            {
                                if bell_count > previous_bell_count {
                                    push_raw_pane_event(
                                        &mut events,
                                        RawPaneEvent {
                                            kind: NotificationKind::Attention,
                                            message: None,
                                            at_ms: history::now_ms(),
                                        },
                                    );
                                }
                                for message in messages {
                                    push_raw_pane_event(
                                        &mut events,
                                        RawPaneEvent {
                                            kind: NotificationKind::Message,
                                            message: Some(message),
                                            at_ms: history::now_ms(),
                                        },
                                    );
                                }
                            }
                            previous_bell_count = bell_count;
                            reader_revision.fetch_add(1, Ordering::Release);
                            reader_history.record(&buffer[..read]);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
            .context("spawn PTY reader thread")?;

        Ok(Arc::new(Self {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            reader: Mutex::new(Some(reader)),
            terminal,
            revision,
            _history: history,
            events,
        }))
    }

    fn write_input(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_INPUT_FRAME {
            bail!("terminal input exceeds {MAX_INPUT_FRAME} bytes");
        }
        let mut writer = self.writer.lock();
        writer.write_all(bytes).context("write terminal input")?;
        writer.flush().context("flush terminal input")?;
        Ok(())
    }
    fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        validate_terminal_dimensions(columns, rows)?;
        self.master
            .lock()
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize PTY")?;
        self.terminal
            .lock()
            .resize(usize::from(columns), usize::from(rows));
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn screen(&self, pane_id: Uuid) -> Result<TerminalScreen> {
        let terminal = self.terminal.lock();
        let (columns, rows) = terminal.dimensions();
        let mut mode_bits = 0;
        for (enabled, mode) in [
            (terminal.bracketed_paste(), TerminalModes::BRACKETED_PASTE),
            (terminal.mouse_reporting(), TerminalModes::MOUSE_REPORTING),
            (terminal.mouse_motion(), TerminalModes::MOUSE_MOTION),
            (terminal.sgr_mouse(), TerminalModes::SGR_MOUSE),
        ] {
            if enabled {
                mode_bits |= mode;
            }
        }
        Ok(TerminalScreen {
            pane_id,
            revision: self.revision.load(Ordering::Acquire),
            columns: u16::try_from(columns).context("terminal columns exceed protocol range")?,
            rows: u16::try_from(rows).context("terminal rows exceed protocol range")?,
            lines: terminal.styled_lines(),
            cursor: terminal.cursor(),
            selection: terminal.selection(),
            display_offset: u32::try_from(terminal.display_offset())
                .context("terminal display offset exceeds protocol range")?,
            history_size: u32::try_from(terminal.history_size())
                .context("terminal history exceeds protocol range")?,
            modes: TerminalModes::new(mode_bits),
        })
    }

    fn begin_selection(&self, point: TerminalPoint, kind: TerminalSelectionKind) {
        self.terminal.lock().begin_selection(point, kind);
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn update_selection(&self, point: TerminalPoint) {
        self.terminal.lock().update_selection(point);
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn clear_selection(&self) {
        self.terminal.lock().clear_selection();
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn selected_text(&self) -> Option<String> {
        self.terminal.lock().selected_text()
    }

    fn scroll(&self, lines: i32) {
        self.terminal.lock().scroll(lines.clamp(-10_000, 10_000));
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn search_literal(&self, query: &str, forward: bool) -> Result<bool> {
        if query.chars().count() > 256 || query.chars().any(char::is_control) {
            bail!("terminal search must be at most 256 visible characters");
        }
        let found = self.terminal.lock().search_literal(query, forward);
        if found {
            self.revision.fetch_add(1, Ordering::Release);
        }
        Ok(found)
    }

    fn mouse_input(
        &self,
        point: TerminalPoint,
        button: TerminalMouseButton,
        action: TerminalMouseAction,
        modifiers: TerminalModifiers,
    ) -> Result<()> {
        let report = self
            .terminal
            .lock()
            .mouse_report(point, button, action, modifiers);
        if let Some(report) = report {
            self.write_input(&report)?;
        }
        Ok(())
    }

    fn terminate_and_wait(&self) -> Result<()> {
        let mut child = self.child.lock();
        if child
            .try_wait()
            .context("observe PTY child before close")?
            .is_none()
        {
            child.kill().context("terminate PTY child")?;
        }
        child.wait().context("observe PTY child exit after close")?;
        Ok(())
    }

    fn exit_status(&self) -> Result<Option<String>> {
        self.child
            .lock()
            .try_wait()
            .map(|status| status.map(|status| status.to_string()))
            .context("observe PTY child exit")
    }

    /// A successful `spawn` only means the executable started. tmux reports a
    /// missing/dead target by exiting immediately, so do not register a tab
    /// until it survived a short bounded startup window.
    fn confirm_live_for_tmux_attach(&self) -> Result<()> {
        let deadline = Instant::now() + TMUX_ATTACH_STARTUP_GRACE;
        loop {
            if let Some(status) = self.exit_status()? {
                bail!("tmux attach exited before the terminal became live ({status})");
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn process_id(&self) -> Option<u32> {
        self.child.lock().process_id()
    }

    fn terminal_title(&self) -> Option<String> {
        self.terminal.lock().terminal_title()
    }
}
fn push_raw_pane_event(events: &mut VecDeque<RawPaneEvent>, event: RawPaneEvent) {
    if events.len() == MAX_RAW_PANE_EVENTS {
        events.pop_front();
    }
    events.push_back(event);
}

#[derive(Debug)]
struct RuntimePane {
    backend: RuntimePaneBackend,
}

#[derive(Debug)]
enum RuntimePaneBackend {
    Terminal(TerminalRuntimePane),
    Browser,
}

#[derive(Debug)]
struct TerminalRuntimePane {
    session: Arc<PtySession>,
    last_valid_cwd: PathBuf,
    kind: RuntimePaneKind,
    recovered: bool,
    exit_status: Option<String>,
    detected_command_profile: Option<TerminalProfile>,
}

impl RuntimePane {
    fn terminal(&self) -> Option<&TerminalRuntimePane> {
        match &self.backend {
            RuntimePaneBackend::Terminal(terminal) => Some(terminal),
            RuntimePaneBackend::Browser => None,
        }
    }

    fn terminal_mut(&mut self) -> Option<&mut TerminalRuntimePane> {
        match &mut self.backend {
            RuntimePaneBackend::Terminal(terminal) => Some(terminal),
            RuntimePaneBackend::Browser => None,
        }
    }
}

#[derive(Clone, Copy)]
struct SshWorkspaceIds {
    workspace: Uuid,
    tab: Uuid,
    pane: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RuntimePaneKind {
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
    fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    fn is_runtime_only(&self) -> bool {
        matches!(self, Self::TmuxLocal { .. } | Self::TmuxSystemSsh { .. })
    }

    /// Runs over the workstation's SSH transport, so its liveness reflects
    /// whether that workstation is still reachable.
    fn is_remote(&self) -> bool {
        matches!(self, Self::SystemSsh { .. } | Self::TmuxSystemSsh { .. })
    }

    fn tmux_session_id(&self) -> Option<&TmuxSessionId> {
        match self {
            Self::TmuxLocal { session_id } | Self::TmuxSystemSsh { session_id, .. } => {
                Some(session_id)
            }
            Self::Local | Self::SystemSsh { .. } => None,
        }
    }

    fn shell_label(&self) -> String {
        match self {
            Self::Local => shell_title(),
            Self::SystemSsh { host } => format!("ssh {host}"),
            Self::TmuxLocal { .. } | Self::TmuxSystemSsh { .. } => "tmux".to_owned(),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct TmuxAttachmentPlan {
    launch: Vec<TmuxSession>,
    skipped: Vec<TmuxSessionAttachIssue>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TmuxAttachmentResult {
    pub pane_ids: Vec<Uuid>,
    pub skipped: Vec<TmuxSessionAttachIssue>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TmuxScanResult {
    pub scope: TmuxScanScope,
    pub sessions: Vec<TmuxSession>,
    pub open_session_ids: Vec<TmuxSessionId>,
    pub no_server: bool,
}

fn runtime_kind_for_workspace(connection: &WorkspaceConnection) -> RuntimePaneKind {
    match connection {
        WorkspaceConnection::Local => RuntimePaneKind::Local,
        WorkspaceConnection::SystemSsh { destination, .. } => RuntimePaneKind::SystemSsh {
            host: destination.clone(),
        },
    }
}

#[derive(Debug)]
struct RegistryState {
    snapshot: SessionSnapshot,
    panes: HashMap<Uuid, RuntimePane>,
    notifications: VecDeque<SessionNotification>,
    next_notification_id: u64,
    next_terminal_number: u32,
    next_group_number: u32,
    system: System,
    last_identity_refresh: Option<Instant>,
}

impl RegistryState {
    fn new_pane(&mut self, id: Uuid) -> Pane {
        let title = format!("Terminal {}", self.next_terminal_number);
        self.next_terminal_number += 1;
        Pane {
            id,
            kind: PaneKind::Terminal,
            title,
            shell: shell_title(),
            color: None,
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        }
    }

    fn terminal_pane(&self, pane_id: Uuid) -> Result<&TerminalRuntimePane> {
        self.panes
            .get(&pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?
            .terminal()
            .with_context(|| format!("pane {pane_id} is a browser, not a terminal"))
    }

    fn terminal_pane_mut(&mut self, pane_id: Uuid) -> Result<&mut TerminalRuntimePane> {
        self.panes
            .get_mut(&pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?
            .terminal_mut()
            .with_context(|| format!("pane {pane_id} is a browser, not a terminal"))
    }

    fn require_terminal_layout_pane(&self, pane_id: Uuid) -> Result<()> {
        match self.panes.get(&pane_id) {
            Some(RuntimePane {
                backend: RuntimePaneBackend::Terminal(_),
            }) => Ok(()),
            Some(RuntimePane {
                backend: RuntimePaneBackend::Browser,
            }) => bail!("browser tabs cannot create terminal panes"),
            None => bail!("pane {pane_id} does not exist"),
        }
    }
}

impl RegistryState {
    fn drain_pane_events(&mut self) {
        let pending = self
            .panes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                let terminal = runtime.terminal()?;
                let mut events = terminal.session.events.try_lock()?;
                (!events.is_empty()).then(|| (*pane_id, events.drain(..).collect::<Vec<_>>()))
            })
            .collect::<Vec<_>>();
        for (pane_id, events) in pending {
            for event in events {
                self.append_notification(pane_id, event.kind, event.message, event.at_ms);
            }
        }
    }

    fn append_notification(
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

struct InitialTerminalSpawn {
    session: Arc<PtySession>,
    kind: RuntimePaneKind,
    pane_title: String,
    pane_shell: String,
    tab_title: String,
}

#[derive(Clone, Debug)]
pub struct SessionRegistry {
    state: Arc<RwLock<RegistryState>>,
    diagnostics_sampler: Arc<Mutex<DiagnosticsSampler>>,
    store: Option<SnapshotStore>,
    history: HistoryArchive,
    tmux_scan_gate: Arc<Mutex<TmuxScanGate>>,
    remote_ls_gate: Arc<Mutex<RemoteLsGate>>,
}

#[derive(Debug, Default)]
struct TmuxScanGate {
    active: HashSet<Uuid>,
    last_completed: HashMap<Uuid, Instant>,
}

#[derive(Debug)]
struct TmuxScanPermit {
    gate: Arc<Mutex<TmuxScanGate>>,
    workspace_id: Uuid,
}

impl Drop for TmuxScanPermit {
    fn drop(&mut self) {
        let mut gate = self.gate.lock();
        gate.active.remove(&self.workspace_id);
        gate.last_completed
            .insert(self.workspace_id, Instant::now());
    }
}
#[derive(Debug, Default)]
struct RemoteLsGate {
    active: HashSet<Uuid>,
    last_completed: HashMap<Uuid, Instant>,
}

#[derive(Debug)]
struct RemoteLsPermit {
    gate: Arc<Mutex<RemoteLsGate>>,
    workspace_id: Uuid,
}

impl Drop for RemoteLsPermit {
    fn drop(&mut self) {
        let mut gate = self.gate.lock();
        gate.active.remove(&self.workspace_id);
        gate.last_completed
            .insert(self.workspace_id, Instant::now());
    }
}

#[derive(Debug)]
struct DiagnosticsSampler {
    system: System,
    last_refresh: Option<Instant>,
    cpu_milli_percent: u32,
    memory_bytes: u64,
}

impl Default for DiagnosticsSampler {
    fn default() -> Self {
        Self {
            system: System::new(),
            last_refresh: None,
            cpu_milli_percent: 0,
            memory_bytes: 0,
        }
    }
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
        let registry = Self {
            state: Arc::new(RwLock::new(RegistryState {
                snapshot: recovered.snapshot,
                panes,
                notifications: VecDeque::new(),
                next_notification_id: 1,
                next_terminal_number,
                next_group_number: 1,
                last_identity_refresh: None,
                system: System::new(),
            })),
            diagnostics_sampler: Arc::new(Mutex::new(DiagnosticsSampler::default())),
            tmux_scan_gate: Arc::new(Mutex::new(TmuxScanGate::default())),
            remote_ls_gate: Arc::new(Mutex::new(RemoteLsGate::default())),
            store: Some(store),
            history,
        };
        registry.persist()?;
        Ok(registry)
    }

    fn seeded(store: Option<SnapshotStore>, history: HistoryArchive) -> Result<Self> {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).context("seeded snapshot has no pane")?;
        let workspace_id = snapshot.workspaces[0].id;
        if let Some(pane) = find_pane_mut_in_snapshot(&mut snapshot, pane_id) {
            pane.shell = shell_title();
        }
        let cwd = fallback_cwd()?;
        let session = PtySession::spawn_local(pane_id, workspace_id, &cwd, &history)?;
        Ok(Self {
            state: Arc::new(RwLock::new(RegistryState {
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
                        }),
                    },
                )]),
                next_terminal_number: 2,
                next_group_number: 1,
                last_identity_refresh: None,
                system: System::new(),
            })),
            diagnostics_sampler: Arc::new(Mutex::new(DiagnosticsSampler::default())),
            tmux_scan_gate: Arc::new(Mutex::new(TmuxScanGate::default())),
            store,
            remote_ls_gate: Arc::new(Mutex::new(RemoteLsGate::default())),
            history,
        })
    }

    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        let mut state = self.state.write();
        refresh_runtime_metadata(&mut state, false)?;
        Ok(state.snapshot.clone())
    }

    pub fn state(&self) -> Result<(SessionSnapshot, Vec<TerminalScreen>)> {
        let mut state = self.state.write();
        refresh_runtime_metadata(&mut state, false)?;
        let snapshot = state.snapshot.clone();
        let screens = state
            .panes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                runtime
                    .terminal()
                    .map(|terminal| terminal.session.screen(*pane_id))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((snapshot, screens))
    }

    fn refresh_pending_pane_events(&self) -> Result<()> {
        let (has_pending_events, has_finished_reader) = {
            let state = self.state.read();
            let has_pending_events = state.panes.values().any(|runtime| {
                runtime.terminal().is_some_and(|terminal| {
                    terminal
                        .session
                        .events
                        .try_lock()
                        .is_some_and(|events| !events.is_empty())
                })
            });
            let has_finished_reader = state.panes.values().any(|runtime| {
                runtime.terminal().is_some_and(|terminal| {
                    terminal.exit_status.is_none()
                        && terminal
                            .session
                            .reader
                            .lock()
                            .as_ref()
                            .is_some_and(thread::JoinHandle::is_finished)
                })
            });
            (has_pending_events, has_finished_reader)
        };
        if has_pending_events || has_finished_reader {
            let mut state = self.state.write();
            if has_finished_reader {
                refresh_runtime_metadata(&mut state, false)?;
            }
            state.drain_pane_events();
        }
        Ok(())
    }

    fn stream_diagnostics(
        &self,
        started: Instant,
        measure_bytes: bool,
        snapshot: Option<&SessionSnapshot>,
        screens: &[TerminalScreen],
        pane_states: &[PaneStreamState],
        coalesced_revisions: u64,
    ) -> Result<StreamDiagnostics> {
        let snapshot_bytes = if measure_bytes {
            snapshot.map(serialized_len).transpose()?.unwrap_or(0)
        } else {
            0
        };
        let screen_bytes = if measure_bytes {
            screens.iter().try_fold(0_u64, |total, screen| {
                Ok::<_, anyhow::Error>(total.saturating_add(serialized_len(screen)?))
            })?
        } else {
            0
        };
        let (service_cpu_milli_percent, service_memory_bytes) = self.service_metrics();
        Ok(StreamDiagnostics {
            panes_considered: u32::try_from(pane_states.len()).unwrap_or(u32::MAX),
            panes_subscribed: u32::try_from(
                pane_states.iter().filter(|pane| pane.subscribed).count(),
            )
            .unwrap_or(u32::MAX),
            screens_queued: u32::try_from(screens.len()).unwrap_or(u32::MAX),
            screens_delivered: u32::try_from(screens.len()).unwrap_or(u32::MAX),
            coalesced_revisions,
            snapshot_bytes,
            screen_bytes,
            preparation_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            desktop_apply_micros: 0,
            service_cpu_milli_percent,
            service_memory_bytes,
        })
    }

    /// Builds one coalesced receiver update without serializing unchanged or
    /// unsubscribed terminal screens. PTY reader threads continue advancing
    /// terminal models independently of this method.
    ///
    /// `measure_bytes` opts into the `snapshot_bytes`/`screen_bytes`
    /// diagnostics, each of which costs a full extra serialization of the
    /// payload. The socket path leaves it off; tests that assert on payload
    /// size turn it on.
    pub fn pane_updates(
        &self,
        snapshot_revision: Option<u64>,
        pane_revisions: &[PaneRevisionCursor],
        subscribed_panes: &[Uuid],
        measure_bytes: bool,
        notifications_after: u64,
    ) -> Result<PaneUpdateBatch> {
        if pane_revisions.len() > MAX_PANES || subscribed_panes.len() > MAX_PANES {
            bail!("pane update request exceeds the {MAX_PANES}-pane limit");
        }
        let started = Instant::now();
        let known_revisions = pane_revisions
            .iter()
            .map(|cursor| (cursor.pane_id, cursor.revision))
            .collect::<HashMap<_, _>>();
        let subscribed = subscribed_panes.iter().copied().collect::<HashSet<_>>();
        self.refresh_pending_pane_events()?;
        let state = self.state.read();

        let session_revision = state.snapshot.revision;
        let snapshot =
            (snapshot_revision != Some(session_revision)).then(|| state.snapshot.clone());
        let mut screens = Vec::new();
        let mut pane_states = Vec::with_capacity(state.panes.len());
        let mut coalesced_revisions = 0_u64;
        for (pane_id, runtime) in &state.panes {
            let Some(runtime) = runtime.terminal() else {
                pane_states.push(PaneStreamState {
                    pane_id: *pane_id,
                    revision: 0,
                    subscribed: false,
                    dirty: false,
                    exited: false,
                });
                continue;
            };
            let subscribed = subscribed.contains(pane_id);
            let known_revision = known_revisions.get(pane_id).copied();
            let observed_revision = runtime.session.revision.load(Ordering::Acquire);
            let changed = known_revision != Some(observed_revision);
            let delivered = subscribed && changed;
            let revision = if delivered {
                let screen = runtime.session.screen(*pane_id)?;
                let revision = screen.revision;
                if let Some(known) = known_revision {
                    coalesced_revisions = coalesced_revisions
                        .saturating_add(revision.saturating_sub(known).saturating_sub(1));
                }
                screens.push(screen);
                revision
            } else {
                observed_revision
            };
            pane_states.push(PaneStreamState {
                pane_id: *pane_id,
                revision,
                subscribed,
                dirty: !delivered && known_revision != Some(revision),
                exited: runtime.exit_status.is_some(),
            });
        }
        let notifications = state
            .notifications
            .iter()
            .filter(|notification| notification.id > notifications_after)
            .cloned()
            .collect();
        drop(state);

        pane_states.sort_unstable_by_key(|pane| pane.pane_id);
        screens.sort_unstable_by_key(|screen| screen.pane_id);
        let diagnostics = self.stream_diagnostics(
            started,
            measure_bytes,
            snapshot.as_ref(),
            &screens,
            &pane_states,
            coalesced_revisions,
        )?;
        Ok(PaneUpdateBatch {
            session_revision,
            snapshot,
            screens,
            pane_states,
            notifications,
            diagnostics,
        })
    }

    pub fn notifications(&self) -> Result<Vec<SessionNotification>> {
        let mut state = self.state.write();
        refresh_runtime_metadata(&mut state, false)?;
        state.drain_pane_events();
        Ok(state.notifications.iter().cloned().collect())
    }

    pub fn mark_notifications_read(&self, ids: &[u64]) {
        let ids = ids.iter().copied().collect::<HashSet<_>>();
        let mut state = self.state.write();
        for notification in &mut state.notifications {
            if ids.contains(&notification.id) {
                notification.read = true;
            }
        }
    }

    pub fn clear_notifications(&self) {
        let mut state = self.state.write();
        state.drain_pane_events();
        state.notifications.clear();
    }

    /// Returns one current screen for deterministic focus/reconnect resync.
    pub fn pane_snapshot(&self, pane_id: Uuid) -> Result<(TerminalScreen, StreamDiagnostics)> {
        let started = Instant::now();
        let screen = self.pane(pane_id)?.screen(pane_id)?;
        let screen_bytes = serialized_len(&screen)?;
        let (service_cpu_milli_percent, service_memory_bytes) = self.service_metrics();
        Ok((
            screen,
            StreamDiagnostics {
                panes_considered: 1,
                panes_subscribed: 1,
                screens_queued: 1,
                screens_delivered: 1,
                screen_bytes,
                preparation_micros: u64::try_from(started.elapsed().as_micros())
                    .unwrap_or(u64::MAX),
                service_cpu_milli_percent,
                service_memory_bytes,
                ..StreamDiagnostics::default()
            },
        ))
    }

    fn service_metrics(&self) -> (u32, u64) {
        let pid = Pid::from_u32(std::process::id());
        let mut sampler = self.diagnostics_sampler.lock();
        let now = Instant::now();
        let should_refresh = sampler
            .last_refresh
            .is_none_or(|last| now.saturating_duration_since(last) >= DIAGNOSTICS_SAMPLE_INTERVAL);
        if should_refresh {
            sampler.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                ProcessRefreshKind::new().with_cpu().with_memory(),
            );
            if let Some((cpu_milli_percent, memory_bytes)) = sampler
                .system
                .process(pid)
                .map(|process| (cpu_milli_percent(process.cpu_usage()), process.memory()))
            {
                sampler.cpu_milli_percent = cpu_milli_percent;
                sampler.memory_bytes = memory_bytes;
            }
            sampler.last_refresh = Some(now);
        }
        (sampler.cpu_milli_percent, sampler.memory_bytes)
    }

    pub fn persist(&self) -> Result<()> {
        let bytes = {
            let mut state = self.state.write();
            refresh_runtime_metadata(&mut state, true)?;
            encode_desired_state(&state)?
        };
        self.write_snapshot(&bytes)
    }

    fn write_snapshot(&self, bytes: &[u8]) -> Result<()> {
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

    pub fn create_pane(&self, target_pane: Uuid, axis: SplitAxis) -> Result<Uuid> {
        {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            state.require_terminal_layout_pane(target_pane)?;
        }
        let new_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(target_pane)?;
        let workspace_id = self.workspace_for_pane(target_pane)?;
        let (session, kind) = self.spawn_pane_for_workspace(new_id, workspace_id, &cwd, None)?;
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let mut new_pane = state.new_pane(new_id);
        if matches!(kind, RuntimePaneKind::SystemSsh { .. }) {
            "ssh".clone_into(&mut new_pane.shell);
        }
        let did_split = state.snapshot.workspaces.iter_mut().any(|workspace| {
            workspace
                .tabs
                .iter_mut()
                .any(|tab| split_layout(&mut tab.layout, target_pane, new_pane.clone(), axis))
        });
        if !did_split {
            bail!("target pane {target_pane} does not exist");
        }
        state.panes.insert(
            new_id,
            RuntimePane {
                backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                    session,
                    last_valid_cwd: cwd,
                    kind,
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
                }),
            },
        );
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(new_id)
    }

    pub fn create_group_terminal(&self, target_pane: Uuid) -> Result<Uuid> {
        let (workspace_id, project_dir) = {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            state.require_terminal_layout_pane(target_pane)?;
            let (workspace, tab) = state
                .snapshot
                .workspaces
                .iter()
                .find_map(|workspace| {
                    workspace.tabs.iter().find_map(|tab| {
                        layout_contains(&tab.layout, target_pane).then_some((workspace, tab))
                    })
                })
                .with_context(|| format!("target pane {target_pane} does not exist"))?;
            let project_dir = tab.project_dir.clone().or_else(|| {
                tab.parent_tab.and_then(|parent_id| {
                    workspace
                        .tabs
                        .iter()
                        .find(|parent| parent.id == parent_id)
                        .and_then(|parent| parent.project_dir.clone())
                })
            });
            (workspace.id, project_dir)
        };
        let new_id = Uuid::new_v4();
        let cwd = match project_dir.as_deref() {
            Some(dir) => local_spawn_dir(Some(dir))?,
            None => self.cwd_for_pane(target_pane)?,
        };
        let (session, kind) =
            self.spawn_pane_for_workspace(new_id, workspace_id, &cwd, project_dir.as_deref())?;
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let mut pane = state.new_pane(new_id);
        if matches!(kind, RuntimePaneKind::SystemSsh { .. }) {
            "ssh".clone_into(&mut pane.shell);
        }
        let did_add = state.snapshot.workspaces.iter_mut().any(|workspace| {
            workspace
                .tabs
                .iter_mut()
                .any(|tab| add_tab(&mut tab.layout, target_pane, pane.clone()))
        });
        if !did_add {
            bail!("target pane {target_pane} does not exist");
        }
        state.panes.insert(
            new_id,
            RuntimePane {
                backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                    session,
                    last_valid_cwd: cwd,
                    kind,
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
                }),
            },
        );
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(new_id)
    }

    pub fn create_group_browser(&self, target_pane: Uuid, url: Option<&str>) -> Result<Uuid> {
        let url = normalize_browser_url_or_default(url)?;
        let title = browser_title(&url, None);
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .find_map(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .find(|tab| layout_contains(&tab.layout, target_pane))
            })
            .with_context(|| format!("target pane {target_pane} does not exist"))?;
        let pane = Pane {
            id: pane_id,
            kind: PaneKind::Browser { url },
            title,
            shell: String::new(),
            color: None,
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        if !add_tab(&mut tab.layout, target_pane, pane) {
            bail!("target pane {target_pane} does not exist");
        }
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Browser,
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_id)
    }

    fn spawn_initial_workspace_terminal(
        &self,
        pane_id: Uuid,
        workspace_id: Uuid,
        connection: &WorkspaceConnection,
        working_dir: Option<&str>,
        cwd: &Path,
    ) -> Result<InitialTerminalSpawn> {
        match connection {
            WorkspaceConnection::Local => Ok(InitialTerminalSpawn {
                session: PtySession::spawn_local(pane_id, workspace_id, cwd, &self.history)?,
                kind: RuntimePaneKind::Local,
                pane_title: "Terminal 1".to_owned(),
                pane_shell: shell_title(),
                tab_title: "Terminals".to_owned(),
            }),
            WorkspaceConnection::SystemSsh { destination, .. } => Ok(InitialTerminalSpawn {
                session: PtySession::spawn_ssh(
                    pane_id,
                    workspace_id,
                    destination,
                    working_dir,
                    &self.history,
                )?,
                kind: RuntimePaneKind::SystemSsh {
                    host: destination.clone(),
                },
                pane_title: format!("SSH {destination}"),
                pane_shell: "ssh".to_owned(),
                tab_title: "Remote".to_owned(),
            }),
        }
    }

    /// Opens the sole initial terminal in a deliberately empty saved workspace.
    /// This request is rejected once any layout exists, so a repeated click or
    /// retried request cannot create duplicate terminals.
    pub fn create_workspace_terminal(&self, workspace_id: Uuid) -> Result<Uuid> {
        let (connection, working_dir) = {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if !workspace.tabs.is_empty() {
                bail!("workstation {workspace_id} already has a terminal layout");
            }
            (workspace.connection.clone(), workspace.working_dir.clone())
        };

        let pane_id = Uuid::new_v4();
        let cwd = local_spawn_dir(working_dir.as_deref())?;
        let InitialTerminalSpawn {
            session,
            kind,
            pane_title,
            pane_shell,
            tab_title,
        } = self.spawn_initial_workspace_terminal(
            pane_id,
            workspace_id,
            &connection,
            working_dir.as_deref(),
            &cwd,
        )?;

        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if !workspace.tabs.is_empty() {
                bail!("workstation {workspace_id} already has a terminal layout");
            }
            let pane = Pane {
                id: pane_id,
                kind: hh_protocol::PaneKind::Terminal,
                title: pane_title,
                shell: pane_shell,
                color: None,
                identity: TerminalIdentity::default(),
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };
            workspace.tabs.push(Tab {
                id: Uuid::new_v4(),
                title: tab_title,
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            });
            workspace.active_terminal_count = 1;
            if let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection {
                *status = WorkspaceConnectionStatus::Connected;
            }
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            {
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            };
            Ok(pane_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    /// Appends one more top-level tab to a workstation that already has a layout.
    /// Unlike `create_workspace_terminal` this is deliberately not idempotent:
    /// every request adds a tab, which is what the workstation menu's "New Tab"
    /// means.
    pub fn create_workspace_tab(&self, workspace_id: Uuid) -> Result<Uuid> {
        self.append_workspace_tab(workspace_id, None, None, None)
    }

    /// Appends a named group holding its first terminal, so the group is visible
    /// and right-clickable before a second terminal exists.
    pub fn create_workspace_group(
        &self,
        workspace_id: Uuid,
        parent_tab: Option<Uuid>,
    ) -> Result<Uuid> {
        let number = {
            let mut state = self.state.write();
            let number = state.next_group_number;
            state.next_group_number = state.next_group_number.saturating_add(1);
            number
        };
        self.append_workspace_tab(
            workspace_id,
            Some(format!("Group {number}")),
            None,
            parent_tab,
        )
    }

    pub fn create_workspace_project(
        &self,
        workspace_id: Uuid,
        working_dir: &str,
        title: Option<&str>,
    ) -> Result<Uuid> {
        validate_workspace_dir(working_dir).map_err(|message| anyhow!(message))?;
        let title = title.map_or_else(
            || {
                working_dir
                    .rsplit('/')
                    .find(|component| !component.is_empty())
                    .unwrap_or("Project")
                    .to_owned()
            },
            str::to_owned,
        );
        self.append_workspace_tab(
            workspace_id,
            Some(title),
            Some(working_dir.to_owned()),
            None,
        )
    }

    fn workspace_tab_dir_override(
        &self,
        workspace_id: Uuid,
        project_dir: Option<&str>,
        parent_tab: Option<Uuid>,
    ) -> Result<Option<String>> {
        let state = self.state.read();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
        }
        let parent_project_dir = if let Some(parent_id) = parent_tab {
            if project_dir.is_some() {
                bail!("parent tab {parent_id} must be a project in the same workstation");
            }
            let parent = workspace
                .tabs
                .iter()
                .find(|tab| tab.id == parent_id)
                .filter(|tab| tab.parent_tab.is_none() && tab.project_dir.is_some())
                .with_context(|| {
                    format!("parent tab {parent_id} must be a project in the same workstation")
                })?;
            parent.project_dir.clone()
        } else {
            None
        };
        Ok(project_dir
            .map(str::to_owned)
            .or(parent_project_dir)
            .or_else(|| workspace.working_dir.clone()))
    }

    fn append_workspace_tab(
        &self,
        workspace_id: Uuid,
        custom_title: Option<String>,
        project_dir: Option<String>,
        parent_tab: Option<Uuid>,
    ) -> Result<Uuid> {
        let dir_override =
            self.workspace_tab_dir_override(workspace_id, project_dir.as_deref(), parent_tab)?;

        let pane_id = Uuid::new_v4();
        let cwd = local_spawn_dir(dir_override.as_deref())?;
        let (session, kind) =
            self.spawn_pane_for_workspace(pane_id, workspace_id, &cwd, dir_override.as_deref())?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let workspace_index = state
                .snapshot
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if state.snapshot.workspaces[workspace_index].tabs.len() >= MAX_TABS_PER_WORKSPACE {
                bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
            }
            let mut pane = state.new_pane(pane_id);
            if matches!(kind, RuntimePaneKind::SystemSsh { .. }) {
                "ssh".clone_into(&mut pane.shell);
            }
            let tab = Tab {
                id: Uuid::new_v4(),
                title: pane.title.clone(),
                custom_title,
                project_dir,
                color: None,
                custom_icon: None,
                parent_tab,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            };
            let workspace = &mut state.snapshot.workspaces[workspace_index];
            let insertion_index = if let Some(parent_id) = parent_tab {
                let parent_index = workspace
                    .tabs
                    .iter()
                    .position(|candidate| {
                        candidate.id == parent_id
                            && candidate.parent_tab.is_none()
                            && candidate.project_dir.is_some()
                    })
                    .with_context(|| {
                        format!("parent tab {parent_id} must be a project in the same workstation")
                    })?;
                workspace
                    .tabs
                    .iter()
                    .enumerate()
                    .filter_map(|(index, candidate)| {
                        (candidate.parent_tab == Some(parent_id)).then_some(index)
                    })
                    .next_back()
                    .unwrap_or(parent_index)
                    + 1
            } else {
                workspace.tabs.len()
            };
            workspace.tabs.insert(insertion_index, tab);
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
            Ok(pane_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    /// Starts the installed OpenSSH client only for an explicit, validated
    /// destination and places it in the target pane's tab strip.
    pub fn connect_ssh(&self, target_pane: Uuid, host: &str) -> Result<Uuid> {
        validate_ssh_host(host).map_err(|message| anyhow!(message))?;
        {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            state.terminal_pane(target_pane)?;
            if !state
                .snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .any(|tab| layout_contains(&tab.layout, target_pane))
            {
                bail!("target pane {target_pane} does not exist");
            }
        }

        let pane_id = Uuid::new_v4();
        let cwd = fallback_cwd()?;
        let workspace_id = self.workspace_for_pane(target_pane)?;
        let session = PtySession::spawn_ssh(pane_id, workspace_id, host, None, &self.history)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let pane = Pane {
                id: pane_id,
                kind: hh_protocol::PaneKind::Terminal,
                title: format!("SSH {host}"),
                shell: "ssh".to_owned(),
                color: None,
                identity: TerminalIdentity::default(),
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };
            let did_add = state.snapshot.workspaces.iter_mut().any(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .any(|tab| add_tab(&mut tab.layout, target_pane, pane.clone()))
            });
            if !did_add {
                bail!("target pane {target_pane} does not exist");
            }
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::SystemSsh {
                            host: host.to_owned(),
                        },
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision += 1;
            Ok(pane_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub fn create_browser_tab(&self, workspace_id: Uuid, url: Option<&str>) -> Result<Uuid> {
        let url = normalize_browser_url_or_default(url)?;
        let title = browser_title(&url, None);
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            bail!("workstation tab limit of {MAX_TABS_PER_WORKSPACE} reached");
        }
        workspace.tabs.push(Tab {
            id: Uuid::new_v4(),
            title: title.clone(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
            layout: PaneLayout::Leaf {
                pane: Pane {
                    id: pane_id,
                    kind: PaneKind::Browser { url },
                    title,
                    shell: String::new(),
                    color: None,
                    identity: TerminalIdentity::default(),
                    custom_title: None,
                    profile_override: None,
                    custom_icon: None,
                },
            },
        });
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Browser,
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_id)
    }

    pub fn set_browser_state(&self, pane_id: Uuid, url: &str, title: Option<&str>) -> Result<()> {
        let url = normalize_browser_url(url)?;
        let title = browser_title(&url, title);
        let mut state = self.state.write();
        match state.panes.get(&pane_id) {
            Some(RuntimePane {
                backend: RuntimePaneBackend::Browser,
            }) => {}
            Some(_) => bail!("pane {pane_id} is a terminal, not a browser"),
            None => bail!("pane {pane_id} does not exist"),
        }
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.title == title
            && matches!(&pane.kind, PaneKind::Browser { url: current } if current == &url)
        {
            return Ok(());
        }
        pane.kind = PaneKind::Browser { url };
        pane.title = title;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn activate_tab(&self, pane_id: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let did_activate = state.snapshot.workspaces.iter_mut().any(|workspace| {
            workspace
                .tabs
                .iter_mut()
                .any(|tab| activate_tab(&mut tab.layout, pane_id))
        });
        if !did_activate {
            bail!("pane tab {pane_id} does not exist");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn swap_panes(&self, source_pane: Uuid, target_pane: Uuid) -> Result<()> {
        if source_pane == target_pane {
            return Ok(());
        }
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) || !state.panes.contains_key(&target_pane) {
            bail!("both panes must exist before they can be rearranged");
        }
        let mut did_swap = false;
        for workspace in &mut state.snapshot.workspaces {
            for tab in &mut workspace.tabs {
                if layout_contains(&tab.layout, source_pane)
                    && layout_contains(&tab.layout, target_pane)
                {
                    swap_pane_ids(&mut tab.layout, source_pane, target_pane);
                    did_swap = true;
                    break;
                }
            }
            if did_swap {
                break;
            }
        }
        if !did_swap {
            bail!("panes can only be rearranged inside the same workstation layout");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn move_pane_to_split(
        &self,
        source_pane: Uuid,
        target_pane: Uuid,
        placement: DropPlacement,
    ) -> Result<()> {
        if source_pane == target_pane {
            return self.split_lone_pane_with_replacement(source_pane, placement);
        }
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) || !state.panes.contains_key(&target_pane) {
            bail!("both panes must exist before they can be rearranged");
        }
        let did_move = state.snapshot.workspaces.iter_mut().any(|workspace| {
            move_workspace_pane_to_split(workspace, source_pane, target_pane, placement)
        });
        if !did_move {
            bail!("source and target panes must exist in the same workstation");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    fn split_lone_pane_with_replacement(
        &self,
        pane_id: Uuid,
        placement: DropPlacement,
    ) -> Result<()> {
        let replacement_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(pane_id)?;
        let workspace_id = self.workspace_for_pane(pane_id)?;
        let replacement_session =
            PtySession::spawn_local(replacement_id, workspace_id, &cwd, &self.history)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let replacement = state.new_pane(replacement_id);
            let did_split = state.snapshot.workspaces.iter_mut().any(|workspace| {
                workspace.tabs.iter_mut().any(|tab| {
                    split_lone_layout_with_replacement(
                        &mut tab.layout,
                        pane_id,
                        replacement.clone(),
                        placement,
                    )
                })
            });
            if !did_split {
                bail!("a self-directed drop requires a pane containing exactly one terminal");
            }
            state.panes.insert(
                replacement_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&replacement_session),
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::Local,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision += 1;
            {
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            };
            Ok(())
        })();
        if result.is_err() {
            let _ = replacement_session.terminate_and_wait();
        }
        result
    }

    pub fn move_pane_to_tab(&self, source_pane: Uuid, target_pane: Uuid) -> Result<()> {
        if source_pane == target_pane {
            return self.activate_tab(source_pane);
        }
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) || !state.panes.contains_key(&target_pane) {
            bail!("both panes must exist before they can be merged");
        }
        let did_move = state
            .snapshot
            .workspaces
            .iter_mut()
            .any(|workspace| move_workspace_pane_to_tab(workspace, source_pane, target_pane));
        if !did_move {
            bail!("source and target panes must exist in the same workstation");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn move_pane_to_group(&self, source_pane: Uuid, target_tab: Uuid) -> Result<()> {
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) {
            bail!("source pane {source_pane} does not exist");
        }
        let source_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| layout_contains(&tab.layout, source_pane))
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("source pane {source_pane} does not belong to a tab"))?;
        let target_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.id == target_tab)
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("target group {target_tab} does not exist"))?;
        if source_location.0 != target_location.0 {
            bail!("panes can only move between groups in the same workstation");
        }
        if source_location == target_location {
            return Ok(());
        }

        let workspace = &mut state.snapshot.workspaces[source_location.0];
        let source_layout = workspace.tabs[source_location.1].layout.clone();
        let (pane, remaining) = detach_pane(source_layout, source_pane);
        let pane = pane.with_context(|| format!("source pane {source_pane} does not exist"))?;
        let mut target_layout = workspace.tabs[target_location.1].layout.clone();
        let target_pane = first_layout_pane(&target_layout);
        if !add_tab(&mut target_layout, target_pane, pane) {
            bail!("target group {target_tab} cannot accept pane {source_pane}");
        }
        workspace.tabs[target_location.1].layout = target_layout;
        if let Some(remaining) = remaining {
            workspace.tabs[source_location.1].layout = remaining;
        } else {
            workspace.tabs.remove(source_location.1);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(())
    }

    fn resolve_move_parent(
        workspace: &Workspace,
        target_index: usize,
        parent_tab: Option<Uuid>,
    ) -> Result<Option<Uuid>> {
        match parent_tab {
            Some(parent) => {
                let valid = workspace.tabs.iter().any(|tab| {
                    tab.id == parent && tab.parent_tab.is_none() && tab.project_dir.is_some()
                });
                if !valid {
                    bail!("parent tab {parent} must be a project in the same workstation");
                }
                Ok(Some(parent))
            }
            None => Ok(workspace.tabs[target_index].parent_tab.filter(|parent| {
                workspace.tabs.iter().any(|tab| {
                    tab.id == *parent && tab.parent_tab.is_none() && tab.project_dir.is_some()
                })
            })),
        }
    }

    pub fn move_pane_to_new_tab(
        &self,
        source_pane: Uuid,
        target_tab: Uuid,
        after: bool,
        parent_tab: Option<Uuid>,
    ) -> Result<()> {
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) {
            bail!("source pane {source_pane} does not exist");
        }
        let source_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| layout_contains(&tab.layout, source_pane))
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("source pane {source_pane} does not belong to a tab"))?;
        let target_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.id == target_tab)
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("target tab {target_tab} does not exist"))?;
        if source_location.0 != target_location.0 {
            bail!("panes can only move between tabs in the same workstation");
        }

        let workspace = &mut state.snapshot.workspaces[source_location.0];
        let append_to_project = parent_tab == Some(target_tab);
        let resolved_parent = Self::resolve_move_parent(workspace, target_location.1, parent_tab)?;
        let source_layout = workspace.tabs[source_location.1].layout.clone();
        let (pane, remaining) = detach_pane(source_layout, source_pane);
        let pane = pane.with_context(|| format!("source pane {source_pane} does not exist"))?;
        if remaining.is_none() && workspace.tabs[source_location.1].id == target_tab {
            return Ok(());
        }
        if let Some(remaining) = remaining {
            workspace.tabs[source_location.1].layout = remaining;
        } else {
            workspace.tabs.remove(source_location.1);
        }
        let target_index = workspace
            .tabs
            .iter()
            .position(|tab| tab.id == target_tab)
            .with_context(|| format!("target tab {target_tab} disappeared during the move"))?;
        let insertion_index = if append_to_project {
            workspace
                .tabs
                .iter()
                .enumerate()
                .filter_map(|(index, candidate)| {
                    (candidate.parent_tab == Some(target_tab)).then_some(index)
                })
                .next_back()
                .unwrap_or(target_index)
                + 1
        } else {
            target_index + usize::from(after)
        };
        workspace.tabs.insert(
            insertion_index,
            Tab {
                id: Uuid::new_v4(),
                title: pane.title.clone(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: resolved_parent,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(())
    }

    pub fn rename_pane(&self, pane_id: Uuid, title: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
            bail!("terminal name must be 1 to 80 visible characters");
        }
        let mut state = self.state.write();
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.custom_title = Some(title.to_owned());
        title.clone_into(&mut pane.title);
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn rename_tab(&self, tab_id: Uuid, title: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
            bail!("group name must be 1 to 80 visible characters");
        }
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("group {tab_id} does not exist"))?;
        tab.custom_title = Some(title.to_owned());
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(())
    }

    pub fn set_pane_profile(&self, pane_id: Uuid, profile: Option<TerminalProfile>) -> Result<()> {
        let mut state = self.state.write();
        let terminal = state.terminal_pane(pane_id)?;
        let (title_signal, command_profile) = (
            terminal.session.terminal_title(),
            terminal.detected_command_profile,
        );
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.custom_title.is_none() {
            pane.custom_title = Some(pane.title.clone());
        }
        pane.profile_override = profile;
        pane.custom_icon = None;
        resolve_pane_identity(pane, title_signal.as_deref(), command_profile);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_pane_custom_icon(&self, pane_id: Uuid, icon: Option<String>) -> Result<()> {
        if let Some(icon) = icon.as_deref() {
            persistence::validate_custom_icon_id(icon)?;
        }
        let mut state = self.state.write();
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.custom_title.is_none() {
            pane.custom_title = Some(pane.title.clone());
        }
        pane.custom_icon = icon;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }
    pub fn set_tab_custom_icon(&self, tab_id: Uuid, icon: Option<String>) -> Result<()> {
        if let Some(icon) = icon.as_deref() {
            persistence::validate_custom_icon_id(icon)?;
        }
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        tab.custom_icon = icon;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn reset_pane_identity(&self, pane_id: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let terminal = state.terminal_pane(pane_id)?;
        let (title_signal, command_profile) = (
            terminal.session.terminal_title(),
            terminal.detected_command_profile,
        );
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.custom_title = None;
        pane.profile_override = None;
        pane.custom_icon = None;
        resolve_pane_identity(pane, title_signal.as_deref(), command_profile);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }
    pub fn close_tab(&self, tab_id: Uuid) -> Result<()> {
        let (workspace_id, tab_ids, pane_ids, sessions, terminal_count) = {
            let state = self.state.read();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
                .with_context(|| format!("tab {tab_id} does not exist"))?;
            let mut tab_ids = HashSet::from([tab_id]);
            loop {
                let previous_len = tab_ids.len();
                let children = workspace
                    .tabs
                    .iter()
                    .filter_map(|tab| {
                        tab.parent_tab
                            .is_some_and(|parent| tab_ids.contains(&parent))
                            .then_some(tab.id)
                    })
                    .collect::<Vec<_>>();
                tab_ids.extend(children);
                if tab_ids.len() == previous_len {
                    break;
                }
            }
            let mut pane_ids = Vec::new();
            for tab in workspace
                .tabs
                .iter()
                .filter(|tab| tab_ids.contains(&tab.id))
            {
                collect_pane_ids(&tab.layout, &mut pane_ids);
            }
            let sessions = pane_ids
                .iter()
                .filter_map(|pane_id| {
                    state
                        .panes
                        .get(pane_id)?
                        .terminal()
                        .map(|terminal| Arc::clone(&terminal.session))
                })
                .collect::<Vec<_>>();
            let terminal_count = u32::try_from(sessions.len()).unwrap_or(u32::MAX);
            (workspace.id, tab_ids, pane_ids, sessions, terminal_count)
        };
        for session in sessions {
            session.terminate_and_wait()?;
        }

        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        if !workspace.tabs.iter().any(|tab| tab.id == tab_id) {
            bail!("tab {tab_id} does not exist");
        }
        workspace.tabs.retain(|tab| !tab_ids.contains(&tab.id));
        workspace.active_terminal_count = workspace
            .active_terminal_count
            .saturating_sub(terminal_count);
        for pane_id in pane_ids {
            state.panes.remove(&pane_id);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn close_pane(&self, pane_id: Uuid) -> Result<()> {
        let (session, was_terminal) = {
            let mut state = self.state.write();
            let pane_exists = state.snapshot.workspaces.iter().any(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|tab| layout_contains(&tab.layout, pane_id))
            });
            if !pane_exists {
                bail!("pane {pane_id} does not exist");
            }
            let runtime = state
                .panes
                .get(&pane_id)
                .context("pane runtime is missing")?;
            let session = runtime
                .terminal()
                .map(|terminal| Arc::clone(&terminal.session));
            let shell_label = runtime
                .terminal()
                .map(|terminal| terminal.kind.shell_label());
            let was_terminal = session.is_some();
            if let Some(shell_label) = shell_label {
                set_pane_runtime_label(
                    &mut state.snapshot,
                    pane_id,
                    false,
                    Some("terminating"),
                    &shell_label,
                );
                state.snapshot.revision = state.snapshot.revision.saturating_add(1);
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            }
            (session, was_terminal)
        };
        if let Some(session) = session {
            session.terminate_and_wait()?;
        }

        let mut state = self.state.write();
        let mut did_close = false;
        for workspace in &mut state.snapshot.workspaces {
            let Some(tab_index) = workspace
                .tabs
                .iter()
                .position(|tab| layout_contains(&tab.layout, pane_id))
            else {
                continue;
            };
            let (_, remaining) = detach_pane(workspace.tabs[tab_index].layout.clone(), pane_id);
            if let Some(remaining) = remaining {
                workspace.tabs[tab_index].layout = remaining;
            } else {
                let removed_tab = workspace.tabs.remove(tab_index).id;
                for tab in &mut workspace.tabs {
                    if tab.parent_tab == Some(removed_tab) {
                        tab.parent_tab = None;
                    }
                }
            }
            if was_terminal {
                workspace.active_terminal_count = workspace.active_terminal_count.saturating_sub(1);
            }
            did_close = true;
            break;
        }
        if !did_close {
            bail!("pane {pane_id} disappeared while closing");
        }
        let removed = state.panes.remove(&pane_id);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(removed);
        self.write_snapshot(&bytes)
    }

    /// Respawns one pane whose process exited, in place, keeping its tab,
    /// layout position, and pane ID.
    ///
    /// This is the recovery path for a transport that died under a live tab —
    /// an SSH drop leaves `tmux attach` dead with a frozen screen that ignores
    /// input. A tmux pane is re-attached with the same plain `attach-session`,
    /// so nothing is ever created or changed on the user's tmux server. A tmux
    /// session that no longer exists fails here instead of registering a fake
    /// live tab.
    pub fn reattach_pane(&self, pane_id: Uuid) -> Result<()> {
        let (kind, cwd, workspace_id) = {
            let state = self.state.read();
            let runtime = state.terminal_pane(pane_id)?;
            if runtime.exit_status.is_none() {
                bail!("this terminal is still live");
            }
            let workspace_id = workspace_id_for_pane(&state.snapshot, pane_id)
                .with_context(|| format!("pane {pane_id} has no workstation"))?;
            (
                runtime.kind.clone(),
                runtime.last_valid_cwd.clone(),
                workspace_id,
            )
        };
        let session = match &kind {
            RuntimePaneKind::Local => {
                PtySession::spawn_local(pane_id, workspace_id, &cwd, &self.history)?
            }
            RuntimePaneKind::SystemSsh { host } => {
                PtySession::spawn_ssh(pane_id, workspace_id, host, None, &self.history)?
            }
            RuntimePaneKind::TmuxLocal { session_id } => {
                PtySession::spawn_tmux_local(pane_id, workspace_id, session_id, &self.history)?
            }
            RuntimePaneKind::TmuxSystemSsh { host, session_id } => {
                PtySession::spawn_tmux_ssh(pane_id, workspace_id, host, session_id, &self.history)?
            }
        };
        if kind.is_runtime_only()
            && let Err(error) = session.confirm_live_for_tmux_attach()
        {
            let _ = session.terminate_and_wait();
            return Err(error);
        }
        let mut state = self.state.write();
        let runtime = state.terminal_pane_mut(pane_id)?;
        let previous = std::mem::replace(&mut runtime.session, session);
        runtime.exit_status = None;
        runtime.recovered = false;
        let shell_label = kind.shell_label();
        let _ = previous.terminate_and_wait();
        drop(previous);
        set_pane_runtime_label(&mut state.snapshot, pane_id, false, None, &shell_label);
        refresh_workspace_activity(&mut state);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_default_terminal_accent(&self, color: AppearanceColor) -> Result<()> {
        let mut state = self.state.write();
        state.snapshot.appearance.default_terminal_accent = color;
        remember_recent_color(&mut state.snapshot, color);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }
    pub fn set_default_workspace_color(&self, color: AppearanceColor) -> Result<()> {
        let mut state = self.state.write();
        state.snapshot.appearance.default_workspace_color = color;
        remember_recent_color(&mut state.snapshot, color);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_pane_color(&self, pane_id: Uuid, color: Option<AppearanceColor>) -> Result<()> {
        let mut state = self.state.write();
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.color = color;
        if let Some(color) = color {
            remember_recent_color(&mut state.snapshot, color);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_tab_color(&self, tab_id: Uuid, color: Option<AppearanceColor>) -> Result<()> {
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        tab.color = color;
        if let Some(color) = color {
            remember_recent_color(&mut state.snapshot, color);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_workspace_color(
        &self,
        workspace_id: Uuid,
        color: Option<AppearanceColor>,
    ) -> Result<()> {
        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        workspace.color = color;
        if let Some(color) = color {
            remember_recent_color(&mut state.snapshot, color);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_workspace_working_dir(
        &self,
        workspace_id: Uuid,
        working_dir: Option<String>,
    ) -> Result<()> {
        if let Some(dir) = working_dir.as_deref() {
            validate_workspace_dir(dir).map_err(|message| anyhow!(message))?;
        }
        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.working_dir == working_dir {
            return Ok(());
        }
        workspace.working_dir = working_dir;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_tab_working_dir(&self, tab_id: Uuid, working_dir: String) -> Result<()> {
        validate_workspace_dir(&working_dir).map_err(|message| anyhow!(message))?;
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .find_map(|workspace| workspace.tabs.iter_mut().find(|tab| tab.id == tab_id))
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        let project_dir = tab
            .project_dir
            .as_mut()
            .with_context(|| format!("tab {tab_id} is not a project"))?;
        if *project_dir == working_dir {
            return Ok(());
        }
        *project_dir = working_dir;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn create_workspace(&self, title: Option<&str>) -> Result<(Uuid, Uuid)> {
        let title = normalize_workspace_title(title)?;
        {
            let state = self.state.read();
            if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
                bail!("workstation limit of {MAX_WORKSPACES} reached");
            }
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
        }
        let workspace_id = Uuid::new_v4();
        let tab_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let cwd = fallback_cwd()?;
        let session = PtySession::spawn_local(pane_id, workspace_id, &cwd, &self.history)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
                bail!("workstation limit of {MAX_WORKSPACES} reached");
            }
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let number = state.snapshot.workspaces.len() + 1;
            let order = next_workspace_order(&state.snapshot.workspaces, false);
            let pane = state.new_pane(pane_id);
            state.snapshot.workspaces.push(Workspace {
                id: workspace_id,
                title: title.unwrap_or_else(|| format!("Workstation {number}")),
                color: None,
                pinned: false,
                pin_order: 0,
                order,
                active_terminal_count: 1,
                connection: WorkspaceConnection::Local,
                working_dir: None,
                tabs: vec![Tab {
                    id: tab_id,
                    title: "Terminals".to_owned(),
                    custom_title: None,
                    project_dir: None,
                    color: None,
                    custom_icon: None,
                    parent_tab: None,
                    pinned: false,
                    layout: PaneLayout::Leaf { pane },
                }],
            });
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::Local,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision += 1;
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
            Ok((workspace_id, pane_id))
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub fn create_ssh_workspace(
        &self,
        title: Option<&str>,
        destination: &str,
    ) -> Result<(Uuid, Uuid)> {
        validate_ssh_host(destination).map_err(|message| anyhow!(message))?;
        let title = normalize_workspace_title(title)?;
        let ids = SshWorkspaceIds {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        };
        let cwd = fallback_cwd()?;
        self.persist_ssh_workspace_intent(title, destination, ids)?;
        let session =
            PtySession::spawn_ssh(ids.pane, ids.workspace, destination, None, &self.history)?;
        let result = self.attach_ssh_workspace(destination, ids, cwd, Arc::clone(&session));
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    fn persist_ssh_workspace_intent(
        &self,
        title: Option<String>,
        destination: &str,
        ids: SshWorkspaceIds,
    ) -> Result<()> {
        let mut state = self.state.write();
        if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
            bail!("workstation limit of {MAX_WORKSPACES} reached");
        }
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let number = state.snapshot.workspaces.len() + 1;
        let order = next_workspace_order(&state.snapshot.workspaces, false);
        let pane = Pane {
            id: ids.pane,
            kind: hh_protocol::PaneKind::Terminal,
            title: format!("SSH {destination}"),
            shell: "ssh".to_owned(),
            color: None,
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        state.snapshot.workspaces.push(Workspace {
            id: ids.workspace,
            title: title.unwrap_or_else(|| format!("SSH Workstation {number}")),
            color: None,
            pinned: false,
            pin_order: 0,
            order,
            active_terminal_count: 0,
            connection: WorkspaceConnection::SystemSsh {
                destination: destination.to_owned(),
                status: WorkspaceConnectionStatus::Offline,
            },
            working_dir: None,
            tabs: vec![Tab {
                id: ids.tab,
                title: "Remote".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            }],
        });
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    fn attach_ssh_workspace(
        &self,
        destination: &str,
        ids: SshWorkspaceIds,
        cwd: PathBuf,
        session: Arc<PtySession>,
    ) -> Result<(Uuid, Uuid)> {
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == ids.workspace)
            .context("saved SSH workstation disappeared before session attachment")?;
        let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection else {
            bail!("saved workstation connection type changed before session attachment");
        };
        *status = WorkspaceConnectionStatus::Connected;
        workspace.active_terminal_count = 1;
        state.panes.insert(
            ids.pane,
            RuntimePane {
                backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                    session,
                    last_valid_cwd: cwd,
                    kind: RuntimePaneKind::SystemSsh {
                        host: destination.to_owned(),
                    },
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
                }),
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok((ids.workspace, ids.pane))
    }

    #[cfg(test)]
    fn create_simulated_ssh_workspace(
        &self,
        title: Option<&str>,
        destination: &str,
    ) -> Result<(Uuid, Uuid)> {
        validate_ssh_host(destination).map_err(|message| anyhow!(message))?;
        let title = normalize_workspace_title(title)?;
        let ids = SshWorkspaceIds {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        };
        let cwd = fallback_cwd()?;
        self.persist_ssh_workspace_intent(title, destination, ids)?;
        let session = PtySession::spawn_local(ids.pane, ids.workspace, &cwd, &self.history)?;
        let result = self.attach_ssh_workspace(destination, ids, cwd, Arc::clone(&session));
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub fn rename_workspace(&self, workspace_id: Uuid, title: &str) -> Result<()> {
        let title =
            normalize_workspace_title(Some(title))?.context("workstation name cannot be empty")?;
        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        workspace.title = title;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_workspace_pinned(&self, workspace_id: Uuid, pinned: bool) -> Result<()> {
        let mut state = self.state.write();
        let next_order = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned)
            .map(|workspace| workspace.pin_order)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let unpinned_order = next_workspace_order(&state.snapshot.workspaces, false);
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        workspace.pinned = pinned;
        workspace.pin_order = if pinned { next_order } else { 0 };
        if !pinned {
            workspace.order = unpinned_order;
        }
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn move_pinned_workspace(
        &self,
        workspace_id: Uuid,
        direction: WorkspacePinMove,
    ) -> Result<()> {
        let mut state = self.state.write();
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        let mut pinned = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned)
            .map(|workspace| (workspace.id, workspace.pin_order))
            .collect::<Vec<_>>();
        pinned.sort_by_key(|(_, order)| *order);
        let index = pinned
            .iter()
            .position(|(id, _)| *id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} is not pinned"))?;
        let other = match direction {
            WorkspacePinMove::Up => index.checked_sub(1),
            WorkspacePinMove::Down => (index + 1 < pinned.len()).then_some(index + 1),
        };
        let Some(other) = other else {
            return Ok(());
        };
        let first = pinned[index];
        let second = pinned[other];
        for workspace in &mut state.snapshot.workspaces {
            if workspace.id == first.0 {
                workspace.pin_order = second.1;
            } else if workspace.id == second.0 {
                workspace.pin_order = first.1;
            }
        }
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn reorder_workspace(
        &self,
        workspace_id: Uuid,
        target_workspace_id: Uuid,
        after: bool,
    ) -> Result<()> {
        let mut state = self.state.write();
        let source_pinned = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?
            .pinned;
        let target_pinned = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == target_workspace_id)
            .with_context(|| format!("workstation {target_workspace_id} does not exist"))?
            .pinned;
        if source_pinned != target_pinned {
            bail!("workstations can only be reordered within the same group");
        }
        let mut ordered = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned == source_pinned)
            .map(|workspace| (workspace.id, workspace.order))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(_, order)| *order);
        let source = ordered
            .iter()
            .position(|(id, _)| *id == workspace_id)
            .context("source workstation was not in its pinned group")?;
        let item = ordered.remove(source);
        let target = ordered
            .iter()
            .position(|(id, _)| *id == target_workspace_id)
            .context("target workstation was not in its pinned group")?;
        ordered.insert(target + usize::from(after), item);
        for (index, (id, _)) in ordered.into_iter().enumerate() {
            if let Some(workspace) = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == id)
            {
                workspace.order = u32::try_from(index + 1).unwrap_or(u32::MAX);
            }
        }
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn reorder_tab(&self, tab_id: Uuid, target_tab_id: Uuid, after: bool) -> Result<()> {
        let mut state = self.state.write();
        let source_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        let target_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == target_tab_id))
            .with_context(|| format!("target tab {target_tab_id} does not exist"))?;
        if source_workspace != target_workspace {
            bail!("tabs can only be reordered within the same workstation");
        }
        if tab_id == target_tab_id {
            return Ok(());
        }
        let tabs = &mut state.snapshot.workspaces[source_workspace].tabs;
        let source = tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .context("source tab disappeared while reordering")?;
        let target = tabs
            .iter()
            .position(|tab| tab.id == target_tab_id)
            .context("target tab disappeared while reordering")?;
        let target_parent = tabs[target].parent_tab.filter(|parent| {
            tabs.iter().any(|tab| {
                tab.id == *parent && tab.parent_tab.is_none() && tab.project_dir.is_some()
            })
        });
        let mut tab = tabs.remove(source);
        tab.parent_tab = if tab.project_dir.is_some() {
            None
        } else {
            target_parent
        };
        let target = tabs
            .iter()
            .position(|candidate| candidate.id == target_tab_id)
            .context("target tab disappeared while reordering")?;
        tabs.insert(target + usize::from(after), tab);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn move_tab_to_project(&self, tab_id: Uuid, project_tab: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let source_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        let project_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == project_tab))
            .with_context(|| format!("project {project_tab} does not exist"))?;
        if source_workspace != project_workspace {
            bail!("tabs can only move within the same workstation");
        }

        let tabs = &mut state.snapshot.workspaces[source_workspace].tabs;
        let source_index = tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .context("source tab disappeared while moving to a project")?;
        let project_index = tabs
            .iter()
            .position(|tab| tab.id == project_tab)
            .context("project tab disappeared while moving a tab")?;
        if tabs[project_index].project_dir.is_none() || tabs[project_index].parent_tab.is_some() {
            bail!("target {project_tab} is not a project");
        }
        if tabs[source_index].project_dir.is_some() {
            bail!("a project cannot nest inside another project");
        }
        if tabs[source_index].parent_tab == Some(project_tab) {
            return Ok(());
        }

        let mut tab = tabs.remove(source_index);
        tab.parent_tab = Some(project_tab);
        let project_index = tabs
            .iter()
            .position(|candidate| candidate.id == project_tab)
            .context("project tab disappeared while moving a tab")?;
        let insertion_index = tabs
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                (candidate.parent_tab == Some(project_tab)).then_some(index)
            })
            .next_back()
            .unwrap_or(project_index)
            + 1;
        tabs.insert(insertion_index, tab);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_tab_pinned(&self, tab_id: Uuid, pinned: bool) -> Result<()> {
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        if tab.pinned == pinned {
            return Ok(());
        }
        tab.pinned = pinned;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn disconnect_workspace(&self, workspace_id: Uuid) -> Result<()> {
        let sessions = {
            let state = self.state.read();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if !matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. }) {
                bail!("only a system-SSH workstation can be disconnected");
            }
            pane_ids_for_workspace(workspace)
                .into_iter()
                .filter_map(|pane_id| {
                    let terminal = state.panes.get(&pane_id)?.terminal()?;
                    matches!(terminal.kind, RuntimePaneKind::SystemSsh { .. })
                        .then(|| (pane_id, Arc::clone(&terminal.session)))
                })
                .collect::<Vec<_>>()
        };
        for (_, session) in &sessions {
            let _ = session.terminate_and_wait();
        }

        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| {
                format!("workstation {workspace_id} disappeared while disconnecting")
            })?;
        let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection else {
            bail!("only a system-SSH workstation can be disconnected");
        };
        *status = WorkspaceConnectionStatus::Offline;
        workspace.active_terminal_count = workspace
            .active_terminal_count
            .saturating_sub(u32::try_from(sessions.len()).unwrap_or(u32::MAX));
        for (pane_id, _) in sessions {
            if let Ok(runtime) = state.terminal_pane_mut(pane_id) {
                runtime.exit_status = Some("disconnected".to_owned());
            }
            set_pane_runtime_label(
                &mut state.snapshot,
                pane_id,
                false,
                Some("disconnected"),
                "system OpenSSH",
            );
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn reconnect_workspace(&self, workspace_id: Uuid) -> Result<Uuid> {
        let (destination, working_dir, mut pane_ids) = {
            let state = self.state.read();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            let WorkspaceConnection::SystemSsh {
                destination,
                status,
            } = &workspace.connection
            else {
                bail!("only a system-SSH workstation can be reconnected");
            };
            if *status == WorkspaceConnectionStatus::Connected {
                bail!("workstation is already connected");
            }
            let pane_ids = pane_ids_for_workspace(workspace)
                .into_iter()
                .filter(|pane_id| {
                    state.panes.get(pane_id).is_none_or(|runtime| {
                        runtime.terminal().is_some_and(|terminal| {
                            matches!(terminal.kind, RuntimePaneKind::SystemSsh { .. })
                                && terminal.exit_status.is_some()
                        })
                    })
                })
                .collect::<Vec<_>>();
            (destination.clone(), workspace.working_dir.clone(), pane_ids)
        };
        validate_ssh_host(&destination).map_err(|message| anyhow!(message))?;
        let created_layout = pane_ids.is_empty();
        if created_layout {
            pane_ids.push(Uuid::new_v4());
        }
        let mut sessions = Vec::with_capacity(pane_ids.len());
        for pane_id in &pane_ids {
            match PtySession::spawn_ssh(
                *pane_id,
                workspace_id,
                &destination,
                working_dir.as_deref(),
                &self.history,
            ) {
                Ok(session) => sessions.push((*pane_id, session)),
                Err(error) => {
                    for (_, session) in sessions {
                        let _ = session.terminate_and_wait();
                    }
                    return Err(error);
                }
            }
        }
        let result = (|| {
            let mut state = self.state.write();
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| {
                    format!("workstation {workspace_id} disappeared while reconnecting")
                })?;
            let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection else {
                bail!("only a system-SSH workstation can be reconnected");
            };
            if created_layout {
                let pane_id = pane_ids[0];
                let pane = Pane {
                    id: pane_id,
                    kind: hh_protocol::PaneKind::Terminal,
                    title: format!("SSH {destination}"),
                    shell: "ssh".to_owned(),
                    color: None,
                    identity: TerminalIdentity::default(),
                    custom_title: None,
                    profile_override: None,
                    custom_icon: None,
                };
                workspace.tabs.push(Tab {
                    id: Uuid::new_v4(),
                    title: "Remote".to_owned(),
                    custom_title: None,
                    project_dir: None,
                    color: None,
                    custom_icon: None,
                    parent_tab: None,
                    pinned: false,
                    layout: PaneLayout::Leaf { pane },
                });
            } else {
                for pane_id in &pane_ids {
                    if let Some(pane) = workspace
                        .tabs
                        .iter_mut()
                        .find_map(|tab| find_pane_mut(&mut tab.layout, *pane_id))
                    {
                        pane.title = format!("SSH {destination}");
                        "ssh".clone_into(&mut pane.shell);
                    }
                }
            }
            *status = WorkspaceConnectionStatus::Connected;
            workspace.active_terminal_count = workspace
                .active_terminal_count
                .saturating_add(u32::try_from(sessions.len()).unwrap_or(u32::MAX));
            let cwd = fallback_cwd()?;
            for (pane_id, session) in &sessions {
                state.panes.insert(
                    *pane_id,
                    RuntimePane {
                        backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                            session: Arc::clone(session),
                            last_valid_cwd: cwd.clone(),
                            kind: RuntimePaneKind::SystemSsh {
                                host: destination.clone(),
                            },
                            recovered: false,
                            exit_status: None,
                            detected_command_profile: None,
                        }),
                    },
                );
            }
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            {
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            };
            Ok(pane_ids[0])
        })();
        if result.is_err() {
            for (_, session) in sessions {
                let _ = session.terminate_and_wait();
            }
        }
        result
    }

    pub fn delete_workspace(&self, workspace_id: Uuid) -> Result<()> {
        let (pane_ids, sessions) = {
            let state = self.state.read();
            if state.snapshot.workspaces.len() <= 1 {
                bail!("the last workstation cannot be deleted");
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            let pane_ids = pane_ids_for_workspace(workspace);
            let sessions = pane_ids
                .iter()
                .filter_map(|pane_id| {
                    state
                        .panes
                        .get(pane_id)?
                        .terminal()
                        .map(|terminal| Arc::clone(&terminal.session))
                })
                .collect::<Vec<_>>();
            (pane_ids, sessions)
        };
        for session in &sessions {
            let _ = session.terminate_and_wait();
        }
        let mut state = self.state.write();
        let before = state.snapshot.workspaces.len();
        state
            .snapshot
            .workspaces
            .retain(|workspace| workspace.id != workspace_id);
        if state.snapshot.workspaces.len() == before {
            bail!("workstation {workspace_id} disappeared while deleting");
        }
        let removed = pane_ids
            .into_iter()
            .filter_map(|pane_id| state.panes.remove(&pane_id))
            .collect::<Vec<_>>();
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(removed);
        self.write_snapshot(&bytes)
    }

    pub fn write_input(&self, pane_id: Uuid, bytes: &[u8]) -> Result<()> {
        self.pane(pane_id)?.write_input(bytes)
    }

    pub fn begin_selection(
        &self,
        pane_id: Uuid,
        point: TerminalPoint,
        kind: TerminalSelectionKind,
    ) -> Result<()> {
        self.pane(pane_id)?.begin_selection(point, kind);
        Ok(())
    }

    pub fn update_selection(&self, pane_id: Uuid, point: TerminalPoint) -> Result<()> {
        self.pane(pane_id)?.update_selection(point);
        Ok(())
    }

    pub fn clear_selection(&self, pane_id: Uuid) -> Result<()> {
        self.pane(pane_id)?.clear_selection();
        Ok(())
    }

    pub fn selected_text(&self, pane_id: Uuid) -> Result<Option<String>> {
        Ok(self.pane(pane_id)?.selected_text())
    }

    pub fn scroll_pane(&self, pane_id: Uuid, lines: i32) -> Result<()> {
        self.pane(pane_id)?.scroll(lines);
        Ok(())
    }

    pub fn search_pane(&self, pane_id: Uuid, query: &str, forward: bool) -> Result<bool> {
        self.pane(pane_id)?.search_literal(query, forward)
    }

    pub fn mouse_input(
        &self,
        pane_id: Uuid,
        point: TerminalPoint,
        button: TerminalMouseButton,
        action: TerminalMouseAction,
        modifiers: TerminalModifiers,
    ) -> Result<()> {
        self.pane(pane_id)?
            .mouse_input(point, button, action, modifiers)
    }

    pub fn resize_pane(&self, pane_id: Uuid, columns: u16, rows: u16) -> Result<()> {
        self.pane(pane_id)?.resize(columns, rows)
    }

    pub fn pane_process_id(&self, pane_id: Uuid) -> Result<Option<u32>> {
        Ok(self.pane(pane_id)?.process_id())
    }

    fn begin_remote_ls(&self, workspace_id: Uuid) -> Result<RemoteLsPermit> {
        let gate = Arc::clone(&self.remote_ls_gate);
        {
            let mut state = gate.lock();
            if state.active.contains(&workspace_id) {
                bail!("a directory listing is already running for this workstation");
            }
            if state
                .last_completed
                .get(&workspace_id)
                .is_some_and(|completed| completed.elapsed() < REMOTE_LS_MIN_INTERVAL)
            {
                bail!("wait before listing remote directories again");
            }
            state.active.insert(workspace_id);
        }
        Ok(RemoteLsPermit { gate, workspace_id })
    }

    pub fn list_remote_directory(&self, workspace_id: Uuid, path: &str) -> Result<Vec<String>> {
        validate_workspace_dir(path).map_err(|message| anyhow!(message))?;
        let _permit = self.begin_remote_ls(workspace_id)?;
        let mut entries = match self.workspace_connection(workspace_id)? {
            WorkspaceConnection::Local => {
                let mut entries = Vec::new();
                for entry in
                    std::fs::read_dir(path).with_context(|| format!("read directory {path}"))?
                {
                    let entry = entry.with_context(|| format!("read directory entry in {path}"))?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !name.starts_with('.') && entry.file_type()?.is_dir() {
                        entries.push(name);
                    }
                }
                entries
            }
            WorkspaceConnection::SystemSsh { destination, .. } => {
                let output = run_bounded_command(
                    remote_directory_command(&destination, path)?,
                    REMOTE_LS_TIMEOUT,
                    "remote directory listing",
                )?;
                if !output.success {
                    let message = output.stderr.trim();
                    if message.is_empty() {
                        bail!("remote directory listing failed");
                    }
                    bail!("remote directory listing failed: {message}");
                }
                output
                    .stdout
                    .lines()
                    .filter_map(|line| line.strip_suffix('/'))
                    .filter(|name| !name.is_empty() && !name.starts_with('.'))
                    .map(str::to_owned)
                    .collect()
            }
        };
        entries.sort_unstable();
        entries.dedup();
        entries.truncate(MAX_REMOTE_DIRECTORY_ENTRIES);
        Ok(entries)
    }

    fn begin_tmux_scan(&self, workspace_id: Uuid) -> Result<TmuxScanPermit> {
        let gate = Arc::clone(&self.tmux_scan_gate);
        {
            let mut state = gate.lock();
            if state.active.contains(&workspace_id) {
                bail!("a tmux scan is already running for this workstation");
            }
            if state
                .last_completed
                .get(&workspace_id)
                .is_some_and(|completed| completed.elapsed() < TMUX_SCAN_MIN_INTERVAL)
            {
                bail!("wait before scanning tmux sessions again");
            }
            state.active.insert(workspace_id);
        }
        Ok(TmuxScanPermit { gate, workspace_id })
    }

    /// Performs an explicit bounded metadata-only scan of the default tmux
    /// server for one workstation. It never starts tmux, reconnects a saved
    /// SSH workstation, or writes scan output to terminal history.
    pub fn scan_tmux_sessions(&self, workspace_id: Uuid) -> Result<TmuxScanResult> {
        let _scan_permit = self.begin_tmux_scan(workspace_id)?;
        let connection = self.workspace_connection(workspace_id)?;
        let (scope, probe) = match connection {
            WorkspaceConnection::Local => (TmuxScanScope::Local, tmux_local_probe_command()?),
            WorkspaceConnection::SystemSsh {
                destination,
                status: WorkspaceConnectionStatus::Connected,
            } => (
                TmuxScanScope::SystemSsh {
                    destination: destination.clone(),
                },
                tmux_ssh_probe_command(&destination)?,
            ),
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            } => bail!("reconnect this SSH workstation before scanning tmux sessions"),
        };
        let open_session_ids = self.open_tmux_session_ids(workspace_id)?;
        let output = run_tmux_probe(probe)?;
        if !output.success {
            if tmux_reports_no_server(&output.stderr) {
                return Ok(TmuxScanResult {
                    scope,
                    sessions: Vec::new(),
                    open_session_ids: Vec::new(),
                    no_server: true,
                });
            }
            bail!("tmux scan failed: {}", probe_error_summary(&output.stderr));
        }
        let sessions = parse_tmux_scan(&output.stdout)?;
        let open_session_ids = sessions
            .iter()
            .filter(|session| open_session_ids.contains(&session.id))
            .map(|session| session.id.clone())
            .collect();
        Ok(TmuxScanResult {
            scope,
            sessions,
            open_session_ids,
            no_server: false,
        })
    }

    /// Opens each selected existing tmux session in an independent live tab.
    /// Each target is isolated: an immediate attach failure returns a clear
    /// issue for that target while the rest can still open.
    pub fn attach_tmux_sessions(
        &self,
        workspace_id: Uuid,
        session_ids: &[TmuxSessionId],
    ) -> Result<TmuxAttachmentResult> {
        let connection = {
            let state = self.state.read();
            state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| workspace.connection.clone())
                .with_context(|| format!("workstation {workspace_id} does not exist"))?
        };
        if matches!(
            connection,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            }
        ) {
            bail!("reconnect this SSH workstation before opening tmux");
        }
        let probe = match &connection {
            WorkspaceConnection::Local => tmux_local_probe_command()?,
            WorkspaceConnection::SystemSsh {
                destination,
                status: WorkspaceConnectionStatus::Connected,
            } => tmux_ssh_probe_command(destination)?,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            } => unreachable!("offline connection rejected above"),
        };
        let output = run_tmux_probe(probe)?;
        if !output.success {
            bail!("tmux scan failed: {}", probe_error_summary(&output.stderr));
        }
        let sessions = parse_tmux_scan(&output.stdout)?;
        let known_sessions = sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect::<HashMap<_, _>>();
        let already_open = self.open_tmux_session_ids(workspace_id)?;
        let plan = plan_tmux_session_attachments(session_ids, &already_open, &known_sessions)?;
        let mut result = TmuxAttachmentResult {
            pane_ids: Vec::new(),
            skipped: plan.skipped,
        };
        for session in plan.launch {
            match self.attach_tmux_session_one(workspace_id, &session, &connection) {
                Ok(pane_id) => result.pane_ids.push(pane_id),
                Err(error) => result.skipped.push(TmuxSessionAttachIssue {
                    session_id: session.id,
                    message: error.to_string(),
                }),
            }
        }
        Ok(result)
    }

    /// Sessions this workstation currently shows in a *live* tab. A tab whose
    /// attach died must not keep its session hostage: the picker has to offer
    /// it again so the user can reopen it.
    fn open_tmux_session_ids(&self, workspace_id: Uuid) -> Result<HashSet<TmuxSessionId>> {
        let state = self.state.read();
        let workspace = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        Ok(pane_ids_for_workspace(workspace)
            .into_iter()
            .filter_map(|pane_id| state.panes.get(&pane_id)?.terminal())
            .filter(|terminal| terminal.exit_status.is_none())
            .filter_map(|terminal| terminal.kind.tmux_session_id().cloned())
            .collect())
    }

    fn attach_tmux_session_one(
        &self,
        workspace_id: Uuid,
        tmux_session: &TmuxSession,
        connection: &WorkspaceConnection,
    ) -> Result<Uuid> {
        let pane_id = Uuid::new_v4();
        let (session, kind) = match connection {
            WorkspaceConnection::Local => (
                PtySession::spawn_tmux_local(
                    pane_id,
                    workspace_id,
                    &tmux_session.id,
                    &self.history,
                )?,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            ),
            WorkspaceConnection::SystemSsh {
                destination,
                status: WorkspaceConnectionStatus::Connected,
            } => (
                PtySession::spawn_tmux_ssh(
                    pane_id,
                    workspace_id,
                    destination,
                    &tmux_session.id,
                    &self.history,
                )?,
                RuntimePaneKind::TmuxSystemSsh {
                    host: destination.clone(),
                    session_id: tmux_session.id.clone(),
                },
            ),
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            } => bail!("reconnect this SSH workstation before opening tmux"),
        };
        self.register_live_tmux_tab(workspace_id, pane_id, tmux_session, &session, kind)
    }

    fn register_live_tmux_tab(
        &self,
        workspace_id: Uuid,
        pane_id: Uuid,
        tmux_session: &TmuxSession,
        session: &Arc<PtySession>,
        kind: RuntimePaneKind,
    ) -> Result<Uuid> {
        if let Err(error) = session.confirm_live_for_tmux_attach() {
            let _ = session.terminate_and_wait();
            return Err(error);
        }
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let already_open = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(pane_ids_for_workspace)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|pane_id| state.panes.get(&pane_id)?.terminal())
                .filter(|terminal| terminal.exit_status.is_none())
                .filter_map(|terminal| terminal.kind.tmux_session_id())
                .any(|existing| existing == &tmux_session.id);
            if already_open {
                bail!("already open in this workstation");
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            let still_connected = matches!(
                workspace.connection,
                WorkspaceConnection::Local
                    | WorkspaceConnection::SystemSsh {
                        status: WorkspaceConnectionStatus::Connected,
                        ..
                    }
            );
            if !still_connected {
                bail!("workstation went offline before opening tmux");
            }
            workspace.tabs.push(Tab {
                id: Uuid::new_v4(),
                title: tmux_session.name.clone(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf {
                    pane: Pane {
                        id: pane_id,
                        kind: hh_protocol::PaneKind::Terminal,
                        title: format!("tmux {}", tmux_session.name),
                        shell: "tmux".to_owned(),
                        color: None,
                        identity: TerminalIdentity {
                            profile: TerminalProfile::Tmux,
                            source: TerminalIdentitySource::Command,
                        },
                        custom_title: None,
                        profile_override: None,
                        custom_icon: None,
                    },
                },
            });
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(session),
                        last_valid_cwd: fallback_cwd()?,
                        kind,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            {
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            };
            Ok(pane_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    fn pane(&self, pane_id: Uuid) -> Result<Arc<PtySession>> {
        let state = self.state.read();
        Ok(Arc::clone(&state.terminal_pane(pane_id)?.session))
    }
    fn cwd_for_pane(&self, pane_id: Uuid) -> Result<PathBuf> {
        let mut state = self.state.write();
        refresh_runtime_metadata(&mut state, false)?;
        let runtime = state.terminal_pane(pane_id)?;
        match &runtime.kind {
            RuntimePaneKind::Local => Ok(runtime.last_valid_cwd.clone()),
            RuntimePaneKind::SystemSsh { .. }
            | RuntimePaneKind::TmuxLocal { .. }
            | RuntimePaneKind::TmuxSystemSsh { .. } => fallback_cwd(),
        }
    }
    fn workspace_for_pane(&self, pane_id: Uuid) -> Result<Uuid> {
        let state = self.state.read();
        workspace_id_for_pane(&state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} has no workspace"))
    }

    fn workspace_connection(&self, workspace_id: Uuid) -> Result<WorkspaceConnection> {
        let state = self.state.read();
        state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.connection.clone())
            .with_context(|| format!("workstation {workspace_id} does not exist"))
    }

    fn spawn_pane_for_workspace(
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

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new().expect("start seeded configured-shell PTY")
    }
}

struct CountingWriter(u64);

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

fn serialized_len(value: &impl Serialize) -> Result<u64> {
    let mut counter = CountingWriter(0);
    serde_json::to_writer(&mut counter, value).context("measure protocol payload")?;
    Ok(counter.0)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn cpu_milli_percent(percent: f32) -> u32 {
    (percent.max(0.0) * 1_000.0).round().min(u32::MAX as f32) as u32
}

fn remember_recent_color(snapshot: &mut SessionSnapshot, color: AppearanceColor) {
    snapshot
        .appearance
        .recent_colors
        .retain(|recent| *recent != color);
    snapshot.appearance.recent_colors.insert(0, color);
    snapshot
        .appearance
        .recent_colors
        .truncate(MAX_RECENT_COLORS);
}

fn encode_desired_state(state: &RegistryState) -> Result<Vec<u8>> {
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
    let cwd_by_pane = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| {
            let terminal = runtime.terminal()?;
            terminal
                .kind
                .is_local()
                .then(|| (*pane_id, terminal.last_valid_cwd.clone()))
        })
        .collect();
    SnapshotStore::encode(&snapshot, &cwd_by_pane)
}

fn refresh_runtime_metadata(state: &mut RegistryState, force_process_refresh: bool) -> Result<()> {
    let pids = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| {
            runtime
                .terminal()?
                .session
                .process_id()
                .map(|process_id| (*pane_id, Pid::from_u32(process_id)))
        })
        .collect::<Vec<_>>();
    let refresh_identity = force_process_refresh
        || state.last_identity_refresh.is_none_or(|last| {
            Instant::now().saturating_duration_since(last) >= IDENTITY_REFRESH_INTERVAL
        });
    if refresh_identity {
        if !pids.is_empty() {
            let process_ids = pids.iter().map(|(_, pid)| *pid).collect::<Vec<_>>();
            state.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&process_ids),
                ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
            );
        }
        refresh_command_profiles(state);
        state.last_identity_refresh = Some(Instant::now());
    }

    let mut labels = Vec::new();
    let system = &state.system;
    for (pane_id, runtime) in &mut state.panes {
        let Some(runtime) = runtime.terminal_mut() else {
            continue;
        };
        if runtime.kind.is_local()
            && runtime.exit_status.is_none()
            && let Some((_, pid)) = pids.iter().find(|(id, _)| id == pane_id)
            && let Some(cwd) = system.process(*pid).and_then(sysinfo::Process::cwd)
            && valid_local_cwd(cwd)
        {
            runtime.last_valid_cwd = cwd.to_path_buf();
        }
        let observed = runtime.session.exit_status()?;
        if observed.is_some() && observed != runtime.exit_status {
            runtime.exit_status.clone_from(&observed);
            labels.push((
                *pane_id,
                runtime.recovered,
                observed,
                runtime.kind.shell_label(),
            ));
        }
    }
    if !labels.is_empty() {
        for (pane_id, recovered, status, shell_label) in labels {
            set_pane_runtime_label(
                &mut state.snapshot,
                pane_id,
                recovered,
                status.as_deref(),
                &shell_label,
            );
            if status.is_some() {
                state.append_notification(
                    pane_id,
                    NotificationKind::Completed,
                    None,
                    history::now_ms(),
                );
            }
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    let identity_inputs = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| {
            let terminal = runtime.terminal()?;
            terminal.kind.is_local().then(|| {
                (
                    *pane_id,
                    terminal.session.terminal_title(),
                    terminal.detected_command_profile,
                )
            })
        })
        .collect::<Vec<_>>();
    let mut identity_changed = false;
    for (pane_id, title_signal, command_profile) in identity_inputs {
        if let Some(pane) = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id) {
            identity_changed |=
                resolve_pane_identity(pane, title_signal.as_deref(), command_profile);
        }
    }
    if identity_changed {
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    if refresh_workspace_activity(state) {
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    Ok(())
}

/// Recomputes per-workstation terminal counts and SSH reachability.
///
/// An SSH workstation goes offline only when every remote pane it still owns
/// has died — a real transport failure. Deliberately closing terminals is not
/// a disconnect, so a workstation with zero tabs stays connected and its next
/// terminal simply opens.
fn refresh_workspace_activity(state: &mut RegistryState) -> bool {
    let workspace_activity = state
        .snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let mut active = 0_u32;
            let mut remote_panes = 0_u32;
            let mut remote_live = 0_u32;
            for pane_id in pane_ids_for_workspace(workspace) {
                let Some(runtime) = state.panes.get(&pane_id).and_then(RuntimePane::terminal)
                else {
                    continue;
                };
                let live = runtime.exit_status.is_none();
                if live {
                    active = active.saturating_add(1);
                }
                if runtime.kind.is_remote() {
                    remote_panes = remote_panes.saturating_add(1);
                    if live {
                        remote_live = remote_live.saturating_add(1);
                    }
                }
            }
            (workspace.id, active, remote_panes, remote_live)
        })
        .collect::<Vec<_>>();
    let mut workspace_changed = false;
    for workspace in &mut state.snapshot.workspaces {
        let Some((_, active, remote_panes, remote_live)) = workspace_activity
            .iter()
            .find(|(workspace_id, _, _, _)| *workspace_id == workspace.id)
        else {
            continue;
        };
        if workspace.active_terminal_count != *active {
            workspace.active_terminal_count = *active;
            workspace_changed = true;
        }
        if let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection {
            let next = if *remote_live > 0 {
                Some(WorkspaceConnectionStatus::Connected)
            } else if *remote_panes > 0 {
                Some(WorkspaceConnectionStatus::Offline)
            } else {
                None
            };
            if let Some(next) = next
                && *status != next
            {
                *status = next;
                workspace_changed = true;
            }
        }
    }
    workspace_changed
}

fn refresh_command_profiles(state: &mut RegistryState) {
    if !state.panes.iter().any(|(pane_id, runtime)| {
        runtime
            .terminal()
            .is_some_and(|terminal| terminal.kind.is_local())
            && find_pane_in_snapshot(&state.snapshot, *pane_id)
                .is_some_and(|pane| pane.custom_title.is_none() && pane.profile_override.is_none())
    }) {
        return;
    }

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        ProcessRefreshKind::new().with_exe(UpdateKind::Always),
    );
    if system.processes().len() > MAX_DISCOVERY_PROCESSES {
        return;
    }
    for runtime in state
        .panes
        .values_mut()
        .filter_map(RuntimePane::terminal_mut)
    {
        if runtime.kind.is_local() {
            runtime.detected_command_profile = runtime
                .session
                .process_id()
                .map(Pid::from_u32)
                .and_then(|root| discover_descendant_profile(&system, root));
        }
    }
}

fn discover_descendant_profile(system: &System, root: Pid) -> Option<TerminalProfile> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    let mut queue = VecDeque::from([(root, 0_usize)]);
    let mut inspected = 0_usize;
    while let Some((parent, depth)) = queue.pop_front() {
        if depth >= MAX_DISCOVERY_DEPTH {
            continue;
        }
        for child in children.get(&parent).into_iter().flatten() {
            inspected += 1;
            if inspected > MAX_DISCOVERY_DESCENDANTS_PER_PANE {
                return None;
            }
            if let Some(process) = system.process(*child)
                && let Some(profile) = process
                    .name()
                    .to_str()
                    .and_then(terminal_profile_for_command)
                    .or_else(|| process.exe().and_then(terminal_profile_for_executable))
            {
                return Some(profile);
            }
            queue.push_back((*child, depth + 1));
        }
    }
    None
}

fn resolve_pane_identity(
    pane: &mut Pane,
    terminal_title: Option<&str>,
    command_profile: Option<TerminalProfile>,
) -> bool {
    let (profile, mut source, generated_title) = if let Some(profile) = pane.profile_override {
        (
            profile,
            TerminalIdentitySource::UserProfile,
            profile.display_name().to_owned(),
        )
    } else if let Some(profile) = terminal_title.and_then(terminal_profile_for_title) {
        (
            profile,
            TerminalIdentitySource::TerminalTitle,
            profile.display_name().to_owned(),
        )
    } else if let Some(profile) = command_profile {
        (
            profile,
            TerminalIdentitySource::Command,
            profile.display_name().to_owned(),
        )
    } else {
        let title = if pane.identity.source == TerminalIdentitySource::Fallback
            && pane.title.starts_with("Terminal")
        {
            pane.title.clone()
        } else {
            TerminalProfile::Terminal.display_name().to_owned()
        };
        (
            TerminalProfile::Terminal,
            TerminalIdentitySource::Fallback,
            title,
        )
    };
    let title = pane.custom_title.clone().unwrap_or(generated_title);
    if pane.custom_title.is_some() && source == TerminalIdentitySource::Fallback {
        source = TerminalIdentitySource::UserRename;
    }
    let identity = TerminalIdentity { profile, source };
    let changed = pane.identity != identity || pane.title != title;
    pane.identity = identity;
    pane.title = title;
    changed
}

fn set_pane_runtime_label(
    snapshot: &mut SessionSnapshot,
    pane_id: Uuid,
    recovered: bool,
    status: Option<&str>,
    shell_label: &str,
) {
    let label = match status {
        Some("terminating") => format!("{shell_label} · terminating"),
        Some(status) => format!("{shell_label} · exited ({status})"),
        None if recovered => format!("{shell_label} · recovered with a fresh shell"),
        None => shell_label.to_owned(),
    };
    if let Some(pane) = find_pane_mut_in_snapshot(snapshot, pane_id) {
        pane.shell = label;
    }
}

fn pane_ids_in_snapshot(snapshot: &SessionSnapshot) -> Vec<Uuid> {
    let mut pane_ids = Vec::new();
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            collect_pane_ids(&tab.layout, &mut pane_ids);
        }
    }
    pane_ids
}

fn pane_ids_for_workspace(workspace: &Workspace) -> Vec<Uuid> {
    let mut pane_ids = Vec::new();
    for tab in &workspace.tabs {
        collect_pane_ids(&tab.layout, &mut pane_ids);
    }
    pane_ids
}

fn collect_pane_ids(layout: &PaneLayout, pane_ids: &mut Vec<Uuid>) {
    match layout {
        PaneLayout::Leaf { pane } => pane_ids.push(pane.id),
        PaneLayout::Stack { panes, .. } => {
            pane_ids.extend(panes.iter().map(|pane| pane.id));
        }
        PaneLayout::Split { first, second, .. } => {
            collect_pane_ids(first, pane_ids);
            collect_pane_ids(second, pane_ids);
        }
    }
}

fn browser_title(url: &str, title: Option<&str>) -> String {
    let explicit = title
        .map(|title| {
            title
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect::<String>()
        })
        .map(|title| {
            title
                .trim()
                .chars()
                .take(MAX_WORKSPACE_TITLE_CHARS)
                .collect()
        })
        .filter(|title: &String| !title.is_empty());
    explicit
        .or_else(|| {
            url::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
        })
        .unwrap_or_else(|| "Browser".to_owned())
}

fn normalize_workspace_title(title: Option<&str>) -> Result<Option<String>> {
    let Some(title) = title else {
        return Ok(None);
    };
    let title = title.trim();
    if title.is_empty() {
        return Ok(None);
    }
    if title.chars().count() > MAX_WORKSPACE_TITLE_CHARS {
        bail!("workstation name may contain at most {MAX_WORKSPACE_TITLE_CHARS} characters");
    }
    if title.chars().any(char::is_control) {
        bail!("workstation name may not contain control characters");
    }
    Ok(Some(title.to_owned()))
}

fn next_workspace_order(workspaces: &[Workspace], pinned: bool) -> u32 {
    workspaces
        .iter()
        .filter(|workspace| workspace.pinned == pinned)
        .map(|workspace| workspace.order)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn normalize_workspace_orders(workspaces: &mut [Workspace]) {
    let mut pinned = workspaces
        .iter()
        .enumerate()
        .filter(|(_, workspace)| workspace.pinned)
        .map(|(index, workspace)| (index, workspace.pin_order))
        .collect::<Vec<_>>();
    pinned.sort_by_key(|(index, order)| (*order, *index));
    for (order, (index, _)) in pinned.into_iter().enumerate() {
        workspaces[index].pin_order = u32::try_from(order + 1).unwrap_or(u32::MAX);
    }
    for workspace in workspaces.iter_mut().filter(|workspace| !workspace.pinned) {
        workspace.pin_order = 0;
    }
    for pinned in [true, false] {
        let mut group = workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| workspace.pinned == pinned)
            .map(|(index, workspace)| (index, workspace.order))
            .collect::<Vec<_>>();
        group.sort_by_key(|(index, order)| (*order, *index));
        for (order, (index, _)) in group.into_iter().enumerate() {
            workspaces[index].order = u32::try_from(order + 1).unwrap_or(u32::MAX);
        }
    }
}

fn terminate_runtime_panes(panes: &HashMap<Uuid, RuntimePane>) {
    for terminal in panes.values().filter_map(RuntimePane::terminal) {
        let _ = terminal.session.terminate_and_wait();
    }
}

fn fallback_cwd() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| valid_local_cwd(path))
        .context("HOME does not name an accessible local directory")?;
    Ok(home)
}

fn local_spawn_dir(dir_override: Option<&str>) -> Result<PathBuf> {
    if let Some(dir) = dir_override {
        let path = Path::new(dir);
        if valid_local_cwd(path) {
            return Ok(path.to_path_buf());
        }
    }
    fallback_cwd()
}

fn valid_local_cwd(path: &Path) -> bool {
    path.is_absolute() && path.metadata().is_ok_and(|metadata| metadata.is_dir())
}

fn configured_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| shell.starts_with('/') && std::path::Path::new(shell).exists())
        .unwrap_or_else(|| "/bin/sh".to_owned())
}

fn local_shell_command(pane_id: Uuid, cwd: &Path) -> CommandBuilder {
    let mut command = command_with_terminal_env([OsString::from(configured_shell())], pane_id);
    command.cwd(cwd);
    command
}

fn system_ssh_command(
    pane_id: Uuid,
    host: &str,
    remote_dir: Option<&str>,
) -> Result<CommandBuilder> {
    system_ssh_command_with(system_ssh_binary()?, pane_id, host, remote_dir)
}

fn system_ssh_command_with(
    binary: impl AsRef<OsStr>,
    pane_id: Uuid,
    host: &str,
    remote_dir: Option<&str>,
) -> Result<CommandBuilder> {
    validate_ssh_host(host).map_err(|message| anyhow!(message))?;
    let mut argv = vec![binary.as_ref().to_owned()];
    if let Some(dir) = remote_dir {
        validate_workspace_dir(dir).map_err(|message| anyhow!(message))?;
        let quoted = format!("'{}'", dir.replace('\'', "'\\''"));
        let remote = OsString::from(format!("cd {quoted} 2>/dev/null; exec \"$SHELL\" -l"));
        argv.extend([
            OsString::from("-tt"),
            OsString::from("--"),
            OsString::from(host),
            remote,
        ]);
    } else {
        argv.extend([OsString::from("--"), OsString::from(host)]);
    }
    Ok(command_with_terminal_env(argv, pane_id))
}

fn plan_tmux_session_attachments(
    session_ids: &[TmuxSessionId],
    already_open: &HashSet<TmuxSessionId>,
    known_sessions: &HashMap<TmuxSessionId, TmuxSession>,
) -> Result<TmuxAttachmentPlan> {
    if session_ids.is_empty() {
        bail!("select at least one tmux session to open");
    }
    if session_ids.len() > MAX_TMUX_ATTACH_SESSIONS {
        bail!("select at most {MAX_TMUX_ATTACH_SESSIONS} tmux sessions at once");
    }
    let mut seen = HashSet::new();
    let mut plan = TmuxAttachmentPlan {
        launch: Vec::new(),
        skipped: Vec::new(),
    };
    for session_id in session_ids {
        let message = if !seen.insert(session_id.clone()) {
            Some("selected more than once")
        } else if already_open.contains(session_id) {
            Some("already open in this workstation")
        } else if !known_sessions.contains_key(session_id) {
            Some("session no longer exists")
        } else {
            None
        };
        if let Some(message) = message {
            plan.skipped.push(TmuxSessionAttachIssue {
                session_id: session_id.clone(),
                message: message.to_owned(),
            });
        } else if let Some(session) = known_sessions.get(session_id) {
            plan.launch.push(session.clone());
        }
    }
    Ok(plan)
}

/// Attaches exactly the way the user would by hand. This deliberately creates
/// no helper session and sets no option: every `set-option` reachable from a
/// directly attached session would persist on the user's own tmux server.
fn tmux_local_attach_command(pane_id: Uuid, session_id: &TmuxSessionId) -> CommandBuilder {
    command_with_terminal_env(
        [
            OsString::from("tmux"),
            OsString::from("attach-session"),
            OsString::from("-t"),
            OsString::from(session_id.as_str()),
        ],
        pane_id,
    )
}

/// Single-quoting is safe because `TmuxSessionId` is `$` + ASCII digits.
fn tmux_remote_attach_command(session_id: &TmuxSessionId) -> OsString {
    OsString::from(format!("exec tmux attach-session -t '{session_id}'"))
}

fn tmux_ssh_attach_command(
    pane_id: Uuid,
    host: &str,
    session_id: &TmuxSessionId,
) -> Result<CommandBuilder> {
    validate_ssh_host(host).map_err(|message| anyhow!(message))?;
    // OpenSSH does not allocate a remote PTY for a supplied command by
    // default. tmux attach requires one, while the metadata-only scan does
    // not, so force it only for this fixed attach path.
    Ok(command_with_terminal_env(
        [
            system_ssh_binary()?.into_os_string(),
            OsString::from("-tt"),
            OsString::from("--"),
            OsString::from(host),
            tmux_remote_attach_command(session_id),
        ],
        pane_id,
    ))
}

fn command_with_terminal_env(
    argv: impl IntoIterator<Item = OsString>,
    pane_id: Uuid,
) -> CommandBuilder {
    let mut command = CommandBuilder::from_argv(argv.into_iter().collect());
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let pane_id = pane_id.to_string();
    command.env(hh_protocol::pane_id_env(), pane_id);
    if let Some(home) = std::env::var_os("HOME") {
        command.cwd(home);
    }
    command
}

fn system_ssh_binary() -> Result<PathBuf> {
    for path in [Path::new("/usr/bin/ssh"), Path::new("/bin/ssh")] {
        if is_executable_file(path) {
            return Ok(path.to_path_buf());
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path).filter(|path| path.is_absolute()) {
            let candidate = directory.join("ssh");
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    bail!("installed system OpenSSH client was not found")
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[derive(Debug)]
struct BoundedCommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn system_tmux_binary() -> Result<PathBuf> {
    for path in [
        Path::new("/opt/homebrew/bin/tmux"),
        Path::new("/usr/local/bin/tmux"),
        Path::new("/usr/bin/tmux"),
    ] {
        let Ok(resolved) = std::fs::canonicalize(path) else {
            continue;
        };
        if is_trusted_executable_file(&resolved) {
            return Ok(resolved);
        }
    }
    bail!("trusted tmux executable was not found in a supported system location")
}

fn is_trusted_executable_file(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
            && metadata.permissions().mode() & 0o022 == 0
    })
}

fn tmux_local_probe_command() -> Result<Command> {
    let mut command = Command::new(system_tmux_binary()?);
    command
        .args(["list-sessions", "-F", TMUX_SESSION_LIST_FORMAT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn tmux_ssh_probe_command(destination: &str) -> Result<Command> {
    validate_ssh_host(destination).map_err(|message| anyhow!(message))?;
    let mut command = Command::new(system_ssh_binary()?);
    // This is intentionally a single fixed remote command, not an arbitrary
    // user string. With piped stdout, OpenSSH does not allocate a tty.
    // The host's tmux format expansion uses the SSH-propagated locale for
    // literal tab escapes. Pin a UTF-8 locale so the parser's fixed tab
    // delimiter cannot vary with how the desktop service was launched.
    command
        .arg("--")
        .arg(destination)
        .arg(TMUX_REMOTE_LIST_COMMAND)
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}
fn remote_directory_command(destination: &str, path: &str) -> Result<Command> {
    validate_ssh_host(destination).map_err(|message| anyhow!(message))?;
    validate_workspace_dir(path).map_err(|message| anyhow!(message))?;
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    let mut command = Command::new(system_ssh_binary()?);
    command
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "--"])
        .arg(destination)
        .arg(format!("ls -1p -- {quoted}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn run_tmux_probe(command: Command) -> Result<BoundedCommandOutput> {
    run_tmux_probe_with_timeout(command, TMUX_PROBE_TIMEOUT)
}

fn run_tmux_probe_with_timeout(
    command: Command,
    timeout: Duration,
) -> Result<BoundedCommandOutput> {
    run_bounded_command(command, timeout, "tmux scan")
}

fn run_bounded_command(
    mut command: Command,
    timeout: Duration,
    operation: &'static str,
) -> Result<BoundedCommandOutput> {
    let mut child = command
        .spawn()
        .with_context(|| format!("start {operation}"))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("{operation} stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .with_context(|| format!("{operation} stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || read_limited_command_output(stdout, operation));
    let stderr_reader = thread::spawn(move || read_limited_command_output(stderr, operation));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("observe {operation}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("{operation} timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = join_command_reader(stdout_reader, operation, "stdout")?;
    let stderr = join_command_reader(stderr_reader, operation, "stderr")?;
    Ok(BoundedCommandOutput {
        success: status.success(),
        stdout: String::from_utf8(stdout)
            .with_context(|| format!("{operation} output was not UTF-8"))?,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn read_limited_command_output(mut reader: impl Read, operation: &'static str) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut overflow = false;
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("read {operation} output"))?;
        if read == 0 {
            break;
        }
        if output.len().saturating_add(read) <= TMUX_PROBE_MAX_BYTES {
            output.extend_from_slice(&buffer[..read]);
        } else {
            overflow = true;
        }
    }
    if overflow {
        bail!("{operation} output exceeded {TMUX_PROBE_MAX_BYTES} bytes");
    }
    Ok(output)
}

fn join_command_reader(
    reader: thread::JoinHandle<Result<Vec<u8>>>,
    operation: &str,
    stream: &str,
) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow!("{operation} {stream} reader panicked"))?
}

fn parse_tmux_scan(output: &str) -> Result<Vec<TmuxSession>> {
    let mut sessions = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        match fields.next() {
            Some("S") => {
                if sessions.len() >= TMUX_PROBE_MAX_SESSIONS {
                    bail!("tmux scan returned more than {TMUX_PROBE_MAX_SESSIONS} sessions");
                }
                let (Some(id), Some(name), Some(window_count), Some(attached), None) = (
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                ) else {
                    bail!("tmux scan returned malformed metadata");
                };
                let id =
                    TmuxSessionId::try_from(id.to_owned()).map_err(|message| anyhow!(message))?;
                if name.is_empty()
                    || name.chars().count() > 80
                    || name.chars().any(char::is_control)
                {
                    bail!("tmux scan returned an unsafe session label");
                }
                let window_count = window_count
                    .parse::<u32>()
                    .context("tmux session window count was invalid")?;
                let attached_clients = attached
                    .parse::<u32>()
                    .context("tmux session attached-client count was invalid")?;
                if sessions
                    .iter()
                    .any(|session: &TmuxSession| session.id == id)
                {
                    bail!("tmux scan returned a duplicate session ID");
                }
                sessions.push(TmuxSession {
                    id,
                    name: name.to_owned(),
                    windows: window_count,
                    attached_clients,
                });
            }
            _ => bail!("tmux scan returned malformed metadata"),
        }
    }
    Ok(sessions)
}

fn tmux_reports_no_server(stderr: &str) -> bool {
    stderr.contains("no server running") || stderr.contains("error connecting to")
}

fn probe_error_summary(stderr: &str) -> String {
    let message = stderr.lines().next().unwrap_or("unknown error");
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect()
}

fn shell_title() -> String {
    std::path::Path::new(&configured_shell())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("shell")
        .to_owned()
}

fn first_pane_id(snapshot: &SessionSnapshot) -> Option<Uuid> {
    fn first(layout: &PaneLayout) -> Uuid {
        match layout {
            PaneLayout::Leaf { pane } => pane.id,
            PaneLayout::Stack { active, .. } => *active,
            PaneLayout::Split { first: layout, .. } => first(layout),
        }
    }
    snapshot
        .workspaces
        .first()
        .and_then(|workspace| workspace.tabs.first())
        .map(|tab| first(&tab.layout))
}

fn find_pane_mut_in_snapshot(snapshot: &mut SessionSnapshot, pane_id: Uuid) -> Option<&mut Pane> {
    snapshot
        .workspaces
        .iter_mut()
        .flat_map(|workspace| workspace.tabs.iter_mut())
        .find_map(|tab| find_pane_mut(&mut tab.layout, pane_id))
}

fn find_pane_in_snapshot(snapshot: &SessionSnapshot, pane_id: Uuid) -> Option<&Pane> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs.iter())
        .find_map(|tab| find_pane(&tab.layout, pane_id))
}

fn find_pane(layout: &PaneLayout, pane_id: Uuid) -> Option<&Pane> {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => Some(pane),
        PaneLayout::Leaf { .. } => None,
        PaneLayout::Stack { panes, .. } => panes.iter().find(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            find_pane(first, pane_id).or_else(|| find_pane(second, pane_id))
        }
    }
}

fn find_pane_mut(layout: &mut PaneLayout, pane_id: Uuid) -> Option<&mut Pane> {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => Some(pane),
        PaneLayout::Leaf { .. } => None,
        PaneLayout::Stack { panes, .. } => panes.iter_mut().find(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            find_pane_mut(first, pane_id).or_else(|| find_pane_mut(second, pane_id))
        }
    }
}

fn split_layout(layout: &mut PaneLayout, target: Uuid, pane: Pane, axis: SplitAxis) -> bool {
    match layout {
        PaneLayout::Leaf { pane: existing } if existing.id == target => {
            let first = layout.clone();
            *layout = PaneLayout::Split {
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(PaneLayout::Leaf { pane }),
            };
            true
        }
        PaneLayout::Stack { panes, .. } if panes.iter().any(|existing| existing.id == target) => {
            let first = layout.clone();
            *layout = PaneLayout::Split {
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(PaneLayout::Leaf { pane }),
            };
            true
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            split_layout(first, target, pane.clone(), axis)
                || split_layout(second, target, pane, axis)
        }
    }
}

fn add_tab(layout: &mut PaneLayout, target: Uuid, pane: Pane) -> bool {
    match layout {
        PaneLayout::Leaf { pane: existing } if existing.id == target => {
            let existing = existing.clone();
            let active = pane.id;
            *layout = PaneLayout::Stack {
                panes: vec![existing, pane],
                active,
            };
            true
        }
        PaneLayout::Stack { panes, active } if panes.iter().any(|pane| pane.id == target) => {
            *active = pane.id;
            panes.push(pane);
            true
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            add_tab(first, target, pane.clone()) || add_tab(second, target, pane)
        }
    }
}

fn activate_tab(layout: &mut PaneLayout, pane_id: Uuid) -> bool {
    match layout {
        PaneLayout::Leaf { pane } => pane.id == pane_id,
        PaneLayout::Stack { panes, active } if panes.iter().any(|pane| pane.id == pane_id) => {
            *active = pane_id;
            true
        }
        PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            activate_tab(first, pane_id) || activate_tab(second, pane_id)
        }
    }
}

fn layout_contains(layout: &PaneLayout, pane_id: Uuid) -> bool {
    match layout {
        PaneLayout::Leaf { pane } => pane.id == pane_id,
        PaneLayout::Stack { panes, .. } => panes.iter().any(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            layout_contains(first, pane_id) || layout_contains(second, pane_id)
        }
    }
}

fn first_layout_pane(layout: &PaneLayout) -> Uuid {
    match layout {
        PaneLayout::Leaf { pane } => pane.id,
        PaneLayout::Stack { active, .. } => *active,
        PaneLayout::Split { first, .. } => first_layout_pane(first),
    }
}

/// Removes runtime-only (tmux) panes from a layout that is about to be
/// persisted, collapsing the nodes they vacate. Returns false when nothing of
/// the layout survives, so the caller drops the tab entirely.
fn retain_persistable_panes(layout: &mut PaneLayout, dropped: &HashSet<Uuid>) -> bool {
    match layout {
        PaneLayout::Leaf { pane } => !dropped.contains(&pane.id),
        PaneLayout::Stack { panes, active } => {
            panes.retain(|pane| !dropped.contains(&pane.id));
            if panes.len() >= 2 {
                if !panes.iter().any(|pane| pane.id == *active) {
                    *active = panes[0].id;
                }
                return true;
            }
            let sole = panes.first().cloned();
            match sole {
                Some(pane) => {
                    *layout = PaneLayout::Leaf { pane };
                    true
                }
                None => false,
            }
        }
        PaneLayout::Split { first, second, .. } => {
            let keep_first = retain_persistable_panes(first, dropped);
            let keep_second = retain_persistable_panes(second, dropped);
            match (keep_first, keep_second) {
                (true, true) => true,
                (true, false) => {
                    let survivor = (**first).clone();
                    *layout = survivor;
                    true
                }
                (false, true) => {
                    let survivor = (**second).clone();
                    *layout = survivor;
                    true
                }
                (false, false) => false,
            }
        }
    }
}

fn workspace_id_for_pane(snapshot: &SessionSnapshot, pane_id: Uuid) -> Option<Uuid> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| {
            workspace
                .tabs
                .iter()
                .any(|tab| layout_contains(&tab.layout, pane_id))
        })
        .map(|workspace| workspace.id)
}

fn move_workspace_pane_to_split(
    workspace: &mut Workspace,
    source: Uuid,
    target: Uuid,
    placement: DropPlacement,
) -> bool {
    let Some(source_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, source))
    else {
        return false;
    };
    let Some(target_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, target))
    else {
        return false;
    };
    if source_tab == target_tab {
        return move_existing_pane_to_split(
            &mut workspace.tabs[source_tab].layout,
            source,
            target,
            placement,
        );
    }

    let (Some(pane), remaining) = detach_pane(workspace.tabs[source_tab].layout.clone(), source)
    else {
        return false;
    };
    let mut target_layout = workspace.tabs[target_tab].layout.clone();
    if !insert_split(&mut target_layout, target, pane, placement) {
        return false;
    }
    workspace.tabs[target_tab].layout = target_layout;
    if let Some(remaining) = remaining {
        workspace.tabs[source_tab].layout = remaining;
    } else {
        workspace.tabs.remove(source_tab);
    }
    true
}

fn move_workspace_pane_to_tab(workspace: &mut Workspace, source: Uuid, target: Uuid) -> bool {
    let Some(source_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, source))
    else {
        return false;
    };
    let Some(target_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, target))
    else {
        return false;
    };
    if source_tab == target_tab {
        return move_existing_pane_to_tab(&mut workspace.tabs[source_tab].layout, source, target);
    }

    let (Some(pane), remaining) = detach_pane(workspace.tabs[source_tab].layout.clone(), source)
    else {
        return false;
    };
    let mut target_layout = workspace.tabs[target_tab].layout.clone();
    if !add_tab(&mut target_layout, target, pane) {
        return false;
    }
    workspace.tabs[target_tab].layout = target_layout;
    if let Some(remaining) = remaining {
        workspace.tabs[source_tab].layout = remaining;
    } else {
        workspace.tabs.remove(source_tab);
    }
    true
}

fn move_existing_pane_to_split(
    layout: &mut PaneLayout,
    source: Uuid,
    target: Uuid,
    placement: DropPlacement,
) -> bool {
    if !layout_contains(layout, source) || !layout_contains(layout, target) || source == target {
        return false;
    }
    let original = layout.clone();
    let (pane, remaining) = detach_pane(original, source);
    let (Some(pane), Some(mut remaining)) = (pane, remaining) else {
        return false;
    };
    if !insert_split(&mut remaining, target, pane, placement) {
        return false;
    }
    *layout = remaining;
    true
}

fn move_existing_pane_to_tab(layout: &mut PaneLayout, source: Uuid, target: Uuid) -> bool {
    if !layout_contains(layout, source) || !layout_contains(layout, target) || source == target {
        return false;
    }
    let original = layout.clone();
    let (pane, remaining) = detach_pane(original, source);
    let (Some(pane), Some(mut remaining)) = (pane, remaining) else {
        return false;
    };
    if !add_tab(&mut remaining, target, pane) {
        return false;
    }
    *layout = remaining;
    true
}

fn split_lone_layout_with_replacement(
    layout: &mut PaneLayout,
    pane_id: Uuid,
    replacement: Pane,
    placement: DropPlacement,
) -> bool {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => {
            let moved = layout.clone();
            let replacement = PaneLayout::Leaf { pane: replacement };
            let (axis, moved_first) = match placement {
                DropPlacement::Left => (SplitAxis::Horizontal, true),
                DropPlacement::Right => (SplitAxis::Horizontal, false),
                DropPlacement::Top => (SplitAxis::Vertical, true),
                DropPlacement::Bottom => (SplitAxis::Vertical, false),
            };
            let (first, second) = if moved_first {
                (moved, replacement)
            } else {
                (replacement, moved)
            };
            *layout = PaneLayout::Split {
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            };
            true
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            split_lone_layout_with_replacement(first, pane_id, replacement.clone(), placement)
                || split_lone_layout_with_replacement(second, pane_id, replacement, placement)
        }
    }
}

fn detach_pane(layout: PaneLayout, source: Uuid) -> (Option<Pane>, Option<PaneLayout>) {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == source => (Some(pane), None),
        PaneLayout::Leaf { pane } => (None, Some(PaneLayout::Leaf { pane })),
        PaneLayout::Stack { mut panes, active } => {
            let Some(index) = panes.iter().position(|pane| pane.id == source) else {
                return (None, Some(PaneLayout::Stack { panes, active }));
            };
            let pane = panes.remove(index);
            let remaining = match panes.len() {
                0 => None,
                1 => Some(PaneLayout::Leaf {
                    pane: panes.remove(0),
                }),
                _ => {
                    let active = if active == source {
                        panes[0].id
                    } else {
                        active
                    };
                    Some(PaneLayout::Stack { panes, active })
                }
            };
            (Some(pane), remaining)
        }
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (pane, first_remaining) = detach_pane(*first, source);
            if pane.is_some() {
                let layout = match first_remaining {
                    Some(first) => PaneLayout::Split {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second,
                    },
                    None => *second,
                };
                return (pane, Some(layout));
            }
            let (pane, second_remaining) = detach_pane(*second, source);
            if pane.is_some() {
                let first = first_remaining.expect("unchanged first");
                let layout = match second_remaining {
                    Some(second) => PaneLayout::Split {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    },
                    None => first,
                };
                (pane, Some(layout))
            } else {
                (
                    None,
                    Some(PaneLayout::Split {
                        axis,
                        ratio,
                        first: Box::new(first_remaining.expect("unchanged first")),
                        second: Box::new(second_remaining.expect("unchanged second")),
                    }),
                )
            }
        }
    }
}

fn insert_split(
    layout: &mut PaneLayout,
    target: Uuid,
    pane: Pane,
    placement: DropPlacement,
) -> bool {
    let is_target = match layout {
        PaneLayout::Leaf { pane } => pane.id == target,
        PaneLayout::Stack { panes, .. } => panes.iter().any(|pane| pane.id == target),
        PaneLayout::Split { .. } => false,
    };
    if is_target {
        let existing = layout.clone();
        let incoming = PaneLayout::Leaf { pane };
        let (axis, incoming_first) = match placement {
            DropPlacement::Left => (SplitAxis::Horizontal, true),
            DropPlacement::Right => (SplitAxis::Horizontal, false),
            DropPlacement::Top => (SplitAxis::Vertical, true),
            DropPlacement::Bottom => (SplitAxis::Vertical, false),
        };
        let (first, second) = if incoming_first {
            (incoming, existing)
        } else {
            (existing, incoming)
        };
        *layout = PaneLayout::Split {
            axis,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        };
        return true;
    }
    match layout {
        PaneLayout::Split { first, second, .. } => {
            insert_split(first, target, pane.clone(), placement)
                || insert_split(second, target, pane, placement)
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
    }
}

fn swap_pane_ids(layout: &mut PaneLayout, source: Uuid, target: Uuid) {
    let swap_id = |id: &mut Uuid| {
        if *id == source {
            *id = target;
        } else if *id == target {
            *id = source;
        }
    };
    match layout {
        PaneLayout::Leaf { pane } => swap_id(&mut pane.id),
        PaneLayout::Stack { panes, active } => {
            for pane in panes {
                swap_id(&mut pane.id);
            }
            swap_id(active);
        }
        PaneLayout::Split { first, second, .. } => {
            swap_pane_ids(first, source, target);
            swap_pane_ids(second, source, target);
        }
    }
}

pub async fn serve_connection(mut stream: UnixStream, sessions: &SessionRegistry) -> Result<()> {
    let peer_uid = stream
        .peer_cred()
        .context("read client peer credentials")?
        .uid();
    let effective_uid = rustix::process::geteuid().as_raw();
    if peer_uid != effective_uid {
        bail!("reject client UID {peer_uid}; service UID is {effective_uid}");
    }
    let hello: ClientRequest = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_message(&mut stream))
        .await
        .context("protocol hello timed out")?
        .context("read protocol hello")?;
    match hello {
        ClientRequest::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {
            write_message(
                &mut stream,
                &ServiceResponse::Hello {
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .await
            .context("write protocol hello")?;
        }
        ClientRequest::Hello { protocol_version } => {
            write_message(
                &mut stream,
                &ServiceResponse::Error {
                    message: format!(
                        "protocol mismatch: client {protocol_version}, service {PROTOCOL_VERSION}"
                    ),
                },
            )
            .await?;
            bail!("protocol mismatch");
        }
        _ => bail!("client must begin with hello"),
    }

    loop {
        let request =
            match tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, read_message(&mut stream)).await {
                Ok(Ok(request)) => request,
                Ok(Err(hh_protocol::WireError::Closed)) => return Ok(()),
                Ok(Err(error)) => return Err(error).context("read client request"),
                Err(_) => bail!("client connection idle timeout"),
            };
        let one_way = request_is_one_way(&request);

        let sessions = sessions.clone();
        let response = tokio::task::spawn_blocking(move || handle_request(&sessions, request))
            .await
            .context("join blocking request handler")?
            .unwrap_or_else(|error| ServiceResponse::Error {
                message: format!("{error:#}"),
            });
        if one_way {
            continue;
        }
        tokio::time::timeout(
            RESPONSE_WRITE_TIMEOUT,
            write_message(&mut stream, &response),
        )
        .await
        .context("write service response timed out")?
        .context("write service response")?;
    }
}

/// Terminal input and selection updates are one-way: the desktop never waits
/// for them, and a queued response nobody reads would eventually block this
/// connection's writer.
fn request_is_one_way(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::WriteInput { .. } | ClientRequest::UpdateSelection { .. }
    )
}

#[allow(clippy::too_many_lines)]
fn handle_request(sessions: &SessionRegistry, request: ClientRequest) -> Result<ServiceResponse> {
    match request {
        ClientRequest::Hello { .. } => Ok(ServiceResponse::Error {
            message: "hello was already completed".to_owned(),
        }),
        ClientRequest::GetSnapshot => Ok(ServiceResponse::Snapshot {
            snapshot: sessions.snapshot()?,
        }),
        ClientRequest::GetUpdates {
            snapshot_revision,
            pane_revisions,
            subscribed_panes,
            notifications_after,
        } => handle_get_updates(
            sessions,
            snapshot_revision,
            &pane_revisions,
            &subscribed_panes,
            notifications_after,
        ),
        ClientRequest::GetNotifications => Ok(ServiceResponse::Notifications {
            items: sessions.notifications()?,
        }),
        ClientRequest::MarkNotificationsRead { ids } => {
            sessions.mark_notifications_read(&ids);
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ClearNotifications => {
            sessions.clear_notifications();
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::GetPaneSnapshot { pane_id } => handle_get_pane_snapshot(sessions, pane_id),
        ClientRequest::CreatePane { target_pane, axis } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_pane(target_pane, axis)?,
        }),
        ClientRequest::CreateGroupTerminal { target_pane } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_group_terminal(target_pane)?,
        }),
        ClientRequest::CreateWorkspaceTerminal { workspace_id } => {
            Ok(ServiceResponse::PaneCreated {
                pane_id: sessions.create_workspace_terminal(workspace_id)?,
            })
        }
        ClientRequest::CreateWorkspaceTab { workspace_id } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_workspace_tab(workspace_id)?,
        }),
        ClientRequest::CreateBrowserTab { workspace_id, url } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_browser_tab(workspace_id, url.as_deref())?,
        }),
        ClientRequest::CreateGroupBrowser { target_pane, url } => {
            Ok(ServiceResponse::PaneCreated {
                pane_id: sessions.create_group_browser(target_pane, url.as_deref())?,
            })
        }
        ClientRequest::CreateWorkspaceGroup {
            workspace_id,
            parent_tab,
        } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_workspace_group(workspace_id, parent_tab)?,
        }),
        ClientRequest::ConnectSsh { target_pane, host } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.connect_ssh(target_pane, &host)?,
        }),
        ClientRequest::ScanTmuxSessions { workspace_id } => {
            let scan = sessions.scan_tmux_sessions(workspace_id)?;
            Ok(ServiceResponse::TmuxSessions {
                scope: scan.scope,
                sessions: scan.sessions,
                open_session_ids: scan.open_session_ids,
                no_server: scan.no_server,
            })
        }
        ClientRequest::ListRemoteDirectory { workspace_id, path } => {
            let entries = sessions.list_remote_directory(workspace_id, &path)?;
            Ok(ServiceResponse::RemoteDirectory { path, entries })
        }
        ClientRequest::AttachTmuxSessions {
            workspace_id,
            session_ids,
        } => {
            let result = sessions.attach_tmux_sessions(workspace_id, &session_ids)?;
            Ok(ServiceResponse::TmuxSessionsAttached {
                pane_ids: result.pane_ids,
                skipped: result.skipped,
            })
        }
        ClientRequest::ActivateTab { pane_id } => {
            sessions.activate_tab(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SwapPanes {
            source_pane,
            target_pane,
        } => {
            sessions.swap_panes(source_pane, target_pane)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToSplit {
            source_pane,
            target_pane,
            placement,
        } => {
            sessions.move_pane_to_split(source_pane, target_pane, placement)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToTab {
            source_pane,
            target_pane,
        } => {
            sessions.move_pane_to_tab(source_pane, target_pane)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToGroup {
            source_pane,
            target_tab,
        } => {
            sessions.move_pane_to_group(source_pane, target_tab)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToNewTab {
            source_pane,
            target_tab,
            after,
            parent_tab,
        } => {
            sessions.move_pane_to_new_tab(source_pane, target_tab, after, parent_tab)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::RenamePane { pane_id, title } => {
            sessions.rename_pane(pane_id, &title)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::RenameTab { tab_id, title } => {
            sessions.rename_tab(tab_id, &title)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetPaneProfile { pane_id, profile } => {
            sessions.set_pane_profile(pane_id, profile)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetPaneCustomIcon { pane_id, icon } => {
            sessions.set_pane_custom_icon(pane_id, icon)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetTabCustomIcon { tab_id, icon } => {
            sessions.set_tab_custom_icon(tab_id, icon)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ResetPaneIdentity { pane_id } => {
            sessions.reset_pane_identity(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ClosePane { pane_id } => {
            sessions.close_pane(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CloseTab { tab_id } => {
            sessions.close_tab(tab_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReattachPane { pane_id } => {
            sessions.reattach_pane(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetBrowserState {
            pane_id,
            url,
            title,
        } => {
            sessions.set_browser_state(pane_id, &url, title.as_deref())?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetDefaultTerminalAccent { color } => {
            sessions.set_default_terminal_accent(color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetDefaultWorkspaceColor { color } => {
            sessions.set_default_workspace_color(color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetPaneColor { pane_id, color } => {
            sessions.set_pane_color(pane_id, color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetTabColor { tab_id, color } => {
            sessions.set_tab_color(tab_id, color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetWorkspaceColor {
            workspace_id,
            color,
        } => {
            sessions.set_workspace_color(workspace_id, color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetWorkspaceWorkingDir {
            workspace_id,
            working_dir,
        } => {
            sessions.set_workspace_working_dir(workspace_id, working_dir)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateWorkspaceProject {
            workspace_id,
            working_dir,
            title,
        } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_workspace_project(
                workspace_id,
                &working_dir,
                title.as_deref(),
            )?,
        }),
        ClientRequest::SetTabWorkingDir {
            tab_id,
            working_dir,
        } => {
            sessions.set_tab_working_dir(tab_id, working_dir)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateWorkspace { title } => {
            let (workspace_id, pane_id) = sessions.create_workspace(title.as_deref())?;
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            })
        }
        ClientRequest::CreateSshWorkspace { title, destination } => {
            let (workspace_id, pane_id) =
                sessions.create_ssh_workspace(title.as_deref(), &destination)?;
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            })
        }
        ClientRequest::RenameWorkspace {
            workspace_id,
            title,
        } => {
            sessions.rename_workspace(workspace_id, &title)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetWorkspacePinned {
            workspace_id,
            pinned,
        } => {
            sessions.set_workspace_pinned(workspace_id, pinned)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePinnedWorkspace {
            workspace_id,
            direction,
        } => {
            sessions.move_pinned_workspace(workspace_id, direction)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReorderWorkspace {
            workspace_id,
            target_workspace_id,
            after,
        } => {
            sessions.reorder_workspace(workspace_id, target_workspace_id, after)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReorderTab {
            tab_id,
            target_tab_id,
            after,
        } => {
            sessions.reorder_tab(tab_id, target_tab_id, after)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MoveTabToProject {
            tab_id,
            project_tab,
        } => {
            sessions.move_tab_to_project(tab_id, project_tab)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetTabPinned { tab_id, pinned } => {
            sessions.set_tab_pinned(tab_id, pinned)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::DisconnectWorkspace { workspace_id } => {
            sessions.disconnect_workspace(workspace_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReconnectWorkspace { workspace_id } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.reconnect_workspace(workspace_id)?,
        }),
        ClientRequest::DeleteWorkspace { workspace_id } => {
            sessions.delete_workspace(workspace_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::WriteInput { pane_id, bytes } => {
            sessions.write_input(pane_id, &bytes)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::BeginSelection {
            pane_id,
            point,
            kind,
        } => {
            sessions.begin_selection(pane_id, point, kind)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::UpdateSelection { pane_id, point } => {
            sessions.update_selection(pane_id, point)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ClearSelection { pane_id } => {
            sessions.clear_selection(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CopySelection { pane_id } => Ok(ServiceResponse::SelectionText {
            text: sessions.selected_text(pane_id)?,
        }),
        ClientRequest::ScrollPane { pane_id, lines } => {
            sessions.scroll_pane(pane_id, lines)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SearchPane {
            pane_id,
            query,
            forward,
        } => Ok(ServiceResponse::SearchResult {
            found: sessions.search_pane(pane_id, &query, forward)?,
        }),
        ClientRequest::MouseInput {
            pane_id,
            point,
            button,
            action,
            modifiers,
        } => {
            sessions.mouse_input(pane_id, point, button, action, modifiers)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ResizePane {
            pane_id,
            columns,
            rows,
        } => {
            sessions.resize_pane(pane_id, columns, rows)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::GetHistoryStatus => Ok(ServiceResponse::HistoryStatus {
            status: sessions.history_status(),
        }),
        ClientRequest::SetHistorySettings { settings } => {
            sessions.set_history_settings(settings)?;
            Ok(ServiceResponse::HistoryStatus {
                status: sessions.history_status(),
            })
        }
        ClientRequest::ClearHistory { scope } => {
            sessions.clear_history(scope)?;
            Ok(ServiceResponse::HistoryStatus {
                status: sessions.history_status(),
            })
        }
        ClientRequest::LoadHistoryPage {
            pane_id,
            cursor,
            direction,
        } => Ok(ServiceResponse::HistoryPage {
            page: sessions.load_history_page(pane_id, cursor, direction)?,
        }),
        ClientRequest::SearchArchivedHistory {
            pane_id,
            query,
            before,
        } => Ok(ServiceResponse::HistorySearchResult {
            page: sessions.search_archived_history(pane_id, &query, before)?,
        }),
    }
}

fn handle_get_updates(
    sessions: &SessionRegistry,
    snapshot_revision: Option<u64>,
    pane_revisions: &[PaneRevisionCursor],
    subscribed_panes: &[Uuid],
    notifications_after: u64,
) -> Result<ServiceResponse> {
    let update = sessions.pane_updates(
        snapshot_revision,
        pane_revisions,
        subscribed_panes,
        false,
        notifications_after,
    )?;
    Ok(ServiceResponse::Updates {
        session_revision: update.session_revision,
        snapshot: update.snapshot,
        screens: update.screens,
        pane_states: update.pane_states,
        notifications: update.notifications,
        diagnostics: update.diagnostics,
    })
}

fn handle_get_pane_snapshot(sessions: &SessionRegistry, pane_id: Uuid) -> Result<ServiceResponse> {
    let (screen, diagnostics) = sessions.pane_snapshot(pane_id)?;
    Ok(ServiceResponse::PaneSnapshot {
        screen,
        diagnostics,
    })
}

async fn write_message<T: Serialize>(
    stream: &mut UnixStream,
    message: &T,
) -> Result<(), hh_protocol::WireError> {
    let frame = hh_protocol::encode_frame(message)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_message<T: DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, hh_protocol::WireError> {
    let mut length = [0_u8; 4];
    if let Err(error) = stream.read_exact(&mut length).await {
        return if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Err(hh_protocol::WireError::Closed)
        } else {
            Err(hh_protocol::WireError::Io(error))
        };
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(hh_protocol::WireError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    tokio::time::timeout(REQUEST_BODY_TIMEOUT, stream.read_exact(&mut payload))
        .await
        .map_err(|_| {
            hh_protocol::WireError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request body timed out",
            ))
        })??;
    hh_protocol::decode_frame(&payload)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn local_terminal_command_remains_the_configured_shell_without_arguments() {
        let pane_id = Uuid::nil();
        let cwd = fallback_cwd().unwrap();
        let command = local_shell_command(pane_id, &cwd);

        assert_eq!(
            command.get_argv(),
            &[OsString::from(configured_shell())],
            "the SSH track must not wrap or alter local shell startup"
        );
    }

    #[test]
    fn ssh_command_uses_structured_argv_without_security_overrides() {
        let command =
            system_ssh_command_with("/usr/bin/ssh", Uuid::nil(), "admin@prod-east", None).unwrap();

        assert_eq!(
            command.get_argv(),
            &[
                OsString::from("/usr/bin/ssh"),
                OsString::from("--"),
                OsString::from("admin@prod-east"),
            ]
        );
    }

    #[test]
    fn ssh_command_quotes_configured_remote_directory() {
        let command = system_ssh_command_with(
            "/usr/bin/ssh",
            Uuid::nil(),
            "admin@prod-east",
            Some("/srv/app d'ir"),
        )
        .unwrap();

        assert_eq!(
            command.get_argv(),
            &[
                OsString::from("/usr/bin/ssh"),
                OsString::from("-tt"),
                OsString::from("--"),
                OsString::from("admin@prod-east"),
                OsString::from("cd '/srv/app d'\\''ir' 2>/dev/null; exec \"$SHELL\" -l"),
            ]
        );
    }

    #[test]
    fn ssh_test_seam_honors_workspace_directory() {
        let directory = std::env::temp_dir().join(format!("hh-ssh-working-dir-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        TEST_LOCAL_SSH_SEAM_ENABLED.store(true, Ordering::Relaxed);

        let registry = SessionRegistry::new().unwrap();
        let (workspace_id, _) = registry
            .create_ssh_workspace(Some("SSH"), "admin@test-host")
            .unwrap();
        registry
            .set_workspace_working_dir(workspace_id, Some(directory.to_string_lossy().into_owned()))
            .unwrap();
        let pane_id = registry.create_workspace_tab(workspace_id).unwrap();
        let state = registry.state.read();
        assert_eq!(
            state.terminal_pane(pane_id).unwrap().last_valid_cwd,
            directory
        );
        drop(state);

        TEST_LOCAL_SSH_SEAM_ENABLED.store(false, Ordering::Relaxed);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_ssh_destinations_are_rejected_before_command_construction() {
        for host in [
            "-oProxyCommand=bad",
            "user@@host",
            "host command",
            "host\n-A",
        ] {
            assert!(
                system_ssh_command_with("/usr/bin/ssh", Uuid::nil(), host, None).is_err(),
                "host: {host:?}"
            );
        }
    }

    #[test]
    fn tmux_attach_uses_only_fixed_structured_commands() {
        let target = TmuxSessionId::try_from("$42".to_owned()).unwrap();
        let pane_id = Uuid::nil();
        let local = tmux_local_attach_command(pane_id, &target);
        assert_eq!(
            local.get_argv(),
            &[
                OsString::from("tmux"),
                OsString::from("attach-session"),
                OsString::from("-t"),
                OsString::from("$42"),
            ]
        );
        // Attaching must never mutate the user's own tmux server: no option is
        // set and no session is created, only a plain attach.
        for mutation in [
            "set-option",
            "new-session",
            "window-size",
            "aggressive-resize",
        ] {
            assert!(
                !local.get_argv().contains(&OsString::from(mutation)),
                "local attach mutates the user's tmux server: {mutation}"
            );
        }

        let expected_remote = "exec tmux attach-session -t '$42'";
        let remote = tmux_remote_attach_command(&target);
        assert_eq!(remote, OsString::from(expected_remote));
        let remote_ssh = tmux_ssh_attach_command(pane_id, "admin@build-node", &target).unwrap();
        assert_eq!(
            remote_ssh.get_argv(),
            &[
                system_ssh_binary().unwrap().into_os_string(),
                OsString::from("-tt"),
                OsString::from("--"),
                OsString::from("admin@build-node"),
                OsString::from(expected_remote),
            ]
        );
        for target in ["name", "$42;bad", "$4 2", "$-1", "42", "$42'bad"] {
            assert!(
                TmuxSessionId::try_from(target.to_owned()).is_err(),
                "target: {target:?}"
            );
        }
    }

    fn tmux_session(id: &str, name: &str) -> TmuxSession {
        TmuxSession {
            id: TmuxSessionId::try_from(id.to_owned()).unwrap(),
            name: name.to_owned(),
            windows: 1,
            attached_clients: 0,
        }
    }

    fn pane_fixture(id: Uuid) -> Pane {
        Pane {
            kind: PaneKind::Terminal,
            id,
            title: format!("Terminal {id}"),
            shell: "shell".to_owned(),
            color: None,
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        }
    }

    #[test]
    fn persistable_pane_pruning_collapses_invalid_layout_shapes() {
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let third_id = Uuid::new_v4();

        let mut leaf = PaneLayout::Leaf {
            pane: pane_fixture(first_id),
        };
        assert!(!retain_persistable_panes(
            &mut leaf,
            &HashSet::from([first_id])
        ));

        let second = pane_fixture(second_id);
        let mut two_pane_stack = PaneLayout::Stack {
            panes: vec![pane_fixture(first_id), second.clone()],
            active: first_id,
        };
        assert!(retain_persistable_panes(
            &mut two_pane_stack,
            &HashSet::from([first_id])
        ));
        assert_eq!(two_pane_stack, PaneLayout::Leaf { pane: second });

        let first = pane_fixture(first_id);
        let third = pane_fixture(third_id);
        let mut three_pane_stack = PaneLayout::Stack {
            panes: vec![first.clone(), pane_fixture(second_id), third.clone()],
            active: second_id,
        };
        assert!(retain_persistable_panes(
            &mut three_pane_stack,
            &HashSet::from([second_id])
        ));
        assert_eq!(
            three_pane_stack,
            PaneLayout::Stack {
                panes: vec![first.clone(), third.clone()],
                active: first_id,
            }
        );

        let mut one_sided_split = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf {
                pane: pane_fixture(second_id),
            }),
            second: Box::new(PaneLayout::Leaf {
                pane: third.clone(),
            }),
        };
        assert!(retain_persistable_panes(
            &mut one_sided_split,
            &HashSet::from([second_id])
        ));
        assert_eq!(one_sided_split, PaneLayout::Leaf { pane: third });

        let mut empty_split = PaneLayout::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf { pane: first }),
            second: Box::new(PaneLayout::Leaf {
                pane: pane_fixture(second_id),
            }),
        };
        assert!(!retain_persistable_panes(
            &mut empty_split,
            &HashSet::from([first_id, second_id])
        ));
    }

    #[test]
    fn tmux_attachment_plan_opens_each_unique_selection_and_skips_invalid_targets() {
        let first = tmux_session("$1", "editor");
        let second = tmux_session("$2", "server");
        let missing = TmuxSessionId::try_from("$3".to_owned()).unwrap();
        let already_open = HashSet::from([second.id.clone()]);
        let known_sessions = HashMap::from([
            (first.id.clone(), first.clone()),
            (second.id.clone(), second.clone()),
        ]);
        let plan = plan_tmux_session_attachments(
            &[
                first.id.clone(),
                second.id.clone(),
                first.id.clone(),
                missing.clone(),
            ],
            &already_open,
            &known_sessions,
        )
        .unwrap();
        assert_eq!(plan.launch, vec![first.clone()]);
        assert_eq!(
            plan.skipped,
            vec![
                TmuxSessionAttachIssue {
                    session_id: second.id,
                    message: "already open in this workstation".to_owned(),
                },
                TmuxSessionAttachIssue {
                    session_id: first.id,
                    message: "selected more than once".to_owned(),
                },
                TmuxSessionAttachIssue {
                    session_id: missing,
                    message: "session no longer exists".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn live_tmux_runtime_tab_is_registered_and_selectable() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("printf fixture; sleep 1"),
            ]),
            "live tmux fixture",
            &registry.history,
        )
        .unwrap();
        let tmux_session = tmux_session("$9", "editor");

        let attached = registry
            .register_live_tmux_tab(
                workspace_id,
                pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap();
        assert_eq!(attached, pane_id);
        registry.activate_tab(pane_id).unwrap();
        assert!(registry.pane_snapshot(pane_id).is_ok());
        let snapshot = registry.snapshot().unwrap();
        assert!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .any(|tab| layout_contains(&tab.layout, pane_id))
        );
    }

    #[test]
    fn a_plain_terminal_added_to_a_tmux_tab_survives_restart_without_the_tmux_pane() {
        let directory = std::env::temp_dir().join(format!("hh-tmux-group-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let tmux_pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            tmux_pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("printf fixture; sleep 1"),
            ]),
            "live tmux fixture",
            &registry.history,
        )
        .unwrap();
        let tmux_session = tmux_session("$11", "persisted-group");
        registry
            .register_live_tmux_tab(
                workspace_id,
                tmux_pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap();

        let plain_pane_id = registry.create_group_terminal(tmux_pane_id).unwrap();
        let live_snapshot = registry.snapshot().unwrap();
        let live_tab = live_snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, tmux_pane_id))
            .expect("tmux tab remains present while attached");
        assert!(matches!(
            &live_tab.layout,
            PaneLayout::Stack { panes, .. }
                if panes.iter().any(|pane| pane.id == tmux_pane_id)
                    && panes.iter().any(|pane| pane.id == plain_pane_id)
        ));

        drop(session);
        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let recovered_snapshot = recovered.snapshot().unwrap();
        assert!(recovered_snapshot.workspaces[0].tabs.iter().any(|tab| {
            matches!(
                &tab.layout,
                PaneLayout::Leaf { pane } if pane.id == plain_pane_id
            )
        }));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn immediate_tmux_attach_exit_never_registers_a_placeholder_tab() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![OsString::from("/usr/bin/false")]),
            "failed tmux fixture",
            &registry.history,
        )
        .unwrap();
        let tmux_session = tmux_session("$10", "logs");

        let error = registry
            .register_live_tmux_tab(
                workspace_id,
                pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("before the terminal became live")
        );
        assert!(registry.pane_snapshot(pane_id).is_err());
    }

    #[test]
    fn tmux_scan_parser_bounds_and_rejects_malicious_metadata() {
        let sessions = parse_tmux_scan("S\t$1\tbuild\t2\t1\nS\t$2\tresearch\t1\t0\n").unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id.as_str(), "$1");
        assert_eq!(sessions[0].windows, 2);
        assert_eq!(sessions[1].name, "research");
        assert_eq!(sessions[1].attached_clients, 0);

        for output in [
            "build\tname\t1\t0\n",
            "S\t$1\tname\t1\t0\textra\n",
            "S\t$1\tbad\u{0007}name\t1\t0\n",
            "S\t$1;bad\tname\t1\t0\n",
            "S\t$1\tname\tnot-a-number\t0\n",
            "S\t$1\tname\t1\t0\nS\t$1\tother\t1\t0\n",
            "S\t$1\tname\t1\t0\nW\t$1\t@1\t0\teditor\t1\t1\n",
        ] {
            assert!(parse_tmux_scan(output).is_err(), "output: {output:?}");
        }
        assert!(tmux_reports_no_server("no server running on /tmp/tmux"));
        assert!(tmux_reports_no_server(
            "error connecting to /tmp/tmux (No such file or directory)"
        ));
    }

    #[test]
    fn remote_tmux_probe_is_a_fixed_command_not_a_user_command() {
        let command = tmux_ssh_probe_command("admin@build-node").unwrap();
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(args[0], OsStr::new("--"));
        assert_eq!(args[1], OsStr::new("admin@build-node"));
        assert_eq!(args[2], OsStr::new(TMUX_REMOTE_LIST_COMMAND));
        assert!(command.get_envs().any(
            |(key, value)| key == OsStr::new("LC_ALL") && value == Some(OsStr::new("C.UTF-8"))
        ));
        assert!(tmux_ssh_probe_command("build;whoami").is_err());
    }

    #[test]
    fn tmux_probe_timeout_is_bounded_and_reports_an_error() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let error = run_tmux_probe_with_timeout(command, Duration::from_millis(20)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn extra_panes_in_saved_ssh_workstations_retain_the_saved_destination() {
        let destination = "admin@build-node";
        let ssh = WorkspaceConnection::SystemSsh {
            destination: destination.to_owned(),
            status: WorkspaceConnectionStatus::Connected,
        };
        assert_eq!(
            runtime_kind_for_workspace(&ssh),
            RuntimePaneKind::SystemSsh {
                host: destination.to_owned(),
            }
        );
        assert_eq!(
            runtime_kind_for_workspace(&WorkspaceConnection::Local),
            RuntimePaneKind::Local
        );
    }

    #[test]
    fn group_names_are_validated_and_survive_restart() {
        let directory = std::env::temp_dir().join(format!("hh-group-name-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;

        let pane_id = registry.create_workspace_group(workspace_id, None).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let tab = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, pane_id))
            .expect("new group owns the returned pane");
        let tab_id = tab.id;
        assert_eq!(tab.custom_title.as_deref(), Some("Group 1"));

        registry.rename_tab(tab_id, "  Design bank  ").unwrap();
        assert!(registry.rename_tab(tab_id, "").is_err());
        assert!(registry.rename_tab(tab_id, &"x".repeat(81)).is_err());
        let renamed = registry.snapshot().unwrap();
        let tab = renamed.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .expect("renamed group remains present");
        assert_eq!(tab.custom_title.as_deref(), Some("Design bank"));

        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = recovered.snapshot().unwrap();
        let tab = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .expect("renamed group survives restart");
        assert_eq!(tab.custom_title.as_deref(), Some("Design bank"));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stable_ssh_workstation_creation_is_delivered_to_the_rail_and_survives_restart() {
        let directory =
            std::env::temp_dir().join(format!("hh-ssh-workstation-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("sessions.json");

        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let before = registry.snapshot().unwrap();
        let (workspace_id, _) = registry
            .create_simulated_ssh_workspace(Some("Safe local simulation"), "test@local-host")
            .unwrap();

        let update = registry
            .pane_updates(Some(before.revision), &[], &[], true, 0)
            .unwrap();
        let delivered = update
            .snapshot
            .expect("new workstation snapshot is delivered");
        assert!(
            delivered
                .workspaces
                .iter()
                .any(|workspace| workspace.id == workspace_id)
        );
        let created = delivered
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .unwrap();
        assert_eq!(created.title, "Safe local simulation");
        assert!(matches!(
            created.connection,
            WorkspaceConnection::SystemSsh {
                ref destination,
                status: WorkspaceConnectionStatus::Connected,
            } if destination == "test@local-host"
        ));

        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = recovered.snapshot().unwrap();
        let saved = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .expect("saved SSH workstation remains after restart");
        assert_eq!(saved.title, "Safe local simulation");
        assert!(matches!(
            saved.connection,
            WorkspaceConnection::SystemSsh {
                ref destination,
                status: WorkspaceConnectionStatus::Offline,
            } if destination == "test@local-host"
        ));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirmed_ssh_workstation_is_durable_before_session_attachment() {
        let directory =
            std::env::temp_dir().join(format!("hh-ssh-workstation-intent-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("sessions.json");
        let ids = SshWorkspaceIds {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        };

        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        registry
            .persist_ssh_workspace_intent(
                Some("Durable before connection".to_owned()),
                "test@local-host",
                ids,
            )
            .unwrap();
        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = recovered.snapshot().unwrap();
        let saved = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == ids.workspace)
            .expect("confirmed SSH workstation remains after a restart");
        assert_eq!(saved.title, "Durable before connection");
        assert_eq!(saved.active_terminal_count, 0);
        assert!(matches!(
            saved.connection,
            WorkspaceConnection::SystemSsh {
                ref destination,
                status: WorkspaceConnectionStatus::Offline,
            } if destination == "test@local-host"
        ));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejected_ssh_intent_does_not_create_or_replace_a_terminal() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();

        assert!(registry.connect_ssh(first, "-A").is_err());

        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert_eq!(registry.state().unwrap().1.len(), 1);
    }

    #[test]
    fn custom_name_and_selected_profile_resolve_independently() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let pane = find_pane_mut_in_snapshot(&mut snapshot, pane_id).unwrap();
        pane.custom_title = Some("Release console".to_owned());
        pane.profile_override = Some(TerminalProfile::Hermes);

        resolve_pane_identity(pane, Some("Claude Code"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Release console");
        assert_eq!(pane.identity.profile, TerminalProfile::Hermes);
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserProfile);

        pane.custom_title = None;
        resolve_pane_identity(pane, Some("Claude Code"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Hermes Agent");
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserProfile);

        pane.profile_override = None;
        resolve_pane_identity(pane, Some("Claude Code"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Codex CLI");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Command);

        resolve_pane_identity(pane, Some("editor"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Codex CLI");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Command);

        resolve_pane_identity(pane, Some("editor"), None);
        assert_eq!(pane.title, "Terminal");
        assert_eq!(pane.identity, TerminalIdentity::default());
    }

    #[test]
    fn custom_name_selected_profile_and_uploaded_icon_remain_independent() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry.rename_pane(pane_id, "My work").unwrap();
        registry
            .set_pane_profile(pane_id, Some(TerminalProfile::Claude))
            .unwrap();
        let icon = format!("{}.png", Uuid::new_v4());
        registry
            .set_pane_custom_icon(pane_id, Some(icon.clone()))
            .unwrap();
        registry.rename_pane(pane_id, "Release watch").unwrap();

        let snapshot = registry.snapshot().unwrap();
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert_eq!(pane.title, "Release watch");
        assert_eq!(pane.custom_title.as_deref(), Some("Release watch"));
        assert_eq!(pane.profile_override, Some(TerminalProfile::Claude));
        assert_eq!(pane.custom_icon.as_deref(), Some(icon.as_str()));
        assert_eq!(pane.identity.profile, TerminalProfile::Claude);

        registry.reset_pane_identity(pane_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert_eq!(pane.custom_title, None);
        assert_eq!(pane.profile_override, None);
        assert_eq!(pane.custom_icon, None);
    }

    #[test]
    fn appearance_mutations_keep_global_defaults_and_entity_overrides_independent() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let pane_id = first_pane_id(&snapshot).unwrap();
        let terminal_default = AppearanceColor::new(0x95, 0xcc, 0x7f);
        let workspace_default = AppearanceColor::new(0xc9, 0x90, 0xe5);
        let terminal_override = AppearanceColor::new(0xef, 0x71, 0x7a);
        let workspace_override = AppearanceColor::new(0xe4, 0xbd, 0x72);

        registry
            .set_default_terminal_accent(terminal_default)
            .unwrap();
        registry
            .set_default_workspace_color(workspace_default)
            .unwrap();
        registry
            .set_pane_color(pane_id, Some(terminal_override))
            .unwrap();
        registry
            .set_workspace_color(workspace_id, Some(workspace_override))
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.appearance.default_terminal_accent,
            terminal_default
        );
        assert_eq!(
            snapshot.appearance.default_workspace_color,
            workspace_default
        );
        assert_eq!(snapshot.workspaces[0].color, Some(workspace_override));
        assert_eq!(
            find_pane_mut_in_snapshot(&mut snapshot.clone(), pane_id).and_then(|pane| pane.color),
            Some(terminal_override)
        );
        assert_eq!(snapshot.appearance.recent_colors[0], workspace_override);

        registry.set_pane_color(pane_id, None).unwrap();
        registry.set_workspace_color(workspace_id, None).unwrap();
        let reset = registry.snapshot().unwrap();
        assert_eq!(reset.workspaces[0].color, None);
        assert_eq!(
            find_pane_mut_in_snapshot(&mut reset.clone(), pane_id).and_then(|pane| pane.color),
            None
        );
        assert_eq!(reset.appearance.default_terminal_accent, terminal_default);
        assert_eq!(reset.appearance.default_workspace_color, workspace_default);
    }

    #[test]
    fn saved_workspace_management_renames_pins_reorders_and_deletes_deterministically() {
        let registry = SessionRegistry::new().unwrap();
        let first = registry.snapshot().unwrap().workspaces[0].id;
        let (second, _) = registry.create_workspace(Some("Second")).unwrap();
        let (third, _) = registry.create_workspace(Some("Third")).unwrap();

        registry.rename_workspace(second, "Build tools").unwrap();
        registry.set_workspace_pinned(second, true).unwrap();
        registry.set_workspace_pinned(third, true).unwrap();
        registry
            .move_pinned_workspace(third, WorkspacePinMove::Up)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let build = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == second)
            .unwrap();
        let third_workspace = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == third)
            .unwrap();
        assert_eq!(build.title, "Build tools");
        assert!(build.pinned);
        assert!(third_workspace.pinned);
        assert_eq!(third_workspace.pin_order, 1);
        assert_eq!(build.pin_order, 2);

        registry.delete_workspace(second).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert!(
            snapshot
                .workspaces
                .iter()
                .all(|workspace| workspace.id != second)
        );
        assert_eq!(
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == third)
                .unwrap()
                .pin_order,
            1
        );
        assert!(registry.delete_workspace(first).is_ok());
        assert!(registry.delete_workspace(third).is_err());
    }

    #[test]
    fn workspace_reorder_stays_in_its_group_and_persists_explicit_order() {
        let registry = SessionRegistry::new().unwrap();
        let first = registry.snapshot().unwrap().workspaces[0].id;
        let (second, _) = registry.create_workspace(Some("Second")).unwrap();
        let (third, _) = registry.create_workspace(Some("Third")).unwrap();

        registry.reorder_workspace(third, first, false).unwrap();
        registry.set_workspace_pinned(second, true).unwrap();
        assert!(registry.reorder_workspace(third, second, false).is_err());

        let snapshot = registry.snapshot().unwrap();
        let mut regular = snapshot
            .workspaces
            .iter()
            .filter(|workspace| !workspace.pinned)
            .map(|workspace| (workspace.title.as_str(), workspace.order))
            .collect::<Vec<_>>();
        regular.sort_by_key(|(_, order)| *order);
        assert_eq!(regular, vec![("Third", 1), ("Workstation 1", 2)]);
        assert!(
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == second)
                .is_some_and(|workspace| workspace.pinned)
        );
    }

    #[test]
    fn tab_reorder_moves_whole_tabs_only_within_their_workstation() {
        let directory =
            std::env::temp_dir().join(format!("hh-tab-reorder-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let first_tab = initial.workspaces[0].tabs[0].id;
        let second_pane = registry.create_workspace_tab(workspace_id).unwrap();
        let third_pane = registry.create_workspace_tab(workspace_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace = &snapshot.workspaces[0];
        let second_tab = workspace
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, second_pane))
            .unwrap()
            .id;
        let third_tab = workspace
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, third_pane))
            .unwrap()
            .id;

        registry.reorder_tab(third_tab, first_tab, false).unwrap();
        registry.reorder_tab(first_tab, second_tab, true).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            vec![third_tab, second_tab, first_tab]
        );
        drop(snapshot);
        drop(registry);

        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            vec![third_tab, second_tab, first_tab]
        );
        drop(snapshot);

        let (other_workspace, _) = registry.create_workspace(Some("Other")).unwrap();
        let other_tab = registry
            .snapshot()
            .unwrap()
            .workspaces
            .iter()
            .find(|workspace| workspace.id == other_workspace)
            .unwrap()
            .tabs[0]
            .id;
        assert!(registry.reorder_tab(first_tab, other_tab, false).is_err());
        drop(registry);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disconnect_keeps_saved_workspace_tabs_and_layout_offline() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let pane_id = first_pane_id(&snapshot).unwrap();
        let expected_panes = pane_ids_for_workspace(&snapshot.workspaces[0]);
        {
            let mut state = registry.state.write();
            state.snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Connected,
            };
            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .kind = RuntimePaneKind::SystemSsh {
                host: "build-node".to_owned(),
            };
        }

        registry.disconnect_workspace(workspace_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace = &snapshot.workspaces[0];

        assert_eq!(pane_ids_for_workspace(workspace), expected_panes);
        assert!(matches!(workspace.tabs[0].layout, PaneLayout::Leaf { .. }));
        assert_eq!(workspace.active_terminal_count, 0);
        assert_eq!(
            workspace.connection,
            WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Offline,
            }
        );
        assert!(registry.state.read().panes.contains_key(&pane_id));
    }

    #[test]
    fn closing_the_last_terminal_keeps_an_ssh_workstation_connected() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane_id = first_pane_id(&snapshot).unwrap();
        {
            let mut state = registry.state.write();
            state.snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Connected,
            };
            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .kind = RuntimePaneKind::SystemSsh {
                host: "build-node".to_owned(),
            };
        }

        registry.close_pane(pane_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace = &snapshot.workspaces[0];

        // Zero terminals is an empty workstation, not a disconnect: the next
        // terminal must open instead of demanding a reconnect.
        assert!(workspace.tabs.is_empty());
        assert_eq!(workspace.active_terminal_count, 0);
        assert_eq!(
            workspace.connection,
            WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Connected,
            }
        );
    }

    #[test]
    fn a_live_remote_tmux_tab_keeps_its_workstation_connected_and_a_dead_one_does_not() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane_id = first_pane_id(&snapshot).unwrap();
        {
            let mut state = registry.state.write();
            state.snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Offline,
            };
            // Only a tmux attach remains, exactly what survives closing the
            // initial SSH tab.
            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .kind = RuntimePaneKind::TmuxSystemSsh {
                host: "build-node".to_owned(),
                session_id: TmuxSessionId::try_from("$3".to_owned()).unwrap(),
            };
            assert!(refresh_workspace_activity(&mut state));
            assert_eq!(
                state.snapshot.workspaces[0].connection,
                WorkspaceConnection::SystemSsh {
                    destination: "build-node".to_owned(),
                    status: WorkspaceConnectionStatus::Connected,
                }
            );

            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .exit_status = Some("Exited with code 255".to_owned());
            assert!(refresh_workspace_activity(&mut state));
            assert_eq!(
                state.snapshot.workspaces[0].connection,
                WorkspaceConnection::SystemSsh {
                    destination: "build-node".to_owned(),
                    status: WorkspaceConnectionStatus::Offline,
                }
            );
        }
    }

    #[test]
    fn a_dead_tmux_tab_releases_its_session_back_to_the_picker() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("printf fixture; sleep 5"),
            ]),
            "live tmux fixture",
            &registry.history,
        )
        .unwrap();
        let tmux_session = tmux_session("$9", "editor");
        registry
            .register_live_tmux_tab(
                workspace_id,
                pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap();
        assert_eq!(
            registry.open_tmux_session_ids(workspace_id).unwrap(),
            HashSet::from([tmux_session.id.clone()])
        );

        registry
            .state
            .write()
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .terminal_mut()
            .unwrap()
            .exit_status = Some("Exited with code 255".to_owned());

        // The frozen tab must not reserve the session, or the picker offers
        // nothing to reopen after an SSH drop killed every attach.
        assert!(
            registry
                .open_tmux_session_ids(workspace_id)
                .unwrap()
                .is_empty()
        );
        let plan = plan_tmux_session_attachments(
            std::slice::from_ref(&tmux_session.id),
            &registry.open_tmux_session_ids(workspace_id).unwrap(),
            &HashMap::from([(tmux_session.id.clone(), tmux_session.clone())]),
        )
        .unwrap();
        assert_eq!(plan.launch, vec![tmux_session]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn reattach_respawns_an_exited_pane_in_place_and_refuses_a_live_one() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let tab_ids = snapshot.workspaces[0]
            .tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<Vec<_>>();

        let error = registry.reattach_pane(pane_id).unwrap_err();
        assert!(error.to_string().contains("still live"));

        let dead_session = {
            let mut state = registry.state.write();
            let terminal = state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap();
            terminal.exit_status = Some("Exited with code 255".to_owned());
            Arc::clone(&terminal.session)
        };
        dead_session.terminate_and_wait().unwrap();

        registry.reattach_pane(pane_id).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            tab_ids
        );
        assert!(pane_ids_for_workspace(&snapshot.workspaces[0]).contains(&pane_id));
        assert_eq!(
            registry.state.read().panes.get(&pane_id).map(|runtime| {
                runtime
                    .terminal()
                    .and_then(|terminal| terminal.exit_status.clone())
            }),
            Some(None)
        );
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert!(!pane.shell.contains("exited"), "shell: {}", pane.shell);
        registry
            .write_input(pane_id, b"printf 'REATTACHED\\n'\r")
            .unwrap();
    }

    #[test]
    fn configured_shell_pty_accepts_input_and_produces_real_output() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry
            .write_input(pane_id, b"printf 'RMUX_REAL_PTY_TEST\\n'\r")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (_, screens) = registry.state().unwrap();
            let screen = screens
                .iter()
                .find(|screen| screen.pane_id == pane_id)
                .unwrap();
            if screen
                .lines
                .iter()
                .flat_map(|line| &line.runs)
                .any(|run| run.text.contains("RMUX_REAL_PTY_TEST"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shell command output did not arrive"
            );
            thread::sleep(Duration::from_millis(25));
        }
        assert!(registry.pane_process_id(pane_id).unwrap().is_some());
    }

    #[test]
    fn resize_propagates_the_exact_requested_grid_to_the_terminal_model() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();

        registry.resize_pane(pane_id, 13, 3).unwrap();

        let (_, screens) = registry.state().unwrap();
        let screen = screens
            .iter()
            .find(|screen| screen.pane_id == pane_id)
            .unwrap();
        assert_eq!((screen.columns, screen.rows), (13, 3));
    }
    #[test]
    fn tmux_scan_gate_rejects_concurrent_and_rapid_repeat_scans() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let permit = registry.begin_tmux_scan(workspace_id).unwrap();
        assert!(registry.begin_tmux_scan(workspace_id).is_err());
        drop(permit);
        assert!(registry.begin_tmux_scan(workspace_id).is_err());
    }

    #[test]
    fn split_creates_a_second_live_shell_without_replacing_the_first() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();

        assert_ne!(first, second);
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert!(registry.pane_process_id(second).unwrap().is_some());
        assert_eq!(registry.state().unwrap().1.len(), 2);
    }

    #[test]
    fn rearrange_swaps_layout_positions_without_restarting_shells() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let second_pid = registry.pane_process_id(second).unwrap();

        registry.swap_panes(first, second).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let layout = &snapshot.workspaces[0].tabs[0].layout;
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = layout
        else {
            panic!("expected split layout");
        };

        assert_eq!(first_pane_in_layout(left), second);
        assert_eq!(first_pane_in_layout(right), first);
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
    }
    #[test]
    fn resize_bounds_reject_oom_dimensions_without_killing_sessions() {
        assert!(validate_terminal_dimensions(1_200, 500).is_ok());
        assert!(validate_terminal_dimensions(2_000, 301).is_err());
        assert!(validate_terminal_dimensions(1, 30).is_err());

        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        assert!(registry.resize_pane(pane_id, u16::MAX, u16::MAX).is_err());
        assert!(registry.pane_process_id(pane_id).unwrap().is_some());
    }

    #[test]
    fn pane_update_vectors_are_bounded_independently() {
        let registry = SessionRegistry::new().unwrap();
        let subscriptions = vec![Uuid::nil(); MAX_PANES + 1];
        assert!(
            registry
                .pane_updates(None, &[], &subscriptions, false, 0)
                .is_err()
        );
        let revisions = vec![
            PaneRevisionCursor {
                pane_id: Uuid::nil(),
                revision: 0,
            };
            MAX_PANES + 1
        ];
        assert!(
            registry
                .pane_updates(None, &revisions, &[], false, 0)
                .is_err()
        );
    }

    #[test]
    fn terminals_receive_human_names_and_can_be_renamed() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_group_terminal(first).unwrap();
        registry.rename_pane(second, "Build logs").unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Stack { panes, .. } = &snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected a pane-local tab stack");
        };
        assert_eq!(panes[0].title, "Terminal 1");
        assert_eq!(panes[1].title, "Build logs");
        assert_eq!(panes[1].shell, shell_title());
    }

    #[test]
    fn moving_a_live_tab_to_a_directional_split_preserves_its_process() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_group_terminal(first).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let second_pid = registry.pane_process_id(second).unwrap();

        registry
            .move_pane_to_split(second, first, DropPlacement::Left)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            axis,
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected moved tab to become a split");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert_eq!(first_pane_in_layout(left), second);
        assert_eq!(first_pane_in_layout(right), first);
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
    }

    #[test]
    fn browser_moves_from_a_top_level_tab_into_a_terminal_split() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let terminal = first_pane_id(&snapshot).unwrap();
        let terminal_pid = registry.pane_process_id(terminal).unwrap();
        let browser = registry
            .create_browser_tab(workspace_id, Some("https://example.com"))
            .unwrap();

        registry
            .move_pane_to_split(browser, terminal, DropPlacement::Right)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
        let PaneLayout::Split {
            axis,
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected the browser to join the terminal split");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert_eq!(first_pane_in_layout(left), terminal);
        assert!(matches!(
            &**right,
            PaneLayout::Leaf { pane }
                if pane.id == browser && matches!(pane.kind, PaneKind::Browser { .. })
        ));
        assert_eq!(registry.pane_process_id(terminal).unwrap(), terminal_pid);
    }

    #[test]
    fn browser_moves_from_a_top_level_tab_into_a_terminal_tab_strip() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let terminal = first_pane_id(&snapshot).unwrap();
        let browser = registry
            .create_browser_tab(workspace_id, Some("https://example.com"))
            .unwrap();

        registry.move_pane_to_tab(browser, terminal).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
        let PaneLayout::Stack { panes, active } = &snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected the browser to join the terminal tab strip");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [terminal, browser]
        );
        assert_eq!(*active, browser);
    }

    #[test]
    fn directional_drop_of_a_lone_tab_keeps_it_live_and_fills_the_vacated_half() {
        let registry = SessionRegistry::new().unwrap();
        let moved = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let moved_pid = registry.pane_process_id(moved).unwrap();

        registry
            .move_pane_to_split(moved, moved, DropPlacement::Bottom)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            axis,
            first: top,
            second: bottom,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected the lone tab drop to create a filled split");
        };
        let replacement = first_pane_in_layout(top);
        assert_eq!(*axis, SplitAxis::Vertical);
        assert_ne!(replacement, moved);
        assert_eq!(first_pane_in_layout(bottom), moved);
        assert_eq!(registry.pane_process_id(moved).unwrap(), moved_pid);
        assert!(registry.pane_process_id(replacement).unwrap().is_some());
        assert_eq!(registry.state().unwrap().1.len(), 2);
    }

    #[test]
    fn merging_a_live_tab_into_another_strip_preserves_its_process() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let target = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let moved = registry.create_group_terminal(first).unwrap();
        let moved_pid = registry.pane_process_id(moved).unwrap();

        registry.move_pane_to_tab(moved, target).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected both tiled panes to remain after the merge");
        };
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        let PaneLayout::Stack { panes, active } = &**right else {
            panic!("dragged terminal must join the target tab strip");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [target, moved]
        );
        assert_eq!(*active, moved);
        assert_eq!(registry.pane_process_id(moved).unwrap(), moved_pid);
    }

    #[test]
    fn sidebar_drag_moves_browser_into_group_and_back_to_a_top_level_tab() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let group_pane = registry.create_workspace_group(workspace_id, None).unwrap();
        let group_tab = registry.snapshot().unwrap().workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, group_pane))
            .unwrap()
            .id;
        let browser = registry
            .create_browser_tab(workspace_id, Some("https://example.com"))
            .unwrap();

        registry.move_pane_to_group(browser, group_tab).unwrap();

        let grouped = registry.snapshot().unwrap();
        assert_eq!(grouped.workspaces[0].tabs.len(), 2);
        let group = grouped.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == group_tab)
            .unwrap();
        let PaneLayout::Stack { panes, active } = &group.layout else {
            panic!("browser must join the target sidebar group");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [group_pane, browser]
        );
        assert_eq!(*active, browser);

        registry
            .move_pane_to_new_tab(browser, group_tab, true, None)
            .unwrap();

        let extracted = registry.snapshot().unwrap();
        assert_eq!(extracted.workspaces[0].tabs.len(), 3);
        let group_index = extracted.workspaces[0]
            .tabs
            .iter()
            .position(|tab| tab.id == group_tab)
            .unwrap();
        assert!(matches!(
            &extracted.workspaces[0].tabs[group_index].layout,
            PaneLayout::Leaf { pane } if pane.id == group_pane
        ));
        assert!(matches!(
            &extracted.workspaces[0].tabs[group_index + 1].layout,
            PaneLayout::Leaf { pane }
                if pane.id == browser
                    && matches!(pane.kind, PaneKind::Browser { .. })
        ));
    }

    #[test]
    fn closing_one_tab_terminates_only_that_tab_and_preserves_its_pane_group() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let closing = registry.create_group_terminal(first).unwrap();

        registry.close_pane(closing).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert!(matches!(
            &snapshot.workspaces[0].tabs[0].layout,
            PaneLayout::Leaf { pane } if pane.id == first
        ));
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert!(registry.pane_process_id(closing).is_err());
    }

    #[test]
    fn closing_the_last_terminal_leaves_a_saved_empty_workspace_until_explicit_reopen() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let first = first_pane_id(&initial).unwrap();
        let second = registry.create_pane(first, SplitAxis::Vertical).unwrap();
        let second_pid = registry.pane_process_id(second).unwrap();

        registry.close_pane(first).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(first_pane_id(&snapshot), Some(second));
        assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
        assert!(registry.pane_process_id(first).is_err());

        registry.close_pane(second).unwrap();

        let empty = registry.snapshot().unwrap();
        assert_eq!(empty.workspaces.len(), 1);
        assert_eq!(empty.workspaces[0].id, workspace_id);
        assert!(empty.workspaces[0].tabs.is_empty());
        assert_eq!(empty.workspaces[0].active_terminal_count, 0);
        assert!(registry.state().unwrap().1.is_empty());

        let reopened = registry.create_workspace_terminal(workspace_id).unwrap();
        let reopened_snapshot = registry.snapshot().unwrap();
        assert_eq!(first_pane_id(&reopened_snapshot), Some(reopened));
        assert_eq!(reopened_snapshot.workspaces[0].active_terminal_count, 1);
        assert!(registry.create_workspace_terminal(workspace_id).is_err());
    }

    #[test]
    fn natural_shell_exit_stays_visible_until_explicit_layout_close() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let exiting = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        registry.write_input(exiting, b"exit 7\r").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = registry.snapshot().unwrap();
            let pane = snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find_map(|tab| pane_in_layout(&tab.layout, exiting))
                .expect("exited pane must remain in its layout");
            if pane.shell.contains("exited") {
                assert!(pane.shell.contains('7'));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "natural child exit was not reflected in pane metadata"
            );
            thread::sleep(Duration::from_millis(25));
        }

        assert!(registry.pane(exiting).is_ok());
        registry.close_pane(exiting).unwrap();
        assert!(registry.pane(exiting).is_err());
        assert_eq!(first_pane_id(&registry.snapshot().unwrap()), Some(first));
    }

    #[test]
    fn pane_local_tab_actions_only_mutate_the_explicit_second_pane() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let second_tab = registry.create_group_terminal(second).unwrap();
        registry.rename_pane(second_tab, "Second pane tab").unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected two pane columns");
        };
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        let PaneLayout::Stack { panes, active } = &**right else {
            panic!("new tab must be placed in the targeted second pane");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [second, second_tab]
        );
        assert_eq!(*active, second_tab);
        assert_eq!(panes[1].title, "Second pane tab");

        registry.activate_tab(second).unwrap();
        registry.close_pane(second_tab).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("closing a second-pane tab must preserve both pane columns");
        };
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        assert!(matches!(&**right, PaneLayout::Leaf { pane } if pane.id == second));
    }

    #[test]
    fn pane_local_split_only_mutates_the_explicit_second_pane() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let nested = registry.create_pane(second, SplitAxis::Vertical).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            axis,
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected outer two pane columns");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        let PaneLayout::Split {
            axis,
            first: top,
            second: bottom,
            ..
        } = &**right
        else {
            panic!("split control must split the targeted second pane");
        };
        assert_eq!(*axis, SplitAxis::Vertical);
        assert_eq!(first_pane_in_layout(top), second);
        assert_eq!(first_pane_in_layout(bottom), nested);
    }

    #[test]
    fn reorder_next_to_a_project_child_adopts_the_project() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let moved_tab = initial.workspaces[0].tabs[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let child_tab = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);

        registry.reorder_tab(moved_tab, child_tab, true).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let tabs = &snapshot.workspaces[0].tabs;
        let moved_index = tabs.iter().position(|tab| tab.id == moved_tab).unwrap();
        let child_index = tabs.iter().position(|tab| tab.id == child_tab).unwrap();
        assert_eq!(tabs[moved_index].parent_tab, Some(project_tab));
        assert_eq!(moved_index, child_index + 1);
    }

    #[test]
    fn reorder_next_to_a_root_tab_unnests_a_project_child() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let root_tab = initial.workspaces[0].tabs[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let child_tab = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);

        registry.reorder_tab(child_tab, root_tab, true).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let tabs = &snapshot.workspaces[0].tabs;
        let root_index = tabs.iter().position(|tab| tab.id == root_tab).unwrap();
        let child_index = tabs.iter().position(|tab| tab.id == child_tab).unwrap();
        assert_eq!(tabs[child_index].parent_tab, None);
        assert_eq!(child_index, root_index + 1);
    }

    #[test]
    fn projects_never_nest() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let parent_project = {
            let pane = registry
                .create_workspace_project(workspace_id, "/tmp", Some("Project A"))
                .unwrap();
            tab_id_for_pane(&registry.snapshot().unwrap(), pane)
        };
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(parent_project))
            .unwrap();
        let child = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);
        let moving_project = {
            let pane = registry
                .create_workspace_project(workspace_id, "/tmp", Some("Project B"))
                .unwrap();
            tab_id_for_pane(&registry.snapshot().unwrap(), pane)
        };

        let error = registry
            .move_tab_to_project(moving_project, parent_project)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("a project cannot nest inside another project")
        );

        registry.reorder_tab(moving_project, child, true).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let moved_project = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == moving_project)
            .unwrap();
        assert_eq!(moved_project.parent_tab, None);
    }

    #[test]
    fn move_tab_to_project_appends_after_the_last_child() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let moved_tab = initial.workspaces[0].tabs[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let first_child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let first_child = tab_id_for_pane(&registry.snapshot().unwrap(), first_child_pane);
        let second_child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let second_child = tab_id_for_pane(&registry.snapshot().unwrap(), second_child_pane);

        registry
            .move_tab_to_project(moved_tab, project_tab)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let tabs = &snapshot.workspaces[0].tabs;
        let project_index = tabs.iter().position(|tab| tab.id == project_tab).unwrap();
        assert_eq!(tabs[project_index + 1].id, first_child);
        assert_eq!(tabs[project_index + 2].id, second_child);
        assert_eq!(tabs[project_index + 3].id, moved_tab);
        assert_eq!(tabs[project_index + 3].parent_tab, Some(project_tab));

        let revision = snapshot.revision;
        registry
            .move_tab_to_project(moved_tab, project_tab)
            .unwrap();
        assert_eq!(registry.snapshot().unwrap().revision, revision);
    }

    #[test]
    fn pane_detached_into_a_project_becomes_a_child_tab() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let initial_pane = first_pane_id(&initial).unwrap();
        let detached_pane = registry.create_group_terminal(initial_pane).unwrap();
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let target_child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let target_child = tab_id_for_pane(&registry.snapshot().unwrap(), target_child_pane);

        registry
            .move_pane_to_new_tab(detached_pane, project_tab, false, Some(project_tab))
            .unwrap();
        let snapshot = registry.snapshot().unwrap();
        let detached_tab = tab_id_for_pane(&snapshot, detached_pane);
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .find(|tab| tab.id == detached_tab)
                .unwrap()
                .parent_tab,
            Some(project_tab)
        );

        let source_group_pane = registry.create_workspace_group(workspace_id, None).unwrap();
        let adopted_pane = registry.create_group_terminal(source_group_pane).unwrap();
        registry
            .move_pane_to_new_tab(adopted_pane, target_child, true, None)
            .unwrap();
        let snapshot = registry.snapshot().unwrap();
        let adopted_tab = tab_id_for_pane(&snapshot, adopted_pane);
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .find(|tab| tab.id == adopted_tab)
                .unwrap()
                .parent_tab,
            Some(project_tab)
        );
    }

    #[test]
    fn set_tab_pinned_round_trips() {
        let registry = SessionRegistry::new().unwrap();
        let tab_id = registry.snapshot().unwrap().workspaces[0].tabs[0].id;

        registry.set_tab_pinned(tab_id, true).unwrap();
        assert!(registry.snapshot().unwrap().workspaces[0].tabs[0].pinned);

        registry.set_tab_pinned(tab_id, false).unwrap();
        assert!(!registry.snapshot().unwrap().workspaces[0].tabs[0].pinned);

        let error = registry.set_tab_pinned(Uuid::new_v4(), true).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    fn tab_id_for_pane(snapshot: &SessionSnapshot, pane_id: Uuid) -> Uuid {
        snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .find(|tab| layout_contains(&tab.layout, pane_id))
            .map(|tab| tab.id)
            .unwrap()
    }

    fn first_pane_in_layout(layout: &PaneLayout) -> Uuid {
        match layout {
            PaneLayout::Leaf { pane } => pane.id,
            PaneLayout::Stack { active, .. } => *active,
            PaneLayout::Split { first, .. } => first_pane_in_layout(first),
        }
    }

    fn pane_in_layout(layout: &PaneLayout, pane_id: Uuid) -> Option<&Pane> {
        match layout {
            PaneLayout::Leaf { pane } => (pane.id == pane_id).then_some(pane),
            PaneLayout::Stack { panes, .. } => panes.iter().find(|pane| pane.id == pane_id),
            PaneLayout::Split { first, second, .. } => {
                pane_in_layout(first, pane_id).or_else(|| pane_in_layout(second, pane_id))
            }
        }
    }
}
