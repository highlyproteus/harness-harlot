use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use hh_protocol::{
    AssistantAccess, AssistantApproval, AssistantEntry, AssistantImage, AssistantModel,
    AssistantNoticeLevel, AssistantSettings, AssistantStatus, AssistantThreadView, CodingAgent,
    MAX_ASSISTANT_ENTRIES, MAX_ASSISTANT_IMAGE_BYTES, MAX_ASSISTANT_PROMPT_CHARS, PaneStatus,
    TerminalProfile,
};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use uuid::Uuid;

use super::discovery::{PiInstall, discover_coding_agents, discover_pi};
use super::extension::ensure_extension_file;
use super::rpc::{PiProcess, PiSpawnArgs};
use crate::layout::layout_contains;
use crate::process::{fallback_cwd, valid_local_cwd};
use crate::registry::RegistryState;

const PI_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const PI_ABORT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ASSISTANT_VIEW_BYTES: usize = 512 * 1024;
const MAX_TOOL_OUTPUT_CHARS: usize = 16 * 1024;

pub(crate) type PiDiscoveryCache = Arc<Mutex<Option<Result<PiInstall, String>>>>;
pub(crate) type CodingAgentCache = Arc<Mutex<Option<Vec<CodingAgent>>>>;

#[derive(Debug)]
pub(crate) struct AssistantRuntime {
    pane_id: Uuid,
    workspace_id: Uuid,
    session_dir: PathBuf,
    process: Mutex<Option<Arc<PiProcess>>>,
    state: Mutex<AssistantThreadState>,
    revision: AtomicU64,
    registry: RwLock<std::sync::Weak<RwLock<RegistryState>>>,
    discovery_cache: PiDiscoveryCache,
    coding_agents: CodingAgentCache,
}

#[derive(Debug)]
struct AssistantThreadState {
    status: AssistantStatus,
    model: Option<String>,
    entries: VecDeque<AssistantEntry>,
    truncated: u32,
    pending_approval: Option<AssistantApproval>,
    open_assistant: Option<usize>,
    run_active: bool,
}

impl AssistantRuntime {
    pub(crate) fn new(
        pane_id: Uuid,
        workspace_id: Uuid,
        discovery_cache: PiDiscoveryCache,
        coding_agents: CodingAgentCache,
    ) -> Result<Arc<Self>> {
        let state_directory =
            hh_protocol::state_directory().context("assistant state directory is unavailable")?;
        Ok(Arc::new(Self {
            pane_id,
            workspace_id,
            session_dir: state_directory
                .join("assistant")
                .join("sessions")
                .join(pane_id.to_string()),
            process: Mutex::new(None),
            state: Mutex::new(AssistantThreadState {
                status: AssistantStatus::Starting,
                model: None,
                entries: VecDeque::new(),
                truncated: 0,
                pending_approval: None,
                open_assistant: None,
                run_active: false,
            }),
            revision: AtomicU64::new(now_ms()),
            registry: RwLock::new(std::sync::Weak::new()),
            discovery_cache,
            coding_agents,
        }))
    }

    pub(crate) fn bind_registry(&self, registry: &Arc<RwLock<RegistryState>>) {
        *self.registry.write() = Arc::downgrade(registry);
    }

    pub(crate) fn start(self: &Arc<Self>) {
        self.set_status(AssistantStatus::Starting);
        if let Err(error) = self.start_inner() {
            if let Some(process) = self.process.lock().take() {
                process.shutdown();
            }
            self.set_status(AssistantStatus::Unavailable {
                message: format!("{error:#}"),
            });
        }
    }

