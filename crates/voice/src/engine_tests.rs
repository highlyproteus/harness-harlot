use super::*;

#[test]
fn relay_emits_prefixed_user_item_then_response_create() {
    let events = orchestrator_relay_events("The command finished successfully.".to_owned());
    assert_eq!(
        events,
        [
            ClientEvent::ConversationItemCreate {
                item: ConversationItem::Message {
                    role: ConversationRole::User,
                    content: vec![InputContent::InputText {
                        text: "[Orchestrator update]\nThe command finished successfully."
                            .to_owned(),
                    }],
                },
                previous_item_id: None,
            },
            ClientEvent::ResponseCreate { response: None },
        ]
    );
}

#[test]
fn relay_waits_for_silence_and_connection() {
    assert!(!relay_flush_allowed(true, false, true));
    assert!(!relay_flush_allowed(false, true, true));
    assert!(!relay_flush_allowed(false, false, false));
    assert!(relay_flush_allowed(false, false, true));
}

#[test]
fn completed_user_transcription_never_creates_a_provider_response() {
    assert!(completed_transcription_provider_events().is_empty());
}

#[test]
fn relay_text_is_bounded_before_provider_submission() {
    let events = orchestrator_relay_events("x".repeat(MAX_RELAY_CHARS + 20));
    let ClientEvent::ConversationItemCreate {
        item: ConversationItem::Message { content, .. },
        ..
    } = &events[0]
    else {
        panic!("relay must create a conversation message");
    };
    let InputContent::InputText { text } = &content[0] else {
        panic!("relay must contain text");
    };
    assert_eq!(
        text.chars().count(),
        "[Orchestrator update]\n".chars().count() + MAX_RELAY_CHARS
    );
}

#[test]
fn completed_transcription_ids_are_deduplicated_and_bounded() {
    let mut completed = VecDeque::new();
    assert!(accept_completed_transcription(
        &mut completed,
        Some("one".to_owned())
    ));
    assert!(!accept_completed_transcription(
        &mut completed,
        Some("one".to_owned())
    ));
    for index in 0..MAX_TRANSCRIPTION_IDS {
        let _ = accept_completed_transcription(&mut completed, Some(format!("id-{index}")));
    }
    assert_eq!(completed.len(), MAX_TRANSCRIPTION_IDS);
    assert!(!transcription_already_completed(
        &completed,
        Some("missing")
    ));
    assert!(accept_completed_transcription(&mut completed, None));
}

#[test]
fn microphone_requires_explicit_consent() {
    let mut consent = MicrophoneConsent::default();
    assert!(!consent.capture_enabled());
    consent.apply_command(true);
    assert!(consent.capture_enabled());
    consent.apply_command(false);
    assert!(!consent.capture_enabled());
}

#[test]
fn half_duplex_blocks_only_active_playback_or_recent_output() {
    assert!(microphone_streaming_allowed(
        false,
        MicrophoneActivity {
            response_active: false,
            output_quiet: true,
            playback_active: false,
        }
    ));
    assert!(!microphone_streaming_allowed(
        false,
        MicrophoneActivity {
            response_active: false,
            output_quiet: false,
            playback_active: false,
        }
    ));
    assert!(!microphone_streaming_allowed(
        false,
        MicrophoneActivity {
            response_active: false,
            output_quiet: true,
            playback_active: true,
        }
    ));
    assert!(microphone_streaming_allowed(
        true,
        MicrophoneActivity {
            response_active: false,
            output_quiet: false,
            playback_active: true,
        }
    ));
}

#[test]
fn idle_timeout_zero_disables_and_short_values_clamp() {
    assert_eq!(effective_idle_timeout(0), None);
    assert_eq!(effective_idle_timeout(1), Some(Duration::from_mins(1)));
    assert_eq!(effective_idle_timeout(120), Some(Duration::from_mins(2)));
}

#[test]
fn only_completed_or_missing_response_status_is_successful() {
    assert!(response_done_successful(None));
    assert!(response_done_successful(Some("completed")));
    assert!(!response_done_successful(Some("cancelled")));
}
