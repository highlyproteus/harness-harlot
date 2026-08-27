mod audio;
mod engine;
mod harness;
mod memory;
mod realtime;
mod settings;
pub mod threads;
mod tools;

use std::thread::JoinHandle;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;

pub use settings::{HonchoSettings, VoiceSettings};
pub use threads::{
    Thread, ThreadGeneration, ThreadRecord, ThreadRole, ThreadSummary, adopt_thread, append_record,
    list_threads, read_summary, read_thread,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineState {
    Connecting,
    Listening,
    Thinking,
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
    ToolCallStarted {
        name: String,
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
    pub(crate) command_tx: std::sync::mpsc::SyncSender<VoiceCommand>,
    pub(crate) join: Option<JoinHandle<()>>,
}

impl VoiceEngineHandle {
    pub fn send(&self, command: VoiceCommand) {
        let _ = self.try_send(command);
    }

    /// Queues a command without blocking the caller.
    #[must_use]
    pub fn try_send(&self, command: VoiceCommand) -> bool {
        self.command_tx.try_send(command).is_ok()
    }

    /// Whether the engine thread has already exited (startup failure or
    /// shutdown). A finished engine never recovers; respawn instead.
    pub fn is_finished(&self) -> bool {
        self.join
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    pub fn shutdown(mut self) {
        let _ = self.command_tx.try_send(VoiceCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !join.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if join.is_finished() {
                let _ = join.join();
            }
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
    pub workspace_kind: hh_protocol::WorkspaceKind,
    pub working_dir: Option<String>,
    pub instructions: Option<String>,
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
