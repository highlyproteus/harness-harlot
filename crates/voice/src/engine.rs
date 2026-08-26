use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::channel::mpsc::UnboundedSender;
use hh_protocol::{NotificationKind, SessionNotification};

use crate::audio::{AudioInputEvent, AudioSystem};
use crate::memory::{MemoryBackend, NullBackend, Role, backend};
use crate::realtime::{
    ClientEvent, ConversationItem, ConversationRole, InputContent, RealtimeHandle, RealtimeInbound,
    ServerEvent, SessionConfig,
};
use crate::tools::{ToolExecutor, tool_schemas};
use crate::{
    AssistantContext, EngineState, VoiceCommand, VoiceEngineHandle, VoiceSettings, VoiceUiEvent,
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const LOOP_SLEEP: Duration = Duration::from_millis(10);
const NARRATION_INTERVAL: Duration = Duration::from_secs(2);
const MIC_LEVEL_INTERVAL: Duration = Duration::from_millis(100);
const BARGE_IN_RMS: f32 = 0.05;
const BARGE_IN_CHUNKS: u8 = 3;
const BARGE_IN_PREROLL: usize = 6;
const SESSION_ROLL_AGE: Duration = Duration::from_mins(50);
const SESSION_ROLL_INPUT_TOKENS: u64 = 90_000;
const MAX_TRANSCRIPT_TURNS: usize = 100;
const SUMMARY_TURNS: usize = 15;
const MAX_SUMMARY_CHARS: usize = 2_000;

const BASE_INSTRUCTIONS: &str = "You are the Harness Harlot voice assistant — a hands-on project manager for a terminal workstation app. You manage workstations, tabs, git worktrees, and coding-agent CLIs (omp, hermes, codex, claude) on the user's behalf using your tools. Keep replies to one short sentence unless the user asks for detail; the only exception: before a long-running tool sequence, say a 3-6 word preamble like \"on it — creating that worktree\". Don't volunteer your capabilities unprompted and don't repeat yourself. Act, don't interrogate: when the user asks to open or create a terminal tab, call open_terminal_tab immediately with the current workstation id and never claim success without its tool result. When the user asks you to run a shell command in a tab you created, call send_input immediately; infer directories from working_dir and project_dir in list_workstations or use ~-relative paths the shell expands — do not ask for exact paths. You cannot guess filesystem paths: when the user names a project or directory whose exact path you have not seen in a tool result, call find_directory with the spoken name (or list_directory to browse from home) and use a returned path for open_project_tab, create_workstation, or create_worktree_tab. If a directory tool errors with a list of existing directories, pick the correct one from that list and retry instead of reporting failure. If a command fails, read_pane and report one short line. If a tool errors, report the error briefly and suggest the closest fix. Never invent tool results. When a tool returns status needs_approval, ask the user aloud exactly what you want to do and treat informal affirmatives (\"yeah\", \"sure\", \"go ahead\", \"mm-hm\") as yes for approve_action; anything ambiguous is not yes, and never tell the user to click buttons. Exception: when the result contains requires_ui_click, you cannot approve it — tell the user to click Approve in the pane if they really want it. Never try to close or delete your own assistant tab or workstation. If the user says stop or cancel, stop talking, start no new tool calls, and deny any pending approval via approve_action with approved false. Proactively report [event] messages about agents needing approval or input, naming the workstation and tab; ignore other events unless asked. When the user names a project, call attach_project before any other tool that needs that project — not on mere mentions. To start an agent on a task: create_worktree_tab (or open_project_tab), launch_agent, then send_input with the task text.";

pub(crate) fn spawn(
    mut settings: VoiceSettings,
    context: AssistantContext,
    ui: UnboundedSender<VoiceUiEvent>,
) -> Result<VoiceEngineHandle> {
    if settings.api_key.trim().is_empty()
        && let Ok(api_key) = std::env::var("HH_OPENAI_API_KEY")
    {
        settings.api_key = api_key;
    }
    if settings.api_key.trim().is_empty() {
        anyhow::bail!("OpenAI API key is empty");
    }
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let join = std::thread::Builder::new()
        .name("hh-voice-engine".to_owned())
        .spawn(
            move || match VoiceEngine::new(settings, context, ui.clone(), command_rx) {
                Ok(mut engine) => engine.run(),
                Err(error) => {
                    eprintln!("voice engine failed to start: {error:#}");
                    let _ = ui.unbounded_send(VoiceUiEvent::State(EngineState::Error(format!(
                        "{error:#}"
                    ))));
                }
            },
        )
        .context("spawn voice engine thread")?;
    Ok(VoiceEngineHandle {
        command_tx,
        join: Some(join),
    })
}

#[allow(clippy::struct_excessive_bools)]
struct VoiceEngine {
    settings: VoiceSettings,
    context: AssistantContext,
    ui: UnboundedSender<VoiceUiEvent>,
    command_rx: Receiver<VoiceCommand>,
    realtime_tx: Sender<RealtimeInbound>,
    realtime_rx: Receiver<RealtimeInbound>,
    realtime: Option<RealtimeHandle>,
    audio: AudioSystem,
    tools: ToolExecutor,
    memory: Box<dyn MemoryBackend>,
    transcripts: VecDeque<(Role, String)>,
    completed_input_transcriptions: VecDeque<String>,
    pending_user_content: VecDeque<InputContent>,
    narration: VecDeque<String>,
    response_active: bool,
    user_speaking: bool,
    barge_streak: u8,
    barge_preroll: VecDeque<Vec<i16>>,
    mic_enabled: bool,
    speaker_muted: bool,
    suspended: bool,
    connected_at: Option<Instant>,
    reconnect_roll: bool,
    last_activity: Instant,
    last_poll: Instant,
    last_narration: Option<Instant>,
    last_mic_level: Instant,
    last_playback_progress: Instant,
    pending_instructions: Option<String>,
}

impl VoiceEngine {
    fn new(
        settings: VoiceSettings,
        context: AssistantContext,
        ui: UnboundedSender<VoiceUiEvent>,
        command_rx: Receiver<VoiceCommand>,
    ) -> Result<Self> {
        let _ = ui.unbounded_send(VoiceUiEvent::State(EngineState::Connecting));
        let mut tools = ToolExecutor::connect()?;
        if let Some(workspace_id) = context.workspace_id {
            tools.attach_workspace(workspace_id);
        }
        if let Some(pane_id) = context.pane_id {
            tools.set_own_pane(pane_id);
        }
        let pending_instructions = context
            .prior_context
            .as_deref()
            .filter(|summary| !summary.is_empty())
            .map(str::to_owned);
        let audio = AudioSystem::start()?;
        let memory: Box<dyn MemoryBackend> = match backend(settings.honcho.clone()) {
            Ok(memory) => memory,
            Err(error) => {
                let _ = ui.unbounded_send(VoiceUiEvent::ToolCall {
                    name: "memory.error".to_owned(),
                    summary: format!("Honcho disabled: {error:#}"),
                });
                Box::<NullBackend>::default()
            }
        };
        let (realtime_tx, realtime_rx) = std::sync::mpsc::channel();
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
            realtime_tx,
            realtime_rx,
            realtime,
            audio,
            tools,
            memory,
            transcripts: VecDeque::with_capacity(MAX_TRANSCRIPT_TURNS),
            completed_input_transcriptions: VecDeque::with_capacity(MAX_TRANSCRIPT_TURNS),
            pending_user_content: VecDeque::new(),
            narration: VecDeque::new(),
            response_active: false,
            user_speaking: false,
            barge_streak: 0,
            barge_preroll: VecDeque::new(),
            mic_enabled: true,
            speaker_muted: false,
            suspended: false,
            connected_at: None,
            reconnect_roll: false,
            last_activity: now,
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
                Ok(VoiceCommand::Shutdown) | Err(TryRecvError::Disconnected) => return Ok(true),
                Ok(VoiceCommand::SetMicEnabled(enabled)) => {
                    self.mic_enabled = enabled;
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
                    self.pending_user_content
                        .push_back(InputContent::InputText { text });
                    self.last_activity = Instant::now();
                    if self.suspended {
                        self.resume()?;
                    }
                }
                Ok(VoiceCommand::SendUserImage { data_url }) => {
                    self.pending_user_content
                        .push_back(InputContent::InputImage {
                            image_url: data_url,
                        });
                    self.last_activity = Instant::now();
                    if self.suspended {
                        self.resume()?;
                    }
                }
                Ok(VoiceCommand::BargeIn) => self.barge_in(true)?,
                Ok(VoiceCommand::Approve { approval_id }) => {
                    self.resolve_ui_approval(approval_id, true)?;
                }
                Ok(VoiceCommand::Deny { approval_id }) => {
                    self.resolve_ui_approval(approval_id, false)?;
                }
                Ok(VoiceCommand::Suspend) => self.suspend()?,
                Ok(VoiceCommand::Resume) => self.resume()?,
                Err(TryRecvError::Empty) => return Ok(false),
            }
        }
    }

    fn drain_realtime(&mut self) {
        while let Ok(event) = self.realtime_rx.try_recv() {
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
                        let _ = self.ui.unbounded_send(VoiceUiEvent::MicLevel(chunk.rms));
                    }
                    let streaming = !self.suspended
                        && self.mic_enabled
                        && self.connected_at.is_some()
                        && (self.settings.full_duplex || !self.audio.playback_active());
                    if streaming {
                        self.barge_streak = 0;
                        self.barge_preroll.clear();
                        self.send_mic_chunk(&chunk.samples);
                    } else if !self.suspended && self.mic_enabled && self.connected_at.is_some() {
                        if self.barge_preroll.len() == BARGE_IN_PREROLL {
                            self.barge_preroll.pop_front();
                        }
                        self.barge_preroll.push_back(chunk.samples);
                        self.barge_streak = barge_in_streak(self.barge_streak, chunk.rms);
                        if self.barge_streak >= BARGE_IN_CHUNKS {
                            if let Err(error) = self.barge_in(true) {
                                self.emit_error(&error);
                            }
                            for samples in std::mem::take(&mut self.barge_preroll) {
                                self.send_mic_chunk(&samples);
                            }
                            self.barge_streak = 0;
                        }
                    }
                }
                AudioInputEvent::Error(error) => {
                    let _ = self.ui.unbounded_send(VoiceUiEvent::ToolCall {
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
                        self.memory.session_preamble()
                    }
                });
                let instructions = instructions_with_context(&self.context, prior.as_deref());
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
                let _ = self
                    .ui
                    .unbounded_send(VoiceUiEvent::State(EngineState::Listening));
            }
            RealtimeInbound::Disconnected(error) => {
                let _ = self.ui.unbounded_send(VoiceUiEvent::ToolCall {
                    name: "realtime.reconnect".to_owned(),
                    summary: error,
                });
                self.clear_user_speaking();
                self.connected_at = None;
                self.reconnect_roll = true;
                if !self.suspended {
                    let _ = self
                        .ui
                        .unbounded_send(VoiceUiEvent::State(EngineState::Connecting));
                }
            }
            RealtimeInbound::Warning(error) => {
                let _ = self.ui.unbounded_send(VoiceUiEvent::ToolCall {
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
                let _ = self.ui.unbounded_send(VoiceUiEvent::ToolCall {
                    name: "realtime.error".to_owned(),
                    summary: format!("{code}: {}", error.message),
                });
            }
            ServerEvent::SpeechStarted { .. } => {
                self.user_speaking = true;
                let _ = self
                    .ui
                    .unbounded_send(VoiceUiEvent::UserSpeech { active: true });
                self.last_activity = Instant::now();
                if self.settings.full_duplex {
                    self.barge_in(false)?;
                }
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
                let _ = self.ui.unbounded_send(VoiceUiEvent::UserTranscript {
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
                self.memory.record_turn(Role::User, &transcript);
                self.push_transcript(Role::User, transcript.clone());
                let _ = self.ui.unbounded_send(VoiceUiEvent::UserTranscript {
                    text: transcript,
                    final_: true,
                });
            }
            ServerEvent::ResponseCreated { .. } => {
                self.last_activity = Instant::now();
                self.response_active = true;
            }
            ServerEvent::ResponseDone { response } => {
                self.response_active = false;
                self.audio.finish_output()?;
                if let Some(usage) = response.usage {
                    let _ = self.ui.unbounded_send(VoiceUiEvent::Usage {
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
                let _ = self.ui.unbounded_send(VoiceUiEvent::State(state));
            }
            ServerEvent::OutputAudioDelta { item_id, delta } => {
                self.last_activity = Instant::now();
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
                let _ = self
                    .ui
                    .unbounded_send(VoiceUiEvent::State(EngineState::Speaking));
            }
            ServerEvent::OutputTranscriptDelta { delta, .. } => {
                let _ = self.ui.unbounded_send(VoiceUiEvent::AssistantTranscript {
                    text: delta,
                    final_: false,
                });
            }
            ServerEvent::OutputTranscriptDone { transcript, .. } => {
                self.memory.record_turn(Role::Assistant, &transcript);
                self.push_transcript(Role::Assistant, transcript.clone());
                let _ = self.ui.unbounded_send(VoiceUiEvent::AssistantTranscript {
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
                    .unbounded_send(VoiceUiEvent::State(EngineState::ToolRunning));
                let output = self
                    .tools
                    .execute(&name, &arguments, self.memory.as_mut(), &self.ui);
                self.send(ClientEvent::ConversationItemCreate {
                    item: ConversationItem::FunctionCallOutput { call_id, output },
                    previous_item_id: None,
                })?;
                self.send(ClientEvent::ResponseCreate { response: None })?;
                self.response_active = true;
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
        if now.saturating_duration_since(self.last_playback_progress) >= MIC_LEVEL_INTERVAL {
            self.last_playback_progress = now;
            if let Some((played_ms, total_ms)) = self.audio.playback_progress() {
                let _ = self.ui.unbounded_send(VoiceUiEvent::PlaybackProgress {
                    played_ms,
                    total_ms,
                });
            }
        }
        if now.saturating_duration_since(self.last_poll) >= POLL_INTERVAL {
            self.last_poll = now;
            match self.tools.poll_updates() {
                Ok(notifications) => {
                    for notification in notifications {
                        if self.tools.notification_is_attached(&notification) {
                            self.narration.push_back(narration_text(&notification));
                        }
                    }
                }
                Err(error) => {
                    let _ = self.ui.unbounded_send(VoiceUiEvent::ToolCall {
                        name: "status.poll".to_owned(),
                        summary: format!("{error:#}"),
                    });
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
            if let Err(error) = self.inject_system(&event, true) {
                self.emit_error(&error);
            }
        }
        if self.connected_at.is_some()
            && !self.response_active
            && !self.user_speaking
            && !self.pending_user_content.is_empty()
            && let Some(realtime) = self.realtime.as_ref()
        {
            let content = std::mem::take(&mut self.pending_user_content)
                .into_iter()
                .collect::<Vec<_>>();
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
            match realtime.send_recoverable(event) {
                Ok(()) => {
                    for text in user_text {
                        self.memory.record_turn(Role::User, &text);
                        self.push_transcript(Role::User, text);
                    }
                    if let Err(error) = self.send(ClientEvent::ResponseCreate { response: None }) {
                        self.emit_error(&error);
                    } else {
                        self.response_active = true;
                        self.last_activity = now;
                    }
                }
                Err((error, unsent)) => {
                    restore_pending_user_content(&mut self.pending_user_content, unsent);
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
            && !self.tools.has_pending_approvals()
            && now.saturating_duration_since(self.last_activity) >= timeout
            && let Err(error) = self.suspend()
        {
            self.emit_error(&error);
        }
    }

    fn resolve_ui_approval(&mut self, approval_id: u64, approved: bool) -> Result<()> {
        let result = self
            .tools
            .resolve_approval(approval_id, approved, true, &self.ui)?;
        self.last_activity = Instant::now();
        self.inject_system(
            &format!(
                "[system] approval {approval_id} {} via UI; result: {}",
                if approved { "approved" } else { "denied" },
                serde_json::to_string(&result).unwrap_or_else(|_| "null".to_owned())
            ),
            !self.response_active,
        )
    }

    fn inject_system(&mut self, text: &str, create_response: bool) -> Result<()> {
        self.send(ClientEvent::ConversationItemCreate {
            item: ConversationItem::Message {
                role: ConversationRole::System,
                content: vec![InputContent::InputText {
                    text: text.to_owned(),
                }],
            },
            previous_item_id: None,
        })?;
        if create_response {
            self.send(ClientEvent::ResponseCreate { response: None })?;
            self.response_active = true;
        }
        Ok(())
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
            let _ = self
                .ui
                .unbounded_send(VoiceUiEvent::UserSpeech { active: false });
        }
    }

    fn barge_in(&mut self, cancel_response: bool) -> Result<()> {
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
        let _ = self
            .ui
            .unbounded_send(VoiceUiEvent::State(EngineState::Listening));
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        if self.suspended {
            return Ok(());
        }
        if let Some(realtime) = self.realtime.take() {
            realtime.shutdown();
        }
        self.audio.set_mic_enabled(false)?;
        self.suspended = true;
        self.connected_at = None;
        let summary = self.build_summary();
        if !summary.is_empty() {
            let _ = self
                .ui
                .unbounded_send(VoiceUiEvent::SessionSummary { text: summary });
        }
        self.clear_user_speaking();
        let _ = self
            .ui
            .unbounded_send(VoiceUiEvent::State(EngineState::Suspended));
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
        self.audio.set_mic_enabled(self.mic_enabled)?;
        self.suspended = false;
        self.last_activity = Instant::now();
        let _ = self
            .ui
            .unbounded_send(VoiceUiEvent::State(EngineState::Connecting));
        Ok(())
    }

    fn roll_session(&mut self, reason: &str) -> Result<()> {
        let summary = self.build_summary();
        self.pending_instructions = (!summary.is_empty()).then_some(summary);
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
        let _ = self.ui.unbounded_send(VoiceUiEvent::ToolCall {
            name: "session.roll".to_owned(),
            summary: reason.to_owned(),
        });
        Ok(())
    }

    fn build_summary(&mut self) -> String {
        self.memory.flush();
        let mut summary = self.memory.session_preamble().unwrap_or_default();
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

    fn push_transcript(&mut self, role: Role, text: String) {
        if self.transcripts.len() == MAX_TRANSCRIPT_TURNS {
            self.transcripts.pop_front();
        }
        self.transcripts.push_back((role, text));
    }

    fn send(&self, event: ClientEvent) -> Result<()> {
        self.realtime
            .as_ref()
            .context("Realtime session is not connected")?
            .send(event)
    }

    fn emit_error(&self, error: &anyhow::Error) {
        eprintln!("voice engine error: {error:#}");
        let _ = self
            .ui
            .unbounded_send(VoiceUiEvent::State(EngineState::Error(format!(
                "{error:#}"
            ))));
    }

    fn shutdown_resources(&mut self) {
        self.pending_user_content.clear();
        if let Some(realtime) = self.realtime.take() {
            realtime.shutdown();
        }

        let _ = self.audio.stop_and_clear();
        let _ = self.audio.set_mic_enabled(false);
        let summary = self.build_summary();
        if !summary.is_empty() {
            let _ = self
                .ui
                .unbounded_send(VoiceUiEvent::SessionSummary { text: summary });
        }
        self.memory.flush();
    }
}
fn restore_pending_user_content(pending: &mut VecDeque<InputContent>, unsent: ClientEvent) {
    if let ClientEvent::ConversationItemCreate {
        item: ConversationItem::Message { content, .. },
        ..
    } = unsent
    {
        pending.extend(content);
    }
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

pub(crate) fn barge_in_streak(streak: u8, rms: f32) -> u8 {
    if rms >= BARGE_IN_RMS {
        streak.saturating_add(1)
    } else {
        0
    }
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

fn instructions_with_context(context: &AssistantContext, prior: Option<&str>) -> String {
    let mut instructions = BASE_INSTRUCTIONS.to_owned();
    instructions.push_str("\n\n");
    instructions.push_str(&location_block(context));
    if let Some(prior) = prior {
        instructions.push_str("\n\n## Prior context\n");
        instructions.push_str(prior);
    }
    instructions
}

/// Formats the "where this assistant is planted" block injected into the
/// session instructions.
pub(crate) fn location_block(context: &AssistantContext) -> String {
    let workstation = match context.workspace_id {
        Some(id) => format!(
            "'{}' (id {id})",
            if context.workspace_title.is_empty() {
                "untitled"
            } else {
                &context.workspace_title
            }
        ),
        None => "'unattached'".to_owned(),
    };
    let working_dir = context
        .working_dir
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .unwrap_or("not set");
    format!(
        "## Where you live\nWorkstation: {workstation}. Working directory: {working_dir}. \
         This workstation is already attached; create tabs, worktrees, and agents there by \
         default and pass its id/directory to tools without asking."
    )
}

fn narration_text(notification: &SessionNotification) -> String {
    let kind = match notification.kind {
        NotificationKind::Completed => "completed",
        NotificationKind::Attention => "attention",
        NotificationKind::Message => "message",
    };
    let message = notification.message.as_deref().unwrap_or("state changed");
    format!(
        "[event] pane '{}' ({}) in '{}': {kind} — {message}",
        notification.pane_title,
        notification.profile.display_name(),
        notification.workspace_title,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    #[test]
    fn unsent_user_content_returns_to_the_pending_queue() {
        let content = vec![
            InputContent::InputText {
                text: "retry me".to_owned(),
            },
            InputContent::InputImage {
                image_url: "data:image/png;base64,AAA".to_owned(),
            },
        ];
        let event = ClientEvent::ConversationItemCreate {
            item: ConversationItem::Message {
                role: ConversationRole::User,
                content: content.clone(),
            },
            previous_item_id: None,
        };
        let mut pending = VecDeque::new();
        restore_pending_user_content(&mut pending, event);
        assert_eq!(pending.into_iter().collect::<Vec<_>>(), content);
    }
    #[test]
    fn repeated_transcription_item_is_emitted_once() {
        let mut completed = VecDeque::new();
        assert!(accept_completed_transcription(
            &mut completed,
            Some("item-1".to_owned())
        ));
        assert!(transcription_already_completed(&completed, Some("item-1")));
        assert!(!accept_completed_transcription(
            &mut completed,
            Some("item-1".to_owned())
        ));
        assert!(accept_completed_transcription(
            &mut completed,
            Some("item-2".to_owned())
        ));
    }

    #[test]
    fn talk_over_requires_three_consecutive_loud_chunks() {
        let mut streak = barge_in_streak(0, BARGE_IN_RMS);
        assert_eq!(streak, 1);
        streak = barge_in_streak(streak, BARGE_IN_RMS - f32::EPSILON);
        assert_eq!(streak, 0);
        for expected in 1..=BARGE_IN_CHUNKS {
            streak = barge_in_streak(streak, BARGE_IN_RMS);
            assert_eq!(streak, expected);
        }
    }

    #[test]
    fn idle_timeout_is_disabled_at_zero_and_floored_at_one_minute() {
        assert_eq!(effective_idle_timeout(0), None);
        assert_eq!(effective_idle_timeout(15), Some(Duration::from_mins(1)));
        assert_eq!(effective_idle_timeout(900), Some(Duration::from_mins(15)));
    }

    #[test]
    fn session_rolls_only_at_declared_age_or_token_threshold() {
        let now = Instant::now();
        assert!(!session_roll_due(None, now, 89_999));
        assert!(session_roll_due(None, now, 90_000));
        assert!(!session_roll_due(
            Some(now.checked_sub(Duration::from_mins(49)).unwrap()),
            now,
            0
        ));
        assert!(session_roll_due(
            Some(now.checked_sub(Duration::from_mins(50)).unwrap()),
            now,
            0
        ));
    }

    #[test]
    fn narration_injection_is_coalesced_to_two_seconds() {
        let now = Instant::now();
        assert!(narration_ready(None, now));
        assert!(!narration_ready(
            Some(now.checked_sub(Duration::from_millis(1_999)).unwrap()),
            now
        ));
        assert!(narration_ready(
            Some(now.checked_sub(Duration::from_secs(2)).unwrap()),
            now
        ));
    }

    #[test]
    fn prior_context_is_appended_without_replacing_base_policy() {
        let context = AssistantContext::default();
        let instructions =
            instructions_with_context(&context, Some("user prefers concise updates"));
        assert!(instructions.starts_with(BASE_INSTRUCTIONS));
        assert!(instructions.contains("## Prior context\nuser prefers concise updates"));
        assert!(instructions.contains("call open_terminal_tab immediately"));
        assert!(instructions.contains("requires_ui_click"));
        assert!(instructions.contains("find_directory"));
    }

    #[test]
    fn location_block_names_workstation_and_directory() {
        let context = AssistantContext {
            workspace_id: Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()),
            pane_id: None,
            workspace_title: "Growth".to_owned(),
            working_dir: Some("/Users/demo/Projects/growth".to_owned()),
            prior_context: None,
        };
        let block = location_block(&context);
        assert!(block.starts_with("## Where you live\n"));
        assert!(block.contains("Workstation: 'Growth' (id 00000000-0000-0000-0000-000000000001)."));
        assert!(block.contains("Working directory: /Users/demo/Projects/growth."));
        assert!(block.contains("already attached"));

        let unattached = location_block(&AssistantContext::default());
        assert!(unattached.contains("Workstation: 'unattached'."));
        assert!(unattached.contains("Working directory: not set."));
    }

    #[test]
    fn instructions_include_the_location_block_before_prior_context() {
        let context = AssistantContext {
            workspace_id: None,
            pane_id: None,
            workspace_title: String::new(),
            working_dir: None,
            prior_context: None,
        };
        let instructions = instructions_with_context(&context, Some("earlier talk"));
        let location_index = instructions
            .find("## Where you live")
            .expect("location block");
        let prior_index = instructions
            .find("## Prior context")
            .expect("prior context");
        assert!(location_index < prior_index);
    }
}
