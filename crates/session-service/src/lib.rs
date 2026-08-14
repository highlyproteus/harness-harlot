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
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nah_protocol::{
    AppearanceColor, ClientRequest, DropPlacement, HistoryArchiveStatus, HistoryClearScope,
    HistoryCursor, HistoryPageDirection, HistorySettings, MAX_FRAME_SIZE, PROTOCOL_VERSION, Pane,
    PaneLayout, PaneRevisionCursor, PaneStreamState, ServiceResponse, SessionSnapshot, SplitAxis,
    StreamDiagnostics, Tab, TerminalHistoryPage, TerminalIdentity, TerminalIdentitySource,
    TerminalModes, TerminalModifiers, TerminalMouseAction, TerminalMouseButton, TerminalPoint,
    TerminalProfile, TerminalScreen, TerminalSelectionKind, TmuxScanScope, TmuxSession,
    TmuxSessionAttachIssue, TmuxSessionId, Workspace, WorkspaceConnection,
    WorkspaceConnectionStatus, WorkspacePinMove, terminal_profile_for_command,
    terminal_profile_for_executable, terminal_profile_for_title, validate_ssh_host,
};
use nah_terminal_model::TerminalModel;
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
const MAX_PANES: usize = 32;
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
const MAX_TMUX_ATTACH_SESSIONS: usize = 32;
const TMUX_ATTACH_STARTUP_GRACE: Duration = Duration::from_millis(75);
const TMUX_SESSION_LIST_FORMAT: &str =
    "S\t#{session_id}\t#{session_name}\t#{session_windows}\t#{session_attached}";
const TMUX_REMOTE_LIST_COMMAND: &str = "exec tmux list-sessions -F 'S\t#{session_id}\t#{session_name}\t#{session_windows}\t#{session_attached}'";
#[cfg(debug_assertions)]
const LOCAL_SSH_TEST_SEAM_ENV: &str = "NAH_TEST_LOCAL_SSH_SEAM";

