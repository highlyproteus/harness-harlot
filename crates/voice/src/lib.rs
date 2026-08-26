mod audio;
mod engine;
mod harness;
mod memory;
mod realtime;
mod settings;
mod tools;

use std::thread::JoinHandle;

use futures::channel::mpsc::UnboundedSender;

pub use settings::{HonchoSettings, VoiceSettings};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineState {
    Connecting,
    Listening,
    Speaking,
    ToolRunning,
    Suspended,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum VoiceCommand {
    SetMicEnabled(bool),
    SetSpeakerMuted(bool),
    BargeIn,
    Approve { approval_id: u64 },
    Deny { approval_id: u64 },
    Suspend,
    Resume,
    SendUserText(String),
    SendUserImage { data_url: String },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VoiceUiEvent {
    State(EngineState),
    UserSpeech {
        active: bool,
    },
    UserTranscript {
        text: String,
        final_: bool,
    },
    AssistantTranscript {
        text: String,
        final_: bool,
    },
    PlaybackProgress {
        played_ms: u64,
        total_ms: u64,
    },
    ToolCall {
        name: String,
        summary: String,
    },
    ApprovalRequested {
        id: u64,
        description: String,
    },
    ApprovalResolved {
        id: u64,
        approved: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    MicLevel(f32),
    SessionSummary {
        text: String,
    },
}

#[derive(Debug)]
pub struct VoiceEngineHandle {
    pub(crate) command_tx: std::sync::mpsc::Sender<VoiceCommand>,
    pub(crate) join: Option<JoinHandle<()>>,
}

impl VoiceEngineHandle {
    pub fn send(&self, command: VoiceCommand) {
        let _ = self.command_tx.send(command);
    }

    /// Whether the engine thread has already exited (startup failure or
    /// shutdown). A finished engine never recovers; respawn instead.
    pub fn is_finished(&self) -> bool {
        self.join
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    pub fn shutdown(mut self) {
        let _ = self.command_tx.send(VoiceCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Where a voice-assistant pane is planted, plus the conversation summary
/// persisted across suspends and app restarts.
#[derive(Clone, Debug, Default)]
pub struct AssistantContext {
    pub workspace_id: Option<uuid::Uuid>,
    pub pane_id: Option<uuid::Uuid>,
    pub workspace_title: String,
    pub working_dir: Option<String>,
    pub prior_context: Option<String>,
}

/// Starts the dedicated voice assistant engine thread.
///
/// # Errors
///
/// Returns an error when the API key is unavailable or the engine thread
/// cannot be created. Device, daemon, and network failures are reported as UI
/// state events after startup.
pub fn spawn_engine(
    settings: VoiceSettings,
    context: AssistantContext,
    ui_tx: UnboundedSender<VoiceUiEvent>,
) -> anyhow::Result<VoiceEngineHandle> {
    engine::spawn(settings, context, ui_tx)
}
