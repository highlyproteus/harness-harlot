#![allow(clippy::missing_errors_doc)]

mod persistence;

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rust_mux_protocol::{
    AppearanceColor, ClientRequest, DropPlacement, MAX_FRAME_SIZE, PROTOCOL_VERSION, Pane,
    PaneLayout, ServiceResponse, SessionSnapshot, SplitAxis, Tab, TerminalModes, TerminalModifiers,
    TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalScreen, TerminalSelectionKind,
    Workspace, validate_ssh_host,
};
use rust_mux_terminal_model::TerminalModel;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use uuid::Uuid;

use crate::persistence::{SnapshotStore, default_snapshot_path};

const INITIAL_COLUMNS: u16 = 100;
const INITIAL_ROWS: u16 = 30;
const MAX_INPUT_FRAME: usize = 64 * 1024;
const MAX_PANES: usize = 32;
const MAX_RECENT_COLORS: usize = 8;

struct PtySession {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    terminal: Arc<Mutex<TerminalModel>>,
    revision: Arc<AtomicU64>,
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
        let Ok(child) = self.child.get_mut() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

impl PtySession {
    fn spawn_local(pane_id: Uuid, cwd: &Path) -> Result<Arc<Self>> {
        let shell = configured_shell();
        Self::spawn_command(
            pane_id,
            local_shell_command(pane_id, cwd),
            &format!("configured shell {shell}"),
        )
    }

