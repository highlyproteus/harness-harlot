use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, Keystroke, ParentElement,
    PathPromptOptions, ScrollHandle, StatefulInteractiveElement, Styled, StyledImage, div, img, px,
    relative, rgb,
};
use hh_protocol::{
    AssistantAccess, AssistantEntry, AssistantImage, AssistantModel, AssistantStatus,
    AssistantThreadView, ClientRequest, CodingAgent, Pane, ServiceResponse, TerminalProfile,
};
use hh_voice::{
    AssistantContext, EngineState, VoiceCommand, VoiceEngineHandle, VoiceSettings, VoiceUiEvent,
    spawn_engine, voice_ui_channel,
};
use uuid::Uuid;

use crate::helpers::element_key;
use crate::view_models::{AssistantComposer, ComposerAttachment, Modal};
use crate::{HhApp, PANE_HEADER_HEIGHT, THEME};
use gpui::AppContext as _;

const MAX_ASSISTANT_IMAGE_BYTES: usize = hh_protocol::MAX_ASSISTANT_IMAGE_BYTES;
const MAX_ASSISTANT_IMAGE_DIMENSION: u32 = 8_192;
const MAX_ASSISTANT_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;
pub(crate) const VOICE_PRIVACY_URL: &str =
    "https://gitlab.com/highlyproteus/harness-harlot/-/blob/main/PRIVACY.md";

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

