use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hh_protocol::{
    ClientRequest, Pane, PaneLayout, PaneStreamState, ServiceResponse, SessionNotification,
    SessionSnapshot, TerminalLine,
};
use hh_session_client::SessionClient;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::VoiceUiEvent;
use crate::harness::{Agent, launch_command};
use crate::memory::MemoryBackend;
use crate::threads::{self, ThreadRecord, ThreadRole};

const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;
const DEFAULT_PANE_LINES: usize = 60;
const MAX_PANE_LINES: usize = 200;
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(30);
const SUBPROCESS_REAP_TIMEOUT: Duration = Duration::from_secs(1);
const STDERR_COMPLETION_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_SUBPROCESS_STDERR_BYTES: usize = 16 * 1024;
const MAX_DIRECTORY_MATCHES: usize = 20;
const MAX_LISTED_THREADS: usize = 20;
const MAX_READ_THREAD_TURNS: usize = 30;
const MAX_SEARCH_DEPTH: usize = 4;
const MAX_SEARCH_VISITS: usize = 20_000;
const SEARCH_SKIP_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "Library",
    "Applications",
    "Movies",
    "Music",
    "Pictures",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustTier {
    T0,
    T1,
    T2,
    Meta,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingAction {
    SendInput {
        pane_id: Uuid,
        text: String,
        submit: bool,
    },
    SendKeys {
        pane_id: Uuid,
        keys: Vec<String>,
    },
    CloseTab {
        tab_id: Uuid,
    },
    CloseWorkstation {
        workspace_id: Uuid,
    },
}

impl PendingAction {
    fn description(&self) -> String {
        match self {
            Self::SendInput { pane_id, text, .. } => {
                format!("send potentially destructive input to pane {pane_id}: {text}")
            }
            Self::SendKeys { pane_id, keys } => {
                format!("send keys {} to pane {pane_id}", keys.join(", "))
            }
            Self::CloseTab { tab_id } => format!("close tab {tab_id}"),
            Self::CloseWorkstation { workspace_id } => {
                format!("close workstation {workspace_id}")
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ToolExecutor {
    client: SessionClient,
    attached_workspaces: HashSet<Uuid>,
    voice_created_panes: HashSet<Uuid>,
    pending_approvals: HashMap<u64, PendingAction>,
    ui_only_approvals: HashSet<u64>,
    next_approval_id: u64,
    snapshot: Option<SessionSnapshot>,
    pane_states: HashMap<Uuid, PaneStreamState>,
    notification_buffer: Vec<SessionNotification>,
    snapshot_revision: Option<u64>,
    notification_cursor: u64,
    own_pane: Option<Uuid>,
}

impl ToolExecutor {
    pub(crate) fn connect() -> Result<Self> {
        let mut client = SessionClient::connect().context("connect voice session client")?;
        let notification_cursor = match client.call(&ClientRequest::GetNotifications)? {
            ServiceResponse::Notifications { items } => initial_notification_cursor(&items),
            response => bail!("unexpected GetNotifications response: {response:?}"),
        };
        Ok(Self {
            client,
            attached_workspaces: HashSet::new(),
            voice_created_panes: HashSet::new(),
            pending_approvals: HashMap::new(),
            ui_only_approvals: HashSet::new(),
            next_approval_id: 1,
            snapshot: None,
            pane_states: HashMap::new(),
            notification_buffer: Vec::new(),
            snapshot_revision: None,
            notification_cursor,
            own_pane: None,
        })
    }

    /// Marks a workstation as attached so its panes are addressable without
    /// an explicit `attach_project` call. Used when an assistant pane is
    /// planted into a known workstation.
    pub(crate) fn attach_workspace(&mut self, workspace_id: Uuid) {
        self.attached_workspaces.insert(workspace_id);
    }
    pub(crate) fn set_own_pane(&mut self, pane_id: Uuid) {
        self.own_pane = Some(pane_id);
    }

    pub(crate) fn execute(
        &mut self,
        name: &str,
        arguments: &str,
        memory: &mut dyn MemoryBackend,
        ui: &futures::channel::mpsc::UnboundedSender<VoiceUiEvent>,
    ) -> String {
        let parsed = serde_json::from_str::<Value>(arguments)
            .with_context(|| format!("parse arguments for {name}"))
            .and_then(|arguments| self.execute_value(name, &arguments, memory, ui));
        let output = match parsed {
            Ok(value) => value,
            Err(error) => json!({ "error": format!("{error:#}") }),
        };
        bounded_json(&output)
    }

    fn execute_value(
        &mut self,
        name: &str,
        arguments: &Value,
        memory: &mut dyn MemoryBackend,
        ui: &futures::channel::mpsc::UnboundedSender<VoiceUiEvent>,
    ) -> Result<Value> {
        let tier = classify_tool(name, arguments, &self.voice_created_panes)?;
        let result = match name {
            "list_workstations" => self.list_workstations(),
            "read_pane" => self.read_pane(arguments),
            "check_status" => self.check_status(),
            "attach_project" => self.attach_project(arguments),
            "list_directory" => Self::list_directory(arguments),
            "find_directory" => Self::find_directory(arguments),
            "list_threads" => Ok(Self::list_threads(arguments)),
            "read_thread" => Self::read_thread(arguments),
            "recall_memory" => {
                let query = required_str(arguments, "query")?;
                memory
                    .recall(query)
                    .map(|content| json!({ "content": content }))
            }
            "create_workstation" => self.create_workstation(arguments),
            "open_terminal_tab" => self.open_terminal_tab(arguments),
            "rename_tab" => self.rename_tab(arguments),
            "open_project_tab" => self.open_project_tab(arguments),
            "create_worktree_tab" => self.create_worktree_tab(arguments),
            "launch_agent" => self.launch_agent(arguments),
            "send_input" if tier == TrustTier::T1 => self.send_input_now(arguments),
            "send_input" => Ok(self.request_approval(pending_send_input(arguments)?, false, ui)),
            "send_keys" => Ok(self.request_approval(pending_send_keys(arguments)?, false, ui)),
            "close_tab" => {
                let tab_id = required_uuid(arguments, "tab_id")?;
                let ui_only = self.tab_contains_own_pane(tab_id)?;
                Ok(self.request_approval(PendingAction::CloseTab { tab_id }, ui_only, ui))
            }
            "close_workstation" => {
                let workspace_id = required_uuid(arguments, "workspace_id")?;
                let ui_only = self.workspace_contains_own_pane(workspace_id);
                Ok(self.request_approval(
                    PendingAction::CloseWorkstation { workspace_id },
                    ui_only,
                    ui,
                ))
            }
            "approve_action" => {
                let approval_id = required_u64(arguments, "approval_id")?;
                let approved = required_bool(arguments, "approved")?;
                self.resolve_approval(approval_id, approved, false, ui)
            }
            _ => bail!("unknown voice tool {name}"),
        }?;
        let _ = ui.unbounded_send(VoiceUiEvent::ToolCall {
            name: name.to_owned(),
            summary: summarize_output(&result),
        });
        Ok(result)
    }

    pub(crate) fn resolve_approval(
        &mut self,
        approval_id: u64,
        approved: bool,
        via_ui: bool,
        ui: &futures::channel::mpsc::UnboundedSender<VoiceUiEvent>,
    ) -> Result<Value> {
        let ui_only = self.ui_only_approvals.contains(&approval_id);
        if approval_blocked_for_voice(ui_only, via_ui, approved) {
            return Ok(json!({
                "status": "requires_ui_approval",
                "approval_id": approval_id,
                "note": "tell the user to click Approve in the pane; a spoken yes cannot close the assistant's own tab",
            }));
        }
        let action = self
            .pending_approvals
            .remove(&approval_id)
            .with_context(|| format!("approval {approval_id} is not pending"))?;
        self.ui_only_approvals.remove(&approval_id);
        let result = if approved {
            self.execute_pending(action)?
        } else {
            json!({ "status": "denied", "approval_id": approval_id })
        };
        let _ = ui.unbounded_send(VoiceUiEvent::ApprovalResolved {
            id: approval_id,
            approved,
        });
        Ok(result)
    }

    pub(crate) fn poll_updates(&mut self) -> Result<Vec<SessionNotification>> {
        let response = self.client.call(&ClientRequest::GetUpdates {
            snapshot_revision: self.snapshot_revision,
            pane_revisions: Vec::new(),
            subscribed_panes: Vec::new(),
            notifications_after: self.notification_cursor,
        })?;
        let ServiceResponse::Updates {
            session_revision,
            snapshot,
            pane_states,
            notifications,
            ..
        } = response
        else {
            bail!("unexpected GetUpdates response: {response:?}");
        };
        self.snapshot_revision = Some(session_revision);
        if let Some(snapshot) = snapshot {
            self.snapshot = Some(snapshot);
        }
        for state in pane_states {
            self.pane_states.insert(state.pane_id, state);
        }
        if let Some(cursor) = notifications
            .iter()
            .map(|notification| notification.id)
            .max()
        {
            self.notification_cursor = self.notification_cursor.max(cursor);
        }
        self.notification_buffer
            .extend(notifications.iter().cloned());
        Ok(notifications)
    }

    pub(crate) fn notification_is_attached(&self, notification: &SessionNotification) -> bool {
        self.attached_workspaces
            .contains(&notification.workspace_id)
            || self.voice_created_panes.contains(&notification.pane_id)
    }

    pub(crate) fn has_pending_approvals(&self) -> bool {
        !self.pending_approvals.is_empty()
    }

    fn ensure_snapshot(&mut self) -> Result<&SessionSnapshot> {
        if self.snapshot.is_none() {
            let response = self.client.call(&ClientRequest::GetSnapshot)?;
            let ServiceResponse::Snapshot { snapshot } = response else {
                bail!("unexpected GetSnapshot response: {response:?}");
            };
            self.snapshot_revision = Some(snapshot.revision);
            self.snapshot = Some(snapshot);
        }
        Ok(self.snapshot.as_ref().expect("snapshot initialized"))
    }

    fn list_workstations(&mut self) -> Result<Value> {
        let snapshot = self.ensure_snapshot()?.clone();
        let pane_states = self.pane_states.clone();
        Ok(snapshot_summary(&snapshot, &pane_states))
    }

    fn read_pane(&mut self, arguments: &Value) -> Result<Value> {
        let pane_id = required_uuid(arguments, "pane_id")?;
        let lines = arguments
            .get("lines")
            .and_then(Value::as_u64)
            .and_then(|lines| usize::try_from(lines).ok())
            .unwrap_or(DEFAULT_PANE_LINES)
            .clamp(1, MAX_PANE_LINES);
        let response = self
            .client
            .call(&ClientRequest::GetPaneSnapshot { pane_id })?;
        let ServiceResponse::PaneSnapshot { screen, .. } = response else {
            bail!("unexpected GetPaneSnapshot response: {response:?}");
        };
        let mut text_lines = screen
            .lines
            .iter()
            .map(terminal_line_text)
            .collect::<Vec<_>>();
        while text_lines.last().is_some_and(String::is_empty) {
            text_lines.pop();
        }
        let start = text_lines.len().saturating_sub(lines);
        Ok(json!({
            "pane_id": pane_id,
            "text": text_lines[start..].join("\n"),
        }))
    }

    fn check_status(&mut self) -> Result<Value> {
        let snapshot = self.ensure_snapshot()?.clone();
        let statuses = snapshot
            .workspaces
            .iter()
            .filter(|workspace| self.attached_workspaces.contains(&workspace.id))
            .flat_map(|workspace| {
                workspace.tabs.iter().flat_map(move |tab| {
                    let mut panes = Vec::new();
                    collect_panes(&tab.layout, &mut panes);
                    panes.into_iter().map(move |pane| {
                        json!({
                            "workspace_id": workspace.id,
                            "workspace": workspace.title,
                            "pane_id": pane.id,
                            "pane": pane.title,
                            "profile": pane.identity.profile,
                            "status": pane.status,
                        })
                    })
                })
            })
            .collect::<Vec<_>>();
        let notifications = std::mem::take(&mut self.notification_buffer);
        Ok(json!({ "panes": statuses, "notifications": notifications }))
    }

    fn attach_project(&mut self, arguments: &Value) -> Result<Value> {
        let workspace_id = required_uuid(arguments, "workspace_id")?;
        let (title, working_dir) = {
            let workspace = terminal_workspace(self.ensure_snapshot()?, workspace_id)?;
            (workspace.title.clone(), workspace.working_dir.clone())
        };
        self.attached_workspaces.insert(workspace_id);
        Ok(json!({
            "workspace_id": workspace_id,
            "title": title,
            "working_dir": working_dir,
            "attached": true,
        }))
    }

    fn list_directory(arguments: &Value) -> Result<Value> {
        let path = arguments.get("path").and_then(Value::as_str).unwrap_or("~");
        let canonical = canonical_existing_directory(path)?;
        let (directories, truncated) = subdirectory_names(&canonical)?;
        Ok(json!({
            "path": canonical,
            "directories": directories,
            "truncated": truncated,
        }))
    }

    fn find_directory(arguments: &Value) -> Result<Value> {
        let query = required_str(arguments, "query")?;
        let home = std::env::var("HOME").context("HOME is not set")?;
        let matches = find_directories(Path::new(&home), query);
        let truncated = matches.len() > MAX_DIRECTORY_MATCHES;
        let matches = matches
            .into_iter()
            .take(MAX_DIRECTORY_MATCHES)
            .map(|path| json!({ "path": path }))
            .collect::<Vec<_>>();
        Ok(json!({ "query": query, "matches": matches, "truncated": truncated }))
    }

    fn list_threads(_arguments: &Value) -> Value {
        let threads = threads::list_threads();
        let truncated = threads.len() > MAX_LISTED_THREADS;
        let threads = threads
            .into_iter()
            .take(MAX_LISTED_THREADS)
            .map(|thread| {
                json!({
                    "thread_id": thread.thread_id,
                    "title": thread.title,
                    "workspace": thread.workspace_title,
                    "last_active_ms": thread.last_at_ms,
                    "turns": thread.turns,
                })
            })
            .collect::<Vec<_>>();
        json!({ "threads": threads, "truncated": truncated })
    }

    fn read_thread(arguments: &Value) -> Result<Value> {
        let thread_id = required_uuid(arguments, "thread_id")?;
        let thread = threads::read_thread(thread_id)
            .with_context(|| format!("thread {thread_id} not found"))?;
        let mut turns = thread
            .entries
            .into_iter()
            .rev()
            .filter_map(|record| match record {
                ThreadRecord::Turn { role, text, .. } => Some(json!({
                    "role": match role {
                        ThreadRole::User => "user",
                        ThreadRole::Assistant => "assistant",
                    },
                    "text": text,
                })),
                _ => None,
            })
            .take(MAX_READ_THREAD_TURNS)
            .collect::<Vec<_>>();
        turns.reverse();
        Ok(json!({
            "thread_id": thread.thread_id,
            "title": thread.title,
            "summary": thread.summary,
            "turns": turns,
        }))
    }

    fn create_workstation(&mut self, arguments: &Value) -> Result<Value> {
        let title = required_str(arguments, "title")?.to_owned();
        let working_dir = arguments
            .get("working_dir")
            .and_then(Value::as_str)
            .map(canonical_existing_directory)
            .transpose()?
            .map(|directory| directory.to_string_lossy().into_owned());
        let response = self.client.call(&ClientRequest::CreateWorkspace {
            title: Some(title.clone()),
        })?;
        let ServiceResponse::WorkspaceCreated {
            workspace_id,
            pane_id,
        } = response
        else {
            bail!("unexpected CreateWorkspace response: {response:?}");
        };
        if let Some(directory) = &working_dir {
            expect_ack(&self.client.call(&ClientRequest::SetWorkspaceWorkingDir {
                workspace_id,
                working_dir: Some(directory.clone()),
            })?)?;
        }
        self.attached_workspaces.insert(workspace_id);
        self.voice_created_panes.insert(pane_id);
        self.snapshot = None;
        Ok(json!({
            "workspace_id": workspace_id,
            "pane_id": pane_id,
            "title": title,
            "working_dir": working_dir,
        }))
    }

    fn open_terminal_tab(&mut self, arguments: &Value) -> Result<Value> {
        let workspace_id = required_uuid(arguments, "workspace_id")?;
        self.require_attached_workstation(workspace_id)?;
        let response = self
            .client
            .call(&ClientRequest::CreateWorkspaceTab { workspace_id })?;
        let ServiceResponse::PaneCreated { pane_id } = response else {
            bail!("unexpected CreateWorkspaceTab response: {response:?}");
        };
        self.voice_created_panes.insert(pane_id);
        self.snapshot = None;
        Ok(json!({ "pane_id": pane_id }))
    }
    fn rename_tab(&mut self, arguments: &Value) -> Result<Value> {
        let tab_id = required_uuid(arguments, "tab_id")?;
        let title = required_str(arguments, "title")?.to_owned();
        expect_ack(&self.client.call(&ClientRequest::RenameTab {
            tab_id,
            title: title.clone(),
        })?)?;
        self.snapshot = None;
        Ok(json!({ "tab_id": tab_id, "title": title }))
    }

    fn open_project_tab(&mut self, arguments: &Value) -> Result<Value> {
        let workspace_id = required_uuid(arguments, "workspace_id")?;
        self.require_attached_workstation(workspace_id)?;
        let working_dir = canonical_existing_directory(required_str(arguments, "working_dir")?)?
            .to_string_lossy()
            .into_owned();
        let title = arguments
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let response = self.client.call(&ClientRequest::CreateWorkspaceProject {
            workspace_id,
            working_dir: working_dir.clone(),
            title,
        })?;
        let ServiceResponse::PaneCreated { pane_id } = response else {
            bail!("unexpected CreateWorkspaceProject response: {response:?}");
        };
        self.voice_created_panes.insert(pane_id);
        self.snapshot = None;
        Ok(json!({ "pane_id": pane_id, "working_dir": working_dir }))
    }

    fn create_worktree_tab(&mut self, arguments: &Value) -> Result<Value> {
        let workspace_id = required_uuid(arguments, "workspace_id")?;
        self.require_attached_workstation(workspace_id)?;
        let repo_dir = canonical_git_repository(required_str(arguments, "repo_dir")?)?;
        let branch = required_str(arguments, "branch")?;
        let base = arguments.get("base").and_then(Value::as_str);
        validate_worktree_branch(branch)?;
        let worktree_path = worktree_path(&repo_dir, branch)?;
        let parent = worktree_path
            .parent()
            .context("worktree path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
        run_git_worktree(&repo_dir, &worktree_path, branch, base)?;
        let response = self.client.call(&ClientRequest::CreateWorkspaceProject {
            workspace_id,
            working_dir: worktree_path.to_string_lossy().into_owned(),
            title: Some(branch.to_owned()),
        })?;
        let ServiceResponse::PaneCreated { pane_id } = response else {
            bail!("unexpected CreateWorkspaceProject response: {response:?}");
        };
        self.voice_created_panes.insert(pane_id);
        self.snapshot = None;
        Ok(json!({
            "pane_id": pane_id,
            "branch": branch,
            "worktree_path": worktree_path,
        }))
    }

    fn launch_agent(&mut self, arguments: &Value) -> Result<Value> {
        let pane_id = required_uuid(arguments, "pane_id")?;
        if !self.voice_created_panes.contains(&pane_id) {
            bail!("pane was not created by Voice Mode");
        }
        let workspace_id = self.workspace_for_pane(pane_id)?;
        self.require_attached(workspace_id)?;
        let agent = Agent::parse(required_str(arguments, "agent")?)?;
        let command = launch_command(agent);
        expect_ack(&self.client.call(&ClientRequest::WriteInput {
            pane_id,
            bytes: format!("{command}\r").into_bytes(),
        })?)?;
        Ok(json!({ "pane_id": pane_id, "launched": command }))
    }

    fn send_input_now(&mut self, arguments: &Value) -> Result<Value> {
        let PendingAction::SendInput {
            pane_id,
            text,
            submit,
        } = pending_send_input(arguments)?
        else {
            unreachable!();
        };
        self.write_input(pane_id, &text, submit)
    }

    fn request_approval(
        &mut self,
        action: PendingAction,
        ui_only: bool,
        ui: &futures::channel::mpsc::UnboundedSender<VoiceUiEvent>,
    ) -> Value {
        let id = self.next_approval_id;
        self.next_approval_id = self.next_approval_id.saturating_add(1);
        let description = action.description();
        self.pending_approvals.insert(id, action);
        if ui_only {
            self.ui_only_approvals.insert(id);
        }
        let _ = ui.unbounded_send(VoiceUiEvent::ApprovalRequested {
            id,
            description: description.clone(),
        });
        if ui_only {
            json!({
                "status": "needs_approval",
                "approval_id": id,
                "action": description,
                "requires_ui_click": true,
                "note": "closing the assistant's own surface requires the user to click Approve in the pane",
            })
        } else {
            json!({
                "status": "needs_approval",
                "approval_id": id,
                "action": description,
            })
        }
    }

    fn execute_pending(&mut self, action: PendingAction) -> Result<Value> {
        match action {
            PendingAction::SendInput {
                pane_id,
                text,
                submit,
            } => self.write_input(pane_id, &text, submit),
            PendingAction::SendKeys { pane_id, keys } => {
                let mut bytes = Vec::new();
                for key in &keys {
                    bytes.extend_from_slice(key_bytes(key)?);
                }
                expect_ack(
                    &self
                        .client
                        .call(&ClientRequest::WriteInput { pane_id, bytes })?,
                )?;
                Ok(json!({ "status": "executed", "pane_id": pane_id }))
            }
            PendingAction::CloseTab { tab_id } => {
                expect_ack(&self.client.call(&ClientRequest::CloseTab { tab_id })?)?;
                self.snapshot = None;
                Ok(json!({ "status": "executed", "tab_id": tab_id }))
            }
            PendingAction::CloseWorkstation { workspace_id } => {
                expect_ack(
                    &self
                        .client
                        .call(&ClientRequest::DeleteWorkspace { workspace_id })?,
                )?;
                self.attached_workspaces.remove(&workspace_id);
                self.snapshot = None;
                Ok(json!({ "status": "executed", "workspace_id": workspace_id }))
            }
        }
    }

    fn write_input(&mut self, pane_id: Uuid, text: &str, submit: bool) -> Result<Value> {
        let mut bytes = text.as_bytes().to_vec();
        if submit {
            bytes.push(b'\r');
        }
        expect_ack(
            &self
                .client
                .call(&ClientRequest::WriteInput { pane_id, bytes })?,
        )?;
        Ok(json!({ "status": "executed", "pane_id": pane_id }))
    }

    fn require_attached(&self, workspace_id: Uuid) -> Result<()> {
        if !self.attached_workspaces.contains(&workspace_id) {
            bail!("workstation {workspace_id} is not attached; call attach_project first");
        }
        Ok(())
    }

    fn require_attached_workstation(&mut self, workspace_id: Uuid) -> Result<()> {
        {
            terminal_workspace(self.ensure_snapshot()?, workspace_id)?;
        }
        self.require_attached(workspace_id)
    }

    fn tab_contains_own_pane(&mut self, tab_id: Uuid) -> Result<bool> {
        let Some(own_pane) = self.own_pane else {
            return Ok(false);
        };
        let snapshot = self.ensure_snapshot()?;
        for tab in snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
        {
            if tab.id == tab_id {
                let mut panes = Vec::new();
                collect_panes(&tab.layout, &mut panes);
                return Ok(panes.into_iter().any(|pane| pane.id == own_pane));
            }
        }
        Ok(false)
    }

    fn workspace_contains_own_pane(&mut self, workspace_id: Uuid) -> bool {
        let Some(own_pane) = self.own_pane else {
            return false;
        };
        match self.workspace_for_pane(own_pane) {
            Ok(own_workspace_id) => own_workspace_id == workspace_id,
            Err(_) => false,
        }
    }

    fn workspace_for_pane(&mut self, pane_id: Uuid) -> Result<Uuid> {
        let snapshot = self.ensure_snapshot()?;
        for workspace in &snapshot.workspaces {
            if workspace.tabs.iter().any(|tab| {
                let mut panes = Vec::new();
                collect_panes(&tab.layout, &mut panes);
                panes.into_iter().any(|pane| pane.id == pane_id)
            }) {
                return Ok(workspace.id);
            }
        }
        bail!("pane {pane_id} does not exist")
    }
}

pub(crate) fn classify_tool(
    name: &str,
    arguments: &Value,
    voice_created_panes: &HashSet<Uuid>,
) -> Result<TrustTier> {
    match name {
        "list_workstations" | "read_pane" | "check_status" | "attach_project" | "recall_memory"
        | "list_directory" | "find_directory" | "list_threads" | "read_thread" => Ok(TrustTier::T0),
        "create_workstation"
        | "open_terminal_tab"
        | "rename_tab"
        | "open_project_tab"
        | "create_worktree_tab"
        | "launch_agent" => Ok(TrustTier::T1),
        "send_input" => {
            let pane_id = required_uuid(arguments, "pane_id")?;
            let text = required_str(arguments, "text")?;
            if voice_created_panes.contains(&pane_id) && !is_destructive_input(text) {
                Ok(TrustTier::T1)
            } else {
                Ok(TrustTier::T2)
            }
        }
        "send_keys" | "close_tab" | "close_workstation" => Ok(TrustTier::T2),
        "approve_action" => Ok(TrustTier::Meta),
        _ => bail!("unknown voice tool {name}"),
    }
}

pub(crate) fn tool_schemas() -> Vec<Value> {
    vec![
        tool(
            "list_workstations",
            "List all workspaces with kind, tabs, panes, profiles, and statuses",
            json!({}),
            &[],
        ),
        tool(
            "read_pane",
            "Read recent visible terminal lines",
            json!({
                "pane_id": {"type":"string"}, "lines": {"type":"integer","minimum":1,"maximum":200}
            }),
            &["pane_id"],
        ),
        tool(
            "check_status",
            "Check attached agent statuses and new notifications",
            json!({}),
            &[],
        ),
        tool(
            "attach_project",
            "Attach Voice Mode to an existing kind=workstation target from list_workstations",
            json!({"workspace_id": {"type":"string"}}),
            &["workspace_id"],
        ),
        tool(
            "recall_memory",
            "Recall project-management memory",
            json!({"query": {"type":"string"}}),
            &["query"],
        ),
        tool(
            "list_directory",
            "List subdirectories of a directory; path defaults to the user's home. Use to browse for a project location",
            json!({"path": {"type":"string"}}),
            &[],
        ),
        tool(
            "find_directory",
            "Fuzzy-search directories under the user's home by spoken name; returns real absolute paths. Use before opening any directory whose exact path you have not seen",
            json!({"query": {"type":"string"}}),
            &["query"],
        ),
        tool(
            "list_threads",
            "List saved assistant conversation threads, newest first",
            json!({}),
            &[],
        ),
        tool(
            "read_thread",
            "Read a saved conversation thread by thread_id from list_threads",
            json!({"thread_id": {"type":"string"}}),
            &["thread_id"],
        ),
        tool(
            "create_workstation",
            "Create and attach a workstation",
            json!({
                "title": {"type":"string"}, "working_dir": {"type":"string"}
            }),
            &["title"],
        ),
        tool(
            "open_terminal_tab",
            "Open a terminal tab in an attached kind=workstation target; returns pane_id for send_input",
            json!({"workspace_id": {"type":"string"}}),
            &["workspace_id"],
        ),
        tool(
            "rename_tab",
            "Rename a tab; find tab ids with list_workstations",
            json!({"tab_id": {"type":"string"}, "title": {"type":"string"}}),
            &["tab_id", "title"],
        ),
        tool(
            "open_project_tab",
            "Open an existing directory as a project tab",
            json!({
                "workspace_id": {"type":"string"}, "working_dir": {"type":"string"}, "title": {"type":"string"}
            }),
            &["workspace_id", "working_dir"],
        ),
        tool(
            "create_worktree_tab",
            "Create a git worktree and open it as a project tab",
            json!({
                "workspace_id": {"type":"string"}, "repo_dir": {"type":"string"}, "branch": {"type":"string"}, "base": {"type":"string"}
            }),
            &["workspace_id", "repo_dir", "branch"],
        ),
        tool(
            "launch_agent",
            "Launch an interactive coding agent in a Voice-created pane",
            json!({
                "pane_id": {"type":"string"}, "agent": {"type":"string","enum":["omp","hermes","codex","claude"]}
            }),
            &["pane_id", "agent"],
        ),
        tool(
            "send_input",
            "Type text into a pane",
            json!({
                "pane_id": {"type":"string"}, "text": {"type":"string"}, "submit": {"type":"boolean"}
            }),
            &["pane_id", "text"],
        ),
        tool(
            "send_keys",
            "Send control or navigation keys after approval",
            json!({
                "pane_id": {"type":"string"}, "keys": {"type":"array","items":{"type":"string","enum":["enter","esc","up","down","tab","ctrl-c"]}}
            }),
            &["pane_id", "keys"],
        ),
        tool(
            "close_tab",
            "Close a tab after approval",
            json!({"tab_id":{"type":"string"}}),
            &["tab_id"],
        ),
        tool(
            "close_workstation",
            "Close a workstation after approval",
            json!({"workspace_id":{"type":"string"}}),
            &["workspace_id"],
        ),
        tool(
            "approve_action",
            "Resolve a pending action after explicit confirmation",
            json!({
                "approval_id": {"type":"integer"}, "approved": {"type":"boolean"}
            }),
            &["approval_id", "approved"],
        ),
    ]
}

#[allow(clippy::needless_pass_by_value)]
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }
    })
}

fn approval_blocked_for_voice(ui_only: bool, via_ui: bool, approved: bool) -> bool {
    approved && !via_ui && ui_only
}

fn initial_notification_cursor(items: &[SessionNotification]) -> u64 {
    items
        .iter()
        .map(|notification| notification.id)
        .max()
        .unwrap_or(0)
}

pub(crate) fn is_destructive_input(text: &str) -> bool {
    let normalized = text.trim_start().to_ascii_lowercase();
    let command = normalized.split_whitespace().next().unwrap_or_default();
    matches!(
        command,
        "sudo" | "rm" | "dd" | "shutdown" | "reboot" | "halt"
    ) || command.starts_with("mkfs")
        || (contains_command_pair(&normalized, "git", "push") && normalized.contains("--force"))
        || (contains_command_pair(&normalized, "git", "reset") && normalized.contains("--hard"))
}

fn contains_command_pair(text: &str, first: &str, second: &str) -> bool {
    text.split(|character: char| character.is_whitespace() || matches!(character, ';' | '&' | '|'))
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair == [first, second])
}

pub(crate) fn validate_worktree_branch(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.len() > 100
        || !branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
    {
        bail!("branch must match [A-Za-z0-9._/-]{{1,100}}");
    }
    Ok(())
}

pub(crate) fn worktree_path(repo_dir: &Path, branch: &str) -> Result<PathBuf> {
    validate_worktree_branch(branch)?;
    let repo_name = repo_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("repository directory has no UTF-8 name")?;
    let parent = repo_dir
        .parent()
        .context("repository directory has no parent")?;
    let path_name = branch.replace('/', "-");
    if matches!(path_name.as_str(), "." | "..") {
        bail!("branch does not produce a safe worktree directory name");
    }
    Ok(parent
        .join(format!("{repo_name}-worktrees"))
        .join(path_name))
}

fn canonical_existing_directory(path: &str) -> Result<PathBuf> {
    let path = expand_home(path);
    if !path.is_absolute() {
        bail!("working directory must be an absolute path or start with ~/");
    }
    let canonical = std::fs::canonicalize(&path).map_err(|_| directory_not_found_error(&path))?;
    if !canonical.is_dir() {
        bail!("working directory must be an existing directory");
    }
    Ok(canonical)
}

fn expand_home(path: &str) -> PathBuf {
    let Ok(home) = std::env::var("HOME") else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return PathBuf::from(home);
    }
    path.strip_prefix("~/")
        .map_or_else(|| PathBuf::from(path), |rest| Path::new(&home).join(rest))
}