    fn start_inner(self: &Arc<Self>) -> Result<()> {
        hh_protocol::ensure_private_directory(&self.session_dir).with_context(|| {
            format!(
                "prepare assistant session directory {}",
                self.session_dir.display()
            )
        })?;
        let install = self.discover_pi_cached().map_err(anyhow::Error::msg)?;
        debug_assert!(install.version >= (0, 85, 0));
        let extension_path = ensure_extension_file()?;
        let (settings, cwd, system_prompt) = self.spawn_context()?;
        let resume_session = newest_jsonl(&self.session_dir)?;
        let weak = Arc::downgrade(self);
        let sink = Arc::new(move |value: Value| {
            if let Some(runtime) = weak.upgrade() {
                runtime.apply_event(&value);
            }
        });
        let spawn_args = PiSpawnArgs {
            extension_path,
            session_dir: self.session_dir.clone(),
            system_prompt,
            model: settings.model,
            resume_session,
            cwd,
            pane_id: self.pane_id,
            workspace_id: self.workspace_id,
        };
        let process = PiProcess::spawn(&install, &spawn_args, sink)?;
        *self.process.lock() = Some(Arc::clone(&process));

        let state = process
            .request(json!({"type":"get_state"}), PI_REQUEST_TIMEOUT)
            .context("pi did not answer the RPC handshake")?;
        let model = model_identifier(state.get("model"));
        let messages = process
            .request(json!({"type":"get_messages"}), PI_REQUEST_TIMEOUT)
            .context("read pi conversation")?;
        let messages = messages
            .as_array()
            .or_else(|| messages.get("messages").and_then(Value::as_array))
            .cloned()
            .unwrap_or_default();
        let (entries, dropped) = entries_from_messages(&messages);
        {
            let mut thread = self.state.lock();
            thread.status = AssistantStatus::Idle;
            thread.model = model;
            thread.entries = entries.into();
            thread.truncated = dropped;
            thread.pending_approval = None;
            thread.open_assistant = None;
            thread.run_active = false;
            enforce_entry_cap(&mut thread);
        }
        self.bump_revision();
        self.update_pane_status(PaneStatus::Idle);
        Ok(())
    }

    fn discover_pi_cached(&self) -> Result<PiInstall, String> {
        let mut cache = self.discovery_cache.lock();
        if let Some(result) = cache.as_ref() {
            return result.clone();
        }
        let result = discover_pi().map_err(|error| format!("{error:#}"));
        *cache = Some(result.clone());
        result
    }

