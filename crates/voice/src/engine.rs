use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hh_protocol::{SessionNotification, WorkspaceKind};

use crate::audio::{AudioInputEvent, AudioSystem};
use crate::memory::{MemoryBackend, NullBackend, Role, backend};
use crate::realtime::{
    ClientEvent, ConversationItem, ConversationRole, InputContent, RealtimeHandle, RealtimeInbound,
    ServerEvent, SessionConfig,
};
use crate::threads::{self, ThreadRecord, ThreadRole};
use crate::tools::{ToolExecutor, tool_schemas};
use crate::{
    AssistantContext, EngineState, VoiceCommand, VoiceEngineHandle, VoiceSettings, VoiceUiEvent,
    VoiceUiSender,
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const LOOP_SLEEP: Duration = Duration::from_millis(10);
const MAX_CRITICAL_UI_EVENTS_PER_REALTIME_INBOUND: usize = 8;
const NARRATION_INTERVAL: Duration = Duration::from_secs(2);
const MIC_LEVEL_INTERVAL: Duration = Duration::from_millis(100);
const HALF_DUPLEX_RELEASE_DELAY: Duration = Duration::from_millis(500);
const SESSION_ROLL_AGE: Duration = Duration::from_mins(50);
const SESSION_ROLL_INPUT_TOKENS: u64 = 90_000;
const MAX_TRANSCRIPT_TURNS: usize = 100;
const MAX_TRANSCRIPT_CHARS: usize = 32 * 1024;
const MAX_USER_TEXT_CHARS: usize = 32 * 1024;
const MAX_PENDING_USER_ITEMS: usize = crate::MAX_ACCEPTED_USER_ITEMS;
const MAX_NARRATION_ITEMS: usize = 64;
#[cfg(test)]
const MAX_NARRATION_CHARS: usize = 2 * 1024;
const REALTIME_INBOUND_CAPACITY: usize = 256;
const SUMMARY_TURNS: usize = 15;
const MAX_SUMMARY_CHARS: usize = 2_000;

#[derive(Debug, Default)]
struct MicrophoneConsent {
    explicitly_enabled: bool,
}

impl MicrophoneConsent {
    const fn capture_enabled(&self) -> bool {
        self.explicitly_enabled
    }

    fn apply_command(&mut self, enabled: bool) {
        self.explicitly_enabled = enabled;
    }
}

const BASE_INSTRUCTIONS: &str = concat!(
    "You are the Harness Harlot voice assistant — a hands-on project manager for a terminal ",
    "workstation app. You manage workstations, tabs, git worktrees, and coding-agent CLIs (omp, ",
    "hermes, codex, claude) on the user's behalf using your tools. ",
    "Keep replies to one short sentence unless the user asks for detail; the only exception: ",
    "before a long-running tool sequence, say a 3-6 word preamble like \"on it — creating that ",
    "worktree\". Don't volunteer your capabilities unprompted and don't repeat yourself. ",
    "Act, don't interrogate: when the user asks to open or create a terminal tab and Where you ",
    "live is a workstation, call open_terminal_tab immediately with that workstation id. From ",
    "an assistant workspace, call list_workstations; choose the user-named kind=workstation ",
    "target or the sole workstation, call attach_project, then call open_terminal_tab. If several ",
    "workstations exist and none was named, ask one short question listing their titles; if none ",
    "exists, call create_workstation. Never pass a kind=assistant id to terminal, project, or ",
    "worktree tools, and never claim success without the tool result. When the user asks you to ",
    "run a shell command in a tab you created, call send_input immediately; infer directories ",
    "from working_dir and project_dir in list_workstations or use ~-relative paths the shell ",
    "expands — do not ask for exact paths. ",
    "You cannot guess filesystem paths: when the user names a project or directory whose exact ",
    "path you have not seen in a tool result, call find_directory with the spoken name (or ",
    "list_directory to browse from home) and use a returned path for open_project_tab, ",
    "create_workstation, or create_worktree_tab. ",
    "Earlier conversations are saved as threads: call list_threads to see them and read_thread ",
    "to review one whenever the user references past work — never claim earlier conversations ",
    "are unavailable without checking. ",
    "If a directory tool errors with a list of existing directories, pick the correct one from ",
    "that list and retry instead of reporting failure. If a command fails, read_pane and report ",
    "one short line. If a tool errors, report the error briefly and suggest the closest fix. ",
    "Never invent tool results. Every terminal mutation or launch requires an independently captured ",
    "UI decision. When a tool returns status needs_approval or requires_ui_click, briefly describe ",
    "the exact pending action and tell the user to click Approve or Deny in the pane. Spoken ",
    "confirmation is never authorization and you have no tool that can resolve an approval. Never ",
    "try to close or delete your own assistant tab or workstation. If the user says stop or cancel, ",
    "stop talking, start no new tool calls, and tell them they can click Deny for any pending action. ",
    "naming the workstation and tab; ignore other events unless asked. When the user names a ",
    "project, call attach_project before any other tool that needs that project — not on mere ",
    "mentions. To start an agent on a task: create_worktree_tab (or open_project_tab), ",
    "launch_agent, then send_input with the task text.",
);

pub(crate) fn spawn(
    mut settings: VoiceSettings,
    context: AssistantContext,
    ui: VoiceUiSender,
) -> Result<VoiceEngineHandle> {
    if settings.api_key.trim().is_empty()
        && let Ok(api_key) = std::env::var("HH_OPENAI_API_KEY")
    {
        settings.api_key = api_key;
    }
    if settings.api_key.trim().is_empty() {
        anyhow::bail!("OpenAI API key is empty");
    }
    let (command_tx, command_rx) = std::sync::mpsc::sync_channel(64);
    let accepted_user_items = Arc::new(AtomicUsize::new(0));
    let engine_accepted_user_items = Arc::clone(&accepted_user_items);
    let join = std::thread::Builder::new()
        .name("hh-voice-engine".to_owned())
        .spawn(move || {
            match VoiceEngine::new(
                settings,
                context,
                ui.clone(),
                command_rx,
                engine_accepted_user_items,
            ) {
                Ok(mut engine) => engine.run(),
                Err(error) => {
                    eprintln!("voice engine failed to start: {error:#}");
                    let _ = ui.emit(VoiceUiEvent::State(EngineState::Error(format!(
                        "{error:#}"
                    ))));
                }
            }
        })
        .context("spawn voice engine thread")?;
    Ok(VoiceEngineHandle {
        command_tx,
        accepted_user_items,
        join: Some(join),
    })
}

#[allow(clippy::struct_excessive_bools)]
struct VoiceEngine {
    settings: VoiceSettings,
    context: AssistantContext,
    ui: VoiceUiSender,
    command_rx: Receiver<VoiceCommand>,
    accepted_user_items: Arc<AtomicUsize>,
    realtime_tx: SyncSender<RealtimeInbound>,
    realtime_rx: Receiver<RealtimeInbound>,
    realtime: Option<RealtimeHandle>,
    audio: AudioSystem,
    tools: Option<ToolExecutor>,
    memory: Option<Box<dyn MemoryBackend>>,
    active_tool: Option<ActiveTool>,
    pending_memory_turns: VecDeque<(Role, String)>,
    transcripts: VecDeque<(Role, String)>,
    thread_id: Option<uuid::Uuid>,
    thread_generation: Option<threads::ThreadGeneration>,
    thread_has_title: bool,
    completed_input_transcriptions: VecDeque<String>,
    pending_user_content: VecDeque<InputContent>,
    pending_user_send: Option<PendingUserSend>,
    narration: VecDeque<ClientEvent>,
    response_active: bool,
    user_speaking: bool,
    mic_consent: MicrophoneConsent,
    speaker_muted: bool,
    suspended: bool,
    connected_at: Option<Instant>,
    reconnect_roll: bool,
    last_activity: Instant,
    last_output_audio: Option<Instant>,
    last_poll: Instant,
    last_narration: Option<Instant>,
    last_mic_level: Instant,
    last_playback_progress: Instant,
    pending_instructions: Option<String>,
}

struct PendingUserSend {
    result: Receiver<crate::realtime::RecoverableSendResult>,
    user_text: Vec<String>,
    user_item_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UserSendFailureDisposition {
    DefinitelyUnsent,
    Indeterminate,
}

impl UserSendFailureDisposition {
    const fn retires_admission(self) -> bool {
        matches!(self, Self::Indeterminate)
    }

    #[cfg(test)]
    const fn restores_content(self) -> bool {
        matches!(self, Self::DefinitelyUnsent)
    }

    #[cfg(test)]
    const fn creates_response() -> bool {
        false
    }
}

const fn user_send_failure_disposition(has_unsent_event: bool) -> UserSendFailureDisposition {
    if has_unsent_event {
        UserSendFailureDisposition::DefinitelyUnsent
    } else {
        UserSendFailureDisposition::Indeterminate
    }
}

struct ActiveTool {
    cancelled: Arc<AtomicBool>,
    result: Receiver<ToolJobResult>,
}

struct ToolJobResult {
    tools: ToolExecutor,
    memory: Box<dyn MemoryBackend>,
    completion: ToolCompletion,
}

enum ToolCompletion {
    Function {
        call_id: String,
        name: String,
        output: String,
    },
    Approval {
        approval_id: u64,
        approved: bool,
        result: Result<serde_json::Value>,
    },
    Cancelled,
}

impl VoiceEngine {
    fn new(
        settings: VoiceSettings,
        context: AssistantContext,
        ui: VoiceUiSender,
        command_rx: Receiver<VoiceCommand>,
        accepted_user_items: Arc<AtomicUsize>,
    ) -> Result<Self> {
        let thread_id = context.pane_id;
        let _ = ui.emit(VoiceUiEvent::State(EngineState::Connecting));
        let mut tools = ToolExecutor::connect()?;
        tools.authorize_context(context.workspace_id, context.working_dir.as_deref())?;
        if context.workspace_kind == WorkspaceKind::Workstation
            && let Some(workspace_id) = context.workspace_id
        {
            tools.attach_workspace(workspace_id);
        }
        let pending_instructions = context
            .prior_context
            .as_deref()
            .filter(|summary| !summary.is_empty())
            .map(str::to_owned);
        // Do not create a durable writer authority until the fallible tool
        // authorization setup has succeeded. This keeps failed startup from
        // leaving a pre-file authority behind indefinitely.
        let thread_generation = thread_id.map(threads::prepare_writer).transpose()?;
        let mut thread_has_title = false;
        if let Some(thread_id) = thread_id {
            thread_has_title =
                threads::read_thread(thread_id)?.is_some_and(|thread| thread.title.is_some());
            threads::append_record(
                thread_generation.context("thread generation is missing")?,
                thread_id,
                &ThreadRecord::Meta {
                    thread_id,
                    workspace_id: context.workspace_id,
                    workspace_title: context.workspace_title.clone(),
                    at_ms: threads::now_ms(),
                },
            )?;
        }
        let audio = AudioSystem::start()?;
        let memory: Box<dyn MemoryBackend> = match backend(settings.honcho.clone()) {
            Ok(memory) => memory,
            Err(error) => {
                let _ = ui.emit(VoiceUiEvent::ToolCall {
                    name: "memory.error".to_owned(),
                    summary: format!("Honcho disabled: {error:#}"),
                });
                Box::<NullBackend>::default()
            }
        };
        let (realtime_tx, realtime_rx) = std::sync::mpsc::sync_channel(REALTIME_INBOUND_CAPACITY);
        let realtime = Some(crate::realtime::spawn(
            settings.api_key.clone(),
            settings.model.clone(),
            realtime_tx.clone(),
        )?);
        let now = Instant::now();
        Ok(Self {
            settings,
            context,
            ui,
            command_rx,
            accepted_user_items,
            realtime_tx,
            realtime_rx,
            realtime,
            audio,
            tools: Some(tools),
            memory: Some(memory),
            active_tool: None,
            pending_memory_turns: VecDeque::new(),
            thread_id,
            thread_generation,
            thread_has_title,
            transcripts: VecDeque::with_capacity(MAX_TRANSCRIPT_TURNS),
            completed_input_transcriptions: VecDeque::with_capacity(MAX_TRANSCRIPT_TURNS),
            pending_user_content: VecDeque::new(),
            pending_user_send: None,
            narration: VecDeque::new(),
            response_active: false,
            user_speaking: false,
            mic_consent: MicrophoneConsent::default(),
            speaker_muted: false,
            suspended: false,
            connected_at: None,
            reconnect_roll: false,
            last_activity: now,
            last_output_audio: None,
            last_poll: now,
            last_narration: None,
            last_mic_level: now.checked_sub(MIC_LEVEL_INTERVAL).unwrap_or(now),
            last_playback_progress: now.checked_sub(MIC_LEVEL_INTERVAL).unwrap_or(now),
            pending_instructions,
        })
    }

    fn run(&mut self) {
        loop {
            match self.drain_commands() {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => self.emit_error(&error),
            }
            self.finish_active_tool();
            self.drain_realtime();
            self.drain_audio();
            self.tick();
            std::thread::sleep(LOOP_SLEEP);
        }
        self.shutdown_resources();
    }

    fn drain_commands(&mut self) -> Result<bool> {
        loop {
            match self.command_rx.try_recv() {
                Ok(VoiceCommand::Shutdown) | Err(TryRecvError::Disconnected) => {
                    self.cancel_active_tool();
                    return Ok(true);
                }
                Ok(VoiceCommand::SetMicEnabled(enabled)) => {
                    self.mic_consent.apply_command(enabled);
                    self.audio.set_mic_enabled(enabled && !self.suspended)?;
                }
                Ok(VoiceCommand::SetSpeakerMuted(muted)) => {
                    if muted {
                        let (item_id, played_ms) = self.audio.stop_and_clear();
                        if let Some(item_id) = item_id {
                            self.send(ClientEvent::ConversationItemTruncate {
                                item_id,
                                content_index: 0,
                                audio_end_ms: played_ms,
                            })?;
                        }
                    }
                    self.speaker_muted = muted;
                }
                Ok(VoiceCommand::SendUserText(text)) => {
                    queue_pending_user_content(
                        &mut self.pending_user_content,
                        InputContent::InputText { text },
                    );
                    self.last_activity = Instant::now();
                    if self.suspended {
                        self.resume()?;
                    }
                }
                Ok(VoiceCommand::SendUserImage { data_url }) => {
                    queue_pending_user_content(
                        &mut self.pending_user_content,
                        InputContent::InputImage {
                            image_url: data_url,
                        },
                    );
                    self.last_activity = Instant::now();
                    if self.suspended {
                        self.resume()?;
                    }
                }
                Ok(VoiceCommand::BargeIn) => self.barge_in(true)?,
                Ok(VoiceCommand::Approve { approval_id }) => {
                    self.start_approval_tool(approval_id, true)?;
                }
                Ok(VoiceCommand::Deny { approval_id }) => {
                    self.start_approval_tool(approval_id, false)?;
                }
                Ok(VoiceCommand::Suspend) => self.suspend()?,
                Ok(VoiceCommand::Resume) => self.resume()?,
                Err(TryRecvError::Empty) => return Ok(false),
            }
        }
    }

    fn drain_realtime(&mut self) {
        while self.ui.critical_capacity() >= MAX_CRITICAL_UI_EVENTS_PER_REALTIME_INBOUND
            && let Ok(event) = self.realtime_rx.try_recv()
        {
            if let Err(error) = self.handle_realtime(event) {
                self.emit_error(&error);
            }
        }
    }

    fn drain_audio(&mut self) {
        while let Some(event) = self.audio.try_input() {
            match event {
                AudioInputEvent::Chunk(chunk) => {
                    let now = Instant::now();
                    if now.saturating_duration_since(self.last_mic_level) >= MIC_LEVEL_INTERVAL {
                        self.last_mic_level = now;
                        let _ = self.ui.emit(VoiceUiEvent::MicLevel(chunk.rms));
                    }
                    let output_quiet = self.last_output_audio.is_none_or(|last_output| {
                        now.saturating_duration_since(last_output) >= HALF_DUPLEX_RELEASE_DELAY
                    });
                    let streaming = !self.suspended
                        && self.mic_consent.capture_enabled()
                        && self.connected_at.is_some()
                        && microphone_streaming_allowed(
                            self.settings.full_duplex,
                            MicrophoneActivity {
                                response_active: self.response_active,
                                output_quiet,
                                playback_active: self.audio.playback_active(),
                            },
                        );
                    if streaming {
                        self.send_mic_chunk(&chunk.samples);
                    }
                }
                AudioInputEvent::Error(error) => {
                    let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                        name: "audio.error".to_owned(),
                        summary: error,
                    });
                }
            }
        }
    }

    fn handle_realtime(&mut self, inbound: RealtimeInbound) -> Result<()> {
        match inbound {
            RealtimeInbound::Connected => {
                let prior = self.pending_instructions.take().or_else(|| {
                    if self.reconnect_roll {
                        let summary = self.build_summary();
                        (!summary.is_empty()).then_some(summary)
                    } else {
                        self.memory
                            .as_mut()
                            .and_then(|memory| memory.session_preamble())
                    }
                });
                let instructions = instructions_with_context(&self.context, prior.as_deref())?;
                self.send(ClientEvent::SessionUpdate {
                    session: Box::new(SessionConfig::new(
                        instructions,
                        self.settings.voice.clone(),
                        tool_schemas(),
                    )),
                })?;
                self.connected_at = Some(Instant::now());
                self.reconnect_roll = false;
                self.suspended = false;
                let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Listening));
            }
            RealtimeInbound::Disconnected(error) => {
                let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                    name: "realtime.reconnect".to_owned(),
                    summary: error,
                });
                self.clear_user_speaking();
                self.connected_at = None;
                self.reconnect_roll = true;
                if !self.suspended {
                    let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Connecting));
                }
            }
            RealtimeInbound::Warning(error) => {
                let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                    name: "realtime.warning".to_owned(),
                    summary: error,
                });
            }
            RealtimeInbound::Event(event) => self.handle_server_event(event)?,
        }
        Ok(())
    }

    fn handle_server_event(&mut self, event: ServerEvent) -> Result<()> {
        match event {
            ServerEvent::SessionCreated { .. }
            | ServerEvent::SessionUpdated { .. }
            | ServerEvent::AudioCommitted { .. }
            | ServerEvent::RateLimitsUpdated { .. }
            | ServerEvent::Unknown => {}
            ServerEvent::Error { error } => {
                let code = error.code.as_deref().unwrap_or(&error.error_type);
                let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                    name: "realtime.error".to_owned(),
                    summary: format!("{code}: {}", error.message),
                });
            }
            ServerEvent::SpeechStarted { .. } => {
                self.user_speaking = true;
                let _ = self.ui.emit(VoiceUiEvent::UserSpeech { active: true });
                self.last_activity = Instant::now();
                // Server VAD is configured to interrupt the response. Always
                // clear local playback too, including in the default mode, so
                // a spoken "stop" takes effect immediately.
                self.barge_in(false)?;
            }
            ServerEvent::SpeechStopped { .. } => self.clear_user_speaking(),
            ServerEvent::InputTranscriptionDelta { delta, item_id } => {
                if transcription_already_completed(
                    &self.completed_input_transcriptions,
                    item_id.as_deref(),
                ) {
                    return Ok(());
                }
                self.last_activity = Instant::now();
                let _ = self.ui.emit(VoiceUiEvent::UserTranscript {
                    text: delta,
                    final_: false,
                });
            }
            ServerEvent::InputTranscriptionCompleted {
                transcript,
                item_id,
            } => {
                if !accept_completed_transcription(
                    &mut self.completed_input_transcriptions,
                    item_id,
                ) {
                    return Ok(());
                }
                self.clear_user_speaking();
                self.last_activity = Instant::now();
                self.record_user_turn(&transcript)?;
                let _ = self.ui.emit(VoiceUiEvent::UserTranscript {
                    text: transcript,
                    final_: true,
                });
            }
            ServerEvent::ResponseCreated { .. } => {
                self.last_activity = Instant::now();
                self.response_active = true;
                let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Thinking));
            }
            ServerEvent::ResponseDone { response } => {
                self.response_active = false;
                if response_done_successful(response.status.as_deref()) {
                    self.audio.finish_output()?;
                } else {
                    let _ = self.audio.stop_and_clear();
                    let status = response.status.as_deref().unwrap_or("unknown");
                    let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                        name: "realtime.response".to_owned(),
                        summary: format!("response ended with status {status}"),
                    });
                }
                if let Some(usage) = response.usage {
                    let _ = self.ui.emit(VoiceUiEvent::Usage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                    });
                    if session_roll_due(self.connected_at, Instant::now(), usage.input_tokens) {
                        self.roll_session("context threshold")?;
                    }
                }
                let state = if self.audio.playback_active() {
                    EngineState::Speaking
                } else {
                    EngineState::Listening
                };
                let _ = self.ui.emit(VoiceUiEvent::State(state));
            }
            ServerEvent::OutputAudioDelta { item_id, delta } => {
                let now = Instant::now();
                self.last_activity = now;
                if self.speaker_muted {
                    // Speaker is muted: discard synthesized audio while the
                    // transcript keeps streaming.
                    return Ok(());
                }
                let bytes = BASE64.decode(delta).context("decode assistant PCM audio")?;
                let samples = bytes
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect::<Vec<_>>();
                self.audio.push_output(&item_id, &samples)?;
                self.last_output_audio = Some(now);
                let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Speaking));
            }
            ServerEvent::OutputTranscriptDelta { delta, .. } => {
                let _ = self.ui.emit(VoiceUiEvent::AssistantTranscript {
                    text: delta,
                    final_: false,
                });
            }
            ServerEvent::OutputTranscriptDone { transcript, .. } => {
                self.record_memory_turn(Role::Assistant, &transcript);
                self.push_transcript(Role::Assistant, transcript.clone());
                self.append_thread_record(&ThreadRecord::Turn {
                    role: ThreadRole::Assistant,
                    text: transcript.clone(),
                    at_ms: threads::now_ms(),
                })?;
                let _ = self.ui.emit(VoiceUiEvent::AssistantTranscript {
                    text: transcript,
                    final_: true,
                });
            }
            ServerEvent::FunctionCallArgumentsDone {
                call_id,
                name,
                arguments,
            } => {
                let _ = self
                    .ui
                    .emit(VoiceUiEvent::ToolCallStarted { name: name.clone() });
                let _ = self.ui.emit(VoiceUiEvent::State(EngineState::ToolRunning));
                self.start_function_tool(call_id, name, arguments)?;
            }
        }
        Ok(())
    }

    fn tick(&mut self) {
        // A suspended engine is dormant by design: no polling, no narration,
        // no idle roll. Only an explicit Resume command wakes it.
        if self.suspended {
            return;
        }
        let now = Instant::now();
        self.finish_pending_user_send(now);
        if now.saturating_duration_since(self.last_playback_progress) >= MIC_LEVEL_INTERVAL {
            self.last_playback_progress = now;
            if let Some((played_ms, total_ms)) = self.audio.playback_progress() {
                let _ = self.ui.emit(VoiceUiEvent::PlaybackProgress {
                    played_ms,
                    total_ms,
                });
            }
        }
        if now.saturating_duration_since(self.last_poll) >= POLL_INTERVAL {
            self.last_poll = now;
            if let Some(tools) = self.tools.as_mut() {
                match tools.poll_updates() {
                    Ok(notifications) => {
                        for notification in notifications {
                            if tools.notification_is_attached(&notification) {
                                for event in notification_model_events(&notification) {
                                    queue_narration(&mut self.narration, event);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                            name: "status.poll".to_owned(),
                            summary: format!("{error:#}"),
                        });
                    }
                }
            }
        }
        if narration_ready(self.last_narration, now)
            && !self.response_active
            && !self.user_speaking
            && !self.suspended
            && self.connected_at.is_some()
            && let Some(event) = self.narration.pop_front()
        {
            self.last_narration = Some(now);
            if let Err(error) = self.send(event).and_then(|()| {
                self.send(ClientEvent::ResponseCreate { response: None })?;
                self.response_active = true;
                Ok(())
            }) {
                self.emit_error(&error);
            }
        }
        if self.connected_at.is_some()
            && !self.response_active
            && !self.user_speaking
            && !self.pending_user_content.is_empty()
            && self.pending_user_send.is_none()
            && let Some(realtime) = self.realtime.as_ref()
        {
            let content = std::mem::take(&mut self.pending_user_content)
                .into_iter()
                .collect::<Vec<_>>();
            let user_item_count = content.len();
            let user_text = content
                .iter()
                .filter_map(|item| match item {
                    InputContent::InputText { text } => Some(text.clone()),
                    InputContent::InputImage { .. } => None,
                })
                .collect::<Vec<_>>();
            let event = ClientEvent::ConversationItemCreate {
                item: ConversationItem::Message {
                    role: ConversationRole::User,
                    content,
                },
                previous_item_id: None,
            };
            match realtime.send_recoverable_async(event.clone()) {
                Ok(result) => {
                    self.pending_user_send = Some(PendingUserSend {
                        result,
                        user_text,
                        user_item_count,
                    });
                }
                Err(error) => {
                    restore_pending_user_content(&mut self.pending_user_content, event);
                    self.emit_error(&error);
                }
            }
        }
        if session_roll_due(self.connected_at, now, 0)
            && let Err(error) = self.roll_session("fifty-minute session limit")
        {
            self.emit_error(&error);
        }
        if let Some(timeout) = effective_idle_timeout(self.settings.idle_timeout_secs)
            && !self.suspended
            && !self.response_active
            && !self.user_speaking
            && !self.audio.playback_active()
            && !self
                .tools
                .as_ref()
                .is_some_and(ToolExecutor::has_pending_approvals)
            && self.active_tool.is_none()
            && now.saturating_duration_since(self.last_activity) >= timeout
            && let Err(error) = self.suspend()
        {
            self.emit_error(&error);
        }
    }

    fn finish_pending_user_send(&mut self, now: Instant) {
        let Some(pending) = self.pending_user_send.take() else {
            return;
        };
        match pending.result.try_recv() {
            Ok(Ok(())) => {
                self.accepted_user_items
                    .fetch_sub(pending.user_item_count, Ordering::AcqRel);
                for text in pending.user_text {
                    if let Err(error) = self.record_user_turn(&text) {
                        self.emit_error(&error);
                    }
                }
                if let Err(error) = self.send(ClientEvent::ResponseCreate { response: None }) {
                    self.emit_error(&error);
                } else {
                    self.response_active = true;
                    self.last_activity = now;
                }
            }
            Ok(Err((error, unsent))) => {
                let disposition = user_send_failure_disposition(unsent.is_some());
                if let Some(unsent) = unsent {
                    restore_pending_user_content(&mut self.pending_user_content, unsent);
                }
                if disposition.retires_admission() {
                    self.accepted_user_items
                        .fetch_sub(pending.user_item_count, Ordering::AcqRel);
                    let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                        name: "realtime.indeterminate".to_owned(),
                        summary: "Your turn may have reached the provider, but delivery could not be confirmed. It was not replayed and no response was requested; resend explicitly if needed.".to_owned(),
                    });
                }
                self.emit_error(&error);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.pending_user_send = Some(pending);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.accepted_user_items
                    .fetch_sub(pending.user_item_count, Ordering::AcqRel);
                let _ = self.ui.emit(VoiceUiEvent::ToolCall {
                    name: "realtime.indeterminate".to_owned(),
                    summary: "Your turn's delivery result is indeterminate. It was not replayed and no response was requested; resend explicitly if needed.".to_owned(),
                });
                self.emit_error(&anyhow::anyhow!("Realtime acknowledgement waiter stopped"));
            }
        }
    }

    fn send_mic_chunk(&mut self, samples: &[i16]) {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let _ = self.send(ClientEvent::InputAudioBufferAppend {
            audio: BASE64.encode(bytes),
        });
    }

    fn clear_user_speaking(&mut self) {
        if self.user_speaking {
            self.user_speaking = false;
            let _ = self.ui.emit(VoiceUiEvent::UserSpeech { active: false });
        }
    }

    fn barge_in(&mut self, cancel_response: bool) -> Result<()> {
        self.cancel_active_tool();
        let (item_id, played_ms) = self.audio.stop_and_clear();
        if let Some(item_id) = item_id {
            self.send(ClientEvent::ConversationItemTruncate {
                item_id,
                content_index: 0,
                audio_end_ms: played_ms,
            })?;
        }
        if cancel_response && self.response_active {
            self.send(ClientEvent::ResponseCancel)?;
            self.response_active = false;
        }
        let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Listening));
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        if self.suspended {
            return Ok(());
        }
        self.cancel_active_tool();
        if let Some(realtime) = self.realtime.take() {
            realtime.shutdown();
        }
        self.audio.set_mic_enabled(false)?;
        self.suspended = true;
        self.connected_at = None;
        let summary = self.build_summary();
        if !summary.is_empty() {
            self.append_thread_record(&ThreadRecord::Summary {
                text: summary.clone(),
                at_ms: threads::now_ms(),
            })?;
            let _ = self.ui.emit(VoiceUiEvent::SessionSummary { text: summary });
        }
        self.clear_user_speaking();
        let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Suspended));
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if !self.suspended && self.realtime.is_some() {
            return Ok(());
        }
        // Resuming continues the prior conversation: seed the next session
        // with the running summary before the socket reconnects.
        let summary = self.build_summary();
        if !summary.is_empty() {
            self.pending_instructions = Some(summary);
        }
        self.realtime = Some(crate::realtime::spawn(
            self.settings.api_key.clone(),
            self.settings.model.clone(),
            self.realtime_tx.clone(),
        )?);
        self.audio
            .set_mic_enabled(self.mic_consent.capture_enabled())?;
        self.suspended = false;
        self.last_activity = Instant::now();
        let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Connecting));
        Ok(())
    }

    fn roll_session(&mut self, reason: &str) -> Result<()> {
        let summary = self.build_summary();
        if summary.is_empty() {
            self.pending_instructions = None;
        } else {
            self.append_thread_record(&ThreadRecord::Summary {
                text: summary.clone(),
                at_ms: threads::now_ms(),
            })?;
            let _ = self.ui.emit(VoiceUiEvent::SessionSummary {
                text: summary.clone(),
            });
            self.pending_instructions = Some(summary);
        }
        if let Some(realtime) = self.realtime.take() {
            realtime.shutdown();
        }
        self.clear_user_speaking();
        self.connected_at = None;
        self.realtime = Some(crate::realtime::spawn(
            self.settings.api_key.clone(),
            self.settings.model.clone(),
            self.realtime_tx.clone(),
        )?);
        let _ = self.ui.emit(VoiceUiEvent::ToolCall {
            name: "session.roll".to_owned(),
            summary: reason.to_owned(),
        });
        Ok(())
    }

    fn start_function_tool(
        &mut self,
        call_id: String,
        name: String,
        arguments: String,
    ) -> Result<()> {
        if self.active_tool.is_some() {
            anyhow::bail!("tool execution is already in progress");
        }
        let mut tools = self.tools.take().context("tool executor is unavailable")?;
        let mut memory = self
            .memory
            .take()
            .context("memory backend is unavailable")?;
        let ui = self.ui.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (result_tx, result) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let output = tools.execute(&name, &arguments, memory.as_mut(), &ui, &worker_cancelled);
            let completion = if worker_cancelled.load(Ordering::Acquire) {
                ToolCompletion::Cancelled
            } else {
                ToolCompletion::Function {
                    call_id,
                    name,
                    output,
                }
            };
            let _ = result_tx.send(ToolJobResult {
                tools,
                memory,
                completion,
            });
        });
        self.active_tool = Some(ActiveTool { cancelled, result });
        Ok(())
    }

    fn start_approval_tool(&mut self, approval_id: u64, approved: bool) -> Result<()> {
        if self.active_tool.is_some() {
            anyhow::bail!("tool execution is already in progress");
        }
        let mut tools = self.tools.take().context("tool executor is unavailable")?;
        let mut memory = self
            .memory
            .take()
            .context("memory backend is unavailable")?;
        let ui = self.ui.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (result_tx, result) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = tools.resolve_ui_approval(
                approval_id,
                approved,
                memory.as_mut(),
                &ui,
                &worker_cancelled,
            );
            let completion = if worker_cancelled.load(Ordering::Acquire) {
                ToolCompletion::Cancelled
            } else {
                ToolCompletion::Approval {
                    approval_id,
                    approved,
                    result,
                }
            };
            let _ = result_tx.send(ToolJobResult {
                tools,
                memory,
                completion,
            });
        });
        self.active_tool = Some(ActiveTool { cancelled, result });
        Ok(())
    }

    fn finish_active_tool(&mut self) {
        let Some(active) = self.active_tool.take() else {
            return;
        };
        let result = match active.result.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.active_tool = Some(active);
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.emit_error(&anyhow::anyhow!("tool worker stopped unexpectedly"));
                return;
            }
        };
        self.tools = Some(result.tools);
        self.memory = Some(result.memory);
        self.flush_pending_memory_turns();

        match result.completion {
            ToolCompletion::Function {
                call_id,
                name,
                output,
            } => {
                if let Err(error) = self.append_thread_record(&ThreadRecord::Tool {
                    name,
                    summary: output.chars().take(200).collect(),
                    at_ms: threads::now_ms(),
                }) {
                    self.emit_error(&error);
                }
                if let Err(error) = self
                    .send(ClientEvent::ConversationItemCreate {
                        item: ConversationItem::FunctionCallOutput { call_id, output },
                        previous_item_id: None,
                    })
                    .and_then(|()| self.send(ClientEvent::ResponseCreate { response: None }))
                {
                    self.emit_error(&error);
                } else {
                    self.response_active = true;
                    let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Thinking));
                }
            }
            ToolCompletion::Approval {
                approval_id,
                approved,
                result,
            } => match result {
                Ok(result) => {
                    self.last_activity = Instant::now();
                    if let Err(error) = self
                        .send(approval_context_event(approval_id, approved, &result))
                        .and_then(|()| {
                            if !self.response_active {
                                self.send(ClientEvent::ResponseCreate { response: None })?;
                                self.response_active = true;
                            }
                            Ok(())
                        })
                    {
                        self.emit_error(&error);
                    }
                }
                Err(error) => self.emit_error(&error),
            },
            ToolCompletion::Cancelled => {
                let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Listening));
            }
        }
    }

    fn cancel_active_tool(&self) {
        if let Some(active) = &self.active_tool {
            active.cancelled.store(true, Ordering::Release);
        }
    }

    fn record_memory_turn(&mut self, role: Role, text: &str) {
        if let Some(memory) = self.memory.as_mut() {
            memory.record_turn(role, text);
            return;
        }
        if self.pending_memory_turns.len() == MAX_TRANSCRIPT_TURNS {
            self.pending_memory_turns.pop_front();
        }
        self.pending_memory_turns.push_back((role, text.to_owned()));
    }

    fn flush_pending_memory_turns(&mut self) {
        let Some(memory) = self.memory.as_mut() else {
            return;
        };
        while let Some((role, text)) = self.pending_memory_turns.pop_front() {
            memory.record_turn(role, &text);
        }
    }

    fn build_summary(&mut self) -> String {
        let mut summary = self
            .memory
            .as_mut()
            .map(|memory| {
                memory.flush();
                memory.session_preamble().unwrap_or_default()
            })
            .unwrap_or_default();
        for (role, text) in self.transcripts.iter().rev().take(SUMMARY_TURNS).rev() {
            if !summary.is_empty() {
                summary.push('\n');
            }
            summary.push_str(match role {
                Role::User => "user: ",
                Role::Assistant => "assistant: ",
            });
            summary.push_str(text);
        }
        summary.chars().take(MAX_SUMMARY_CHARS).collect()
    }

    fn append_thread_record(&self, record: &ThreadRecord) -> Result<bool> {
        match (self.thread_generation, self.thread_id) {
            (Some(generation), Some(thread_id)) => {
                threads::append_record(generation, thread_id, record)
            }
            _ => Ok(false),
        }
    }

    fn record_user_turn(&mut self, text: &str) -> Result<()> {
        self.record_memory_turn(Role::User, text);
        self.push_transcript(Role::User, text.to_owned());
        if self.thread_id.is_some() {
            let at_ms = threads::now_ms();
            self.append_thread_record(&ThreadRecord::Turn {
                role: ThreadRole::User,
                text: text.to_owned(),
                at_ms,
            })?;
            if !self.thread_has_title
                && self.append_thread_record(&ThreadRecord::Title {
                    text: threads::thread_title(text),
                    at_ms,
                })?
            {
                self.thread_has_title = true;
            }
        }
        Ok(())
    }

    fn push_transcript(&mut self, role: Role, text: String) {
        if self.transcripts.len() == MAX_TRANSCRIPT_TURNS {
            self.transcripts.pop_front();
        }
        self.transcripts
            .push_back((role, truncate_chars(text, MAX_TRANSCRIPT_CHARS)));
    }

    fn send(&self, event: ClientEvent) -> Result<()> {
        self.realtime
            .as_ref()
            .context("Realtime session is not connected")?
            .send(event)
    }

    fn emit_error(&self, error: &anyhow::Error) {
        eprintln!("voice engine error: {error:#}");
        let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Error(format!(
            "{error:#}"
        ))));
    }

    fn shutdown_resources(&mut self) {
        self.cancel_active_tool();
        self.pending_user_content.clear();
        if let Some(realtime) = self.realtime.take() {
            realtime.shutdown();
        }

        let _ = self.audio.stop_and_clear();
        let _ = self.audio.set_mic_enabled(false);
        let summary = self.build_summary();
        if !summary.is_empty() {
            if let Err(error) = self.append_thread_record(&ThreadRecord::Summary {
                text: summary.clone(),
                at_ms: threads::now_ms(),
            }) {
                self.emit_error(&error);
            }
            let _ = self.ui.emit(VoiceUiEvent::SessionSummary { text: summary });
        }
        if let Some(memory) = self.memory.as_mut() {
            memory.flush();
        }
    }
}
fn restore_pending_user_content(pending: &mut VecDeque<InputContent>, unsent: ClientEvent) {
    if let ClientEvent::ConversationItemCreate {
        item: ConversationItem::Message { content, .. },
        ..
    } = unsent
    {
        for item in content.into_iter().rev() {
            pending.push_front(item);
        }
    }
}

