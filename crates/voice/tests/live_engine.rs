//! Manual live probe: runs the real engine against the local service and the
//! Realtime API using the operator's saved settings, printing every UI event.
use std::time::{Duration, Instant};

use hh_protocol::{ClientRequest, ServiceResponse};
use hh_session_client::SessionClient;
use hh_voice::{
    AssistantContext, EngineState, VoiceCommand, VoiceSettings, VoiceUiEvent, spawn_engine,
};

#[test]
#[ignore = "requires saved voice settings, a running hh-service, and network access"]
fn live_engine_reports_state_transitions() {
    let settings = VoiceSettings::load();
    assert!(
        !settings.api_key.trim().is_empty(),
        "no API key in saved voice settings"
    );
    let (ui_tx, mut ui_rx) = futures::channel::mpsc::unbounded();
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

#[test]
#[ignore = "creates a real terminal tab and requires saved settings, hh-service, and network"]
fn live_engine_opens_requested_terminal_tab() {
    let settings = VoiceSettings::load();
    assert!(
        !settings.api_key.trim().is_empty(),
        "no API key in saved voice settings"
    );
    let mut client = SessionClient::connect().expect("connect session service");
    let ServiceResponse::Snapshot { snapshot } = client
        .call(&ClientRequest::GetSnapshot)
        .expect("get snapshot")
    else {
        panic!("session service did not return a snapshot");
    };
    let workspace = snapshot
        .workspaces
        .first()
        .expect("at least one workstation is required");
    let before_tabs = workspace.tabs.len();
    let workspace_id = workspace.id;
    let context = AssistantContext {
        workspace_id: Some(workspace_id),
        pane_id: None,
        workspace_title: workspace.title.clone(),
        working_dir: workspace.working_dir.clone(),
        prior_context: None,
    };
    let (ui_tx, mut ui_rx) = futures::channel::mpsc::unbounded();
    let engine = spawn_engine(settings, context, ui_tx).expect("spawn engine");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut prompted = false;
    let mut opened = false;
    while Instant::now() < deadline {
        match ui_rx.try_recv() {
            Ok(VoiceUiEvent::MicLevel(_)) => {}
            Ok(VoiceUiEvent::State(EngineState::Listening)) if !prompted => {
                engine.send(VoiceCommand::SendUserText(
                    "Open a terminal tab now. Use open_terminal_tab and report only its result."
                        .to_owned(),
                ));
                prompted = true;
            }
            Ok(VoiceUiEvent::ToolCall { name, summary }) => {
                println!("ToolCall {{ name: {name:?}, summary: {summary:?} }}");
                if name == "open_terminal_tab" {
                    opened = true;
                    break;
                }
            }
            Ok(event) => println!("{event:?}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    engine.shutdown();

    assert!(prompted, "engine never reached Listening");
    assert!(opened, "assistant never called open_terminal_tab");
    let ServiceResponse::Snapshot { snapshot } = client
        .call(&ClientRequest::GetSnapshot)
        .expect("get updated snapshot")
    else {
        panic!("session service did not return an updated snapshot");
    };
    let after_tabs = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .expect("target workstation remains present")
        .tabs
        .len();
    assert_eq!(after_tabs, before_tabs + 1);
}

#[test]
#[ignore = "creates a real assistant tab in the running development app"]
fn live_service_creates_assistant_for_ui_probe() {
    let mut client = SessionClient::connect().expect("connect session service");
    let ServiceResponse::Snapshot { snapshot } = client
        .call(&ClientRequest::GetSnapshot)
        .expect("get snapshot")
    else {
        panic!("session service did not return a snapshot");
    };
    let workspace_id = snapshot
        .workspaces
        .first()
        .expect("at least one workstation is required")
        .id;
    let ServiceResponse::PaneCreated { pane_id } = client
        .call(&ClientRequest::CreateAssistantTab { workspace_id })
        .expect("create assistant tab")
    else {
        panic!("session service did not create an assistant pane");
    };
    println!("created assistant pane {pane_id}");
}