    fn spawn_context(&self) -> Result<(AssistantSettings, PathBuf, String)> {
        let registry = self
            .registry
            .read()
            .upgrade()
            .context("assistant runtime is not bound to the session registry")?;
        let state = registry.read();
        let workspace = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == self.workspace_id)
            .context("assistant workspace no longer exists")?;
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, self.pane_id))
            .context("assistant tab no longer exists")?;
        let parent_project_dir = tab.parent_tab.and_then(|parent_id| {
            workspace
                .tabs
                .iter()
                .find(|candidate| candidate.id == parent_id)
                .and_then(|candidate| candidate.project_dir.as_deref())
        });
        let cwd = [
            tab.project_dir.as_deref(),
            parent_project_dir,
            workspace.working_dir.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|path| valid_local_cwd(path))
        .map_or_else(fallback_cwd, Ok)?;
        let settings = state.snapshot.assistant.clone();
        let instructions = workspace.instructions.clone();
        drop(state);
        let agents = if let Some(list) = self.coding_agents.lock().clone() {
            list
        } else {
            let list = discover_coding_agents().unwrap_or_default();
            *self.coding_agents.lock() = Some(list.clone());
            list
        };
        let system_prompt = system_prompt(
            &cwd,
            &agents,
            settings.preferred_agent,
            instructions.as_deref(),
        );
        Ok((settings, cwd, system_prompt))
    }

    pub(crate) fn view(&self) -> AssistantThreadView {
        let thread = self.state.lock();
        let mut view = AssistantThreadView {
            pane_id: self.pane_id,
            revision: self.revision.load(Ordering::Acquire),
            status: thread.status.clone(),
            model: thread.model.clone(),
            entries: thread.entries.iter().cloned().collect(),
            truncated_entries: thread.truncated,
            pending_approval: thread.pending_approval.clone(),
        };
        drop(thread);
        while serde_json::to_vec(&view)
            .is_ok_and(|encoded| encoded.len() > MAX_ASSISTANT_VIEW_BYTES)
            && !view.entries.is_empty()
        {
            view.entries.remove(0);
            view.truncated_entries = view.truncated_entries.saturating_add(1);
        }
        view
    }

    pub(crate) fn prompt(&self, text: String, images: &[AssistantImage]) -> Result<()> {
        if text.chars().count() > MAX_ASSISTANT_PROMPT_CHARS {
            bail!("assistant prompt exceeds {MAX_ASSISTANT_PROMPT_CHARS} characters");
        }
        for image in images {
            if !matches!(
                image.mime_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp"
            ) {
                bail!("unsupported assistant image type: {}", image.mime_type);
            }
            let encoded_limit = MAX_ASSISTANT_IMAGE_BYTES.saturating_mul(4) / 3 + 4;
            if image.base64.len() > encoded_limit {
                bail!("assistant image exceeds {MAX_ASSISTANT_IMAGE_BYTES} bytes");
            }
        }
        let process = self.process()?;
        let state = process.request(json!({"type":"get_state"}), PI_REQUEST_TIMEOUT)?;
        let mut request = json!({
            "type":"prompt",
            "message":text,
            "images":images.iter().map(|image| json!({
                "type":"image",
                "data":image.base64,
                "mimeType":image.mime_type,
            })).collect::<Vec<_>>()
        });
        if state
            .get("isStreaming")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            request["streamingBehavior"] = Value::String("steer".to_owned());
        }
        process.request(request, PI_REQUEST_TIMEOUT)?;
        let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        let mut thread = self.state.lock();
        push_entry(
            &mut thread,
            AssistantEntry::User {
                text,
                image_count,
                timestamp_ms: now_ms(),
            },
        );
        drop(thread);
        self.bump_revision();
        Ok(())
    }

    pub(crate) fn abort(&self) -> Result<()> {
        self.process()?
            .request(json!({"type":"abort"}), PI_ABORT_TIMEOUT)?;
        Ok(())
    }

    pub(crate) fn restart(self: &Arc<Self>) {
        if let Some(process) = self.process.lock().take() {
            process.shutdown();
        }
        *self.discovery_cache.lock() = None;
        *self.coding_agents.lock() = None;
        self.start();
    }

    pub(crate) fn approval_response(&self, request_id: &str, allow: bool) -> Result<()> {
        {
            let thread = self.state.lock();
            let pending = thread
                .pending_approval
                .as_ref()
                .filter(|pending| pending.request_id == request_id)
                .with_context(|| format!("approval {request_id} is not pending"))?;
            let _ = pending;
        }
        self.process()?.notify(&json!({
            "type":"extension_ui_response",
            "id":request_id,
            "confirmed":allow
        }))?;
        let mut thread = self.state.lock();
        thread.pending_approval = None;
        let run_active = thread.run_active;
        drop(thread);
        self.bump_revision();
        self.update_pane_status(if run_active {
            PaneStatus::Working
        } else {
            PaneStatus::Idle
        });
        Ok(())
    }

    pub(crate) fn models(&self) -> Result<Vec<AssistantModel>> {
        let value = self
            .process()?
            .request(json!({"type":"get_available_models"}), PI_REQUEST_TIMEOUT)?;
        let models = value
            .as_array()
            .or_else(|| value.get("models").and_then(Value::as_array))
            .into_iter()
            .flatten()
            .filter_map(|model| {
                Some(AssistantModel {
                    provider: model.get("provider")?.as_str()?.to_owned(),
                    id: model.get("id")?.as_str()?.to_owned(),
                    name: model
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| model.get("id").and_then(Value::as_str).unwrap_or(""))
                        .to_owned(),
                })
            })
            .collect();
        Ok(models)
    }

    pub(crate) fn set_model(&self, provider: &str, model_id: &str) -> Result<()> {
        self.process()?.request(
            json!({"type":"set_model","provider":provider,"modelId":model_id}),
            PI_REQUEST_TIMEOUT,
        )?;
        self.state.lock().model = Some(format!("{provider}/{model_id}"));
        self.bump_revision();
        Ok(())
    }

    pub(crate) fn shutdown(&self) {
        if let Some(process) = self.process.lock().take() {
            process.shutdown();
        }
    }

    pub(crate) fn remove_session_dir(&self) -> Result<()> {
        match fs::remove_dir_all(&self.session_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "remove assistant session directory {}",
                    self.session_dir.display()
                )
            }),
        }
    }

    pub(crate) fn apply_event(&self, value: &Value) {
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "agent_start" => {
                let mut thread = self.state.lock();
                thread.status = AssistantStatus::Streaming;
                thread.run_active = true;
                drop(thread);
                self.update_pane_status(PaneStatus::Working);
                self.bump_revision();
            }
            "agent_settled" => {
                let mut thread = self.state.lock();
                thread.status = AssistantStatus::Idle;
                thread.run_active = false;
                if let Some(index) = thread.open_assistant.take()
                    && let Some(AssistantEntry::Assistant { final_, .. }) =
                        thread.entries.get_mut(index)
                {
                    *final_ = true;
                }
                drop(thread);
                self.update_pane_status(PaneStatus::Idle);
                self.bump_revision();
            }
            "compaction_start" => {
                self.state.lock().status = AssistantStatus::Compacting;
                self.update_pane_status(PaneStatus::Working);
                self.bump_revision();
            }
            "compaction_end" => {
                let mut thread = self.state.lock();
                thread.status = if thread.run_active {
                    AssistantStatus::Streaming
                } else {
                    AssistantStatus::Idle
                };
                let working = thread.run_active;
                drop(thread);
                self.update_pane_status(if working {
                    PaneStatus::Working
                } else {
                    PaneStatus::Idle
                });
                self.bump_revision();
            }
            "message_start" => self.apply_message_start(value),
            "message_update" => self.apply_message_update(value),
            "message_end" => self.apply_message_end(value),
            "tool_execution_update" => self.apply_tool_update(value, false),
            "tool_execution_end" => self.apply_tool_update(value, true),
            "extension_ui_request" => self.apply_ui_request(value),
            "extension_error" => {
                let message = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("pi extension error")
                    .to_owned();
                let mut thread = self.state.lock();
                push_entry(
                    &mut thread,
                    AssistantEntry::Notice {
                        message,
                        level: AssistantNoticeLevel::Error,
                    },
                );
                drop(thread);
                self.bump_revision();
            }
            "__hh_exit" => {
                let explicit_message = value.get("message").and_then(Value::as_str);
                let process = self.process.lock().take();
                let message = explicit_message.map_or_else(
                    || {
                        process.as_ref().map_or_else(
                            || "pi exited".to_owned(),
                            |process| process.stderr_message(),
                        )
                    },
                    str::to_owned,
                );
                let mut thread = self.state.lock();
                thread.status = AssistantStatus::Exited { message };
                thread.pending_approval = None;
                thread.run_active = false;
                thread.open_assistant = None;
                drop(thread);
                drop(process);
                self.update_pane_status(PaneStatus::Idle);
                self.bump_revision();
            }
            _ => {}
        }
    }

    fn apply_message_start(&self, value: &Value) {
        let Some(message) = value.get("message") else {
            return;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let mut thread = self.state.lock();
        push_entry(
            &mut thread,
            AssistantEntry::Assistant {
                text: String::new(),
                final_: false,
                timestamp_ms: timestamp_ms(message),
            },
        );
        thread.open_assistant = Some(thread.entries.len().saturating_sub(1));
        drop(thread);
        self.bump_revision();
    }

    fn apply_message_update(&self, value: &Value) {
        let event = value
            .get("assistantMessageEvent")
            .or_else(|| {
                value
                    .get("event")
                    .and_then(|event| event.get("assistantMessageEvent"))
            })
            .or_else(|| value.get("event"))
            .unwrap_or(value);
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut thread = self.state.lock();
        match event_type {
            "text_delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let index = ensure_open_assistant(&mut thread);
                if let Some(AssistantEntry::Assistant { text, .. }) = thread.entries.get_mut(index)
                {
                    text.push_str(delta);
                }
            }
            "toolcall_start" => {
                let id = tool_call_id(event).unwrap_or_default().to_owned();
                let name = tool_name(event).unwrap_or_default().to_owned();
                push_entry(
                    &mut thread,
                    AssistantEntry::ToolCall {
                        tool_call_id: id,
                        tool_name: name,
                        summary: String::new(),
                        output: String::new(),
                        done: false,
                        is_error: false,
                        target_pane: None,
                    },
                );
            }
            "toolcall_end" => {
                let Some(call) = event.get("toolCall") else {
                    return;
                };
                let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                let arguments = call.get("arguments").cloned().unwrap_or(Value::Null);
                if let Some(AssistantEntry::ToolCall {
                    tool_name, summary, ..
                }) = find_tool_call_mut(&mut thread.entries, id)
                {
                    *summary = summarize_tool_args(name, &arguments);
                    if tool_name.is_empty() {
                        name.clone_into(tool_name);
                    }
                } else {
                    push_entry(
                        &mut thread,
                        AssistantEntry::ToolCall {
                            tool_call_id: id.to_owned(),
                            tool_name: name.to_owned(),
                            summary: summarize_tool_args(name, &arguments),
                            output: String::new(),
                            done: false,
                            is_error: false,
                            target_pane: None,
                        },
                    );
                }
            }
            _ => return,
        }
        drop(thread);
        self.bump_revision();
    }

    fn apply_message_end(&self, value: &Value) {
        let Some(message) = value.get("message") else {
            return;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let mut thread = self.state.lock();
        let text = content_text(message.get("content"));
        if let Some(index) = thread.open_assistant.take() {
            if let Some(AssistantEntry::Assistant {
                text: current,
                final_,
                ..
            }) = thread.entries.get_mut(index)
            {
                *current = text;
                *final_ = true;
            }
        } else if !text.is_empty() {
            push_entry(
                &mut thread,
                AssistantEntry::Assistant {
                    text,
                    final_: true,
                    timestamp_ms: timestamp_ms(message),
                },
            );
        }
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                    continue;
                }
                let id = tool_call_id(block).unwrap_or_default();
                if find_tool_call_mut(&mut thread.entries, id).is_some() {
                    continue;
                }
                let name = tool_name(block).unwrap_or_default();
                push_entry(
                    &mut thread,
                    AssistantEntry::ToolCall {
                        tool_call_id: id.to_owned(),
                        tool_name: name.to_owned(),
                        summary: summarize_tool_args(
                            name,
                            block.get("arguments").unwrap_or(&Value::Null),
                        ),
                        output: String::new(),
                        done: false,
                        is_error: false,
                        target_pane: None,
                    },
                );
            }
        }
        drop(thread);
        self.bump_revision();
    }

    fn apply_tool_update(&self, value: &Value, done: bool) {
        let Some(id) = tool_call_id(value) else {
            return;
        };
        let result = if done {
            value.get("result")
        } else {
            value.get("partialResult").or_else(|| value.get("result"))
        }
        .unwrap_or(&Value::Null);
        let output = bounded_tool_output(result_text(result));
        let is_error = value
            .get("isError")
            .or_else(|| result.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let target_pane = result
            .get("details")
            .and_then(|details| details.get("pane_id"))
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok());
        let mut thread = self.state.lock();
        if let Some(AssistantEntry::ToolCall {
            output: current,
            done: current_done,
            is_error: current_error,
            target_pane: current_target,
            ..
        }) = find_tool_call_mut(&mut thread.entries, id)
        {
            *current = output;
            *current_done = done;
            *current_error = is_error;
            *current_target = target_pane;
        }
        drop(thread);
        self.bump_revision();
    }

    fn apply_ui_request(&self, value: &Value) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = value.get("id").and_then(Value::as_str).unwrap_or_default();
        let title = value
            .get("title")
            .or_else(|| value.get("params").and_then(|params| params.get("title")))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = value
            .get("message")
            .or_else(|| value.get("params").and_then(|params| params.get("message")))
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "confirm" => {
                if self.access_mode() == AssistantAccess::Full {
                    let _ = self.process().and_then(|process| {
                        process.notify(&json!({
                            "type":"extension_ui_response",
                            "id":id,
                            "confirmed":true
                        }))
                    });
                } else {
                    self.state.lock().pending_approval = Some(AssistantApproval {
                        request_id: id.to_owned(),
                        title: title.to_owned(),
                        message: message.to_owned(),
                    });
                    self.update_pane_status(PaneStatus::NeedsApproval);
                }
                self.bump_revision();
            }
            "notify" => {
                let level = match value
                    .get("notifyType")
                    .or_else(|| value.get("params").and_then(|params| params.get("type")))
                    .and_then(Value::as_str)
                {
                    Some("warning") => AssistantNoticeLevel::Warning,
                    Some("error") => AssistantNoticeLevel::Error,
                    _ => AssistantNoticeLevel::Info,
                };
                let mut thread = self.state.lock();
                push_entry(
                    &mut thread,
                    AssistantEntry::Notice {
                        message: message.to_owned(),
                        level,
                    },
                );
                drop(thread);
                self.bump_revision();
            }
            "select" | "input" | "editor" => {
                let _ = self.process().and_then(|process| {
                    process.notify(&json!({
                        "type":"extension_ui_response",
                        "id":id,
                        "cancelled":true
                    }))
                });
                self.bump_revision();
            }
            _ => {}
        }
    }

    fn access_mode(&self) -> AssistantAccess {
        self.registry
            .read()
            .upgrade()
            .map(|registry| registry.read().snapshot.assistant.access)
            .unwrap_or_default()
    }

    fn process(&self) -> Result<Arc<PiProcess>> {
        self.process
            .lock()
            .as_ref()
            .cloned()
            .context("assistant is not running")
    }

    fn set_status(&self, status: AssistantStatus) {
        let pane_status = match &status {
            AssistantStatus::Streaming | AssistantStatus::Compacting => PaneStatus::Working,
            AssistantStatus::Starting
            | AssistantStatus::Idle
            | AssistantStatus::Exited { .. }
            | AssistantStatus::Unavailable { .. } => PaneStatus::Idle,
        };
        self.state.lock().status = status;
        self.update_pane_status(pane_status);
        self.bump_revision();
    }

    fn update_pane_status(&self, status: PaneStatus) {
        if let Some(registry) = self.registry.read().upgrade() {
            registry.write().set_pane_status(self.pane_id, status);
        }
    }

    fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for AssistantRuntime {
    fn drop(&mut self) {
        if let Some(process) = self.process.get_mut().take() {
            process.shutdown();
        }
    }
}

