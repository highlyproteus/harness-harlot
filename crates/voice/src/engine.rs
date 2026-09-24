use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use crate::audio::{AudioInputEvent, AudioSystem};
use crate::realtime::{
    ClientEvent, ConversationItem, ConversationRole, InputContent, RealtimeHandle, RealtimeInbound,
    ServerEvent, SessionConfig,
};
use crate::{
    AssistantContext, EngineState, VoiceCommand, VoiceEngineHandle, VoiceSettings, VoiceUiEvent,
    VoiceUiSender,
};
use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

const LOOP_SLEEP: Duration = Duration::from_millis(10);
const MAX_CRITICAL_UI_EVENTS_PER_REALTIME_INBOUND: usize = 8;

const MIC_LEVEL_INTERVAL: Duration = Duration::from_millis(100);
const HALF_DUPLEX_RELEASE_DELAY: Duration = Duration::from_millis(500);
const REALTIME_INBOUND_CAPACITY: usize = 256;
const MAX_TRANSCRIPTION_IDS: usize = 100;
const MAX_RELAY_CHARS: usize = 8 * 1024;
const MAX_PENDING_RELAYS: usize = 8;

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

const BASE_INSTRUCTIONS: &str = "You are the voice of Harness Harlot. A separate orchestrator does all the work; you only relay. When you receive a message starting with \"[Orchestrator update]\", tell the user what it says in one or two natural spoken sentences, keeping any question the orchestrator asks. Never claim to have done anything yourself and never invent progress. If the user speaks, stay silent; their words are forwarded to the orchestrator automatically.";

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
    let join = std::thread::Builder::new()
        .name("hh-voice-engine".to_owned())
        .spawn(
            move || match VoiceEngine::new(settings, context, ui.clone(), command_rx) {
                Ok(mut engine) => engine.run(),
                Err(error) => {
                    eprintln!("voice engine failed to start: {error:#}");
                    let _ = ui.emit(VoiceUiEvent::State(EngineState::Error(format!(
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
    ui: VoiceUiSender,
    command_rx: Receiver<VoiceCommand>,
    realtime_tx: SyncSender<RealtimeInbound>,
    realtime_rx: Receiver<RealtimeInbound>,
    realtime: Option<RealtimeHandle>,
    audio: AudioSystem,
    completed_input_transcriptions: VecDeque<String>,
    pending_relays: VecDeque<(Option<u64>, String)>,
    response_active: bool,
    speaking_entry: Option<u64>,
    user_speaking: bool,
    mic_consent: MicrophoneConsent,
    speaker_muted: bool,
    suspended: bool,
    connected_at: Option<Instant>,
    last_activity: Instant,
    last_output_audio: Option<Instant>,
    last_mic_level: Instant,
    last_playback_progress: Instant,
}

impl VoiceEngine {
    fn new(
        settings: VoiceSettings,
        context: AssistantContext,
        ui: VoiceUiSender,
        command_rx: Receiver<VoiceCommand>,
    ) -> Result<Self> {
        let _ = ui.emit(VoiceUiEvent::State(EngineState::Connecting));
        let audio = AudioSystem::start()?;
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
            realtime_tx,
            realtime_rx,
            realtime,
            audio,
            completed_input_transcriptions: VecDeque::with_capacity(MAX_TRANSCRIPTION_IDS),
            pending_relays: VecDeque::with_capacity(MAX_PENDING_RELAYS),
            response_active: false,
            speaking_entry: None,
            user_speaking: false,
            mic_consent: MicrophoneConsent::default(),
            speaker_muted: false,
            suspended: false,
            connected_at: None,
            last_activity: now,
            last_output_audio: None,
            last_mic_level: now.checked_sub(MIC_LEVEL_INTERVAL).unwrap_or(now),
            last_playback_progress: now.checked_sub(MIC_LEVEL_INTERVAL).unwrap_or(now),
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
                Ok(VoiceCommand::RelayAssistantText { entry, text }) => {
                    self.queue_relay(entry, text);
                    self.last_activity = Instant::now();
                    if self.suspended {
                        self.resume()?;
                    }
                    self.flush_relay_queue();
                }
                Ok(VoiceCommand::BargeIn) => self.barge_in(true)?,

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
                    let _ = self.ui.emit(VoiceUiEvent::Notice {
                        category: "audio.error".to_owned(),
                        message: error,
                    });
                }
            }
        }
    }

    fn handle_realtime(&mut self, inbound: RealtimeInbound) -> Result<()> {
        match inbound {
            RealtimeInbound::Connected => {
                let instructions = format!(
                    "{BASE_INSTRUCTIONS}\nAssistant: {}",
                    self.context.workspace_title
                );
                self.send(ClientEvent::SessionUpdate {
                    session: Box::new(SessionConfig::new(
                        instructions,
                        self.settings.voice.clone(),
                    )),
                })?;
                self.connected_at = Some(Instant::now());
                self.suspended = false;
                let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Listening));
                self.flush_relay_queue();
            }
            RealtimeInbound::Disconnected(error) => {
                let _ = self.ui.emit(VoiceUiEvent::Notice {
                    category: "realtime.reconnect".to_owned(),
                    message: error,
                });
                self.connected_at = None;
                self.response_active = false;
                self.speaking_entry = None;
                let _ = self.audio.stop_and_clear();
                if !self.suspended {
                    let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Connecting));
                }
            }
            RealtimeInbound::Warning(error) => {
                let _ = self.ui.emit(VoiceUiEvent::Notice {
                    category: "realtime.warning".to_owned(),
                    message: error,
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
            | ServerEvent::FunctionCallArgumentsDone { .. }
            | ServerEvent::Unknown => {}
            ServerEvent::Error { error } => {
                let code = error.code.as_deref().unwrap_or(&error.error_type);
                let _ = self.ui.emit(VoiceUiEvent::Notice {
                    category: "realtime.error".to_owned(),
                    message: format!("{code}: {}", error.message),
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
                    let _ = self.ui.emit(VoiceUiEvent::Notice {
                        category: "realtime.response".to_owned(),
                        message: format!("response ended with status {status}"),
                    });
                }
                if let Some(usage) = response.usage {
                    let _ = self.ui.emit(VoiceUiEvent::Usage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                    });
                }
                let state = if self.audio.playback_active() {
                    EngineState::Speaking
                } else {
                    EngineState::Listening
                };
                let _ = self.ui.emit(VoiceUiEvent::State(state));
                self.speaking_entry = None;
                self.flush_relay_queue();
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
                    entry: self.speaking_entry,
                    text: delta,
                    final_: false,
                });
            }
            ServerEvent::OutputTranscriptDone { transcript, .. } => {
                let _ = self.ui.emit(VoiceUiEvent::AssistantTranscript {
                    entry: self.speaking_entry,
                    text: transcript,
                    final_: true,
                });
            }
        }
        Ok(())
    }

    fn tick(&mut self) {
        // A suspended engine is dormant by design. Only an explicit Resume or
        // a queued orchestrator update wakes it.
        if self.suspended {
            return;
        }
        let now = Instant::now();
        if now.saturating_duration_since(self.last_playback_progress) >= MIC_LEVEL_INTERVAL {
            self.last_playback_progress = now;
            if let Some((played_ms, total_ms)) = self.audio.playback_progress() {
                let _ = self.ui.emit(VoiceUiEvent::PlaybackProgress {
                    played_ms,
                    total_ms,
                });
            }
        }
        self.flush_relay_queue();
        if let Some(timeout) = effective_idle_timeout(self.settings.idle_timeout_secs)
            && !self.suspended
            && !self.response_active
            && !self.user_speaking
            && !self.audio.playback_active()
            && now.saturating_duration_since(self.last_activity) >= timeout
            && let Err(error) = self.suspend()
        {
            self.emit_error(&error);
        }
    }

    fn queue_relay(&mut self, entry: Option<u64>, text: String) {
        if self.pending_relays.len() == MAX_PENDING_RELAYS {
            self.pending_relays.pop_front();
        }
        self.pending_relays
            .push_back((entry, truncate_chars(text, MAX_RELAY_CHARS)));
    }

    fn flush_relay_queue(&mut self) {
        if !relay_flush_allowed(
            self.response_active,
            self.user_speaking,
            self.connected_at.is_some(),
        ) {
            return;
        }
        let Some((entry, text)) = self.pending_relays.front().cloned() else {
            return;
        };
        for event in orchestrator_relay_events(text) {
            if self.send(event).is_err() {
                return;
            }
        }
        self.pending_relays.pop_front();
        self.speaking_entry = entry;
        self.response_active = true;
        self.last_activity = Instant::now();
        let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Thinking));
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
        if let Some(realtime) = self.realtime.take() {
            realtime.shutdown();
        }
        self.audio.set_mic_enabled(false)?;
        self.suspended = true;
        self.connected_at = None;
        self.clear_user_speaking();
        let _ = self.ui.emit(VoiceUiEvent::State(EngineState::Suspended));
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if !self.suspended && self.realtime.is_some() {
            return Ok(());
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
        self.pending_relays.clear();
        if let Some(realtime) = self.realtime.take() {
            realtime.shutdown();
        }
        let _ = self.audio.stop_and_clear();
        let _ = self.audio.set_mic_enabled(false);
    }
}

fn truncate_chars(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text
    } else {
        text.chars().take(max_chars).collect()
    }
}

fn orchestrator_relay_events(text: String) -> [ClientEvent; 2] {
    let text = format!(
        "[Orchestrator update]\n{}",
        truncate_chars(text, MAX_RELAY_CHARS)
    );
    [
        ClientEvent::ConversationItemCreate {
            item: ConversationItem::Message {
                role: ConversationRole::User,
                content: vec![InputContent::InputText { text }],
            },
            previous_item_id: None,
        },
        ClientEvent::ResponseCreate { response: None },
    ]
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
    if completed.len() == MAX_TRANSCRIPTION_IDS {
        completed.pop_front();
    }
    completed.push_back(item_id);
    true
}

const fn relay_flush_allowed(response_active: bool, user_speaking: bool, connected: bool) -> bool {
    !response_active && !user_speaking && connected
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

#[cfg(test)]
fn completed_transcription_provider_events() -> Vec<ClientEvent> {
    Vec::new()
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
