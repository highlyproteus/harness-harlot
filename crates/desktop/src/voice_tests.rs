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
fn assistant_image_data_url_becomes_protocol_payload() {
    let image = assistant_image_from_data_url("data:image/png;base64,AA==").unwrap();
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(image.base64, "AA==");
    assert!(assistant_image_from_data_url("https://example.test/image.png").is_err());
}

#[test]
fn assistant_entry_text_returns_consumer_visible_content() {
    let entry = AssistantEntry::ToolCall {
        tool_call_id: "call".to_owned(),
        tool_name: "hh_read".to_owned(),
        summary: "reading".to_owned(),
        output: "screen contents".to_owned(),
        done: true,
        is_error: false,
        target_pane: None,
    };
    assert_eq!(assistant_entry_text(&entry), "screen contents");
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

#[test]
fn acknowledged_prompt_only_clears_the_unchanged_matching_draft() {
    let pane_id = Uuid::new_v4();
    let mut composer = Some(AssistantComposer {
        pane_id,
        text: "original plus more".to_owned(),
        selection: None,
        attachment: None,
    });
    clear_acknowledged_composer(&mut composer, pane_id, "original");
    assert_eq!(composer.as_ref().unwrap().text, "original plus more");

    composer.as_mut().unwrap().text = "  original  ".to_owned();
    clear_acknowledged_composer(&mut composer, pane_id, "original");
    assert_eq!(composer.as_ref().unwrap().text, "");
}

#[test]
fn failed_prompt_submission_preserves_draft_and_resets_admission() {
    let pane_id = Uuid::new_v4();
    let mut pane = AssistantPaneState {
        prompt_in_flight: true,
        ..AssistantPaneState::default()
    };
    let mut composer = Some(AssistantComposer {
        pane_id,
        text: "keep this".to_owned(),
        selection: Some(0..4),
        attachment: Some(ComposerAttachment {
            filename: "screenshot.png".to_owned(),
            data_url: "data:image/png;base64,AA==".to_owned(),
            path: PathBuf::from("/tmp/screenshot.png"),
        }),
    });
    finish_prompt_submission(
        &mut pane,
        &mut composer,
        pane_id,
        "keep this",
        Err(anyhow::anyhow!("service unavailable")),
    );
    assert!(!pane.prompt_in_flight);
    assert_eq!(pane.local_notice.as_deref(), Some("service unavailable"));
    let draft = composer.as_ref().unwrap();
    assert_eq!(draft.text, "keep this");
    assert_eq!(draft.selection, Some(0..4));
    assert!(draft.attachment.is_some());
}

#[test]
fn relay_gate_skips_muted_suspended_and_finished_backlogs() {
    assert!(assistant_voice_relay_enabled(false, false, false));
    assert!(!assistant_voice_relay_enabled(true, false, false));
    assert!(!assistant_voice_relay_enabled(false, true, false));
    assert!(!assistant_voice_relay_enabled(false, false, true));
}

#[test]
fn spoken_transcripts_use_only_the_supplied_absolute_entry() {
    let mut transcripts = HashMap::new();
    apply_spoken_transcript(&mut transcripts, None, "ignored".to_owned(), false);
    assert!(transcripts.is_empty());

    apply_spoken_transcript(&mut transcripts, Some(42), "hel".to_owned(), false);
    apply_spoken_transcript(&mut transcripts, Some(42), "lo".to_owned(), false);
    assert_eq!(transcripts.get(&42).map(String::as_str), Some("hello"));
    assert!(!transcripts.contains_key(&41));

    apply_spoken_transcript(&mut transcripts, Some(42), "final".to_owned(), true);
    assert_eq!(transcripts.get(&42).map(String::as_str), Some("final"));
    apply_spoken_transcript(&mut transcripts, Some(42), String::new(), true);
    assert!(!transcripts.contains_key(&42));
}

#[test]
fn absolute_voice_cursor_includes_truncated_entries_and_skips_backlog() {
    let view = AssistantThreadView {
        pane_id: Uuid::new_v4(),
        revision: 1,
        status: AssistantStatus::Idle,
        model: None,
        entries: vec![
            AssistantEntry::Assistant {
                text: "older visible".to_owned(),
                final_: true,
                timestamp_ms: 1,
            },
            AssistantEntry::User {
                text: "question".to_owned(),
                image_count: 0,
                timestamp_ms: 2,
            },
            AssistantEntry::Assistant {
                text: "latest".to_owned(),
                final_: true,
                timestamp_ms: 3,
            },
        ],
        truncated_entries: 40,
        pending_approval: None,
    };
    assert_eq!(absolute_entry(&view, 0), 40);
    assert_eq!(latest_final_assistant_entry(&view), Some(42));
}