pub(crate) fn entries_from_messages(messages: &[Value]) -> (Vec<AssistantEntry>, u32) {
    let mut entries = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => entries.push(AssistantEntry::User {
                text: content_text(message.get("content")),
                image_count: message.get("content").and_then(Value::as_array).map_or(
                    0,
                    |content| {
                        u32::try_from(
                            content
                                .iter()
                                .filter(|block| {
                                    block.get("type").and_then(Value::as_str) == Some("image")
                                })
                                .count(),
                        )
                        .unwrap_or(u32::MAX)
                    },
                ),
                timestamp_ms: timestamp_ms(message),
            }),
            Some("assistant") => {
                let text = content_text(message.get("content"));
                if !text.is_empty() {
                    entries.push(AssistantEntry::Assistant {
                        text,
                        final_: true,
                        timestamp_ms: timestamp_ms(message),
                    });
                }
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                            let name = tool_name(block).unwrap_or_default();
                            entries.push(AssistantEntry::ToolCall {
                                tool_call_id: tool_call_id(block).unwrap_or_default().to_owned(),
                                tool_name: name.to_owned(),
                                summary: summarize_tool_args(
                                    name,
                                    block.get("arguments").unwrap_or(&Value::Null),
                                ),
                                output: String::new(),
                                done: true,
                                is_error: false,
                                target_pane: None,
                            });
                        }
                    }
                }
            }
            Some("toolResult") => {
                let id = tool_call_id(message).unwrap_or_default();
                if let Some(AssistantEntry::ToolCall {
                    output,
                    done,
                    is_error,
                    target_pane,
                    ..
                }) = entries.iter_mut().rev().find(|entry| {
                    matches!(entry, AssistantEntry::ToolCall { tool_call_id, .. } if tool_call_id == id)
                }) {
                    *output = bounded_tool_output(result_text(message));
                    *done = true;
                    *is_error = message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    *target_pane = message
                        .get("details")
                        .and_then(|details| details.get("pane_id"))
                        .and_then(Value::as_str)
                        .and_then(|id| Uuid::parse_str(id).ok());
                }
            }
            _ => {}
        }
    }
    let dropped = entries.len().saturating_sub(MAX_ASSISTANT_ENTRIES);
    if dropped > 0 {
        entries.drain(0..dropped);
    }
    (entries, u32::try_from(dropped).unwrap_or(u32::MAX))
}