    fn spawn_ssh(pane_id: Uuid, host: &str) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            system_ssh_command(pane_id, host)?,
            "system OpenSSH",
        )
    }

    fn spawn_command(
        pane_id: Uuid,
        command: CommandBuilder,
        description: &str,
    ) -> Result<Arc<Self>> {
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
        thread::Builder::new()
            .name(format!("rmux-pty-{pane_id}"))
            .spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if let Ok(mut terminal) = reader_terminal.lock() {
                                terminal.process_output(&buffer[..read]);
                                reader_revision.fetch_add(1, Ordering::Release);
                            } else {
                                break;
                            }
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
            terminal,
            revision,
        }))
    }

    fn write_input(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_INPUT_FRAME {
            bail!("terminal input exceeds {MAX_INPUT_FRAME} bytes");
        }
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow!("PTY writer lock was poisoned"))?;
        writer.write_all(bytes).context("write terminal input")?;
        writer.flush().context("flush terminal input")?;
        Ok(())
    }

    fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        let columns = columns.max(1);
        let rows = rows.max(1);
        self.master
            .lock()
            .map_err(|_| anyhow!("PTY master lock was poisoned"))?
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize PTY")?;
        self.terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .resize(usize::from(columns), usize::from(rows));
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn screen(&self, pane_id: Uuid) -> Result<TerminalScreen> {
        let terminal = self
            .terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?;
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

    fn begin_selection(&self, point: TerminalPoint, kind: TerminalSelectionKind) -> Result<()> {
        self.terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .begin_selection(point, kind);
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn update_selection(&self, point: TerminalPoint) -> Result<()> {
        self.terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .update_selection(point);
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn clear_selection(&self) -> Result<()> {
        self.terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .clear_selection();
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn selected_text(&self) -> Result<Option<String>> {
        Ok(self
            .terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .selected_text())
    }

    fn scroll(&self, lines: i32) -> Result<()> {
        self.terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .scroll(lines.clamp(-10_000, 10_000));
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn search_literal(&self, query: &str, forward: bool) -> Result<bool> {
        if query.chars().count() > 256 || query.chars().any(char::is_control) {
            bail!("terminal search must be at most 256 visible characters");
        }
        let found = self
            .terminal
            .lock()
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .search_literal(query, forward);
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
            .map_err(|_| anyhow!("terminal grid lock was poisoned"))?
            .mouse_report(point, button, action, modifiers);
        if let Some(report) = report {
            self.write_input(&report)?;
        }
        Ok(())
    }

    fn terminate_and_wait(&self) -> Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow!("PTY child lock was poisoned"))?;
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
            .map_err(|_| anyhow!("PTY child lock was poisoned"))?
            .try_wait()
            .map(|status| status.map(|status| status.to_string()))
            .context("observe PTY child exit")
    }

    fn process_id(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|child| child.process_id())
    }
}

#[derive(Debug)]
struct RuntimePane {
    session: Arc<PtySession>,
    last_valid_cwd: PathBuf,
    kind: RuntimePaneKind,
    recovered: bool,
    exit_status: Option<String>,
}

#[derive(Debug)]
enum RuntimePaneKind {
    Local,
    SystemSsh { host: String },
}

impl RuntimePaneKind {
    fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    fn shell_label(&self) -> String {
        match self {
            Self::Local => shell_title(),
            Self::SystemSsh { host } => format!("ssh {host}"),
        }
    }
}

#[derive(Debug)]
struct RegistryState {
    snapshot: SessionSnapshot,
    panes: HashMap<Uuid, RuntimePane>,
    next_terminal_number: u32,
    store: Option<SnapshotStore>,
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
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRegistry {
    state: Arc<RwLock<RegistryState>>,
}

impl SessionRegistry {
    pub fn new() -> Result<Self> {
        Self::seeded(None)
    }

    pub fn load_default() -> Result<Self> {
        Self::persistent(default_snapshot_path()?)
    }

    pub fn persistent(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.is_absolute() {
            bail!("recovery snapshot path must be absolute");
        }
        let store = SnapshotStore::new(path);
        let Some(mut recovered) = store.load_or_quarantine()? else {
            let registry = Self::seeded(Some(store))?;
            registry.persist()?;
            return Ok(registry);
        };

        let fallback = fallback_cwd()?;
        let pane_ids = pane_ids_in_snapshot(&recovered.snapshot);
        let mut panes = HashMap::new();
        for pane_id in pane_ids {
            let cwd = recovered
                .cwd_by_pane
                .remove(&pane_id)
                .filter(|cwd| valid_local_cwd(cwd))
                .unwrap_or_else(|| fallback.clone());
            match PtySession::spawn_local(pane_id, &cwd) {
                Ok(session) => {
                    panes.insert(
                        pane_id,
                        RuntimePane {
                            session,
                            last_valid_cwd: cwd,
                            kind: RuntimePaneKind::Local,
                            recovered: true,
                            exit_status: None,
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
                store: Some(store),
            })),
        };
        registry.persist()?;
        Ok(registry)
    }

    fn seeded(store: Option<SnapshotStore>) -> Result<Self> {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).context("seeded snapshot has no pane")?;
        if let Some(pane) = find_pane_mut_in_snapshot(&mut snapshot, pane_id) {
            pane.shell = shell_title();
        }
        let cwd = fallback_cwd()?;
        let session = PtySession::spawn_local(pane_id, &cwd)?;
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
                    },
                )]),
                next_terminal_number: 2,
                store,
            })),
        })
    }

    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        refresh_runtime_metadata(&mut state)?;
        Ok(state.snapshot.clone())
    }

    pub fn state(&self) -> Result<(SessionSnapshot, Vec<TerminalScreen>)> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        refresh_runtime_metadata(&mut state)?;
        let snapshot = state.snapshot.clone();
        let screens = state
            .panes
            .iter()
            .map(|(pane_id, runtime)| runtime.session.screen(*pane_id))
            .collect::<Result<Vec<_>>>()?;
        Ok((snapshot, screens))
    }

    pub fn persist(&self) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        refresh_runtime_metadata(&mut state)?;
        persist_state(&state)
    }

    pub fn create_pane(&self, target_pane: Uuid, axis: SplitAxis) -> Result<Uuid> {
        let new_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(target_pane)?;
        let session = PtySession::spawn_local(new_id, &cwd)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let new_pane = state.new_pane(new_id);
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
                kind: RuntimePaneKind::Local,
                recovered: false,
                exit_status: None,
            },
        );
        state.snapshot.revision += 1;
        persist_state(&state)?;
        Ok(new_id)
    }

    pub fn create_tab(&self, target_pane: Uuid) -> Result<Uuid> {
        let new_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(target_pane)?;
        let session = PtySession::spawn_local(new_id, &cwd)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let pane = state.new_pane(new_id);
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
                kind: RuntimePaneKind::Local,
                recovered: false,
                exit_status: None,
            },
        );
        state.snapshot.revision += 1;
        persist_state(&state)?;
        Ok(new_id)
    }

    /// Starts the installed OpenSSH client only for an explicit, validated
    /// destination and places it in the target pane's tab strip.
    pub fn connect_ssh(&self, target_pane: Uuid, host: &str) -> Result<Uuid> {
        validate_ssh_host(host).map_err(|message| anyhow!(message))?;
        {
            let state = self
                .state
                .read()
                .map_err(|_| anyhow!("session state lock was poisoned"))?;
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
        let session = PtySession::spawn_ssh(pane_id, host)?;
        let result = (|| {
            let mut state = self
                .state
                .write()
                .map_err(|_| anyhow!("session state lock was poisoned"))?;
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let pane = Pane {
                id: pane_id,
                title: format!("SSH {host}"),
                shell: "ssh".to_owned(),
                color: None,
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
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
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
        persist_state(&state)?;
        Ok(())
    }

    pub fn swap_panes(&self, source_pane: Uuid, target_pane: Uuid) -> Result<()> {
        if source_pane == target_pane {
            return Ok(());
        }
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
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
            bail!("panes can only be rearranged inside the same workspace layout");
        }
        state.snapshot.revision += 1;
        persist_state(&state)?;
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
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        let did_move = state.snapshot.workspaces.iter_mut().any(|workspace| {
            workspace.tabs.iter_mut().any(|tab| {
                move_existing_pane_to_split(&mut tab.layout, source_pane, target_pane, placement)
            })
        });
        if !did_move {
            bail!("source and target terminals must exist in the same workspace layout");
        }
        state.snapshot.revision += 1;
        persist_state(&state)?;
        Ok(())
    }

    fn split_lone_pane_with_replacement(
        &self,
        pane_id: Uuid,
        placement: DropPlacement,
    ) -> Result<()> {
        let replacement_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(pane_id)?;
        let replacement_session = PtySession::spawn_local(replacement_id, &cwd)?;
        let result = (|| {
            let mut state = self
                .state
                .write()
                .map_err(|_| anyhow!("session state lock was poisoned"))?;
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
                },
            );
            state.snapshot.revision += 1;
            persist_state(&state)?;
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
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
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
            bail!("source and target terminals must exist in the same workspace layout");
        }
        state.snapshot.revision += 1;
        persist_state(&state)?;
        Ok(())
    }

    pub fn rename_pane(&self, pane_id: Uuid, title: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
            bail!("terminal name must be 1 to 80 visible characters");
        }
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        title.clone_into(&mut pane.title);
        state.snapshot.revision += 1;
        persist_state(&state)?;
        Ok(())
    }

    pub fn close_pane(&self, pane_id: Uuid) -> Result<()> {
        let session = {
            let mut state = self
                .state
                .write()
                .map_err(|_| anyhow!("session state lock was poisoned"))?;
            let pane_exists = state.snapshot.workspaces.iter().any(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|tab| layout_contains(&tab.layout, pane_id))
            });
            if !pane_exists {
                bail!("pane {pane_id} does not exist");
            }
            let can_close = state.snapshot.workspaces.iter().any(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|tab| layout_contains(&tab.layout, pane_id) && pane_count(&tab.layout) > 1)
            });
            if !can_close {
                bail!("a workspace must keep at least one terminal");
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
            persist_state(&state)?;
            session
        };
        session.terminate_and_wait()?;

        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        let mut did_close = false;
        for workspace in &mut state.snapshot.workspaces {
            let Some(tab) = workspace
                .tabs
                .iter_mut()
                .find(|tab| layout_contains(&tab.layout, pane_id))
            else {
                continue;
            };
            let (_, remaining) = detach_pane(tab.layout.clone(), pane_id);
            tab.layout = remaining.context("closing terminal produced an empty layout")?;
            did_close = true;
            break;
        }
        if !did_close {
            bail!("pane {pane_id} disappeared while waiting for process exit");
        }
        state.panes.remove(&pane_id);
        state.snapshot.revision += 1;
        persist_state(&state)
    }

    pub fn set_default_terminal_accent(&self, color: AppearanceColor) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        state.snapshot.appearance.default_terminal_accent = color;
        remember_recent_color(&mut state.snapshot, color);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        persist_state(&state)
    }

    pub fn set_default_workspace_color(&self, color: AppearanceColor) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        state.snapshot.appearance.default_workspace_color = color;
        remember_recent_color(&mut state.snapshot, color);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        persist_state(&state)
    }

    pub fn set_pane_color(&self, pane_id: Uuid, color: Option<AppearanceColor>) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.color = color;
        if let Some(color) = color {
            remember_recent_color(&mut state.snapshot, color);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        persist_state(&state)
    }

    pub fn set_workspace_color(
        &self,
        workspace_id: Uuid,
        color: Option<AppearanceColor>,
    ) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workspace {workspace_id} does not exist"))?;
        workspace.color = color;
        if let Some(color) = color {
            remember_recent_color(&mut state.snapshot, color);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        persist_state(&state)
    }

    pub fn create_workspace(&self, title: Option<String>) -> Result<(Uuid, Uuid)> {
        let workspace_id = Uuid::new_v4();
        let tab_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let cwd = fallback_cwd()?;
        let session = PtySession::spawn_local(pane_id, &cwd)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let number = state.snapshot.workspaces.len() + 1;
        let pane = state.new_pane(pane_id);
        state.snapshot.workspaces.push(Workspace {
            id: workspace_id,
            title: title.unwrap_or_else(|| format!("Workspace {number}")),
            color: None,
            tabs: vec![Tab {
                id: tab_id,
                title: "Terminals".to_owned(),
                layout: PaneLayout::Leaf { pane },
            }],
        });
        state.panes.insert(
            pane_id,
            RuntimePane {
                session,
                last_valid_cwd: cwd,
                kind: RuntimePaneKind::Local,
                recovered: false,
                exit_status: None,
            },
        );
        state.snapshot.revision += 1;
        persist_state(&state)?;
        Ok((workspace_id, pane_id))
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
        self.pane(pane_id)?.begin_selection(point, kind)
    }

    pub fn update_selection(&self, pane_id: Uuid, point: TerminalPoint) -> Result<()> {
        self.pane(pane_id)?.update_selection(point)
    }

    pub fn clear_selection(&self, pane_id: Uuid) -> Result<()> {
        self.pane(pane_id)?.clear_selection()
    }

    pub fn selected_text(&self, pane_id: Uuid) -> Result<Option<String>> {
        self.pane(pane_id)?.selected_text()
    }

    pub fn scroll_pane(&self, pane_id: Uuid, lines: i32) -> Result<()> {
        self.pane(pane_id)?.scroll(lines)
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

    fn pane(&self, pane_id: Uuid) -> Result<Arc<PtySession>> {
        self.state
            .read()
            .map_err(|_| anyhow!("session state lock was poisoned"))?
            .panes
            .get(&pane_id)
            .map(|runtime| Arc::clone(&runtime.session))
            .with_context(|| format!("pane {pane_id} does not exist"))
    }

    fn cwd_for_pane(&self, pane_id: Uuid) -> Result<PathBuf> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow!("session state lock was poisoned"))?;
        refresh_runtime_metadata(&mut state)?;
        let runtime = state
            .panes
            .get(&pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        match &runtime.kind {
            RuntimePaneKind::Local => Ok(runtime.last_valid_cwd.clone()),
            RuntimePaneKind::SystemSsh { .. } => fallback_cwd(),
        }
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new().expect("start seeded configured-shell PTY")
    }
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