fn truncate_chars(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text
    } else {
        text.chars().take(max_chars).collect()
    }
}

fn queue_pending_user_content(pending: &mut VecDeque<InputContent>, content: InputContent) {
    let content = match content {
        InputContent::InputText { text } => InputContent::InputText {
            text: truncate_chars(text, MAX_USER_TEXT_CHARS),
        },
        image @ InputContent::InputImage { .. } => image,
    };
    debug_assert!(pending.len() < MAX_PENDING_USER_ITEMS);
    pending.push_back(content);
}

fn queue_narration(narration: &mut VecDeque<ClientEvent>, event: ClientEvent) {
    if narration.len() == MAX_NARRATION_ITEMS {
        narration.pop_front();
    }
    narration.push_back(event);
}

fn transcription_already_completed(completed: &VecDeque<String>, item_id: Option<&str>) -> bool {
    item_id.is_some_and(|item_id| completed.iter().any(|completed| completed == item_id))
}

fn accept_completed_transcription(
    completed: &mut VecDeque<String>,
    item_id: Option<String>,
) -> bool {
    let Some(item_id) = item_id else {
        return true;
    };
    if transcription_already_completed(completed, Some(&item_id)) {
        return false;
    }
    if completed.len() == MAX_TRANSCRIPT_TURNS {
        completed.pop_front();
    }
    completed.push_back(item_id);
    true
}