pub(crate) fn summarize_tool_args(_name: &str, args: &Value) -> String {
    let serialized = serde_json::to_string(args).unwrap_or_else(|_| "null".to_owned());
    if serialized.chars().count() <= 160 {
        return serialized;
    }
    let mut summary = serialized.chars().take(159).collect::<String>();
    summary.push('…');
    summary
}

fn bounded_tool_output(mut text: String) -> String {
    let Some((byte_index, _)) = text.char_indices().nth(MAX_TOOL_OUTPUT_CHARS) else {
        return text;
    };
    text.truncate(byte_index);
    text.push_str("\n… output truncated");
    text
}

fn system_prompt(
    cwd: &Path,
    agents: &[CodingAgent],
    preferred: Option<TerminalProfile>,
    instructions: Option<&str>,
) -> String {
    let mut prompt = String::from(
        "You are the Harness Harlot orchestrator. You never edit files or run commands yourself. You work by opening terminals and browsers in the user's workstations with the hh_* tools, launching agent CLIs or shell commands inside those terminals, watching them with hh_read and hh_wait, and answering their prompts with hh_send.\nRules:\n1. Call hh_list before using any id you have not seen in this conversation.\n2. Keep one window per task; put a task's related terminals in the same window and give windows and terminals short descriptive titles. Browsers: use hh_browser_goto/read/screenshot/click/fill/press/eval on browser panes; hh_read and hh_send are terminal-only.\n",
    );
    if agents.is_empty() {
        prompt.push_str(
            "3. No coding agent CLI is installed on this machine; say so instead of attempting coding work.\n",
        );
    } else {
        prompt.push_str("3. For coding work, launch a coding agent in a terminal and give it the complete task in its first message. Installed coding agents (command — product): ");
        for (index, agent) in agents.iter().enumerate() {
            if index > 0 {
                prompt.push_str("; ");
            }
            prompt.push_str(&agent.command);
            prompt.push_str(" — ");
            prompt.push_str(agent.profile.display_name());
        }
        prompt.push_str(". ");
        match preferred.and_then(|profile| agents.iter().find(|agent| agent.profile == profile)) {
            Some(agent) => {
                prompt.push_str("Use ");
                prompt.push_str(&agent.command);
                prompt.push_str(" unless the user asks for a different agent.\n");
            }
            None => {
                prompt.push_str("Choose the best fit for the task unless the user names one.\n");
            }
        }
    }
    prompt.push_str("4. After launching something, use hh_wait until it asks a question, goes idle, or exits; then tell the user what happened in one to three short sentences. The user may be listening by voice.\n5. Never claim something finished unless hh_read shows it.\n6. Default working directory: ");
    prompt.push_str(&cwd.to_string_lossy());
    if let Some(instructions) = instructions.filter(|instructions| !instructions.is_empty()) {
        prompt.push_str("\nOperator instructions for this assistant:\n");
        prompt.push_str(instructions);
    }
    prompt
}

