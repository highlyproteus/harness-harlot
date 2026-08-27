use super::*;

fn tiny_png() -> Vec<u8> {
    let image = image::DynamicImage::new_rgba8(1, 1);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

#[test]
fn assistant_attachment_rejects_symlinks() {
    let root = std::env::temp_dir().join(format!("hh-image-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let target = root.join("target.png");
    let link = root.join("link.png");
    std::fs::write(&target, tiny_png()).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(read_assistant_image(&link).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn assistant_attachment_requires_matching_magic_and_successful_decode() {
    let root = std::env::temp_dir().join(format!("hh-image-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let fake = root.join("fake.png");
    std::fs::write(&fake, b"not a png").unwrap();
    assert!(read_assistant_image(&fake).is_err());
    let broken = root.join("broken.png");
    std::fs::write(&broken, b"\x89PNG\r\n\x1a\nbroken").unwrap();
    assert!(read_assistant_image(&broken).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn assistant_attachment_accepts_a_bounded_decodable_image() {
    let root = std::env::temp_dir().join(format!("hh-image-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("pixel.png");
    std::fs::write(&path, tiny_png()).unwrap();
    let attachment = read_assistant_image(&path).unwrap();
    assert_eq!(attachment.0, "pixel.png");
    assert!(attachment.1.starts_with("data:image/png;base64,"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clearing_saved_history_updates_summary_only_session_ui() {
    let pane_id = Uuid::new_v4();
    let mut sessions = HashMap::from([(pane_id, AssistantSession::new())]);
    sessions.get_mut(&pane_id).unwrap().persisted_summary = PersistedSummaryState::Present;
    assert!(!assistant_workspace_shows_idle(
        sessions.get(&pane_id).unwrap()
    ));

    mark_persisted_summaries_cleared(&mut sessions);

    assert!(assistant_workspace_shows_idle(
        sessions.get(&pane_id).unwrap()
    ));
}

#[test]
fn delayed_summary_event_does_not_restore_cleared_ui_state() {
    let pane_id = Uuid::new_v4();
    let mut session = AssistantSession::new();
    session.persisted_summary = PersistedSummaryState::Present;

    reconcile_persisted_summary(&mut session, pane_id, &[]);

    assert_eq!(session.persisted_summary, PersistedSummaryState::Absent);
}

#[test]
fn suspended_assistant_keeps_the_live_surface_when_history_exists() {
    let mut session = AssistantSession::new();
    assert!(assistant_workspace_shows_idle(&session));

    session.transcript.push(VoiceTranscriptEntry {
        role: VoiceTranscriptRole::User,
        text: "keep this visible".to_owned(),
        final_: true,
        timestamp: "12:34".to_owned(),
        image: None,
    });
    assert!(!assistant_workspace_shows_idle(&session));
    session.transcript.clear();

    session.persisted_summary = PersistedSummaryState::Present;
    assert!(!assistant_workspace_shows_idle(&session));
}
#[test]
fn transcript_timestamp_is_compact_local_time() {
    let timestamp = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(&time::macros::format_description!("[hour]:[minute]"))
        .unwrap();
    assert_eq!(timestamp.len(), 5);
    assert_eq!(&timestamp[2..3], ":");
}
#[test]
fn non_voice_start_revokes_prior_microphone_consent() {
    let mut session = AssistantSession::new();
    session.mic_muted = false;
    prepare_non_voice_start(&mut session);
    assert!(session.mic_muted);
}

#[test]
fn audio_controls_toggle_mutes_without_suspending() {
    let mut session = AssistantSession::new();
    session.engine_state = EngineState::Listening;

    assert!(
        session.mic_muted,
        "new/text-only sessions need explicit mic consent"
    );
    assert!(toggle_microphone_muted(&mut session));
    assert!(!session.mic_muted);
    assert_eq!(session.engine_state, EngineState::Listening);

    assert!(toggle_headphones_muted(&mut session));
    assert!(session.speaker_muted);
    assert_eq!(session.engine_state, EngineState::Listening);
}

#[test]
fn activating_composer_preserves_same_pane_draft() {
    let pane_id = Uuid::new_v4();
    let mut composer = Some(AssistantComposer {
        pane_id,
        text: "unfinished".to_owned(),
        selection: None,
        attachment: Some(ComposerAttachment {
            filename: "screenshot.png".to_owned(),
            data_url: "data:image/png;base64,AA==".to_owned(),
            path: PathBuf::from("/tmp/screenshot.png"),
        }),
    });
    activate_assistant_composer(&mut composer, pane_id);
    assert_eq!(composer.as_ref().unwrap().text, "unfinished");
    assert_eq!(
        composer
            .as_ref()
            .unwrap()
            .attachment
            .as_ref()
            .unwrap()
            .filename,
        "screenshot.png"
    );

    activate_assistant_composer(&mut composer, Uuid::new_v4());
    assert_eq!(composer.as_ref().unwrap().text, "");
    assert!(composer.as_ref().unwrap().attachment.is_none());
}

#[test]
fn composer_selection_copies_cuts_and_replaces() {
    let mut composer = AssistantComposer {
        pane_id: Uuid::new_v4(),
        text: "hello".to_owned(),
        selection: None,
        attachment: None,
    };
    composer.select_all();
    assert_eq!(composer.selected_text(), Some("hello"));
    composer.insert("world");
    assert_eq!(composer.text, "world");
    composer.select_all();
    assert_eq!(composer.cut_selection().as_deref(), Some("world"));
    assert!(composer.text.is_empty());
}