#[derive(Clone, Copy)]
struct MicrophoneActivity {
    response_active: bool,
    output_quiet: bool,
    playback_active: bool,
}

fn microphone_streaming_allowed(full_duplex: bool, activity: MicrophoneActivity) -> bool {
    full_duplex || activity.response_active || (!activity.playback_active && activity.output_quiet)
}

fn response_done_successful(status: Option<&str>) -> bool {
    status.is_none_or(|status| status == "completed")
}

pub(crate) fn effective_idle_timeout(configured_secs: u32) -> Option<Duration> {
    (configured_secs > 0).then(|| Duration::from_secs(u64::from(configured_secs.max(60))))
}

pub(crate) fn session_roll_due(
    connected_at: Option<Instant>,
    now: Instant,
    input_tokens: u64,
) -> bool {
    input_tokens >= SESSION_ROLL_INPUT_TOKENS
        || connected_at
            .is_some_and(|connected| now.saturating_duration_since(connected) >= SESSION_ROLL_AGE)
}

pub(crate) fn narration_ready(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= NARRATION_INTERVAL)
}

fn instructions_with_context(context: &AssistantContext, prior: Option<&str>) -> Result<String> {
    let mut instructions = BASE_INSTRUCTIONS.to_owned();
    instructions.push_str("\n\n");
    instructions.push_str(&location_block(context));
    if let Some(operator_instructions) = context
        .instructions
        .as_deref()
        .map(str::trim)
        .filter(|instructions| !instructions.is_empty())
    {
        instructions.push_str("\n\n## Operator instructions\n");
        instructions.push_str(operator_instructions);
    }
    instructions.push_str(&earlier_threads_block(context)?);
    if let Some(prior) = prior {
        instructions.push_str("\n\n## Prior context\n");
        instructions.push_str(prior);
    }
    Ok(instructions)
}