fn persist_state(state: &RegistryState) -> Result<()> {
    let Some(store) = &state.store else {
        return Ok(());
    };
    let ephemeral_panes = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| (!runtime.kind.is_local()).then_some(*pane_id))
        .collect::<HashSet<_>>();
    let Some(snapshot) = local_persistence_projection(&state.snapshot, &ephemeral_panes) else {
        // A workspace containing only live remote sessions has no safe,
        // network-silent restart representation. Keep the previous complete
        // local snapshot instead of serializing remote intent or host data.
        return Ok(());
    };
    let cwd_by_pane = state
        .panes
        .iter()
        .filter(|(_, runtime)| runtime.kind.is_local())
        .map(|(pane_id, runtime)| (*pane_id, runtime.last_valid_cwd.clone()))
        .collect();
    store.save(&snapshot, &cwd_by_pane)
}

fn local_persistence_projection(
    snapshot: &SessionSnapshot,
    ephemeral_panes: &HashSet<Uuid>,
) -> Option<SessionSnapshot> {
    let mut snapshot = snapshot.clone();
    for workspace in &mut snapshot.workspaces {
        workspace.tabs.retain_mut(|tab| {
            let mut remaining = Some(tab.layout.clone());
            for pane_id in ephemeral_panes {
                let Some(layout) = remaining.take() else {
                    break;
                };
                let (_, next) = detach_pane(layout, *pane_id);
                remaining = next;
            }
            if let Some(layout) = remaining {
                tab.layout = layout;
                true
            } else {
                false
            }
        });
    }
    snapshot
        .workspaces
        .retain(|workspace| !workspace.tabs.is_empty());
    (!snapshot.workspaces.is_empty()).then_some(snapshot)
}