fn directory_not_found_error(path: &Path) -> anyhow::Error {
    let mut ancestor = path.parent();
    while let Some(candidate) = ancestor {
        if candidate.is_dir() {
            let (mut names, _) = subdirectory_names(candidate).unwrap_or_default();
            names.truncate(15);
            return anyhow::anyhow!(
                "directory {} does not exist; {} contains: {}",
                path.display(),
                candidate.display(),
                names.join(", ")
            );
        }
        ancestor = candidate.parent();
    }
    anyhow::anyhow!("directory {} does not exist", path.display())
}

fn subdirectory_names(path: &Path) -> Result<(Vec<String>, bool)> {
    const MAX_LISTED_DIRECTORIES: usize = 200;
    let mut names = Vec::new();
    for entry in
        std::fs::read_dir(path).with_context(|| format!("read directory {}", path.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", path.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        names.push(name);
    }
    names.sort_by_key(|name| name.to_lowercase());
    let truncated = names.len() > MAX_LISTED_DIRECTORIES;
    names.truncate(MAX_LISTED_DIRECTORIES);
    Ok((names, truncated))
}

fn normalized_name(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn directory_match_score(name: &str, query: &str) -> usize {
    let name = normalized_name(name);
    let query_joined = normalized_name(query);
    if name.is_empty() || query_joined.is_empty() {
        return 0;
    }
    let mut score = 0;
    if name.contains(&query_joined) || (name.len() >= 3 && query_joined.contains(&name)) {
        score += 2;
    }
    score += query
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 3)
        .filter(|token| name.contains(&normalized_name(token)))
        .count();
    score
}

fn find_directories(root: &Path, query: &str) -> Vec<PathBuf> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0_usize)]);
    let mut scored: Vec<(usize, usize, PathBuf)> = Vec::new();
    let mut visited = 0_usize;
    while let Some((directory, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_SEARCH_VISITS {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SEARCH_SKIP_DIRECTORIES.contains(&name.as_str()) {
                continue;
            }
            let is_symlink = entry.file_type().is_ok_and(|kind| kind.is_symlink());
            if is_symlink || !entry.path().is_dir() {
                continue;
            }
            let score = directory_match_score(&name, query);
            if score > 0 {
                scored.push((score, depth, entry.path()));
            }
            if depth + 1 < MAX_SEARCH_DEPTH {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    scored.into_iter().map(|(_, _, path)| path).collect()
}

fn canonical_git_repository(path: &str) -> Result<PathBuf> {
    let canonical = canonical_existing_directory(path)?;
    if !canonical.join(".git").exists() {
        bail!("repository directory must contain .git");
    }
    Ok(canonical)
}

fn run_git_worktree(repo: &Path, target: &Path, branch: &str, base: Option<&str>) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .arg("worktree")
        .arg("add")
        .arg(target)
        .arg("-b")
        .arg(branch)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(base) = base {
        command.arg(base);
    }
    let (status, stderr) =
        run_child_with_stderr_timeout(&mut command, WORKTREE_TIMEOUT, "git worktree add")?;
    if !status.success() {
        bail!("git worktree add failed: {}", stderr.trim());
    }
    Ok(())
}

fn run_child_with_stderr_timeout(
    command: &mut Command,
    timeout: Duration,
    operation: &str,
) -> Result<(std::process::ExitStatus, String)> {
    let mut child = command
        .spawn()
        .with_context(|| format!("launch {operation}"))?;
    let mut stderr = child
        .stderr
        .take()
        .with_context(|| format!("capture {operation} stderr"))?;
    let (stderr_tx, stderr_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    thread::spawn(move || {
        let mut captured = 0_usize;
        let mut buffer = [0_u8; 8 * 1024];
        while let Ok(read) = stderr.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let keep = read.min(MAX_SUBPROCESS_STDERR_BYTES.saturating_sub(captured));
            if keep > 0 && stderr_tx.send(buffer[..keep].to_vec()).is_err() {
                break;
            }
            captured = captured.saturating_add(keep);
        }
    });

    let deadline = Instant::now() + timeout;
    let mut captured = Vec::with_capacity(MAX_SUBPROCESS_STDERR_BYTES);
    let status = loop {
        while let Ok(chunk) = stderr_rx.try_recv() {
            captured.extend_from_slice(&chunk);
        }
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for {operation}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let reap_deadline = Instant::now() + SUBPROCESS_REAP_TIMEOUT;
            while Instant::now() < reap_deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            bail!("{operation} timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stderr_deadline = Instant::now() + STDERR_COMPLETION_TIMEOUT;
    while captured.len() < MAX_SUBPROCESS_STDERR_BYTES && Instant::now() < stderr_deadline {
        match stderr_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(chunk) => captured.extend_from_slice(&chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    captured.truncate(MAX_SUBPROCESS_STDERR_BYTES);
    Ok((status, String::from_utf8_lossy(&captured).into_owned()))
}

fn pending_send_input(arguments: &Value) -> Result<PendingAction> {
    Ok(PendingAction::SendInput {
        pane_id: required_uuid(arguments, "pane_id")?,
        text: required_str(arguments, "text")?.to_owned(),
        submit: arguments
            .get("submit")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

fn pending_send_keys(arguments: &Value) -> Result<PendingAction> {
    let keys = arguments
        .get("keys")
        .and_then(Value::as_array)
        .context("keys must be an array")?
        .iter()
        .map(|key| {
            key.as_str()
                .context("each key must be a string")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    for key in &keys {
        let _ = key_bytes(key)?;
    }
    Ok(PendingAction::SendKeys {
        pane_id: required_uuid(arguments, "pane_id")?,
        keys,
    })
}

fn key_bytes(key: &str) -> Result<&'static [u8]> {
    match key {
        "enter" => Ok(b"\r"),
        "esc" => Ok(b"\x1b"),
        "up" => Ok(b"\x1b[A"),
        "down" => Ok(b"\x1b[B"),
        "tab" => Ok(b"\t"),
        "ctrl-c" => Ok(b"\x03"),
        _ => bail!("unsupported key {key}"),
    }
}

fn terminal_workspace(
    snapshot: &SessionSnapshot,
    workspace_id: Uuid,
) -> Result<&hh_protocol::Workspace> {
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .with_context(|| format!("workstation {workspace_id} does not exist"))?;
    if workspace.is_assistant() {
        bail!(
            "workspace {workspace_id} is an assistant workspace; choose a kind=workstation target from list_workstations"
        );
    }
    Ok(workspace)
}

fn snapshot_summary(snapshot: &SessionSnapshot, states: &HashMap<Uuid, PaneStreamState>) -> Value {
    let workspaces = snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let tabs = workspace
                .tabs
                .iter()
                .map(|tab| {
                    let mut panes = Vec::new();
                    collect_panes(&tab.layout, &mut panes);
                    let panes = panes
                        .into_iter()
                        .map(|pane| {
                            json!({
                                "id": pane.id,
                                "title": pane.title,
                                "profile": pane.identity.profile,
                                "status": pane.status,
                                "exited": states.get(&pane.id).is_some_and(|state| state.exited),
                            })
                        })
                        .collect::<Vec<_>>();
                    json!({
                        "id": tab.id,
                        "title": tab.title,
                        "project_dir": tab.project_dir,
                        "panes": panes,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "id": workspace.id,
                "title": workspace.title,
                "kind": workspace.kind,
                "working_dir": workspace.working_dir,
                "tabs": tabs,
            })
        })
        .collect::<Vec<_>>();
    json!({ "workspaces": workspaces })
}

fn collect_panes<'a>(layout: &'a PaneLayout, panes: &mut Vec<&'a Pane>) {
    match layout {
        PaneLayout::Leaf { pane } => panes.push(pane),
        PaneLayout::Stack { panes: stacked, .. } => panes.extend(stacked),
        PaneLayout::Split { first, second, .. } => {
            collect_panes(first, panes);
            collect_panes(second, panes);
        }
    }
}

fn terminal_line_text(line: &TerminalLine) -> String {
    line.runs.iter().map(|run| run.text.as_str()).collect()
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("{field} must be a string"))
}

fn required_uuid(value: &Value, field: &str) -> Result<Uuid> {
    required_str(value, field)?
        .parse()
        .with_context(|| format!("{field} must be a UUID"))
}

fn required_u64(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("{field} must be an unsigned integer"))
}

fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .with_context(|| format!("{field} must be a boolean"))
}

fn expect_ack(response: &ServiceResponse) -> Result<()> {
    if matches!(response, ServiceResponse::Ack) {
        Ok(())
    } else {
        bail!("unexpected acknowledgement response: {response:?}")
    }
}

fn bounded_json(value: &Value) -> String {
    let encoded = serde_json::to_string(value)
        .unwrap_or_else(|error| json!({ "error": error.to_string() }).to_string());
    if encoded.len() <= MAX_TOOL_OUTPUT_BYTES {
        return encoded;
    }
    let mut start = encoded.len().saturating_sub(MAX_TOOL_OUTPUT_BYTES - 64);
    while !encoded.is_char_boundary(start) {
        start += 1;
    }
    json!({ "truncated": true, "tail": &encoded[start..] }).to_string()
}

fn summarize_output(value: &Value) -> String {
    let summary = match value {
        Value::Object(object) if object.contains_key("error") => {
            format!("error: {}", object["error"])
        }
        Value::Object(object) if object.contains_key("status") => object["status"].to_string(),
        _ => "completed".to_owned(),
    };
    summary.chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(id: u64) -> SessionNotification {
        SessionNotification {
            id,
            pane_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            kind: hh_protocol::NotificationKind::Message,
            message: None,
            pane_title: "pane".to_owned(),
            workspace_title: "workspace".to_owned(),
            profile: hh_protocol::TerminalProfile::Terminal,
            at_ms: 0,
            read: false,
        }
    }

    #[test]
    fn initial_cursor_skips_the_existing_notification_backlog() {
        assert_eq!(initial_notification_cursor(&[]), 0);
        assert_eq!(
            initial_notification_cursor(&[notification(3), notification(9), notification(5)]),
            9
        );
    }

    #[test]
    fn only_voice_approval_of_ui_only_actions_is_blocked() {
        for ui_only in [false, true] {
            for via_ui in [false, true] {
                for approved in [false, true] {
                    assert_eq!(
                        approval_blocked_for_voice(ui_only, via_ui, approved),
                        ui_only && !via_ui && approved,
                        "ui_only={ui_only}, via_ui={via_ui}, approved={approved}"
                    );
                }
            }
        }
    }

    #[test]
    fn destructive_input_detection_matches_only_declared_command_families() {
        for command in [
            "sudo launchctl reboot",
            " rm -rf /tmp/x",
            "mkfs.ext4 /dev/x",
            "dd if=/dev/zero of=/dev/x",
            "shutdown -h now",
            "reboot",
            "halt",
            "git push origin main --force",
            "git reset HEAD~1 --hard",
        ] {
            assert!(is_destructive_input(command), "command: {command}");
        }
        for command in ["echo rm", "git push origin main", "git reset --soft HEAD~1"] {
            assert!(!is_destructive_input(command), "command: {command}");
        }
    }

    #[test]
    fn trust_tiers_distinguish_voice_created_foreign_and_destructive_input() {
        let voice_pane = Uuid::new_v4();
        let foreign_pane = Uuid::new_v4();
        let voice_created = HashSet::from([voice_pane]);
        assert_eq!(
            classify_tool(
                "send_input",
                &json!({"pane_id": voice_pane, "text": "cargo check"}),
                &voice_created,
            )
            .unwrap(),
            TrustTier::T1
        );
        assert_eq!(
            classify_tool(
                "send_input",
                &json!({"pane_id": foreign_pane, "text": "cargo check"}),
                &voice_created,
            )
            .unwrap(),
            TrustTier::T2
        );
        assert_eq!(
            classify_tool(
                "send_input",
                &json!({"pane_id": voice_pane, "text": "rm -rf target"}),
                &voice_created,
            )
            .unwrap(),
            TrustTier::T2
        );
    }

    #[test]
    fn tool_schemas_require_only_non_optional_parameters() {
        let schemas = tool_schemas();
        let required = |name: &str| {
            schemas
                .iter()
                .find(|schema| schema["name"] == name)
                .unwrap()["parameters"]["required"]
                .clone()
        };
        assert_eq!(required("read_pane"), json!(["pane_id"]));
        assert_eq!(required("create_workstation"), json!(["title"]));
        assert_eq!(required("open_terminal_tab"), json!(["workspace_id"]));
        assert_eq!(required("rename_tab"), json!(["tab_id", "title"]));
        assert_eq!(
            required("create_worktree_tab"),
            json!(["workspace_id", "repo_dir", "branch"])
        );
        assert_eq!(required("send_input"), json!(["pane_id", "text"]));
        assert_eq!(required("list_directory"), json!([]));
        assert_eq!(required("find_directory"), json!(["query"]));
        assert_eq!(required("list_threads"), json!([]));
        assert_eq!(required("read_thread"), json!(["thread_id"]));
    }

    #[test]
    fn snapshot_summary_exposes_workspace_kind() {
        let mut snapshot = SessionSnapshot::seeded();
        let summary = snapshot_summary(&snapshot, &HashMap::new());
        assert_eq!(summary["workspaces"][0]["kind"], "workstation");

        snapshot.workspaces[0].kind = hh_protocol::WorkspaceKind::Assistant;
        let summary = snapshot_summary(&snapshot, &HashMap::new());
        assert_eq!(summary["workspaces"][0]["kind"], "assistant");
    }

    #[test]
    fn terminal_workspace_rejects_assistant_workspace() {
        let mut snapshot = SessionSnapshot::seeded();
        let workspace_id = snapshot.workspaces[0].id;
        assert_eq!(
            terminal_workspace(&snapshot, workspace_id).unwrap().id,
            workspace_id
        );

        snapshot.workspaces[0].kind = hh_protocol::WorkspaceKind::Assistant;
        let error = terminal_workspace(&snapshot, workspace_id).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "workspace {workspace_id} is an assistant workspace; choose a kind=workstation target from list_workstations"
            )
        );
    }

    #[test]
    fn canonical_directory_restores_the_filesystem_case() {
        let leaf = format!("HhCase-{}", Uuid::new_v4());
        let directory = std::env::temp_dir().join(&leaf);
        std::fs::create_dir_all(&directory).unwrap();
        let lowercased = directory.parent().unwrap().join(leaf.to_ascii_lowercase());
        if !lowercased.is_dir() {
            std::fs::remove_dir(&directory).unwrap();
            return;
        }

        let canonical = canonical_existing_directory(lowercased.to_str().unwrap()).unwrap();
        assert_eq!(canonical.file_name().unwrap(), leaf.as_str());
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn worktree_branch_validation_and_path_are_bounded() {
        let repo = Path::new("/tmp/project");
        assert_eq!(
            worktree_path(repo, "feature/voice").unwrap(),
            Path::new("/tmp/project-worktrees/feature-voice")
        );
        for invalid in ["", "bad branch", "../bad!", &"a".repeat(101)] {
            assert!(
                validate_worktree_branch(invalid).is_err(),
                "branch: {invalid}"
            );
        }
    }

    #[test]
    fn subprocess_stderr_is_drained_without_deadlock_and_bounded() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("dd if=/dev/zero bs=1024 count=256 >&2; exit 7")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let (status, stderr) = run_child_with_stderr_timeout(
            &mut command,
            Duration::from_secs(2),
            "stderr stress test",
        )
        .unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(stderr.len() <= MAX_SUBPROCESS_STDERR_BYTES);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn find_directories_matches_spoken_project_names() {
        let root = std::env::temp_dir().join(format!("hh-find-{}", Uuid::new_v4()));
        let projects = root.join("Projects");
        std::fs::create_dir_all(projects.join("proteusland")).unwrap();
        std::fs::create_dir_all(projects.join("other-app")).unwrap();
        std::fs::create_dir_all(projects.join(".hidden")).unwrap();

        let matches = find_directories(&root, "highly Proteus land");
        assert_eq!(matches.first().cloned(), Some(projects.join("proteusland")));
        assert!(find_directories(&root, "zzzqqq").is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_match_score_prefers_whole_and_token_matches() {
        assert!(directory_match_score("proteusland", "highly Proteus land") >= 1);
        assert!(directory_match_score("proteusland", "proteusland") >= 2);
        assert_eq!(directory_match_score("other", "proteus"), 0);
    }

    #[test]
    fn subdirectory_names_skip_hidden_and_files() {
        let root = std::env::temp_dir().join(format!("hh-list-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("Visible")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("notes.txt"), "x").unwrap();

        let (names, truncated) = subdirectory_names(&root).unwrap();
        assert_eq!(names, vec!["Visible".to_owned()]);
        assert!(!truncated);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn expand_home_rewrites_tilde_prefix() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/x"), Path::new(&home).join("x"));
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn not_found_error_lists_sibling_directories() {
        let root = std::env::temp_dir().join(format!("hh-notfound-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("RealName")).unwrap();

        let error = directory_not_found_error(&root.join("wrongname")).to_string();
        assert!(error.contains("RealName"), "error: {error}");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
