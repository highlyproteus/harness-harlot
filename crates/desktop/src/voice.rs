use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
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
const MAX_ASSISTANT_IMAGE_DIMENSION: u32 = 8_192;
const MAX_ASSISTANT_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;

fn read_assistant_image(path: &Path) -> anyhow::Result<(String, String, PathBuf)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| anyhow::anyhow!("open assistant image {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| anyhow::anyhow!("inspect assistant image {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.uid() != rustix::process::geteuid().as_raw() {
        anyhow::bail!("assistant image must be a regular file owned by the current user");
    }
    if metadata.len() > MAX_ASSISTANT_IMAGE_BYTES as u64 {
        anyhow::bail!("image exceeds the 4 MiB attachment limit");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_ASSISTANT_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read assistant image {}", path.display()))?;
    if bytes.len() > MAX_ASSISTANT_IMAGE_BYTES {
        anyhow::bail!("image grew past the 4 MiB attachment limit while reading");
    }
    let format = image::guess_format(&bytes).context("detect assistant image magic bytes")?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::WebP => "image/webp",
        _ => anyhow::bail!("unsupported file type; images (PNG, JPG, WebP) only for now"),
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_ASSISTANT_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_ASSISTANT_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_ASSISTANT_IMAGE_PIXELS * 8);
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(&bytes), format);
    reader.limits(limits);
    let decoded = reader.decode().context("decode bounded assistant image")?;
    if u64::from(decoded.width()) * u64::from(decoded.height()) > MAX_ASSISTANT_IMAGE_PIXELS {
        anyhow::bail!("assistant image exceeds the decoded pixel limit");
    }
    let filename = path.file_name().map_or_else(
        || "image".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    Ok((
        filename,
        format!("data:{mime};base64,{}", BASE64.encode(bytes)),
        path.to_path_buf(),
    ))
}

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
    pub(crate) fn load() -> anyhow::Result<Self> {
        let settings = VoiceSettings::load()?;
        Ok(Self::from_settings(settings))
    }

    fn from_settings(settings: VoiceSettings) -> Self {
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
            mic_muted: true,
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
        let thread = match threads::read_thread(pane_id) {
            Ok(thread) => thread,
            Err(error) => {
                session.engine_state = EngineState::Error(format!("{error:#}"));
                None
            }
        };
        if let Some(thread) = thread {
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

fn prepare_non_voice_start(session: &mut AssistantSession) {
    session.mic_muted = true;
    if let Some(engine) = session.engine.as_ref() {
        engine.send(VoiceCommand::SetMicEnabled(false));
    }
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
    persistence_error: Option<String>,
}

impl VoiceUi {
    pub(crate) fn new() -> Self {
        let (settings_editor, persistence_error) = match VoiceSettingsEditor::load() {
            Ok(editor) => (editor, None),
            Err(error) => (
                VoiceSettingsEditor::from_settings(VoiceSettings::default()),
                Some(format!("{error:#}")),
            ),
        };
        let mut voice = Self {
            sessions: HashMap::new(),
            settings_editor,
            quit_subscription: None,
            thread_index: Vec::new(),
            persistence_error,
        };
        voice.refresh_thread_index();
        voice
    }

    pub(crate) fn refresh_thread_index(&mut self) {
        match threads::list_threads() {
            Ok(threads) => self.thread_index = threads,
            Err(error) => self.persistence_error = Some(format!("{error:#}")),
        }
    }
}

impl HhApp {
    pub(crate) fn start_assistant(&mut self, pane_id: Uuid, cx: &mut Context<Self>) -> bool {
        if self
            .voice
            .sessions
            .get(&pane_id)
            .and_then(|session| session.engine.as_ref())
            .is_some_and(|engine| !engine.is_finished())
        {
            return true;
        }
        // A finished thread (startup failure or shutdown) never recovers;
        // replace it with a fresh engine.
        let settings = match VoiceSettings::load() {
            Ok(settings) => settings,
            Err(error) => {
                let session = self
                    .voice
                    .sessions
                    .entry(pane_id)
                    .or_insert_with(|| AssistantSession::load(pane_id));
                session.engine_state = EngineState::Error(format!("{error:#}"));
                self.report(&error);
                cx.notify();
                return false;
            }
        };
        if settings.api_key.trim().is_empty() && std::env::var("HH_OPENAI_API_KEY").is_err() {
            self.voice.settings_editor = VoiceSettingsEditor::from_settings(settings);
            self.open_appearance_settings(cx);
            return false;
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
                return false;
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
        true
    }

    /// Starts capture only from a visible start-voice control. Text, image,
    /// and history paths call `start_assistant` and remain microphone-muted.
    fn start_voice_assistant(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        let session = self
            .voice
            .sessions
            .entry(pane_id)
            .or_insert_with(|| AssistantSession::load(pane_id));
        session.mic_muted = false;
        if session
            .engine
            .as_ref()
            .is_some_and(|engine| !engine.is_finished())
        {
            if let Some(engine) = session.engine.as_ref() {
                engine.send(VoiceCommand::SetMicEnabled(true));
                engine.send(VoiceCommand::Resume);
            }
        } else {
            self.start_assistant(pane_id, cx);
        }
    }

    fn reopen_thread(&mut self, workspace_id: Uuid, thread_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::CreateAssistantTab { workspace_id },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    match threads::adopt_thread(thread_id, pane_id) {
                        Ok(true) => {
                            this.voice
                                .sessions
                                .insert(pane_id, AssistantSession::load(pane_id));
                        }
                        Ok(false) => {}
                        Err(error) => this.report(&error),
                    }
                    prepare_non_voice_start(
                        this.voice
                            .sessions
                            .entry(pane_id)
                            .or_insert_with(|| AssistantSession::load(pane_id)),
                    );
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

    fn delete_saved_thread(&mut self, thread_id: Uuid, cx: &mut Context<Self>) {
        match threads::delete_thread(thread_id) {
            Ok(_) => delete_assistant_summary(thread_id),
            Err(error) => self.report(&error),
        }
        self.voice.refresh_thread_index();
        cx.notify();
    }

    fn clear_saved_threads(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = threads::clear_all_threads() {
            self.report(&error);
        }
        self.voice.refresh_thread_index();
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
                        prior_context: load_assistant_summary(pane_id),
                    };
                }
            }
        }
        AssistantContext::default()
    }

    pub(crate) fn send_assistant_command(&self, pane_id: Uuid, command: VoiceCommand) -> bool {
        self.voice
            .sessions
            .get(&pane_id)
            .and_then(|session| session.engine.as_ref())
            .is_some_and(|engine| engine.try_send(command))
    }
    pub(crate) fn submit_assistant_composer(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.editor.assistant_composer.clone() else {
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
        if let Some(session) = self.voice.sessions.get_mut(&pane_id) {
            prepare_non_voice_start(session);
        }
        let engine_running = self
            .voice
            .sessions
            .get(&pane_id)
            .and_then(|session| session.engine.as_ref())
            .is_some_and(|engine| !engine.is_finished());
        if !engine_running && !self.start_assistant(pane_id, cx) {
            return;
        }
        if let Some(ComposerAttachment {
            filename,
            data_url,
            path,
        }) = attachment
        {
            if !self.send_assistant_command(pane_id, VoiceCommand::SendUserImage { data_url }) {
                return;
            }
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
            if let Some(active) = self.editor.assistant_composer.as_mut() {
                active.attachment = None;
            }
        }
        if !text.is_empty() {
            if !self.send_assistant_command(pane_id, VoiceCommand::SendUserText(text.clone())) {
                return;
            }
            self.apply_transcript(pane_id, VoiceTranscriptRole::User, text.clone(), true);
            if let Some(active) = self.editor.assistant_composer.as_mut() {
                active.text.clear();
                active.selection = None;
            }
        }
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
                .background_spawn(async move { read_assistant_image(&path) })
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

#[path = "voice_view.rs"]
mod voice_view;

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
#[path = "voice_tests.rs"]
mod tests;