fn refresh_runtime_metadata(state: &mut RegistryState) -> Result<()> {
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
    let mut system = System::new();
    if !pids.is_empty() {
        let process_ids = pids.iter().map(|(_, pid)| *pid).collect::<Vec<_>>();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&process_ids),
            ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
        );
    }

    let mut labels = Vec::new();
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
    Ok(())
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
    fn collect(layout: &PaneLayout, pane_ids: &mut Vec<Uuid>) {
        match layout {
            PaneLayout::Leaf { pane } => pane_ids.push(pane.id),
            PaneLayout::Stack { panes, .. } => {
                pane_ids.extend(panes.iter().map(|pane| pane.id));
            }
            PaneLayout::Split { first, second, .. } => {
                collect(first, pane_ids);
                collect(second, pane_ids);
            }
        }
    }
    let mut pane_ids = Vec::new();
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            collect(&tab.layout, &mut pane_ids);
        }
    }
    pane_ids
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
    validate_ssh_host(host).map_err(|message| anyhow!(message))?;
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

fn command_with_terminal_env(
    argv: impl IntoIterator<Item = OsString>,
    pane_id: Uuid,
) -> CommandBuilder {
    let mut command = CommandBuilder::from_argv(argv.into_iter().collect());
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("RUST_MUX_PANE_ID", pane_id.to_string());
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

fn pane_count(layout: &PaneLayout) -> usize {
    match layout {
        PaneLayout::Leaf { .. } => 1,
        PaneLayout::Stack { panes, .. } => panes.len(),
        PaneLayout::Split { first, second, .. } => pane_count(first) + pane_count(second),
    }
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
            Err(rust_mux_protocol::WireError::Closed) => return Ok(()),
            Err(error) => return Err(error).context("read client request"),
        };

        let response =
            handle_request(sessions, request).unwrap_or_else(|error| ServiceResponse::Error {
                message: format!("{error:#}"),
            });
        write_message(&mut stream, &response)
            .await
            .context("write service response")?;
    }
}