fn assistant_image_from_data_url(data_url: &str) -> anyhow::Result<AssistantImage> {
    let encoded = data_url
        .strip_prefix("data:")
        .and_then(|value| value.split_once(";base64,"))
        .ok_or_else(|| anyhow::anyhow!("invalid assistant image data URL"))?;
    Ok(AssistantImage {
        mime_type: encoded.0.to_owned(),
        base64: encoded.1.to_owned(),
    })
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VoiceSettingsField {
    ApiKey,
    IdleTimeout,
}

#[derive(Clone, Debug)]
pub(crate) struct VoiceSettingsEditor {
    pub settings: VoiceSettings,
    pub api_key_input: String,
    pub idle_timeout_input: String,
    pub active_field: Option<VoiceSettingsField>,
}

impl VoiceSettingsEditor {
    pub(crate) fn load() -> anyhow::Result<Self> {
        let settings = VoiceSettings::load()?;
        Ok(Self::from_settings(settings))
    }

    fn from_settings(settings: VoiceSettings) -> Self {
        Self {
            api_key_input: settings.api_key.clone(),
            idle_timeout_input: settings.idle_timeout_secs.to_string(),
            settings,
            active_field: None,
        }
    }
}

pub(crate) struct VoiceLink {
    pub engine: VoiceEngineHandle,
    pub engine_state: EngineState,
    pub mic_muted: bool,
    pub speaker_muted: bool,
    pub mic_level: f32,
    pub user_speaking: bool,
    pub interim_transcript: String,
    pub spoken_transcripts: HashMap<u64, String>,
}

pub(crate) struct AssistantPaneState {
    pub view: Option<AssistantThreadView>,
    pub voice: Option<VoiceLink>,
    pub transcript_scroll: ScrollHandle,
    pub selected_entry: Option<usize>,
    pub last_spoken_entry: Option<u64>,
    pub local_notice: Option<String>,
    pub prompt_in_flight: bool,
}

impl Default for AssistantPaneState {
    fn default() -> Self {
        Self {
            view: None,
            voice: None,
            transcript_scroll: ScrollHandle::new(),
            selected_entry: None,
            last_spoken_entry: None,
            local_notice: None,
            prompt_in_flight: false,
        }
    }
}

#[derive(Default)]
pub(crate) struct ModelsPicker {
    pub pane_id: Option<Uuid>,
    pub models: Vec<AssistantModel>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

/// Installed coding agents reported by the session service.
#[derive(Default)]
pub(crate) struct CodingAgentsState {
    pub loading: bool,
    pub loaded: bool,
    pub agents: Vec<CodingAgent>,
    pub error: Option<String>,
}

pub(crate) struct AssistantUi {
    pub panes: HashMap<Uuid, AssistantPaneState>,
    pub settings_editor: VoiceSettingsEditor,
    pub models_picker: ModelsPicker,
    pub coding_agents: CodingAgentsState,
    pub quit_subscription: Option<gpui::Subscription>,
}

impl AssistantUi {
    pub(crate) fn new() -> Self {
        let settings_editor = VoiceSettingsEditor::load()
            .unwrap_or_else(|_| VoiceSettingsEditor::from_settings(VoiceSettings::default()));
        Self {
            panes: HashMap::new(),
            settings_editor,
            models_picker: ModelsPicker::default(),
            coding_agents: CodingAgentsState::default(),
            quit_subscription: None,
        }
    }
}

impl HhApp {
    pub(crate) fn start_voice_assistant(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        let running = self
            .assistant
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.voice.as_ref())
            .is_some_and(|voice| !voice.engine.is_finished());
        if running {
            if let Some(voice) = self
                .assistant
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| pane.voice.as_mut())
            {
                voice.mic_muted = false;
                voice.engine.send(VoiceCommand::SetMicEnabled(true));
                voice.engine.send(VoiceCommand::Resume);
            }
            cx.notify();
            return;
        }
        let settings = match VoiceSettings::load() {
            Ok(settings) => settings,
            Err(error) => {
                self.assistant
                    .panes
                    .entry(pane_id)
                    .or_default()
                    .local_notice = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        if settings.api_key.trim().is_empty() && std::env::var("HH_OPENAI_API_KEY").is_err() {
            self.assistant.settings_editor = VoiceSettingsEditor::from_settings(settings);
            self.open_settings(crate::view_models::SettingsSection::Voice, cx);
            return;
        }
        let (ui_tx, mut ui_rx) = voice_ui_channel();
        let context = AssistantContext {
            workspace_title: self
                .pane_metadata(pane_id)
                .map_or_else(|| "Assistant".to_owned(), |pane| pane.title),
        };
        match spawn_engine(settings, context, ui_tx) {
            Ok(engine) => {
                engine.send(VoiceCommand::SetMicEnabled(true));
                self.assistant.panes.entry(pane_id).or_default().voice = Some(VoiceLink {
                    engine,
                    engine_state: EngineState::Connecting,
                    mic_muted: false,
                    speaker_muted: false,
                    mic_level: 0.0,
                    user_speaking: false,
                    interim_transcript: String::new(),
                    spoken_transcripts: HashMap::new(),
                });
                if let Some(pane) = self.assistant.panes.get_mut(&pane_id) {
                    pane.last_spoken_entry =
                        pane.view.as_ref().and_then(latest_final_assistant_entry);
                }
            }
            Err(error) => {
                self.assistant
                    .panes
                    .entry(pane_id)
                    .or_default()
                    .local_notice = Some(format!("{error:#}"));
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

    pub(crate) fn submit_assistant_composer(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.editor.assistant_composer.clone() else {
            return;
        };
        let pane = self.assistant.panes.entry(composer.pane_id).or_default();
        if pane.prompt_in_flight {
            return;
        }
        let text = composer.text.trim().to_owned();
        if text.is_empty() && composer.attachment.is_none() {
            return;
        }
        let images = match composer
            .attachment
            .as_ref()
            .map(|attachment| assistant_image_from_data_url(&attachment.data_url))
            .transpose()
        {
            Ok(image) => image.into_iter().collect(),
            Err(error) => {
                pane.local_notice = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        let pane_id = composer.pane_id;
        let submitted_text = text.clone();
        pane.prompt_in_flight = true;
        let queued = self.dispatch_with(
            ClientRequest::AssistantPrompt {
                pane_id,
                text,
                images,
            },
            Box::new(move |this, cx, result| {
                if let Some(pane) = this.assistant.panes.get_mut(&pane_id) {
                    finish_prompt_submission(
                        pane,
                        &mut this.editor.assistant_composer,
                        pane_id,
                        &submitted_text,
                        result,
                    );
                }
                cx.notify();
            }),
        );
        if !queued {
            self.assistant
                .panes
                .entry(pane_id)
                .or_default()
                .prompt_in_flight = false;
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
                    this.assistant
                        .panes
                        .entry(pane_id)
                        .or_default()
                        .local_notice = Some(format!("{error:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn toggle_assistant_mic(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        let Some(voice) = self
            .assistant
            .panes
            .get_mut(&pane_id)
            .and_then(|pane| pane.voice.as_mut())
        else {
            self.start_voice_assistant(pane_id, cx);
            return;
        };
        voice.mic_muted = !voice.mic_muted;
        voice
            .engine
            .send(VoiceCommand::SetMicEnabled(!voice.mic_muted));
        cx.notify();
    }

    pub(crate) fn toggle_assistant_speaker(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        let Some(voice) = self
            .assistant
            .panes
            .get_mut(&pane_id)
            .and_then(|pane| pane.voice.as_mut())
        else {
            return;
        };
        voice.speaker_muted = !voice.speaker_muted;
        voice
            .engine
            .send(VoiceCommand::SetSpeakerMuted(voice.speaker_muted));
        cx.notify();
    }

    pub(crate) fn send_assistant_command(&self, pane_id: Uuid, command: VoiceCommand) -> bool {
        self.assistant
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.voice.as_ref())
            .is_some_and(|voice| voice.engine.try_send(command))
    }

    pub(crate) fn prune_assistant_panes(&mut self, live: &HashSet<Uuid>, cx: &mut Context<Self>) {
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
            self.assistant.panes.entry(*pane_id).or_default();
        }
        let removed = self
            .assistant
            .panes
            .keys()
            .copied()
            .filter(|pane_id| !live.contains(pane_id))
            .collect::<Vec<_>>();
        for pane_id in removed {
            if let Some(mut pane) = self.assistant.panes.remove(&pane_id)
                && let Some(voice) = pane.voice.take()
            {
                cx.background_spawn(async move { voice.engine.shutdown() })
                    .detach();
            }
        }
    }

    pub(crate) fn shutdown_voice(&mut self) {
        for pane in self.assistant.panes.values_mut() {
            if let Some(voice) = pane.voice.take() {
                voice.engine.shutdown();
            }
        }
    }

    pub(crate) fn apply_assistant_view(&mut self, view: AssistantThreadView) -> bool {
        let pane = self.assistant.panes.entry(view.pane_id).or_default();
        if pane
            .view
            .as_ref()
            .is_some_and(|current| current.revision >= view.revision)
        {
            return false;
        }
        let approval_was_pending = pane
            .view
            .as_ref()
            .is_some_and(|current| current.pending_approval.is_some());
        if pane
            .view
            .as_ref()
            .is_some_and(|current| current.truncated_entries != view.truncated_entries)
        {
            pane.selected_entry = None;
        }
        if let Some(voice) = pane.voice.as_ref() {
            let relay_enabled = assistant_voice_relay_enabled(
                voice.speaker_muted,
                voice.engine.is_finished(),
                matches!(voice.engine_state, EngineState::Suspended),
            );
            if relay_enabled {
                for (index, entry) in view.entries.iter().enumerate() {
                    let absolute = absolute_entry(&view, index);
                    if pane.last_spoken_entry.is_none_or(|last| absolute > last)
                        && let AssistantEntry::Assistant {
                            text, final_: true, ..
                        } = entry
                        && voice.engine.try_send(VoiceCommand::RelayAssistantText {
                            entry: Some(absolute),
                            text: text.clone(),
                        })
                    {
                        pane.last_spoken_entry = Some(absolute);
                    }
                }
                if !approval_was_pending && let Some(approval) = view.pending_approval.as_ref() {
                    voice.engine.send(VoiceCommand::RelayAssistantText {
                        entry: None,
                        text: format!("Approval needed: {}. {}", approval.title, approval.message),
                    });
                }
            } else if let Some(last) = latest_final_assistant_entry(&view) {
                pane.last_spoken_entry = Some(last);
            }
        }
        pane.view = Some(view);
        pane.transcript_scroll.scroll_to_bottom();
        true
    }

    pub(crate) fn respond_to_assistant_approval(&mut self, allow: bool) -> bool {
        let Some(pane_id) = self.layout.focused_pane else {
            return false;
        };
        let Some(request_id) = self
            .assistant
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.view.as_ref())
            .and_then(|view| view.pending_approval.as_ref())
            .map(|approval| approval.request_id.clone())
        else {
            return false;
        };
        self.dispatch(ClientRequest::AssistantApprovalResponse {
            pane_id,
            request_id,
            allow,
        });
        true
    }

    pub(crate) fn open_assistant_models(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.assistant.models_picker = ModelsPicker {
            pane_id: Some(pane_id),
            models: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
        };
        self.editor.modal = Modal::AssistantModels;
        self.dispatch_with(
            ClientRequest::GetAssistantModels { pane_id },
            Box::new(move |this, cx, result| {
                let current = this
                    .assistant
                    .panes
                    .get(&pane_id)
                    .and_then(|pane| pane.view.as_ref())
                    .and_then(|view| view.model.clone());
                let picker = &mut this.assistant.models_picker;
                picker.loading = false;
                match result {
                    Ok(hh_protocol::ServiceResponse::AssistantModels { models }) => {
                        picker.selected = current
                            .as_deref()
                            .and_then(|current| {
                                models.iter().position(|model| {
                                    format!("{}/{}", model.provider, model.id) == current
                                })
                            })
                            .unwrap_or(0);
                        picker.models = models;
                    }
                    Ok(response) => {
                        picker.error = Some(format!("unexpected response: {response:?}"));
                    }
                    Err(error) => picker.error = Some(format!("{error:#}")),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn handle_assistant_models_key(
        &mut self,
        keystroke: &Keystroke,
        cx: &mut Context<Self>,
    ) {
        match keystroke.key.as_str() {
            "escape" => self.editor.modal = Modal::None,
            "up" => {
                self.assistant.models_picker.selected =
                    self.assistant.models_picker.selected.saturating_sub(1);
            }
            "down" => {
                let last = self.assistant.models_picker.models.len().saturating_sub(1);
                self.assistant.models_picker.selected = self
                    .assistant
                    .models_picker
                    .selected
                    .saturating_add(1)
                    .min(last);
            }
            "enter" => self.select_assistant_model(cx),
            _ => {}
        }
        cx.notify();
    }

    pub(crate) fn select_assistant_model(&mut self, cx: &mut Context<Self>) {
        let picker = &self.assistant.models_picker;
        let Some(pane_id) = picker.pane_id else {
            return;
        };
        let Some(model) = picker.models.get(picker.selected).cloned() else {
            return;
        };
        self.dispatch(ClientRequest::SetAssistantModel {
            pane_id,
            provider: model.provider,
            model_id: model.id,
        });
        self.editor.modal = Modal::None;
        cx.notify();
    }

    fn apply_voice_event(&mut self, pane_id: Uuid, event: VoiceUiEvent) {
        let mut prompt = None;
        let Some(pane) = self.assistant.panes.get_mut(&pane_id) else {
            return;
        };
        let Some(voice) = pane.voice.as_mut() else {
            return;
        };
        match event {
            VoiceUiEvent::State(state) => voice.engine_state = state,
            VoiceUiEvent::UserSpeech { active } => voice.user_speaking = active,
            VoiceUiEvent::UserTranscript { text, final_ } => {
                if final_ {
                    voice.interim_transcript.clear();
                    if !text.trim().is_empty() {
                        prompt = Some(text);
                    }
                } else {
                    voice.interim_transcript.push_str(&text);
                }
            }
            VoiceUiEvent::AssistantTranscript {
                entry,
                text,
                final_,
            } => apply_spoken_transcript(&mut voice.spoken_transcripts, entry, text, final_),
            VoiceUiEvent::MicLevel(level) => voice.mic_level = level.clamp(0.0, 1.0),
            VoiceUiEvent::Notice { message, .. } => pane.local_notice = Some(message),
            VoiceUiEvent::PlaybackProgress { .. } | VoiceUiEvent::Usage { .. } => {}
        }
        if let Some(text) = prompt {
            self.dispatch(ClientRequest::AssistantPrompt {
                pane_id,
                text,
                images: Vec::new(),
            });
        }
    }
}

fn finish_prompt_submission(
    pane: &mut AssistantPaneState,
    composer: &mut Option<AssistantComposer>,
    pane_id: Uuid,
    submitted_text: &str,
    result: anyhow::Result<ServiceResponse>,
) {
    pane.prompt_in_flight = false;
    match result {
        Ok(ServiceResponse::Ack) => {
            clear_acknowledged_composer(composer, pane_id, submitted_text);
        }
        Ok(other) => pane.local_notice = Some(format!("unexpected response: {other:?}")),
        Err(error) => pane.local_notice = Some(format!("{error:#}")),
    }
}

fn apply_spoken_transcript(
    transcripts: &mut HashMap<u64, String>,
    entry: Option<u64>,
    text: String,
    final_: bool,
) {
    let Some(entry) = entry else {
        return;
    };
    if final_ {
        if text.trim().is_empty() {
            transcripts.remove(&entry);
        } else {
            transcripts.insert(entry, text);
        }
    } else {
        transcripts.entry(entry).or_default().push_str(&text);
    }
}

fn clear_acknowledged_composer(
    composer: &mut Option<AssistantComposer>,
    pane_id: Uuid,
    submitted_text: &str,
) {
    if let Some(active) = composer.as_mut()
        && active.pane_id == pane_id
        && active.text.trim() == submitted_text
    {
        active.text.clear();
        active.selection = None;
        active.attachment = None;
    }
}

pub(crate) fn absolute_entry(view: &AssistantThreadView, index: usize) -> u64 {
    u64::from(view.truncated_entries) + index as u64
}

fn latest_final_assistant_entry(view: &AssistantThreadView) -> Option<u64> {
    view.entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| matches!(entry, AssistantEntry::Assistant { final_: true, .. }))
        .map(|(index, _)| absolute_entry(view, index))
}

fn assistant_voice_relay_enabled(
    speaker_muted: bool,
    engine_finished: bool,
    suspended: bool,
) -> bool {
    !speaker_muted && !engine_finished && !suspended
}

pub(crate) fn assistant_pane_is_active(pane: &AssistantPaneState) -> bool {
    pane.view.as_ref().is_some_and(|view| {
        matches!(
            view.status,
            AssistantStatus::Starting | AssistantStatus::Streaming | AssistantStatus::Compacting
        )
    })
}

pub(crate) fn assistant_entry_text(entry: &AssistantEntry) -> &str {
    match entry {
        AssistantEntry::User { text, .. } | AssistantEntry::Assistant { text, .. } => text,
        AssistantEntry::ToolCall { output, .. } if !output.is_empty() => output,
        AssistantEntry::Notice { message, .. } => message,
        AssistantEntry::ToolCall { summary, .. } => summary,
    }
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

#[path = "voice_view.rs"]
mod voice_view;

impl HhApp {
    pub(crate) fn paste_voice_setting(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if !matches!(self.editor.modal, Modal::AppearanceSettings) {
            return false;
        }
        let Some(field) = self.assistant.settings_editor.active_field else {
            return false;
        };
        let value = text.trim().to_owned();
        match field {
            VoiceSettingsField::ApiKey => self.assistant.settings_editor.api_key_input = value,
            VoiceSettingsField::IdleTimeout => {
                self.assistant.settings_editor.idle_timeout_input = value
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
        if self.assistant.settings_editor.active_field.is_none() {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.assistant.settings_editor.active_field = None;
            }
            "tab" => {
                self.assistant.settings_editor.active_field =
                    match self.assistant.settings_editor.active_field {
                        None | Some(VoiceSettingsField::IdleTimeout) => {
                            Some(VoiceSettingsField::ApiKey)
                        }
                        Some(VoiceSettingsField::ApiKey) => Some(VoiceSettingsField::IdleTimeout),
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
        match self.assistant.settings_editor.active_field {
            None | Some(VoiceSettingsField::ApiKey) => {
                &mut self.assistant.settings_editor.api_key_input
            }
            Some(VoiceSettingsField::IdleTimeout) => {
                &mut self.assistant.settings_editor.idle_timeout_input
            }
        }
    }

    pub(crate) fn set_assistant_access(&mut self, access: AssistantAccess) {
        let mut settings = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.assistant.clone())
            .unwrap_or_default();
        settings.access = access;
        self.dispatch(ClientRequest::SetAssistantSettings { settings });
    }

    /// Saves the preferred coding agent; None lets the orchestrator choose.
    pub(crate) fn set_preferred_agent(&mut self, preferred: Option<TerminalProfile>) {
        let mut settings = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.assistant.clone())
            .unwrap_or_default();
        settings.preferred_agent = preferred;
        self.dispatch(ClientRequest::SetAssistantSettings { settings });
    }

    /// Re-runs login-PATH discovery on the service and stores the result.
    pub(crate) fn refresh_coding_agents(&mut self, cx: &mut Context<Self>) {
        self.assistant.coding_agents.loading = true;
        self.assistant.coding_agents.error = None;
        cx.notify();
        self.dispatch_with(
            ClientRequest::GetCodingAgents,
            Box::new(|this, cx, result| {
                let state = &mut this.assistant.coding_agents;
                state.loading = false;
                state.loaded = true;
                match result {
                    Ok(ServiceResponse::CodingAgents { agents }) => state.agents = agents,
                    Ok(other) => state.error = Some(format!("unexpected response: {other:?}")),
                    Err(error) => state.error = Some(format!("{error:#}")),
                }
                cx.notify();
            }),
        );
    }

    fn persist_voice_settings(&mut self) {
        let editor = &mut self.assistant.settings_editor;
        editor.settings.api_key.clone_from(&editor.api_key_input);
        if let Ok(timeout) = editor.idle_timeout_input.parse::<u32>() {
            editor.settings.idle_timeout_secs = timeout;
        }
        if let Err(error) = editor.settings.save() {
            eprintln!("voice settings were not saved: {error}");
        }
    }

    pub(crate) fn set_voice_model(&mut self, model: &'static str, cx: &mut Context<Self>) {
        model.clone_into(&mut self.assistant.settings_editor.settings.model);
        self.persist_voice_settings();
        cx.notify();
    }

    pub(crate) fn set_voice(&mut self, voice: &'static str, cx: &mut Context<Self>) {
        voice.clone_into(&mut self.assistant.settings_editor.settings.voice);
        self.persist_voice_settings();
        cx.notify();
    }

    pub(crate) fn toggle_full_duplex(&mut self, cx: &mut Context<Self>) {
        self.assistant.settings_editor.settings.full_duplex =
            !self.assistant.settings_editor.settings.full_duplex;
        self.persist_voice_settings();
        cx.notify();
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
