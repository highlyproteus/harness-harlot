mod audio;
mod engine;
mod realtime;
mod settings;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use futures::SinkExt;
use futures::channel::mpsc::Receiver;

pub const VOICE_UI_EVENT_CAPACITY: usize = 64;
const VOICE_UI_DISPATCH_CAPACITY: usize = 256;

pub use settings::VoiceSettings;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineState {
    Connecting,
    Listening,
    Thinking,
    Speaking,
    Suspended,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum VoiceCommand {
    SetMicEnabled(bool),
    SetSpeakerMuted(bool),
    BargeIn,
    Suspend,
    Resume,
    RelayAssistantText { entry: Option<u64>, text: String },
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
        entry: Option<u64>,
        text: String,
        final_: bool,
    },
    PlaybackProgress {
        played_ms: u64,
        total_ms: u64,
    },
    Notice {
        category: String,
        message: String,
    },

    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    MicLevel(f32),
}

impl VoiceUiEvent {
    const fn droppable_delta(&self) -> bool {
        matches!(
            self,
            Self::MicLevel(_)
                | Self::PlaybackProgress { .. }
                | Self::UserTranscript { final_: false, .. }
                | Self::AssistantTranscript { final_: false, .. }
        )
    }
}

/// Bounded engine-to-desktop event producer. High-rate display deltas may be
/// dropped under overload; critical state and recovery events apply
/// backpressure and are never discarded.
#[derive(Clone, Debug)]
pub struct VoiceUiSender {
    critical_tx: std::sync::mpsc::SyncSender<VoiceUiEvent>,
    delta_tx: std::sync::mpsc::SyncSender<VoiceUiEvent>,
    critical_slots: Arc<AtomicUsize>,
}

impl VoiceUiSender {
    pub(crate) fn emit(&self, event: VoiceUiEvent) -> bool {
        if event.droppable_delta() {
            self.delta_tx.try_send(event).is_ok()
        } else {
            if self
                .critical_slots
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |slots| {
                    if slots > 0 { Some(slots - 1) } else { None }
                })
                .is_err()
            {
                return false;
            }
            if self.critical_tx.try_send(event).is_ok() {
                true
            } else {
                self.critical_slots.fetch_add(1, Ordering::Release);
                false
            }
        }
    }

    pub(crate) fn critical_capacity(&self) -> usize {
        self.critical_slots.load(Ordering::Acquire)
    }
}

#[must_use]
/// Creates the bounded engine-to-desktop Voice event channel.
///
/// # Panics
///
/// Panics if the process cannot spawn the dedicated Voice UI dispatcher thread.
pub fn voice_ui_channel() -> (VoiceUiSender, Receiver<VoiceUiEvent>) {
    let (mut desktop_tx, rx) = futures::channel::mpsc::channel(VOICE_UI_EVENT_CAPACITY - 1);
    let (critical_tx, critical_rx) =
        std::sync::mpsc::sync_channel::<VoiceUiEvent>(VOICE_UI_DISPATCH_CAPACITY);
    let (delta_tx, delta_rx) =
        std::sync::mpsc::sync_channel::<VoiceUiEvent>(VOICE_UI_EVENT_CAPACITY);
    let critical_slots = Arc::new(AtomicUsize::new(VOICE_UI_DISPATCH_CAPACITY));
    let dispatcher_critical_slots = Arc::clone(&critical_slots);
    std::thread::Builder::new()
        .name("hh-voice-ui-dispatch".to_owned())
        .spawn(move || {
            loop {
                let event = match critical_rx.try_recv() {
                    Ok(event) => event,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        match critical_rx.recv_timeout(Duration::from_millis(2)) {
                            Ok(event) => event,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                match delta_rx.try_recv() {
                                    Ok(event) => event,
                                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                                    Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                                }
                            }
                        }
                    }
                };
                let critical = !event.droppable_delta();
                let delivered = futures::executor::block_on(desktop_tx.send(event)).is_ok();
                if critical {
                    dispatcher_critical_slots.fetch_add(1, Ordering::Release);
                }
                if !delivered {
                    break;
                }
            }
        })
        .expect("spawn voice UI dispatcher");
    (
        VoiceUiSender {
            critical_tx,
            delta_tx,
            critical_slots,
        },
        rx,
    )
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

/// Display context for the Realtime relay persona.
#[derive(Clone, Debug, Default)]
pub struct AssistantContext {
    pub workspace_title: String,
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
    ui_tx: VoiceUiSender,
) -> anyhow::Result<VoiceEngineHandle> {
    engine::spawn(settings, context, ui_tx)
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn relay_command_admission_is_non_blocking() {
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(1);
        let handle = VoiceEngineHandle {
            command_tx,
            join: None,
        };
        let first = VoiceCommand::RelayAssistantText {
            entry: Some(42),
            text: "first".to_owned(),
        };
        assert!(handle.try_send(first.clone()));
        assert!(!handle.try_send(VoiceCommand::RelayAssistantText {
            entry: Some(43),
            text: "overflow".to_owned(),
        }));
        assert_eq!(command_rx.try_recv(), Ok(first));
    }

    #[test]
    fn saturated_critical_ui_capacity_is_observable_without_blocking_the_engine() {
        let (ui, _events) = voice_ui_channel();
        let started = std::time::Instant::now();
        let attempts = VOICE_UI_EVENT_CAPACITY + VOICE_UI_DISPATCH_CAPACITY + 32;
        let rejected = (0..attempts)
            .filter(|_| !ui.emit(VoiceUiEvent::State(EngineState::Thinking)))
            .count();

        assert!(
            rejected > 0,
            "stalled desktop consumption never exposed bounded critical capacity"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "critical UI admission blocked the sole engine"
        );
    }

    #[test]
    fn critical_ui_emit_does_not_block_the_engine_when_desktop_is_stalled() {
        let (ui, _events) = voice_ui_channel();
        for _ in 0..VOICE_UI_EVENT_CAPACITY {
            ui.emit(VoiceUiEvent::MicLevel(0.0));
        }
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);

        std::thread::spawn(move || {
            let emitted = ui.emit(VoiceUiEvent::State(EngineState::Thinking));
            let _ = done_tx.send(emitted);
        });

        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Ok(true),
            "critical UI delivery synchronously stalled the engine"
        );
    }

    #[test]
    fn bounded_ui_channel_drops_only_deltas_and_preserves_critical_errors() {
        let (ui, mut events) = voice_ui_channel();
        for _ in 0..(VOICE_UI_EVENT_CAPACITY * 2) {
            ui.emit(VoiceUiEvent::MicLevel(0.0));
        }

        let critical = ui.clone();
        let sender = std::thread::spawn(move || {
            critical.emit(VoiceUiEvent::State(EngineState::Error(
                "critical".to_owned(),
            )));
        });
        let mut retained = 0;
        loop {
            let event = futures::executor::block_on(events.next()).unwrap();
            retained += 1;
            if event == VoiceUiEvent::State(EngineState::Error("critical".to_owned())) {
                break;
            }
        }
        sender.join().unwrap();

        assert!(retained <= VOICE_UI_EVENT_CAPACITY + 1);
    }
}