fn handle_request(sessions: &SessionRegistry, request: ClientRequest) -> Result<ServiceResponse> {
    if matches!(
        &request,
        ClientRequest::BeginSelection { .. }
            | ClientRequest::UpdateSelection { .. }
            | ClientRequest::ClearSelection { .. }
            | ClientRequest::CopySelection { .. }
            | ClientRequest::ScrollPane { .. }
            | ClientRequest::SearchPane { .. }
            | ClientRequest::MouseInput { .. }
    ) {
        return handle_terminal_interaction_request(sessions, request);
    }
    if is_appearance_request(&request) {
        return handle_appearance_request(sessions, &request);
    }
    match request {
        ClientRequest::GetSnapshot => Ok(ServiceResponse::Snapshot {
            snapshot: sessions.snapshot()?,
        }),
        ClientRequest::GetState => {
            let (snapshot, screens) = sessions.state()?;
            Ok(ServiceResponse::State { snapshot, screens })
        }
        ClientRequest::CreatePane { target_pane, axis } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_pane(target_pane, axis)?,
        }),
        ClientRequest::CreateTab { target_pane } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_tab(target_pane)?,
        }),
        ClientRequest::ConnectSsh { target_pane, host } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.connect_ssh(target_pane, &host)?,
        }),
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
        ClientRequest::ClosePane { pane_id } => {
            sessions.close_pane(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateWorkspace { title } => {
            let (workspace_id, pane_id) = sessions.create_workspace(title)?;
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            })
        }
        ClientRequest::WriteInput { pane_id, bytes } => {
            sessions.write_input(pane_id, &bytes)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::BeginSelection { .. }
        | ClientRequest::UpdateSelection { .. }
        | ClientRequest::ClearSelection { .. }
        | ClientRequest::CopySelection { .. }
        | ClientRequest::ScrollPane { .. }
        | ClientRequest::SearchPane { .. }
        | ClientRequest::MouseInput { .. }
        | ClientRequest::SetDefaultTerminalAccent { .. }
        | ClientRequest::SetDefaultWorkspaceColor { .. }
        | ClientRequest::SetPaneColor { .. }
        | ClientRequest::SetWorkspaceColor { .. } => unreachable!("handled above"),
        ClientRequest::ResizePane {
            pane_id,
            columns,
            rows,
        } => {
            sessions.resize_pane(pane_id, columns, rows)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::Hello { .. } => Ok(ServiceResponse::Error {
            message: "hello was already completed".to_owned(),
        }),
    }
}