fn newest_jsonl(directory: &Path) -> Result<Option<PathBuf>> {
    let mut newest = None;
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read assistant session directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        if newest
            .as_ref()
            .is_none_or(|(_, newest_modified)| modified > *newest_modified)
        {
            newest = Some((path, modified));
        }
    }
    Ok(newest.map(|(path, _)| path))
}

fn model_identifier(model: Option<&Value>) -> Option<String> {
    let model = model?;
    if let Some(identifier) = model.as_str() {
        return Some(identifier.to_owned());
    }
    Some(format!(
        "{}/{}",
        model.get("provider")?.as_str()?,
        model.get("id")?.as_str()?
    ))
}

fn ensure_open_assistant(thread: &mut AssistantThreadState) -> usize {
    if let Some(index) = thread.open_assistant {
        return index;
    }
    push_entry(
        thread,
        AssistantEntry::Assistant {
            text: String::new(),
            final_: false,
            timestamp_ms: now_ms(),
        },
    );
    let index = thread.entries.len().saturating_sub(1);
    thread.open_assistant = Some(index);
    index
}

fn push_entry(thread: &mut AssistantThreadState, entry: AssistantEntry) {
    thread.entries.push_back(entry);
    enforce_entry_cap(thread);
}

fn enforce_entry_cap(thread: &mut AssistantThreadState) {
    while thread.entries.len() > MAX_ASSISTANT_ENTRIES {
        thread.entries.pop_front();
        thread.truncated = thread.truncated.saturating_add(1);
        thread.open_assistant = thread.open_assistant.map(|index| index.saturating_sub(1));
    }
}

