use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement, IntoElement, Keystroke, ParentElement,
    PathPromptOptions, ScrollHandle, StatefulInteractiveElement, Styled, StyledImage, div, img, px,
    relative, rgb,
};
use hh_protocol::{ClientRequest, Pane, ServiceResponse};
use hh_voice::threads::{self, ThreadRecord, ThreadRole, ThreadSummary};
use hh_voice::{
    AssistantContext, EngineState, HonchoSettings, VoiceCommand, VoiceEngineHandle, VoiceSettings,
    VoiceUiEvent, spawn_engine,
};
use uuid::Uuid;

use crate::helpers::{element_key, find_pane};
use crate::view_models::{AssistantComposer, ComposerAttachment, Modal, TooltipView};
use crate::{HhApp, PANE_HEADER_HEIGHT, THEME};
use gpui::AppContext as _;

const ASSISTANT_SUMMARY_MAX_BYTES: u64 = 16 * 1024;
const MAX_ASSISTANT_IMAGE_BYTES: usize = 4 * 1024 * 1024;

fn local_datetime(at_ms: u64) -> Option<time::OffsetDateTime> {
    let datetime =
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000).ok()?;
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    Some(datetime.to_offset(offset))
}

fn format_clock(at_ms: u64) -> String {
    local_datetime(at_ms)
        .and_then(|datetime| {
            datetime
                .format(&time::macros::format_description!("[hour]:[minute]"))
                .ok()
        })
        .unwrap_or_else(|| "--:--".to_owned())
}