fn earlier_threads_block(context: &AssistantContext) -> Result<String> {
    let Some(workspace_id) = context.workspace_id else {
        return Ok(String::new());
    };
    let lines = threads::list_threads()?
        .into_iter()
        .filter(|thread| thread.workspace_id == Some(workspace_id))
        .filter(|thread| Some(thread.thread_id) != context.pane_id)
        .take(5)
        .map(|thread| {
            let title = thread
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "Untitled thread".to_owned());
            format!("- {title} ({})", thread.thread_id)
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(
            "\n\n## Earlier threads (read with read_thread)\n{}",
            lines.join("\n")
        ))
    }
}

/// Formats the "where this assistant is planted" block injected into the
/// session instructions.
pub(crate) fn location_block(context: &AssistantContext) -> String {
    let title = if context.workspace_title.is_empty() {
        "untitled"
    } else {
        &context.workspace_title
    };
    let working_dir = context
        .working_dir
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .unwrap_or("not set");
    match (context.workspace_id, context.workspace_kind) {
        (Some(id), WorkspaceKind::Workstation) => format!(
            "## Where you live\nWorkstation: '{title}' (id {id}). Working directory: \
             {working_dir}. This workstation is already attached; create tabs, worktrees, and \
             agents there by default and pass its id/directory to tools without asking."
        ),
        (Some(id), WorkspaceKind::Assistant) => format!(
            "## Where you live\nAssistant workspace: '{title}' (id {id}). Conversation working \
             directory: {working_dir}. This workspace only holds assistant threads and cannot \
             host terminal, project, or worktree tabs. Call list_workstations, choose a \
             kind=workstation target, call attach_project, then pass that workstation id to \
             terminal, project, and worktree tools."
        ),
        (None, _) => format!(
            "## Where you live\nWorkspace: unattached. Conversation working directory: \
             {working_dir}. Call list_workstations, choose a kind=workstation target, and call \
             attach_project before terminal, project, or worktree tools; if none exists, call \
             create_workstation."
        ),
    }
}

fn notification_model_events(_notification: &SessionNotification) -> Vec<ClientEvent> {
    // Terminal notifications originate in OSC output controlled by the process
    // running in the pane. Keep them in the trusted local UI only: no part of
    // the payload is ever promoted into the model conversation.
    Vec::new()
}

fn approval_context_event(
    approval_id: u64,
    approved: bool,
    result: &serde_json::Value,
) -> ClientEvent {
    let payload = serde_json::json!({
        "approval_id": approval_id,
        "approved": approved,
        "result": result,
    });
    untrusted_context_event("ui_approval_result", &payload.to_string())
}

fn untrusted_context_event(kind: &str, payload: &str) -> ClientEvent {
    ClientEvent::ConversationItemCreate {
        item: ConversationItem::Message {
            role: ConversationRole::User,
            content: vec![InputContent::InputText {
                text: format!("<{kind} untrusted=\"true\">\n{payload}\n</{kind}>"),
            }],
        },
        previous_item_id: None,
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