fn find_tool_call_mut<'a>(
    entries: &'a mut VecDeque<AssistantEntry>,
    id: &str,
) -> Option<&'a mut AssistantEntry> {
    entries.iter_mut().rev().find(|entry| {
        matches!(entry, AssistantEntry::ToolCall { tool_call_id, .. } if tool_call_id == id)
    })
}

fn tool_call_id(value: &Value) -> Option<&str> {
    value
        .get("toolCallId")
        .or_else(|| value.get("tool_call_id"))
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
}

fn tool_name(value: &Value) -> Option<&str> {
    value
        .get("toolName")
        .or_else(|| value.get("tool_name"))
        .or_else(|| value.get("name"))
        .and_then(Value::as_str)
}

fn content_text(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect()
}

fn result_text(result: &Value) -> String {
    if let Some(text) = result.as_str() {
        return text.to_owned();
    }
    content_text(result.get("content"))
}

fn timestamp_ms(message: &Value) -> u64 {
    message
        .get("timestamp")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_argument_summary_is_unicode_safe_and_bounded() {
        let summary = summarize_tool_args("hh_send", &json!({"text":"界".repeat(200)}));
        assert_eq!(summary.chars().count(), 160);
        assert!(summary.ends_with('…'));
    }

    fn agent(profile: TerminalProfile, command: &str) -> CodingAgent {
        CodingAgent {
            profile,
            command: command.to_owned(),
            path: format!("/usr/local/bin/{command}"),
        }
    }

    #[test]
    fn prompt_lists_installed_agents_and_preferred_command() {
        let agents = [
            agent(TerminalProfile::Claude, "claude"),
            agent(TerminalProfile::Omp, "omp"),
        ];
        let prompt = system_prompt(Path::new("/tmp"), &agents, Some(TerminalProfile::Omp), None);
        assert!(prompt.contains("Installed coding agents (command — product): "));
        assert!(prompt.contains("claude — Claude Code"));
        assert!(prompt.contains("omp — omp"));
        assert!(prompt.contains("Use omp unless the user asks for a different agent."));
    }

    #[test]
    fn prompt_ignores_a_preferred_agent_that_is_not_installed() {
        let agents = [agent(TerminalProfile::Claude, "claude")];
        let prompt = system_prompt(
            Path::new("/tmp"),
            &agents,
            Some(TerminalProfile::Aider),
            None,
        );
        assert!(prompt.contains("Choose the best fit for the task unless the user names one."));
        assert!(!prompt.contains("unless the user asks for a different agent"));
    }

    #[test]
    fn prompt_states_when_no_agent_is_installed() {
        let prompt = system_prompt(Path::new("/tmp"), &[], Some(TerminalProfile::Omp), None);
        assert!(prompt.contains("No coding agent CLI is installed on this machine"));
        assert!(!prompt.contains("Installed coding agents"));
    }

    #[test]
    fn streamed_tool_events_use_nested_call_and_top_level_error() {
        let runtime = AssistantRuntime::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
        )
        .unwrap();
        runtime.apply_event(&json!({
            "type":"message_update",
            "assistantMessageEvent":{
                "type":"toolcall_start",
                "id":"call-1",
                "toolName":""
            }
        }));
        let after_start = runtime.view().revision;
        runtime.apply_event(&json!({
            "type":"message_update",
            "assistantMessageEvent":{
                "type":"toolcall_delta",
                "contentIndex":0,
                "delta":"ignored"
            }
        }));
        assert_eq!(runtime.view().revision, after_start);

        runtime.apply_event(&json!({
            "type":"message_update",
            "assistantMessageEvent":{
                "type":"toolcall_end",
                "contentIndex":0,
                "toolCall":{
                    "type":"toolCall",
                    "id":"call-1",
                    "name":"hh_read",
                    "arguments":{"pane_id":"pane-1"}
                }
            }
        }));
        runtime.apply_event(&json!({
            "type":"tool_execution_end",
            "toolCallId":"call-1",
            "result":{"content":[{"type":"text","text":"failed"}]},
            "isError":true
        }));

        assert!(matches!(
            &runtime.view().entries[0],
            AssistantEntry::ToolCall {
                tool_name,
                summary,
                output,
                done: true,
                is_error: true,
                ..
            } if tool_name == "hh_read"
                && summary == "{\"pane_id\":\"pane-1\"}"
                && output == "failed"
        ));
        runtime.apply_event(&json!({
            "type":"message_update",
            "assistantMessageEvent":{
                "type":"toolcall_end",
                "toolCall":{
                    "type":"toolCall",
                    "id":"call-2",
                    "name":"hh_list",
                    "arguments":{}
                }
            }
        }));
        assert!(matches!(
            &runtime.view().entries[1],
            AssistantEntry::ToolCall {
                tool_call_id,
                tool_name,
                summary,
                ..
            } if tool_call_id == "call-2" && tool_name == "hh_list" && summary == "{}"
        ));
    }

    #[test]
    fn recovered_entries_report_drops_and_bound_tool_output() {
        let messages = (0..MAX_ASSISTANT_ENTRIES + 3)
            .map(|index| {
                json!({
                    "role":"user",
                    "content":[{"type":"text","text":index.to_string()}]
                })
            })
            .collect::<Vec<_>>();
        let (entries, dropped) = entries_from_messages(&messages);
        assert_eq!(entries.len(), MAX_ASSISTANT_ENTRIES);
        assert_eq!(dropped, 3);
        assert!(matches!(
            &entries[0],
            AssistantEntry::User { text, .. } if text == "3"
        ));

        let oversized = "界".repeat(20_000);
        let (entries, _) = entries_from_messages(&[
            json!({
                "role":"assistant",
                "content":[
                    {"type":"toolCall","id":"call-1","name":"hh_read","arguments":{}}
                ]
            }),
            json!({
                "role":"toolResult",
                "toolCallId":"call-1",
                "content":[{"type":"text","text":oversized}]
            }),
        ]);
        let AssistantEntry::ToolCall { output, .. } = &entries[0] else {
            panic!("expected recovered tool call");
        };
        assert!(output.ends_with("… output truncated"));
        assert_eq!(
            output.chars().count(),
            MAX_TOOL_OUTPUT_CHARS + "\n… output truncated".chars().count()
        );
    }

    #[test]
    fn rebuilds_assistant_tool_results_from_messages() {
        let (entries, dropped) = entries_from_messages(&[
            json!({
                "role":"assistant",
                "timestamp":7,
                "content":[
                    {"type":"text","text":"Working"},
                    {"type":"toolCall","id":"call-1","name":"hh_read","arguments":{"pane_id":"p"}}
                ]
            }),
            json!({
                "role":"toolResult",
                "toolCallId":"call-1",
                "content":[{"type":"text","text":"done"}],
                "isError":false
            }),
        ]);
        assert_eq!(dropped, 0);
        assert!(matches!(
            &entries[1],
            AssistantEntry::ToolCall { output, done: true, .. } if output == "done"
        ));
    }
}
