//! Manual live probe: runs the conversational Voice engine against the
//! Realtime API using saved non-secret settings and `HH_OPENAI_API_KEY`,
//! printing every UI event.
use std::time::{Duration, Instant};

use hh_voice::{
    AssistantContext, EngineState, VoiceCommand, VoiceSettings, VoiceUiEvent, spawn_engine,
    voice_ui_channel,
};

#[test]
#[ignore = "requires HH_OPENAI_API_KEY, a running hh-service, and network access"]
fn live_engine_reports_state_transitions() {
    let settings = VoiceSettings::load().expect("load voice settings and environment overrides");
    assert!(
        !settings.api_key.trim().is_empty(),
        "HH_OPENAI_API_KEY is missing or empty"
    );
    let (ui_tx, mut ui_rx) = voice_ui_channel();
    let engine = spawn_engine(settings, AssistantContext::default(), ui_tx).expect("spawn engine");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut prompted = false;
    let mut listening_at = None;
    let mut spoke_unprompted = false;
    let mut saw_progress = false;
    let mut assistant_final = false;
    let mut last_progress = None;
    let mut playback_stopped = false;
    while Instant::now() < deadline {
        if !prompted
            && listening_at
                .is_some_and(|started: Instant| started.elapsed() >= Duration::from_secs(5))
        {
            assert!(!spoke_unprompted, "assistant spoke unprompted at startup");
            engine.send(VoiceCommand::SendUserText(
                "Reply with exactly: playback progress verified.".to_owned(),
            ));
            prompted = true;
        }
        match ui_rx.try_recv() {
            Ok(VoiceUiEvent::MicLevel(_)) => {}
            Ok(VoiceUiEvent::State(EngineState::Listening)) if listening_at.is_none() => {
                println!("{:?}", VoiceUiEvent::State(EngineState::Listening));
                listening_at = Some(Instant::now());
            }
            Ok(
                event @ (VoiceUiEvent::State(EngineState::Speaking)
                | VoiceUiEvent::AssistantTranscript { .. }),
            ) if !prompted => {
                println!("{event:?}");
                spoke_unprompted = true;
            }
            Ok(VoiceUiEvent::PlaybackProgress {
                played_ms,
                total_ms,
            }) => {
                println!(
                    "{:?}",
                    VoiceUiEvent::PlaybackProgress {
                        played_ms,
                        total_ms
                    }
                );
                saw_progress = true;
                last_progress = Some(Instant::now());
            }
            Ok(VoiceUiEvent::AssistantTranscript { text, final_: true }) => {
                println!(
                    "{:?}",
                    VoiceUiEvent::AssistantTranscript { text, final_: true }
                );
                assistant_final = true;
            }
            Ok(event) => println!("{event:?}"),
            Err(_) => {
                if assistant_final
                    && let Some(last_progress) = last_progress
                    && last_progress.elapsed() >= Duration::from_millis(500)
                {
                    playback_stopped = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    assert!(prompted, "engine never reached Listening");
    assert!(
        saw_progress,
        "no PlaybackProgress event during spoken reply"
    );
    assert!(assistant_final, "assistant reply did not complete");
    assert!(
        playback_stopped,
        "PlaybackProgress events did not stop after the reply"
    );
    engine.shutdown();
}