struct PtySession {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
    terminal: Arc<Mutex<TerminalModel>>,
    revision: Arc<AtomicU64>,
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
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        #[cfg(debug_assertions)]
        if std::env::var_os(LOCAL_SSH_TEST_SEAM_ENV).is_some() {
            return Self::spawn_local(pane_id, workspace_id, &fallback_cwd()?, archive);
        }
        Self::spawn_command(
            pane_id,
            workspace_id,
            system_ssh_command(pane_id, host)?,
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
        let reader_terminal = Arc::clone(&terminal);
        let reader_revision = Arc::clone(&revision);
        let reader_history = Arc::clone(&history);
        let reader = thread::Builder::new()
            .name(format!("rmux-pty-{pane_id}"))
            .spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut terminal = reader_terminal.lock();
                            terminal.process_output(&buffer[..read]);
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
        let columns = columns.max(1);
        let rows = rows.max(1);
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

#[derive(Debug)]
struct RuntimePane {
    session: Arc<PtySession>,
    last_valid_cwd: PathBuf,
    kind: RuntimePaneKind,
    recovered: bool,
    exit_status: Option<String>,
    detected_command_profile: Option<TerminalProfile>,
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
            title,
            shell: shell_title(),
            color: None,
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRegistry {
    state: Arc<RwLock<RegistryState>>,
    diagnostics_sampler: Arc<Mutex<DiagnosticsSampler>>,
    store: Option<SnapshotStore>,
    history: HistoryArchive,
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
                            session,
                            last_valid_cwd: cwd,
                            kind: RuntimePaneKind::Local,
                            recovered: true,
                            exit_status: None,
                            detected_command_profile: None,
                        },
                    );
                }
                Err(error) => {
                    terminate_runtime_panes(&panes);
                    return Err(error).context("recreate fresh shell for recovered pane");
                }
            }
        }
        for pane_id in panes.keys().copied().collect::<Vec<_>>() {
            set_pane_runtime_label(&mut recovered.snapshot, pane_id, true, None, &shell_title());
        }
        let next_terminal_number = u32::try_from(panes.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let registry = Self {
            state: Arc::new(RwLock::new(RegistryState {
                snapshot: recovered.snapshot,
                panes,
                next_terminal_number,
                next_group_number: 1,
                last_identity_refresh: None,
                system: System::new(),
            })),
            diagnostics_sampler: Arc::new(Mutex::new(DiagnosticsSampler::default())),
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
                panes: HashMap::from([(
                    pane_id,
                    RuntimePane {
                        session,
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::Local,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    },
                )]),
                next_terminal_number: 2,
                next_group_number: 1,
                last_identity_refresh: None,
                system: System::new(),
            })),
            diagnostics_sampler: Arc::new(Mutex::new(DiagnosticsSampler::default())),
            store,
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
            .map(|(pane_id, runtime)| runtime.session.screen(*pane_id))
            .collect::<Result<Vec<_>>>()?;
        Ok((snapshot, screens))
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
    ) -> Result<PaneUpdateBatch> {
        let started = Instant::now();
        let known_revisions = pane_revisions
            .iter()
            .map(|cursor| (cursor.pane_id, cursor.revision))
            .collect::<HashMap<_, _>>();
        let subscribed = subscribed_panes.iter().copied().collect::<HashSet<_>>();
        let state = self.state.read();

        let session_revision = state.snapshot.revision;
        let snapshot =
            (snapshot_revision != Some(session_revision)).then(|| state.snapshot.clone());
        let mut screens = Vec::new();
        let mut pane_states = Vec::with_capacity(state.panes.len());
        let mut coalesced_revisions = 0_u64;
        for (pane_id, runtime) in &state.panes {
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
        drop(state);

        pane_states.sort_unstable_by_key(|pane| pane.pane_id);
        screens.sort_unstable_by_key(|screen| screen.pane_id);
        let snapshot_bytes = if measure_bytes {
            snapshot
                .as_ref()
                .map(serialized_len)
                .transpose()?
                .unwrap_or(0)
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
        let diagnostics = StreamDiagnostics {
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
        };
        Ok(PaneUpdateBatch {
            session_revision,
            snapshot,
            screens,
            pane_states,
            diagnostics,
        })
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
        match scope {
            HistoryClearScope::Terminal { pane_id } => {
                self.pane(pane_id)?;
            }
            HistoryClearScope::Workspace { workspace_id } => {
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
            HistoryClearScope::All => {}
        }
        self.history.clear(scope)
    }

    pub fn load_history_page(
        &self,
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    ) -> Result<Option<TerminalHistoryPage>> {
        self.pane(pane_id)?;
        self.history.load_page(pane_id, cursor, direction)
    }

    pub fn search_archived_history(
        &self,
        pane_id: Uuid,
        query: &str,
        before: Option<HistoryCursor>,
    ) -> Result<Option<TerminalHistoryPage>> {
        self.pane(pane_id)?;
        self.history.search(pane_id, query, before)
    }

    pub fn create_pane(&self, target_pane: Uuid, axis: SplitAxis) -> Result<Uuid> {
        let new_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(target_pane)?;
        let workspace_id = self.workspace_for_pane(target_pane)?;
        let (session, kind) = self.spawn_pane_for_workspace(new_id, workspace_id, &cwd)?;
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
                session,
                last_valid_cwd: cwd,
                kind,
                recovered: false,
                exit_status: None,
                detected_command_profile: None,
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
        let new_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(target_pane)?;
        let workspace_id = self.workspace_for_pane(target_pane)?;
        let (session, kind) = self.spawn_pane_for_workspace(new_id, workspace_id, &cwd)?;
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
                session,
                last_valid_cwd: cwd,
                kind,
                recovered: false,
                exit_status: None,
                detected_command_profile: None,
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

    /// Opens the sole initial terminal in a deliberately empty saved workspace.
    /// This request is rejected once any layout exists, so a repeated click or
    /// retried request cannot create duplicate terminals.
    pub fn create_workspace_terminal(&self, workspace_id: Uuid) -> Result<Uuid> {
        let connection = {
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
            workspace.connection.clone()
        };

        let pane_id = Uuid::new_v4();
        let cwd = fallback_cwd()?;
        let (session, kind, pane_title, pane_shell, tab_title) = match &connection {
            WorkspaceConnection::Local => (
                PtySession::spawn_local(pane_id, workspace_id, &cwd, &self.history)?,
                RuntimePaneKind::Local,
                "Terminal 1".to_owned(),
                shell_title(),
                "Terminals".to_owned(),
            ),
            WorkspaceConnection::SystemSsh { destination, .. } => (
                PtySession::spawn_ssh(pane_id, workspace_id, destination, &self.history)?,
                RuntimePaneKind::SystemSsh {
                    host: destination.clone(),
                },
                format!("SSH {destination}"),
                "ssh".to_owned(),
                "Remote".to_owned(),
            ),
        };

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
                title: pane_title,
                shell: pane_shell,
                color: None,
                identity: TerminalIdentity::default(),
                custom_title: None,
                profile_override: None,
            };
            workspace.tabs.push(Tab {
                id: Uuid::new_v4(),
                title: tab_title,
                custom_title: None,
                layout: PaneLayout::Leaf { pane },
            });
            workspace.active_terminal_count = 1;
            if let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection {
                *status = WorkspaceConnectionStatus::Connected;
            }
            state.panes.insert(
                pane_id,
                RuntimePane {
                    session: Arc::clone(&session),
                    last_valid_cwd: cwd,
                    kind,
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
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
        self.append_workspace_tab(workspace_id, None)
    }

    /// Appends a named group holding its first terminal, so the group is visible
    /// and right-clickable before a second terminal exists.
    pub fn create_workspace_group(&self, workspace_id: Uuid) -> Result<Uuid> {
        let number = {
            let mut state = self.state.write();
            let number = state.next_group_number;
            state.next_group_number = state.next_group_number.saturating_add(1);
            number
        };
        self.append_workspace_tab(workspace_id, Some(format!("Group {number}")))
    }

    fn append_workspace_tab(
        &self,
        workspace_id: Uuid,
        custom_title: Option<String>,
    ) -> Result<Uuid> {
        let _connection = {
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
            workspace.connection.clone()
        };

        let pane_id = Uuid::new_v4();
        let cwd = fallback_cwd()?;
        let (session, kind) = self.spawn_pane_for_workspace(pane_id, workspace_id, &cwd)?;
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
                layout: PaneLayout::Leaf { pane },
            };
            let workspace = &mut state.snapshot.workspaces[workspace_index];
            workspace.tabs.push(tab);
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                RuntimePane {
                    session: Arc::clone(&session),
                    last_valid_cwd: cwd,
                    kind,
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
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
        let session = PtySession::spawn_ssh(pane_id, workspace_id, host, &self.history)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let pane = Pane {
                id: pane_id,
                title: format!("SSH {host}"),
                shell: "ssh".to_owned(),
                color: None,
                identity: TerminalIdentity::default(),
                custom_title: None,
                profile_override: None,
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
                    session: Arc::clone(&session),
                    last_valid_cwd: cwd,
                    kind: RuntimePaneKind::SystemSsh {
                        host: host.to_owned(),
                    },
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
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
        let did_move = state.snapshot.workspaces.iter_mut().any(|workspace| {
            workspace.tabs.iter_mut().any(|tab| {
                move_existing_pane_to_split(&mut tab.layout, source_pane, target_pane, placement)
            })
        });
        if !did_move {
            bail!("source and target terminals must exist in the same workstation layout");
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
                    session: Arc::clone(&replacement_session),
                    last_valid_cwd: cwd,
                    kind: RuntimePaneKind::Local,
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
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
            bail!("both terminals must exist before they can be merged");
        }
        let did_move = state.snapshot.workspaces.iter_mut().any(|workspace| {
            workspace
                .tabs
                .iter_mut()
                .any(|tab| move_existing_pane_to_tab(&mut tab.layout, source_pane, target_pane))
        });
        if !did_move {
            bail!("source and target terminals must exist in the same workstation layout");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
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
        resolve_pane_identity(pane, None, None);
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
        let (title_signal, command_profile) = state
            .panes
            .get(&pane_id)
            .map(|runtime| {
                (
                    runtime.session.terminal_title(),
                    runtime.detected_command_profile,
                )
            })
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        // Choosing an identity in the context menu is an explicit correction
        // of any prior free-form name. The resolver itself still preserves
        // rename precedence for compatible recovered states containing both.
        pane.custom_title = None;
        pane.profile_override = profile;
        resolve_pane_identity(pane, title_signal.as_deref(), command_profile);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn reset_pane_identity(&self, pane_id: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let (title_signal, command_profile) = state
            .panes
            .get(&pane_id)
            .map(|runtime| {
                (
                    runtime.session.terminal_title(),
                    runtime.detected_command_profile,
                )
            })
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.custom_title = None;
        pane.profile_override = None;
        resolve_pane_identity(pane, title_signal.as_deref(), command_profile);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn close_pane(&self, pane_id: Uuid) -> Result<()> {
        let session = {
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
                .context("pane process is missing")?;
            let session = Arc::clone(&runtime.session);
            let shell_label = runtime.kind.shell_label();
            set_pane_runtime_label(
                &mut state.snapshot,
                pane_id,
                false,
                Some("terminating"),
                &shell_label,
            );
            state.snapshot.revision += 1;
            {
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            };
            session
        };
        session.terminate_and_wait()?;

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
                workspace.tabs.remove(tab_index);
            }
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_sub(1);
            // Closing terminals is not a disconnect: an SSH workstation with
            // zero tabs stays connected so the next terminal just opens.
            did_close = true;
            break;
        }
        if !did_close {
            bail!("pane {pane_id} disappeared while waiting for process exit");
        }
        let removed = state.panes.remove(&pane_id);
        state.snapshot.revision += 1;
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
            let runtime = state
                .panes
                .get(&pane_id)
                .with_context(|| format!("pane {pane_id} does not exist"))?;
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
                PtySession::spawn_ssh(pane_id, workspace_id, host, &self.history)?
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
        let Some(runtime) = state.panes.get_mut(&pane_id) else {
            let _ = session.terminate_and_wait();
            bail!("pane {pane_id} disappeared while reattaching");
        };
        let previous = std::mem::replace(&mut runtime.session, session);
        runtime.exit_status = None;
        runtime.recovered = false;
        let shell_label = kind.shell_label();
        let _ = previous.terminate_and_wait();
        drop(previous);
        set_pane_runtime_label(&mut state.snapshot, pane_id, false, None, &shell_label);
        refresh_workspace_activity(&mut state);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
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
                tabs: vec![Tab {
                    id: tab_id,
                    title: "Terminals".to_owned(),
                    custom_title: None,
                    layout: PaneLayout::Leaf { pane },
                }],
            });
            state.panes.insert(
                pane_id,
                RuntimePane {
                    session: Arc::clone(&session),
                    last_valid_cwd: cwd,
                    kind: RuntimePaneKind::Local,
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
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
        let session = PtySession::spawn_ssh(ids.pane, ids.workspace, destination, &self.history)?;
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
            title: format!("SSH {destination}"),
            shell: "ssh".to_owned(),
            color: None,
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
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
            tabs: vec![Tab {
                id: ids.tab,
                title: "Remote".to_owned(),
                custom_title: None,
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
                session,
                last_valid_cwd: cwd,
                kind: RuntimePaneKind::SystemSsh {
                    host: destination.to_owned(),
                },
                recovered: false,
                exit_status: None,
                detected_command_profile: None,
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
                    state.panes.get(&pane_id).and_then(|runtime| {
                        matches!(runtime.kind, RuntimePaneKind::SystemSsh { .. })
                            .then(|| (pane_id, Arc::clone(&runtime.session)))
                    })
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
            if let Some(runtime) = state.panes.get_mut(&pane_id) {
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
        let (destination, mut pane_ids) = {
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
                        matches!(runtime.kind, RuntimePaneKind::SystemSsh { .. })
                            && runtime.exit_status.is_some()
                    })
                })
                .collect::<Vec<_>>();
            (destination.clone(), pane_ids)
        };
        validate_ssh_host(&destination).map_err(|message| anyhow!(message))?;
        let created_layout = pane_ids.is_empty();
        if created_layout {
            pane_ids.push(Uuid::new_v4());
        }
        let mut sessions = Vec::with_capacity(pane_ids.len());
        for pane_id in &pane_ids {
            match PtySession::spawn_ssh(*pane_id, workspace_id, &destination, &self.history) {
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
                    title: format!("SSH {destination}"),
                    shell: "ssh".to_owned(),
                    color: None,
                    identity: TerminalIdentity::default(),
                    custom_title: None,
                    profile_override: None,
                };
                workspace.tabs.push(Tab {
                    id: Uuid::new_v4(),
                    title: "Remote".to_owned(),
                    custom_title: None,
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
                        session: Arc::clone(session),
                        last_valid_cwd: cwd.clone(),
                        kind: RuntimePaneKind::SystemSsh {
                            host: destination.clone(),
                        },
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
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
        let sessions = {
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
            pane_ids_for_workspace(workspace)
                .into_iter()
                .filter_map(|pane_id| {
                    state
                        .panes
                        .get(&pane_id)
                        .map(|runtime| (pane_id, Arc::clone(&runtime.session)))
                })
                .collect::<Vec<_>>()
        };
        for (_, session) in &sessions {
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
        let removed = sessions
            .into_iter()
            .filter_map(|(pane_id, _)| state.panes.remove(&pane_id))
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

    /// Performs an explicit bounded metadata-only scan of the default tmux
    /// server for one workstation. It never starts tmux, reconnects a saved
    /// SSH workstation, or writes scan output to terminal history.
    pub fn scan_tmux_sessions(&self, workspace_id: Uuid) -> Result<TmuxScanResult> {
        let connection = self.workspace_connection(workspace_id)?;
        let (scope, probe) = match connection {
            WorkspaceConnection::Local => (TmuxScanScope::Local, tmux_local_probe_command()),
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
            WorkspaceConnection::Local => tmux_local_probe_command(),
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
            .filter_map(|pane_id| state.panes.get(&pane_id))
            .filter(|runtime| runtime.exit_status.is_none())
            .filter_map(|runtime| runtime.kind.tmux_session_id().cloned())
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
                .filter_map(|pane_id| state.panes.get(&pane_id))
                .filter(|runtime| runtime.exit_status.is_none())
                .filter_map(|runtime| runtime.kind.tmux_session_id())
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
                layout: PaneLayout::Leaf {
                    pane: Pane {
                        id: pane_id,
                        title: format!("tmux {}", tmux_session.name),
                        shell: "tmux".to_owned(),
                        color: None,
                        identity: TerminalIdentity::default(),
                        custom_title: None,
                        profile_override: None,
                    },
                },
            });
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                RuntimePane {
                    session: Arc::clone(session),
                    last_valid_cwd: fallback_cwd()?,
                    kind,
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
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
        self.state
            .read()
            .panes
            .get(&pane_id)
            .map(|runtime| Arc::clone(&runtime.session))
            .with_context(|| format!("pane {pane_id} does not exist"))
    }
    fn cwd_for_pane(&self, pane_id: Uuid) -> Result<PathBuf> {
        let mut state = self.state.write();
        refresh_runtime_metadata(&mut state, false)?;
        let runtime = state
            .panes
            .get(&pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
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
    ) -> Result<(Arc<PtySession>, RuntimePaneKind)> {
        let kind = runtime_kind_for_workspace(&self.workspace_connection(workspace_id)?);
        let session = match &kind {
            RuntimePaneKind::Local => {
                PtySession::spawn_local(pane_id, workspace_id, cwd, &self.history)?
            }
            RuntimePaneKind::SystemSsh { host } => {
                PtySession::spawn_ssh(pane_id, workspace_id, host, &self.history)?
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
        .filter_map(|(pane_id, runtime)| runtime.kind.is_runtime_only().then_some(*pane_id))
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
        .filter(|(_, runtime)| runtime.kind.is_local())
        .map(|(pane_id, runtime)| (*pane_id, runtime.last_valid_cwd.clone()))
        .collect();
    SnapshotStore::encode(&snapshot, &cwd_by_pane)
}

fn refresh_runtime_metadata(state: &mut RegistryState, force_process_refresh: bool) -> Result<()> {
    let pids = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| {
            runtime
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
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    let identity_inputs = state
        .panes
        .iter()
        .filter(|(_, runtime)| runtime.kind.is_local())
        .map(|(pane_id, runtime)| {
            (
                *pane_id,
                runtime.session.terminal_title(),
                runtime.detected_command_profile,
            )
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
                let Some(runtime) = state.panes.get(&pane_id) else {
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
        runtime.kind.is_local()
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
        .filter(|runtime| runtime.kind.is_local())
    {
        runtime.detected_command_profile = runtime
            .session
            .process_id()
            .map(Pid::from_u32)
            .and_then(|root| discover_descendant_profile(&system, root));
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
    let (profile, source, title) = if let Some(custom_title) = pane.custom_title.as_deref() {
        (
            TerminalProfile::Terminal,
            TerminalIdentitySource::UserRename,
            custom_title.to_owned(),
        )
    } else if let Some(profile) = pane.profile_override {
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
    for runtime in panes.values() {
        let _ = runtime.session.terminate_and_wait();
    }
}

fn fallback_cwd() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| valid_local_cwd(path))
        .context("HOME does not name an accessible local directory")?;
    Ok(home)
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

fn system_ssh_command(pane_id: Uuid, host: &str) -> Result<CommandBuilder> {
    system_ssh_command_with(system_ssh_binary()?, pane_id, host)
}

fn system_ssh_command_with(
    binary: impl AsRef<OsStr>,
    pane_id: Uuid,
    host: &str,
) -> Result<CommandBuilder> {
    validate_ssh_host(host).map_err(|message| anyhow!(message))?;
    Ok(command_with_terminal_env(
        [
            binary.as_ref().to_owned(),
            OsString::from("--"),
            OsString::from(host),
        ],
        pane_id,
    ))
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
    command.env(nah_protocol::pane_id_env(), pane_id);
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
struct TmuxProbeOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn tmux_local_probe_command() -> Command {
    let mut command = Command::new("tmux");
    command
        .args(["list-sessions", "-F", TMUX_SESSION_LIST_FORMAT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
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

fn run_tmux_probe(command: Command) -> Result<TmuxProbeOutput> {
    run_tmux_probe_with_timeout(command, TMUX_PROBE_TIMEOUT)
}

fn run_tmux_probe_with_timeout(mut command: Command, timeout: Duration) -> Result<TmuxProbeOutput> {
    let mut child = command.spawn().context("start explicit tmux scan")?;
    let stdout = child
        .stdout
        .take()
        .context("tmux scan stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("tmux scan stderr was not piped")?;
    let stdout_reader = thread::spawn(move || read_limited_probe_output(stdout));
    let stderr_reader = thread::spawn(move || read_limited_probe_output(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("observe tmux scan")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("tmux scan timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = join_probe_reader(stdout_reader, "stdout")?;
    let stderr = join_probe_reader(stderr_reader, "stderr")?;
    Ok(TmuxProbeOutput {
        success: status.success(),
        stdout: String::from_utf8(stdout).context("tmux scan output was not UTF-8")?,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn read_limited_probe_output(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut overflow = false;
    loop {
        let read = reader.read(&mut buffer).context("read tmux scan output")?;
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
        bail!("tmux scan output exceeded {TMUX_PROBE_MAX_BYTES} bytes");
    }
    Ok(output)
}

fn join_probe_reader(reader: thread::JoinHandle<Result<Vec<u8>>>, stream: &str) -> Result<Vec<u8>> {
    let output = reader
        .join()
        .map_err(|_| anyhow!("tmux scan {stream} reader panicked"))??;
    Ok(output)
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
    let hello: ClientRequest = read_message(&mut stream)
        .await
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
        let request = match read_message(&mut stream).await {
            Ok(request) => request,
            Err(nah_protocol::WireError::Closed) => return Ok(()),
            Err(error) => return Err(error).context("read client request"),
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
        write_message(&mut stream, &response)
            .await
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
        } => handle_get_updates(
            sessions,
            snapshot_revision,
            &pane_revisions,
            &subscribed_panes,
        ),
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
        ClientRequest::CreateWorkspaceGroup { workspace_id } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_workspace_group(workspace_id)?,
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
        ClientRequest::ResetPaneIdentity { pane_id } => {
            sessions.reset_pane_identity(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ClosePane { pane_id } => {
            sessions.close_pane(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReattachPane { pane_id } => {
            sessions.reattach_pane(pane_id)?;
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
        ClientRequest::SetWorkspaceColor {
            workspace_id,
            color,
        } => {
            sessions.set_workspace_color(workspace_id, color)?;
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
) -> Result<ServiceResponse> {
    let update =
        sessions.pane_updates(snapshot_revision, pane_revisions, subscribed_panes, false)?;
    Ok(ServiceResponse::Updates {
        session_revision: update.session_revision,
        snapshot: update.snapshot,
        screens: update.screens,
        pane_states: update.pane_states,
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
) -> Result<(), nah_protocol::WireError> {
    let frame = nah_protocol::encode_frame(message)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_message<T: DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, nah_protocol::WireError> {
    let mut length = [0_u8; 4];
    if let Err(error) = stream.read_exact(&mut length).await {
        return if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Err(nah_protocol::WireError::Closed)
        } else {
            Err(nah_protocol::WireError::Io(error))
        };
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(nah_protocol::WireError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    nah_protocol::decode_frame(&payload)
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
            system_ssh_command_with("/usr/bin/ssh", Uuid::nil(), "admin@prod-east").unwrap();

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
    fn invalid_ssh_destinations_are_rejected_before_command_construction() {
        for host in [
            "-oProxyCommand=bad",
            "user@@host",
            "host command",
            "host\n-A",
        ] {
            assert!(
                system_ssh_command_with("/usr/bin/ssh", Uuid::nil(), host).is_err(),
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
            id,
            title: format!("Terminal {id}"),
            shell: "shell".to_owned(),
            color: None,
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
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
        let directory =
            std::env::temp_dir().join(format!("nah-tmux-group-test-{}", Uuid::new_v4()));
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
        let directory =
            std::env::temp_dir().join(format!("nah-group-name-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;

        let pane_id = registry.create_workspace_group(workspace_id).unwrap();
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
            std::env::temp_dir().join(format!("nah-ssh-workstation-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("sessions.json");

        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let before = registry.snapshot().unwrap();
        let (workspace_id, _) = registry
            .create_simulated_ssh_workspace(Some("Safe local simulation"), "test@local-host")
            .unwrap();

        let update = registry
            .pane_updates(Some(before.revision), &[], &[], true)
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
        let directory = std::env::temp_dir().join(format!(
            "nah-ssh-workstation-intent-test-{}",
            Uuid::new_v4()
        ));
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
    fn identity_precedence_is_rename_then_profile_then_command_then_fallback() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let pane = find_pane_mut_in_snapshot(&mut snapshot, pane_id).unwrap();
        pane.custom_title = Some("Release console".to_owned());
        pane.profile_override = Some(TerminalProfile::Hermes);

        resolve_pane_identity(pane, Some("Claude Code"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Release console");
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserRename);

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
    fn manual_profile_correction_and_reset_update_only_safe_desired_identity_fields() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry.rename_pane(pane_id, "My work").unwrap();
        registry
            .set_pane_profile(pane_id, Some(TerminalProfile::Claude))
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert_eq!(pane.title, "Claude Code");
        assert_eq!(pane.custom_title, None);
        assert_eq!(pane.profile_override, Some(TerminalProfile::Claude));
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserProfile);

        registry.reset_pane_identity(pane_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert_eq!(pane.custom_title, None);
        assert_eq!(pane.profile_override, None);
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
            state.panes.get_mut(&pane_id).unwrap().kind = RuntimePaneKind::SystemSsh {
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
            state.panes.get_mut(&pane_id).unwrap().kind = RuntimePaneKind::SystemSsh {
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
            state.panes.get_mut(&pane_id).unwrap().kind = RuntimePaneKind::TmuxSystemSsh {
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

            state.panes.get_mut(&pane_id).unwrap().exit_status =
                Some("Exited with code 255".to_owned());
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
            let runtime = state.panes.get_mut(&pane_id).unwrap();
            runtime.exit_status = Some("Exited with code 255".to_owned());
            Arc::clone(&runtime.session)
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
            registry
                .state
                .read()
                .panes
                .get(&pane_id)
                .map(|runtime| runtime.exit_status.clone()),
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