fn format_thread_activity(at_ms: u64) -> String {
    local_datetime(at_ms)
        .and_then(|datetime| {
            datetime
                .format(&time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]"
                ))
                .ok()
        })
        .unwrap_or_else(|| "Unknown time".to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VoiceTranscriptRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VoiceTranscriptEntry {
    pub role: VoiceTranscriptRole,
    pub text: String,
    pub final_: bool,
    pub timestamp: String,
    pub image: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VoiceLedgerEntry {
    pub name: String,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VoiceApproval {
    pub id: u64,
    pub description: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VoiceSettingsField {
    ApiKey,
    IdleTimeout,
    HonchoBaseUrl,
    HonchoWorkspace,
}

#[derive(Clone, Debug)]
pub(crate) struct VoiceSettingsEditor {
    pub settings: VoiceSettings,
    pub api_key_input: String,
    pub idle_timeout_input: String,
    pub honcho_base_url_input: String,
    pub honcho_workspace_input: String,
    pub active_field: Option<VoiceSettingsField>,
}

impl VoiceSettingsEditor {
    pub(crate) fn load() -> Self {
        let settings = VoiceSettings::load();
        let honcho = settings.honcho.clone().unwrap_or_default();
        Self {
            api_key_input: settings.api_key.clone(),
            idle_timeout_input: settings.idle_timeout_secs.to_string(),
            honcho_base_url_input: honcho.base_url,
            honcho_workspace_input: honcho.workspace,
            settings,
            active_field: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistedSummaryState {
    Absent,
    Present,
}

/// Per-pane voice assistant state. Reconciliation creates one entry for every
/// assistant pane; engine absence means "not running".
pub(crate) struct AssistantSession {
    pub engine: Option<VoiceEngineHandle>,
    pub engine_state: EngineState,
    pub active_tool: Option<String>,
    pub transcript: Vec<VoiceTranscriptEntry>,
    pub ledger: Vec<VoiceLedgerEntry>,
    pub approvals: Vec<VoiceApproval>,
    pub mic_muted: bool,
    pub speaker_muted: bool,
    pub mic_level: f32,
    pub user_speaking: bool,
    pub assistant_reveal_chars: usize,
    pub selected_transcript: Option<usize>,
    pub transcript_scroll: ScrollHandle,
    persisted_summary: PersistedSummaryState,
}

impl AssistantSession {
    fn new() -> Self {
        Self {
            engine: None,
            engine_state: EngineState::Suspended,
            active_tool: None,
            transcript: Vec::new(),
            ledger: Vec::new(),
            approvals: Vec::new(),
            mic_muted: false,
            speaker_muted: false,
            mic_level: 0.0,
            user_speaking: false,
            assistant_reveal_chars: 0,
            selected_transcript: None,
            transcript_scroll: ScrollHandle::new(),
            persisted_summary: PersistedSummaryState::Absent,
        }
    }

    fn load(pane_id: Uuid) -> Self {
        let mut session = Self::new();
        if let Some(thread) = threads::read_thread(pane_id) {
            let mut transcript = thread
                .entries
                .into_iter()
                .rev()
                .filter_map(|record| match record {
                    ThreadRecord::Turn { role, text, at_ms } => Some(VoiceTranscriptEntry {
                        role: match role {
                            ThreadRole::User => VoiceTranscriptRole::User,
                            ThreadRole::Assistant => VoiceTranscriptRole::Assistant,
                        },
                        text,
                        final_: true,
                        timestamp: format_clock(at_ms),
                        image: None,
                    }),
                    _ => None,
                })
                .take(50)
                .collect::<Vec<_>>();
            transcript.reverse();
            session.transcript = transcript;
        }
        session.persisted_summary = if load_assistant_summary(pane_id).is_some() {
            PersistedSummaryState::Present
        } else {
            PersistedSummaryState::Absent
        };
        session
    }
}

impl Default for AssistantSession {
    fn default() -> Self {
        Self::new()
    }
}
fn toggle_microphone_muted(session: &mut AssistantSession) -> bool {
    session.mic_muted = !session.mic_muted;
    !session.mic_muted
}

fn toggle_headphones_muted(session: &mut AssistantSession) -> bool {
    session.speaker_muted = !session.speaker_muted;
    session.speaker_muted
}

pub(crate) struct VoiceUi {
    pub thread_index: Vec<ThreadSummary>,
    pub sessions: HashMap<Uuid, AssistantSession>,
    pub settings_editor: VoiceSettingsEditor,
    pub quit_subscription: Option<gpui::Subscription>,
}

impl VoiceUi {
    pub(crate) fn new() -> Self {
        let mut voice = Self {
            sessions: HashMap::new(),
            settings_editor: VoiceSettingsEditor::load(),
            quit_subscription: None,
            thread_index: Vec::new(),
        };
        voice.refresh_thread_index();
        voice
    }

    pub(crate) fn refresh_thread_index(&mut self) {
        self.thread_index = threads::list_threads();
    }
}

impl HhApp {
    pub(crate) fn start_assistant(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        if self
            .voice
            .sessions
            .get(&pane_id)
            .and_then(|session| session.engine.as_ref())
            .is_some_and(|engine| !engine.is_finished())
        {
            return;
        }
        // A finished thread (startup failure or shutdown) never recovers;
        // replace it with a fresh engine.
        let settings = VoiceSettings::load();
        if settings.api_key.trim().is_empty() && std::env::var("HH_OPENAI_API_KEY").is_err() {
            self.voice.settings_editor = VoiceSettingsEditor::load();
            self.open_appearance_settings(cx);
            return;
        }
        let context = self.assistant_context_for_pane(pane_id);
        let (ui_tx, mut ui_rx) = futures::channel::mpsc::unbounded();
        match spawn_engine(settings, context, ui_tx) {
            Ok(engine) => {
                let session = self
                    .voice
                    .sessions
                    .entry(pane_id)
                    .or_insert_with(|| AssistantSession::load(pane_id));
                let mic_muted = session.mic_muted;
                let speaker_muted = session.speaker_muted;
                session.engine = Some(engine);
                session.engine_state = EngineState::Connecting;
                if let Some(engine) = session.engine.as_ref() {
                    engine.send(VoiceCommand::SetMicEnabled(!mic_muted));
                    if speaker_muted {
                        engine.send(VoiceCommand::SetSpeakerMuted(true));
                    }
                }
            }
            Err(error) => {
                let session = self
                    .voice
                    .sessions
                    .entry(pane_id)
                    .or_insert_with(|| AssistantSession::load(pane_id));
                session.engine_state = EngineState::Error(format!("{error:#}"));
                cx.notify();
                return;
            }
        }
        cx.spawn(async move |this, cx| {
            while let Some(event) = ui_rx.next().await {
                let Ok(()) = this.update(cx, |this, cx| {
                    this.apply_voice_event(pane_id, event);
                    cx.notify();
                }) else {
                    break;
                };
            }
        })
        .detach();
        cx.notify();
    }

    fn reopen_thread(&mut self, workspace_id: Uuid, thread_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::CreateAssistantTab { workspace_id },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    if threads::adopt_thread(thread_id, pane_id) {
                        this.voice
                            .sessions
                            .insert(pane_id, AssistantSession::load(pane_id));
                    }
                    this.voice.refresh_thread_index();
                    this.focus_created_pane(workspace_id, pane_id, cx);
                    this.start_assistant(pane_id, cx);
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        self.layout.last_sizes.clear();
        cx.notify();
    }

    /// Resolves the workspace containing an assistant pane and the working
    /// directory of its tab (falling back to the parent project tab, then the
    /// containing workspace's own working directory).
    pub(crate) fn assistant_context_for_pane(&self, pane_id: Uuid) -> AssistantContext {
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return AssistantContext::default();
        };
        for workspace in &snapshot.workspaces {
            for tab in &workspace.tabs {
                if find_pane(&tab.layout, pane_id).is_some() {
                    let working_dir = tab
                        .project_dir
                        .clone()
                        .or_else(|| {
                            tab.parent_tab.and_then(|parent_id| {
                                workspace
                                    .tabs
                                    .iter()
                                    .find(|candidate| candidate.id == parent_id)
                                    .and_then(|candidate| candidate.project_dir.clone())
                            })
                        })
                        .or_else(|| workspace.working_dir.clone());
                    return AssistantContext {
                        workspace_id: Some(workspace.id),
                        pane_id: Some(pane_id),
                        workspace_title: workspace.title.clone(),
                        workspace_kind: workspace.kind,
                        working_dir,
                        instructions: workspace.instructions.clone(),
                        prior_context: load_assistant_summary(pane_id).or_else(|| {
                            threads::read_thread(pane_id).and_then(|thread| thread.summary)
                        }),
                    };
                }
            }
        }
        AssistantContext::default()
    }

    pub(crate) fn send_assistant_command(&self, pane_id: Uuid, command: VoiceCommand) {
        if let Some(engine) = self
            .voice
            .sessions
            .get(&pane_id)
            .and_then(|session| session.engine.as_ref())
        {
            engine.send(command);
        }
    }
    pub(crate) fn submit_assistant_composer(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.editor.assistant_composer.take() else {
            return;
        };
        let pane_id = composer.pane_id;
        let text = composer.text.trim().to_owned();
        let attachment = composer.attachment.clone();
        if text.is_empty() && attachment.is_none() {
            cx.notify();
            return;
        }
        self.voice
            .sessions
            .entry(pane_id)
            .or_insert_with(|| AssistantSession::load(pane_id));
        let engine_running = self
            .voice
            .sessions
            .get(&pane_id)
            .and_then(|session| session.engine.as_ref())
            .is_some_and(|engine| !engine.is_finished());
        if !engine_running {
            self.start_assistant(pane_id, cx);
        }
        if let Some(ComposerAttachment {
            filename,
            data_url,
            path,
        }) = attachment
        {
            self.apply_transcript(
                pane_id,
                VoiceTranscriptRole::User,
                format!("[image attached: {filename}]"),
                true,
            );
            if let Some(entry) = self
                .voice
                .sessions
                .get_mut(&pane_id)
                .and_then(|session| session.transcript.last_mut())
            {
                entry.image = Some(path);
            }
            self.send_assistant_command(pane_id, VoiceCommand::SendUserImage { data_url });
        }
        if !text.is_empty() {
            self.apply_transcript(pane_id, VoiceTranscriptRole::User, text.clone(), true);
            self.send_assistant_command(pane_id, VoiceCommand::SendUserText(text));
        }
        self.editor.assistant_composer = Some(AssistantComposer {
            pane_id,
            text: String::new(),
            selection: None,
            attachment: None,
        });
        cx.notify();
    }

    pub(crate) fn attach_assistant_image(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Attach file".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match selection.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("assistant image picker failed: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let result = cx
                .background_spawn(async move {
                    let filename = path.file_name().map_or_else(
                        || "image".to_owned(),
                        |name| name.to_string_lossy().into_owned(),
                    );
                    let extension = path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(str::to_ascii_lowercase)
                        .ok_or_else(|| anyhow::anyhow!("image must have a supported extension"))?;
                    let mime = match extension.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "webp" => "image/webp",
                        _ => {
                            anyhow::bail!(
                                "unsupported file type; images (PNG, JPG, WebP) only for now"
                            )
                        }
                    };
                    let metadata = std::fs::metadata(&path).map_err(|error| {
                        anyhow::anyhow!("inspect assistant image {}: {error}", path.display())
                    })?;
                    if metadata.len() > MAX_ASSISTANT_IMAGE_BYTES as u64 {
                        anyhow::bail!("image exceeds the 4 MiB attachment limit");
                    }
                    let bytes = std::fs::read(&path).map_err(|error| {
                        anyhow::anyhow!("read assistant image {}: {error}", path.display())
                    })?;
                    if bytes.len() > MAX_ASSISTANT_IMAGE_BYTES {
                        anyhow::bail!("image exceeds the 4 MiB attachment limit");
                    }
                    Ok::<_, anyhow::Error>((
                        filename,
                        format!("data:{mime};base64,{}", BASE64.encode(bytes)),
                        path,
                    ))
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok((filename, data_url, path)) => {
                    this.focus_pane_with_snapshot(pane_id, cx);
                    activate_assistant_composer(&mut this.editor.assistant_composer, pane_id);
                    if let Some(composer) = this.editor.assistant_composer.as_mut() {
                        composer.attachment = Some(ComposerAttachment {
                            filename,
                            data_url,
                            path,
                        });
                    }
                    cx.notify();
                }
                Err(error) => {
                    let session = this
                        .voice
                        .sessions
                        .entry(pane_id)
                        .or_insert_with(|| AssistantSession::load(pane_id));
                    session.ledger.push(VoiceLedgerEntry {
                        name: "image.error".to_owned(),
                        summary: format!("{error:#}"),
                    });
                    if session.ledger.len() > 100 {
                        session.ledger.remove(0);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn toggle_assistant_mic(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        let Some(session) = self.voice.sessions.get_mut(&pane_id) else {
            return;
        };
        let enabled = toggle_microphone_muted(session);
        if let Some(engine) = session.engine.as_ref() {
            engine.send(VoiceCommand::SetMicEnabled(enabled));
        }
        cx.notify();
    }

    pub(crate) fn toggle_assistant_speaker(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        let Some(session) = self.voice.sessions.get_mut(&pane_id) else {
            return;
        };
        let muted = toggle_headphones_muted(session);
        if let Some(engine) = session.engine.as_ref() {
            engine.send(VoiceCommand::SetSpeakerMuted(muted));
        }
        cx.notify();
    }

    /// Drops sessions whose pane no longer exists (or is no longer an
    /// assistant). Engine threads are joined off the UI thread.
    pub(crate) fn prune_assistant_sessions(
        &mut self,
        live: &HashSet<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if self
            .editor
            .assistant_composer
            .as_ref()
            .is_some_and(|composer| !live.contains(&composer.pane_id))
        {
            self.editor.assistant_composer = None;
            self.editor.ime_preedit.clear();
        }
        for pane_id in live {
            self.voice
                .sessions
                .entry(*pane_id)
                .or_insert_with(|| AssistantSession::load(*pane_id));
        }
        let removed = self
            .voice
            .sessions
            .keys()
            .copied()
            .filter(|pane_id| !live.contains(pane_id))
            .collect::<Vec<_>>();
        for pane_id in removed {
            let Some(mut session) = self.voice.sessions.remove(&pane_id) else {
                continue;
            };
            if let Some(engine) = session.engine.take() {
                cx.background_spawn(async move {
                    engine.shutdown();
                })
                .detach();
            }
            delete_assistant_summary(pane_id);
        }
        self.voice.refresh_thread_index();
    }

    pub(crate) fn shutdown_voice(&mut self) {
        for session in self.voice.sessions.values_mut() {
            if let Some(engine) = session.engine.take() {
                engine.shutdown();
            }
        }
    }

    fn apply_voice_event(&mut self, pane_id: Uuid, event: VoiceUiEvent) {
        let Some(session) = self.voice.sessions.get_mut(&pane_id) else {
            return;
        };
        match event {
            VoiceUiEvent::State(state) => {
                if !matches!(&state, EngineState::ToolRunning) {
                    session.active_tool = None;
                }
                if let EngineState::Error(message) = &state {
                    session.ledger.push(VoiceLedgerEntry {
                        name: "engine.error".to_owned(),
                        summary: message.clone(),
                    });
                    if session.ledger.len() > 100 {
                        session.ledger.remove(0);
                    }
                }
                if state == EngineState::Suspended {
                    session.user_speaking = false;
                }
                session.engine_state = state;
            }
            VoiceUiEvent::UserSpeech { active } => {
                session.user_speaking = active;
            }
            VoiceUiEvent::UserTranscript { text, final_ } => {
                self.apply_transcript(pane_id, VoiceTranscriptRole::User, text, final_);
            }
            VoiceUiEvent::AssistantTranscript { text, final_ } => {
                self.apply_transcript(pane_id, VoiceTranscriptRole::Assistant, text, final_);
            }
            VoiceUiEvent::PlaybackProgress {
                played_ms,
                total_ms,
            } => {
                if total_ms > 0
                    && let Some(entry) = session.transcript.last()
                    && entry.role == VoiceTranscriptRole::Assistant
                    && !entry.final_
                {
                    let total_chars = entry.text.chars().count();
                    let target = usize::try_from(
                        u64::try_from(total_chars).unwrap_or_default() * played_ms / total_ms,
                    )
                    .unwrap_or(total_chars)
                    .min(total_chars);
                    session.assistant_reveal_chars = session.assistant_reveal_chars.max(target);
                }
            }
            VoiceUiEvent::ToolCallStarted { name } => {
                session.active_tool = Some(name);
            }
            VoiceUiEvent::ToolCall { name, summary } => {
                session.active_tool = None;
                session.ledger.push(VoiceLedgerEntry { name, summary });
                if session.ledger.len() > 100 {
                    session.ledger.remove(0);
                }
            }
            VoiceUiEvent::ApprovalRequested { id, description } => {
                session.approvals.push(VoiceApproval { id, description });
            }
            VoiceUiEvent::ApprovalResolved { id, .. } => {
                session.approvals.retain(|approval| approval.id != id);
            }
            VoiceUiEvent::Usage { .. } => {}
            VoiceUiEvent::MicLevel(level) => session.mic_level = level.clamp(0.0, 1.0),
            VoiceUiEvent::SessionSummary { text } => {
                save_assistant_summary(pane_id, &text);
                self.voice.refresh_thread_index();
            }
        }
    }

    fn apply_transcript(
        &mut self,
        pane_id: Uuid,
        role: VoiceTranscriptRole,
        text: String,
        final_: bool,
    ) {
        if final_
            && role == VoiceTranscriptRole::User
            && !text.starts_with("[image attached:")
            && self
                .pane_metadata(pane_id)
                .is_some_and(|pane| pane.kind.is_assistant() && pane.title == "Assistant")
        {
            let title = hh_voice::threads::thread_title(&text);
            if !title.is_empty() {
                self.dispatch(ClientRequest::RenamePane { pane_id, title });
            }
        }
        let Some(session) = self.voice.sessions.get_mut(&pane_id) else {
            return;
        };
        let current_timestamp = || {
            time::OffsetDateTime::now_local()
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
                .format(&time::macros::format_description!("[hour]:[minute]"))
                .unwrap_or_else(|_| "--:--".to_owned())
        };
        if final_ {
            let timestamp = session
                .transcript
                .last()
                .filter(|entry| entry.role == role && !entry.final_)
                .map_or_else(&current_timestamp, |entry| entry.timestamp.clone());
            if session
                .transcript
                .last()
                .is_some_and(|entry| entry.role == role && !entry.final_)
            {
                session.transcript.pop();
            }
            session.transcript.push(VoiceTranscriptEntry {
                role,
                text,
                final_: true,
                timestamp,
                image: None,
            });
            if role == VoiceTranscriptRole::Assistant {
                session.assistant_reveal_chars = 0;
            }
        } else if let Some(entry) = session
            .transcript
            .last_mut()
            .filter(|entry| entry.role == role && !entry.final_)
        {
            entry.text.push_str(&text);
        } else {
            session.transcript.push(VoiceTranscriptEntry {
                role,
                text,
                final_: false,
                timestamp: current_timestamp(),
                image: None,
            });
            if role == VoiceTranscriptRole::Assistant {
                session.assistant_reveal_chars = 0;
            }
        }
        if session.transcript.len() > 100 {
            session.transcript.remove(0);
            session.selected_transcript = session
                .selected_transcript
                .and_then(|index| index.checked_sub(1));
        }
        session.transcript_scroll.scroll_to_bottom();
    }
}

fn assistant_context_directory() -> Option<PathBuf> {
    hh_protocol::state_directory().map(|directory| directory.join("assistant-context"))
}

fn assistant_context_path(pane_id: Uuid) -> Option<PathBuf> {
    assistant_context_directory().map(|directory| directory.join(format!("{pane_id}.txt")))
}

fn save_assistant_summary(pane_id: Uuid, text: &str) {
    let Some(path) = assistant_context_path(pane_id) else {
        return;
    };
    let result = (|| -> io::Result<()> {
        if let Some(parent) = path.parent() {
            hh_protocol::ensure_private_directory(parent)?;
        }
        hh_protocol::atomic_write_private(&path, text.as_bytes())
    })();
    if let Err(error) = result {
        eprintln!("assistant context for {pane_id} was not persisted: {error}");
    }
}

fn load_assistant_summary(pane_id: Uuid) -> Option<String> {
    let path = assistant_context_path(pane_id)?;
    let bytes = hh_protocol::read_private_file(&path, ASSISTANT_SUMMARY_MAX_BYTES).ok()?;
    String::from_utf8(bytes)
        .ok()
        .filter(|text| !text.is_empty())
}

fn delete_assistant_summary(pane_id: Uuid) {
    if let Some(path) = assistant_context_path(pane_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether the pane should render the idle "Start/Resume" column instead of
/// the live transcript surface.
pub(crate) fn assistant_session_is_idle(session: &AssistantSession) -> bool {
    session.engine.is_none()
        || session
            .engine
            .as_ref()
            .is_some_and(VoiceEngineHandle::is_finished)
        || session.engine_state == EngineState::Suspended
}
pub(crate) fn assistant_workspace_shows_idle(session: &AssistantSession) -> bool {
    assistant_session_is_idle(session)
        && session.transcript.is_empty()
        && session.persisted_summary == PersistedSummaryState::Absent
}
fn activate_assistant_composer(composer: &mut Option<AssistantComposer>, pane_id: Uuid) {
    if composer
        .as_ref()
        .is_none_or(|active| active.pane_id != pane_id)
    {
        *composer = Some(AssistantComposer {
            pane_id,
            text: String::new(),
            selection: None,
            attachment: None,
        });
    }
}

fn assistant_activity_row(pane_id: Uuid, session: &AssistantSession) -> Option<AnyElement> {
    let label = match &session.engine_state {
        EngineState::Thinking => Some("thinking…".to_owned()),
        EngineState::ToolRunning => Some(format!(
            "running {}…",
            session.active_tool.as_deref().unwrap_or("tool")
        )),
        _ => None,
    }?;
    Some(
        div()
            .id(("voice-activity", element_key(pane_id)))
            .w_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .font_family("SF Mono")
            .text_xs()
            .text_color(rgb(THEME.dim))
            .child(
                div()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded_full()
                    .bg(rgb(THEME.accent)),
            )
            .child(label)
            .into_any_element(),
    )
}

impl HhApp {
    pub(crate) fn render_assistant_workspace(
        &self,
        pane: &Pane,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane_id = pane.id;
        // Reconciliation normally owns every assistant session; keep the
        // ephemeral fallback for the initial frame before a snapshot arrives.
        let empty = AssistantSession::new();
        let session = self.voice.sessions.get(&pane_id).unwrap_or(&empty);
        let show_idle = assistant_workspace_shows_idle(session);
        div()
            .id(("assistant-workspace", element_key(pane_id)))
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_assistant_header(pane_id, session, cx))
            .when(show_idle, |element| {
                element.child(self.render_assistant_idle(pane_id, session, cx))
            })
            .when(!show_idle, |element| {
                element.child(self.render_assistant_live(pane_id, session, cx))
            })
            .into_any_element()
    }

    fn render_assistant_header(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (state_label, state_color) = match &session.engine_state {
            EngineState::Connecting => ("Connecting", THEME.accent_soft),
            EngineState::Listening => ("Listening", THEME.ansi[2]),
            EngineState::Thinking => ("Thinking", THEME.accent),
            EngineState::Speaking => ("Speaking", THEME.accent),
            EngineState::ToolRunning => ("Running tool", THEME.dim),
            EngineState::Suspended => ("Suspended", THEME.dim),
            EngineState::Error(_) => ("Error", THEME.danger),
        };
        let model = self.voice.settings_editor.settings.model.clone();
        div()
            .h(px(PANE_HEADER_HEIGHT))
            .flex_none()
            .px(px(10.0))
            .border_b_1()
            .border_color(rgb(THEME.border))
            .bg(rgb(THEME.surface))
            .flex()
            .items_center()
            .gap(px(7.0))
            .child(
                div()
                    .w(px(7.0))
                    .h(px(7.0))
                    .rounded_full()
                    .bg(rgb(state_color)),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child(state_label),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .font_family("SF Mono")
                    .text_size(px(9.5))
                    .text_color(rgb(THEME.dim))
                    .child(model),
            )
            .child(
                div()
                    .id(("assistant-settings-button", element_key(pane_id)))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child("Settings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_appearance_settings(cx);
                        cx.stop_propagation();
                    })),
            )
            .into_any_element()
    }

    fn render_previous_threads(&self, pane_id: Uuid, cx: &mut Context<Self>) -> Option<AnyElement> {
        let workspace_id = self.workspace_id_for_pane(pane_id)?;
        let summaries = self
            .voice
            .thread_index
            .iter()
            .filter(|summary| summary.workspace_id == Some(workspace_id))
            .filter(|summary| self.workspace_id_for_pane(summary.thread_id).is_none())
            .take(10)
            .cloned()
            .collect::<Vec<_>>();
        if summaries.is_empty() {
            return None;
        }
        let rows = summaries.into_iter().map(|summary| {
            let thread_id = summary.thread_id;
            let title = summary
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "Untitled thread".to_owned());
            let turn_label = format!(
                "{} turn{}",
                summary.turns,
                if summary.turns == 1 { "" } else { "s" }
            );
            let activity = format_thread_activity(summary.last_at_ms);
            div()
                .id(("assistant-previous-thread", element_key(thread_id)))
                .w_full()
                .min_w(px(0.0))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .bg(rgb(THEME.surface))
                .hover(|element| element.bg(rgb(THEME.elevated)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .child(title),
                )
                .child(
                    div()
                        .font_family("SF Mono")
                        .text_size(px(9.0))
                        .text_color(rgb(THEME.dim))
                        .child(format!("{turn_label} · {activity}")),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.reopen_thread(workspace_id, thread_id, cx);
                    cx.stop_propagation();
                }))
                .into_any_element()
        });
        Some(
            div()
                .w_full()
                .max_w(px(440.0))
                .pt(px(10.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.muted))
                        .child("Previous threads"),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_assistant_idle(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = "Start voice assistant";
        let button_id = "assistant-start";
        let engine_present = session
            .engine
            .as_ref()
            .is_some_and(|engine| !engine.is_finished());
        let error_line = match &session.engine_state {
            EngineState::Error(message) => Some(message.clone()),
            _ => None,
        };
        let previous_threads = self.render_previous_threads(pane_id, cx);
        div()
            .id(("assistant-idle", element_key(pane_id)))
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .px(px(24.0))
            .py(px(24.0))
            .overflow_y_scroll()
            .bg(rgb(THEME.terminal))
            .when_some(error_line, |element, message| {
                element.child(
                    div()
                        .font_family("SF Mono")
                        .text_xs()
                        .text_color(rgb(THEME.danger))
                        .child(message),
                )
            })
            .child(
                div()
                    .id((button_id, element_key(pane_id)))
                    .px(px(16.0))
                    .py(px(9.0))
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.accent))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.window))
                    .hover(|element| element.opacity(0.9))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if engine_present {
                            this.send_assistant_command(pane_id, VoiceCommand::Resume);
                        } else {
                            this.start_assistant(pane_id, cx);
                        }
                        cx.stop_propagation();
                    })),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Not connected — no microphone or API usage while idle."),
            )
            .when_some(previous_threads, |element, history| element.child(history))
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn render_assistant_live(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active_bars = ((session.mic_level * 5.0).ceil() as usize).min(5);
        let mic_bars = format!("{}{}", "▮".repeat(active_bars), "▯".repeat(5 - active_bars));
        let transcript_len = session.transcript.len();
        let mut transcript = session
            .transcript
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let user = entry.role == VoiceTranscriptRole::User;
                let selected = session.selected_transcript == Some(index);
                let bubble_background = if user { THEME.elevated } else { THEME.surface };
                let live_assistant = index + 1 == transcript_len
                    && entry.role == VoiceTranscriptRole::Assistant
                    && !entry.final_;
                let text = if live_assistant && !session.speaker_muted {
                    let mut prefix: String = entry
                        .text
                        .chars()
                        .take(session.assistant_reveal_chars)
                        .collect();
                    prefix.push('▮');
                    prefix
                } else {
                    entry.text.clone()
                };
                let message_text = entry.text.clone();
                div()
                    .id(("voice-transcript", index))
                    .w_full()
                    .flex()
                    .when(user, |element| element.justify_end())
                    .child(
                        div()
                            .id(("voice-transcript-bubble", index))
                            .min_w(px(0.0))
                            .max_w(relative(0.85))
                            .px(px(10.0))
                            .py(px(7.0))
                            .rounded(px(7.0))
                            .cursor_text()
                            .bg(rgb(bubble_background))
                            .border_1()
                            .border_color(rgb(if selected {
                                THEME.accent
                            } else {
                                bubble_background
                            }))
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(if user { THEME.muted } else { THEME.foreground }))
                            .flex()
                            .flex_col()
                            .when_some(entry.image.clone(), |element, path| {
                                element.child(
                                    img(path)
                                        .max_w(px(220.0))
                                        .max_h(px(160.0))
                                        .object_fit(gpui::ObjectFit::Contain)
                                        .rounded(px(5.0)),
                                )
                            })
                            .children(text.split('\n').map(|line| {
                                let line = if line.is_empty() { " " } else { line };
                                div().child(line.to_owned())
                            }))
                            .child(
                                div()
                                    .w_full()
                                    .pt(px(3.0))
                                    .flex()
                                    .when(user, |element| element.justify_end())
                                    .font_family("SF Mono")
                                    .text_size(px(9.0))
                                    .text_color(rgb(THEME.dim))
                                    .child(entry.timestamp.clone())
                                    .child(
                                        div()
                                            .id(("voice-transcript-copy", index))
                                            .ml(px(6.0))
                                            .cursor_pointer()
                                            .text_color(rgb(THEME.dim))
                                            .hover(|element| {
                                                element.text_color(rgb(THEME.foreground))
                                            })
                                            .tooltip(|_, cx| {
                                                cx.new(|_| TooltipView {
                                                    text: "Copy message".to_owned(),
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    message_text.clone(),
                                                ));
                                                cx.stop_propagation();
                                            }))
                                            .child("⧉"),
                                    ),
                            )
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Click to select; Command-C copies this message"
                                        .to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(session) = this.voice.sessions.get_mut(&pane_id) {
                                    session.selected_transcript = Some(index);
                                }
                                this.focus_pane_with_snapshot(pane_id, cx);
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let has_live_user_transcript = session
            .transcript
            .last()
            .is_some_and(|entry| entry.role == VoiceTranscriptRole::User && !entry.final_);
        if session.user_speaking && !has_live_user_transcript {
            let dot_phase = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 400
                % 3;
            let dots = ["·", "· ·", "· · ·"][dot_phase as usize];
            transcript.push(
                div()
                    .id(("voice-listening", element_key(pane_id)))
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .min_w(px(0.0))
                            .max_w(relative(0.85))
                            .px(px(10.0))
                            .py(px(7.0))
                            .rounded(px(7.0))
                            .bg(rgb(THEME.elevated))
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.dim))
                            .child(format!("{mic_bars} {dots}")),
                    )
                    .into_any_element(),
            );
        }
        let ledger = session
            .ledger
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                div()
                    .id(("voice-ledger", index))
                    .w_full()
                    .min_w(px(0.0))
                    .truncate()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(format!("{} — {}", entry.name, entry.summary))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let activity = assistant_activity_row(pane_id, session);
        let approvals = session
            .approvals
            .iter()
            .map(|approval| {
                let approval_id = approval.id;
                div()
                    .id(("voice-approval", approval_id))
                    .mx(px(10.0))
                    .mb(px(8.0))
                    .p(px(10.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(rgb(THEME.danger))
                    .bg(rgb(THEME.elevated))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child(approval.description.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id(("voice-approve", approval_id))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.accent))
                                    .text_color(rgb(THEME.window))
                                    .child("Approve")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_assistant_command(
                                            pane_id,
                                            VoiceCommand::Approve { approval_id },
                                        );
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("voice-deny", approval_id))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(rgb(THEME.border_strong))
                                    .text_color(rgb(THEME.muted))
                                    .child("Deny")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_assistant_command(
                                            pane_id,
                                            VoiceCommand::Deny { approval_id },
                                        );
                                        cx.stop_propagation();
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .id(("assistant-live", element_key(pane_id)))
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .bg(rgb(THEME.terminal))
            .child(
                div()
                    .id("voice-transcript-scroll")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&session.transcript_scroll)
                    .px(px(10.0))
                    .py(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(transcript)
                    .children(ledger)
                    .when_some(activity, |element, activity| element.child(activity)),
            )
            .children(approvals)
            .child(self.render_assistant_composer_row(pane_id, session, cx))
            .into_any_element()
    }
    #[allow(clippy::too_many_lines)]
    fn render_assistant_composer_row(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let engine_present = session
            .engine
            .as_ref()
            .is_some_and(|engine| !engine.is_finished());
        let voice_active =
            engine_present && !matches!(&session.engine_state, EngineState::Suspended);
        let attachment_info = self
            .editor
            .assistant_composer
            .as_ref()
            .filter(|composer| composer.pane_id == pane_id)
            .and_then(|composer| composer.attachment.as_ref())
            .map(|attachment| (attachment.filename.clone(), attachment.path.clone()));
        div()
            .flex_none()
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .when_some(attachment_info, |element, (filename, path)| {
                element.child(
                    div()
                        .px(px(10.0))
                        .pt(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .px(px(8.0))
                                .py(px(3.0))
                                .rounded(px(5.0))
                                .bg(rgb(THEME.elevated))
                                .border_1()
                                .border_color(rgb(THEME.border_strong))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .max_w(relative(0.7))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    img(path)
                                        .h(px(48.0))
                                        .max_w(px(160.0))
                                        .object_fit(gpui::ObjectFit::Contain)
                                        .rounded(px(4.0)),
                                )
                                .child(div().min_w(px(0.0)).truncate().child(filename)),
                        )
                        .child(
                            div()
                                .id(("composer-attachment-remove", element_key(pane_id)))
                                .cursor_pointer()
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .hover(|element| element.text_color(rgb(THEME.danger)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(composer) = this.editor.assistant_composer.as_mut()
                                    {
                                        composer.attachment = None;
                                    }
                                    cx.notify();
                                    cx.stop_propagation();
                                }))
                                .child("×"),
                        ),
                )
            })
            .child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .flex()
                    .items_end()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id(("assistant-composer-field", element_key(pane_id)))
                            .min_h(px(26.0))
                            .min_w(px(0.0))
                            .flex_1()
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(rgb(THEME.border_strong))
                            .bg(rgb(THEME.surface))
                            .cursor_pointer()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .font_family("SF Mono")
                            .text_sm()
                            .when(
                                self.editor
                                    .assistant_composer
                                    .as_ref()
                                    .is_some_and(|composer| composer.pane_id == pane_id),
                                |element| {
                                    let composer = self
                                        .editor
                                        .assistant_composer
                                        .as_ref()
                                        .filter(|composer| composer.pane_id == pane_id);
                                    let text =
                                        composer.map_or("", |composer| composer.text.as_str());
                                    let selected_all =
                                        composer.is_some_and(AssistantComposer::all_selected);
                                    let lines = text.split('\n').collect::<Vec<_>>();
                                    let start = lines.len().saturating_sub(5);
                                    let visible = &lines[start..];
                                    let mut rendered =
                                        Vec::with_capacity(visible.len() + usize::from(start > 0));
                                    if start > 0 {
                                        rendered.push(
                                            div()
                                                .text_color(rgb(THEME.dim))
                                                .child("…")
                                                .into_any_element(),
                                        );
                                    }
                                    for (index, line) in visible.iter().enumerate() {
                                        let line = if line.is_empty() { " " } else { *line };
                                        let line = line.to_owned();
                                        if index + 1 == visible.len() && !selected_all {
                                            rendered.push(
                                                div()
                                                    .flex()
                                                    .child(line)
                                                    .child(
                                                        div()
                                                            .text_color(rgb(THEME.accent))
                                                            .child("▮"),
                                                    )
                                                    .into_any_element(),
                                            );
                                        } else {
                                            rendered.push(
                                                div()
                                                    .when(selected_all, |element| {
                                                        element.bg(rgb(THEME.accent_soft))
                                                    })
                                                    .child(line)
                                                    .into_any_element(),
                                            );
                                        }
                                    }
                                    element.text_color(rgb(THEME.foreground)).children(rendered)
                                },
                            )
                            .when(
                                self.editor
                                    .assistant_composer
                                    .as_ref()
                                    .is_none_or(|composer| composer.pane_id != pane_id),
                                |element| {
                                    element
                                        .text_color(rgb(THEME.dim))
                                        .child("Type to the assistant…")
                                },
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.focus_pane_with_snapshot(pane_id, cx);
                                activate_assistant_composer(
                                    &mut this.editor.assistant_composer,
                                    pane_id,
                                );
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("assistant-composer-attach", element_key(pane_id)))
                            .h(px(26.0))
                            .w(px(26.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(rgb(THEME.border_strong))
                            .bg(rgb(THEME.surface))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Attach image".to_owned(),
                                })
                                .into()
                            })
                            .child("+")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.attach_assistant_image(pane_id, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("assistant-composer-send", element_key(pane_id)))
                            .size(px(26.0))
                            .rounded(px(5.0))
                            .bg(rgb(THEME.accent))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.window))
                            .child("↩")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit_assistant_composer(cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("assistant-voice-toggle", element_key(pane_id)))
                            .size(px(26.0))
                            .rounded_full()
                            .border_2()
                            .border_color(rgb(if voice_active {
                                THEME.accent
                            } else {
                                THEME.border_strong
                            }))
                            .bg(rgb(THEME.surface))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .tooltip(move |_, cx| {
                                cx.new(|_| TooltipView {
                                    text: if voice_active {
                                        "Voice on — click to pause".to_owned()
                                    } else {
                                        "Start voice assistant".to_owned()
                                    },
                                })
                                .into()
                            })
                            .child(
                                div()
                                    .text_color(rgb(if voice_active {
                                        THEME.accent
                                    } else {
                                        THEME.dim
                                    }))
                                    .child("●"),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if voice_active {
                                    this.send_assistant_command(pane_id, VoiceCommand::Suspend);
                                } else if engine_present {
                                    this.send_assistant_command(pane_id, VoiceCommand::Resume);
                                } else {
                                    this.start_assistant(pane_id, cx);
                                }
                                cx.stop_propagation();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl HhApp {
    pub(crate) fn paste_voice_setting(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if !matches!(self.editor.modal, Modal::AppearanceSettings) {
            return false;
        }
        let Some(field) = self.voice.settings_editor.active_field else {
            return false;
        };
        let value = text.trim().to_owned();
        match field {
            VoiceSettingsField::ApiKey => self.voice.settings_editor.api_key_input = value,
            VoiceSettingsField::IdleTimeout => {
                self.voice.settings_editor.idle_timeout_input = value
            }
            VoiceSettingsField::HonchoBaseUrl => {
                self.voice.settings_editor.honcho_base_url_input = value
            }
            VoiceSettingsField::HonchoWorkspace => {
                self.voice.settings_editor.honcho_workspace_input = value
            }
        }
        self.persist_voice_settings();
        cx.notify();
        true
    }

    /// Key routing for a focused voice settings field. Escape and modal
    /// dismissal are owned by the `AppearanceSettings` modal arm; this handler
    /// only sees keys while a field is active.
    pub(crate) fn handle_voice_settings_key(
        &mut self,
        keystroke: &Keystroke,
        cx: &mut Context<Self>,
    ) {
        if self.voice.settings_editor.active_field.is_none() {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.voice.settings_editor.active_field = None;
            }
            "tab" => {
                self.voice.settings_editor.active_field =
                    match self.voice.settings_editor.active_field {
                        Some(VoiceSettingsField::ApiKey) | None => {
                            Some(VoiceSettingsField::IdleTimeout)
                        }
                        Some(VoiceSettingsField::IdleTimeout) => {
                            Some(VoiceSettingsField::HonchoBaseUrl)
                        }
                        Some(
                            VoiceSettingsField::HonchoBaseUrl | VoiceSettingsField::HonchoWorkspace,
                        ) => Some(VoiceSettingsField::ApiKey),
                    };
            }
            "backspace" => {
                self.active_voice_setting_text_mut().pop();
                self.persist_voice_settings();
            }
            _ if !keystroke.modifiers.platform
                && !keystroke.modifiers.control
                && !keystroke.modifiers.alt =>
            {
                if let Some(text) = keystroke.key_char.as_deref() {
                    self.active_voice_setting_text_mut().push_str(text);
                    self.persist_voice_settings();
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn active_voice_setting_text_mut(&mut self) -> &mut String {
        match self.voice.settings_editor.active_field {
            Some(VoiceSettingsField::ApiKey) | None => {
                &mut self.voice.settings_editor.api_key_input
            }
            Some(VoiceSettingsField::IdleTimeout) => {
                &mut self.voice.settings_editor.idle_timeout_input
            }
            Some(VoiceSettingsField::HonchoBaseUrl) => {
                &mut self.voice.settings_editor.honcho_base_url_input
            }
            Some(VoiceSettingsField::HonchoWorkspace) => {
                &mut self.voice.settings_editor.honcho_workspace_input
            }
        }
    }

    fn persist_voice_settings(&mut self) {
        let editor = &mut self.voice.settings_editor;
        editor.settings.api_key.clone_from(&editor.api_key_input);
        if let Ok(timeout) = editor.idle_timeout_input.parse::<u32>() {
            editor.settings.idle_timeout_secs = timeout;
        }
        if let Some(honcho) = editor.settings.honcho.as_mut() {
            honcho.base_url.clone_from(&editor.honcho_base_url_input);
            honcho.workspace.clone_from(&editor.honcho_workspace_input);
        }
        if let Err(error) = editor.settings.save() {
            eprintln!("voice settings were not saved: {error}");
        }
    }

    fn toggle_voice_model(&mut self, cx: &mut Context<Self>) {
        self.voice.settings_editor.settings.model =
            if self.voice.settings_editor.settings.model == "gpt-realtime-2.1" {
                "gpt-realtime-2.1-mini".to_owned()
            } else {
                "gpt-realtime-2.1".to_owned()
            };
        self.persist_voice_settings();
        cx.notify();
    }

    fn cycle_voice(&mut self, cx: &mut Context<Self>) {
        match self.voice.settings_editor.settings.voice.as_str() {
            "marin" => "cedar",
            "cedar" => "alloy",
            _ => "marin",
        }
        .clone_into(&mut self.voice.settings_editor.settings.voice);
        self.persist_voice_settings();
        cx.notify();
    }

    fn toggle_full_duplex(&mut self, cx: &mut Context<Self>) {
        self.voice.settings_editor.settings.full_duplex =
            !self.voice.settings_editor.settings.full_duplex;
        self.persist_voice_settings();
        cx.notify();
    }

    fn toggle_honcho(&mut self, cx: &mut Context<Self>) {
        self.voice.settings_editor.settings.honcho =
            if self.voice.settings_editor.settings.honcho.is_some() {
                None
            } else {
                Some(HonchoSettings {
                    base_url: self.voice.settings_editor.honcho_base_url_input.clone(),
                    workspace: self.voice.settings_editor.honcho_workspace_input.clone(),
                    bearer: None,
                })
            };
        self.persist_voice_settings();
        cx.notify();
    }

    /// Voice section transplanted into the merged Settings page.
    pub(crate) fn render_voice_settings_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = &self.voice.settings_editor;
        let api_key_display = if editor.api_key_input.is_empty() {
            "Paste API key".to_owned()
        } else {
            let mut tail = editor
                .api_key_input
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>();
            tail.reverse();
            format!("•••• {}", tail.into_iter().collect::<String>())
        };
        let honcho_enabled = editor.settings.honcho.is_some();
        div()
            .id("voice-settings-section")
            .pt(px(4.0))
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(THEME.foreground))
                    .child("Voice"),
            )
            .child(self.render_voice_setting_field(
                "voice-api-key",
                "OpenAI API key",
                api_key_display,
                VoiceSettingsField::ApiKey,
                cx,
            ))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Select the API key field, then paste. The key is stored in the owner-only state file."),
            )
            .child(
                div()
                    .id("voice-model-picker")
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .cursor_pointer()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("Model"),
                    )
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .child(editor.settings.model.clone()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_voice_model(cx))),
            )
            .child(
                div()
                    .id("voice-picker")
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .cursor_pointer()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("Voice"),
                    )
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .child(editor.settings.voice.clone()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_voice(cx))),
            )
            .child(
                div()
                    .id("voice-full-duplex")
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.accent))
                            .child(if editor.settings.full_duplex {
                                "[x]"
                            } else {
                                "[ ]"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_family(".SystemUIFont")
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .child("Full duplex"),
                            )
                            .child(
                                div()
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.dim))
                                    .child("requires headphones"),
                            ),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_full_duplex(cx))),
            )
            .child(self.render_voice_setting_field(
                "voice-idle-timeout",
                "Idle timeout seconds (0 disables; minimum 60)",
                editor.idle_timeout_input.clone(),
                VoiceSettingsField::IdleTimeout,
                cx,
            ))
            .child(
                div()
                    .id("voice-honcho-toggle")
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child(if honcho_enabled { "[x]" } else { "[ ]" })
                    .child("Honcho memory")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_honcho(cx))),
            )
            .when(honcho_enabled, |element| {
                element
                    .child(self.render_voice_setting_field(
                        "voice-honcho-url",
                        "Honcho base URL",
                        editor.honcho_base_url_input.clone(),
                        VoiceSettingsField::HonchoBaseUrl,
                        cx,
                    ))
                    .child(self.render_voice_setting_field(
                        "voice-honcho-workspace",
                        "Honcho workspace",
                        editor.honcho_workspace_input.clone(),
                        VoiceSettingsField::HonchoWorkspace,
                        cx,
                    ))
            })
            .child(
                div()
                    .pt(px(6.0))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Changes apply to the next assistant session."),
            )
            .into_any_element()
    }

    fn render_voice_setting_field(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        field: VoiceSettingsField,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.voice.settings_editor.active_field == Some(field);
        div()
            .id(id)
            .p(px(10.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(rgb(if active { THEME.accent } else { THEME.border }))
            .cursor_pointer()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child(label),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child(value),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.voice.settings_editor.active_field = Some(field);
                cx.notify();
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_assistant_keeps_the_live_surface_when_history_exists() {
        let mut session = AssistantSession::new();
        assert!(assistant_workspace_shows_idle(&session));

        session.transcript.push(VoiceTranscriptEntry {
            role: VoiceTranscriptRole::User,
            text: "keep this visible".to_owned(),
            final_: true,
            timestamp: "12:34".to_owned(),
            image: None,
        });
        assert!(!assistant_workspace_shows_idle(&session));
        session.transcript.clear();

        session.persisted_summary = PersistedSummaryState::Present;
        assert!(!assistant_workspace_shows_idle(&session));
    }
    #[test]
    fn transcript_timestamp_is_compact_local_time() {
        let timestamp = time::OffsetDateTime::now_local()
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
            .format(&time::macros::format_description!("[hour]:[minute]"))
            .unwrap();
        assert_eq!(timestamp.len(), 5);
        assert_eq!(&timestamp[2..3], ":");
    }
    #[test]
    fn audio_controls_toggle_mutes_without_suspending() {
        let mut session = AssistantSession::new();
        session.engine_state = EngineState::Listening;

        assert!(!toggle_microphone_muted(&mut session));
        assert!(session.mic_muted);
        assert_eq!(session.engine_state, EngineState::Listening);

        assert!(toggle_headphones_muted(&mut session));
        assert!(session.speaker_muted);
        assert_eq!(session.engine_state, EngineState::Listening);
    }

    #[test]
    fn activating_composer_preserves_same_pane_draft() {
        let pane_id = Uuid::new_v4();
        let mut composer = Some(AssistantComposer {
            pane_id,
            text: "unfinished".to_owned(),
            selection: None,
            attachment: Some(ComposerAttachment {
                filename: "screenshot.png".to_owned(),
                data_url: "data:image/png;base64,AA==".to_owned(),
                path: PathBuf::from("/tmp/screenshot.png"),
            }),
        });
        activate_assistant_composer(&mut composer, pane_id);
        assert_eq!(composer.as_ref().unwrap().text, "unfinished");
        assert_eq!(
            composer
                .as_ref()
                .unwrap()
                .attachment
                .as_ref()
                .unwrap()
                .filename,
            "screenshot.png"
        );

        activate_assistant_composer(&mut composer, Uuid::new_v4());
        assert_eq!(composer.as_ref().unwrap().text, "");
        assert!(composer.as_ref().unwrap().attachment.is_none());
    }

    #[test]
    fn composer_selection_copies_cuts_and_replaces() {
        let mut composer = AssistantComposer {
            pane_id: Uuid::new_v4(),
            text: "hello".to_owned(),
            selection: None,
            attachment: None,
        };
        composer.select_all();
        assert_eq!(composer.selected_text(), Some("hello"));
        composer.insert("world");
        assert_eq!(composer.text, "world");
        composer.select_all();
        assert_eq!(composer.cut_selection().as_deref(), Some("world"));
        assert!(composer.text.is_empty());
    }
}