fn handle_appearance_request(
    sessions: &SessionRegistry,
    request: &ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::SetDefaultTerminalAccent { color } => {
            sessions.set_default_terminal_accent(*color)?;
        }
        ClientRequest::SetDefaultWorkspaceColor { color } => {
            sessions.set_default_workspace_color(*color)?;
        }
        ClientRequest::SetPaneColor { pane_id, color } => {
            sessions.set_pane_color(*pane_id, *color)?;
        }
        ClientRequest::SetWorkspaceColor {
            workspace_id,
            color,
        } => {
            sessions.set_workspace_color(*workspace_id, *color)?;
        }
        _ => unreachable!("only appearance requests are routed here"),
    }
    Ok(ServiceResponse::Ack)
}

fn is_appearance_request(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::SetDefaultTerminalAccent { .. }
            | ClientRequest::SetDefaultWorkspaceColor { .. }
            | ClientRequest::SetPaneColor { .. }
            | ClientRequest::SetWorkspaceColor { .. }
    )
}

fn handle_terminal_interaction_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
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
        _ => unreachable!("only terminal interactions are routed here"),
    }
}

async fn write_message<T: Serialize>(
    stream: &mut UnixStream,
    message: &T,
) -> Result<(), rust_mux_protocol::WireError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(rust_mux_protocol::WireError::FrameTooLarge(payload.len()));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| rust_mux_protocol::WireError::FrameTooLarge(payload.len()))?;
    stream.write_u32(length).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_message<T: DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, rust_mux_protocol::WireError> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(rust_mux_protocol::WireError::Closed);
        }
        Err(error) => return Err(rust_mux_protocol::WireError::Io(error)),
    };
    if length > MAX_FRAME_SIZE {
        return Err(rust_mux_protocol::WireError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
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
        let command = system_ssh_command_with("/usr/bin/ssh", Uuid::nil(), "prod-east").unwrap();

        assert_eq!(
            command.get_argv(),
            &[
                OsString::from("/usr/bin/ssh"),
                OsString::from("--"),
                OsString::from("prod-east"),
            ]
        );
    }

    #[test]
    fn invalid_ssh_destinations_are_rejected_before_command_construction() {
        for host in [
            "-oProxyCommand=bad",
            "user@host",
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
    fn rejected_ssh_intent_does_not_create_or_replace_a_terminal() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();

        assert!(registry.connect_ssh(first, "-A").is_err());

        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert_eq!(registry.state().unwrap().1.len(), 1);
    }

    #[test]
    fn remote_panes_are_excluded_from_the_network_silent_recovery_projection() {
        let mut snapshot = SessionSnapshot::seeded();
        let local = first_pane_id(&snapshot).unwrap();
        let local_pane = find_pane_mut_in_snapshot(&mut snapshot, local)
            .expect("seeded local pane")
            .clone();
        let remote = Uuid::new_v4();
        snapshot.workspaces[0].tabs[0].layout = PaneLayout::Stack {
            panes: vec![
                local_pane,
                Pane {
                    id: remote,
                    title: "SSH private-alias".to_owned(),
                    shell: "ssh".to_owned(),
                    color: None,
                },
            ],
            active: remote,
        };

        let projected = local_persistence_projection(&snapshot, &HashSet::from([remote]))
            .expect("the local pane remains recoverable");

        assert_eq!(pane_ids_in_snapshot(&projected), vec![local]);
        assert!(!format!("{projected:?}").contains("private-alias"));
        assert!(local_persistence_projection(&snapshot, &HashSet::from([local, remote])).is_none());
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
        let second = registry.create_tab(first).unwrap();
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
        let second = registry.create_tab(first).unwrap();
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
        let moved = registry.create_tab(first).unwrap();
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
        let closing = registry.create_tab(first).unwrap();

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
    fn closing_a_terminal_collapses_the_layout_and_keeps_its_neighbor() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_pane(first, SplitAxis::Vertical).unwrap();
        let second_pid = registry.pane_process_id(second).unwrap();

        registry.close_pane(first).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(first_pane_id(&snapshot), Some(second));
        assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
        assert!(registry.pane_process_id(first).is_err());
        assert!(registry.close_pane(second).is_err());
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
        let second_tab = registry.create_tab(second).unwrap();
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
