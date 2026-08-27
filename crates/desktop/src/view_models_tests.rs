use super::{
    ClientRequest, CloseConfirmation, CloseConfirmationKind, DialogTextEditor, DragDestination,
    DragHoverState, DropPlacement, HashSet, MAX_ASSISTANT_INSTRUCTIONS_CHARS, MAX_SSH_INPUT_LEN,
    MAX_WORKSPACE_DIR_BYTES, MouseButton, Pane, SidebarResizeLifecycle, SidebarResizeMove,
    TmuxScanScope, TmuxSession, TmuxSessionId, TmuxSessionPicker, Uuid, WorkspaceCreationDialog,
    WorkspaceCreationField, WorkspaceCreationKind, WorkspaceCreationStep,
    route_workspace_creation_paste,
};

#[test]
fn per_tab_close_requires_an_explicit_confirmation_for_the_exact_terminal() {
    let pane = Pane {
        id: Uuid::new_v4(),
        kind: hh_protocol::PaneKind::Terminal,
        title: "build".to_owned(),
        shell: "zsh".to_owned(),
        color: None,
        identity: hh_protocol::TerminalIdentity::default(),
        status: hh_protocol::PaneStatus::default(),
        custom_title: Some("build".to_owned()),
        profile_override: None,
        custom_icon: None,
    };

    let confirmation = CloseConfirmation::for_pane(&pane, true);

    assert_eq!(confirmation.pane_id, pane.id);
    assert_eq!(confirmation.title, "build");
    assert_eq!(confirmation.kind, CloseConfirmationKind::Terminal);
    assert!(confirmation.leaves_workspace_empty);
    assert_eq!(
        confirmation.request(),
        ClientRequest::ClosePane { pane_id: pane.id }
    );
}
#[test]
fn ssh_workspace_cannot_create_a_network_action_before_review_and_confirmation() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.kind = WorkspaceCreationKind::SystemSsh;
    dialog.destination = DialogTextEditor::with_text("prod-east");

    assert_eq!(dialog.approved_request(), None);

    dialog.review();

    assert_eq!(dialog.step, WorkspaceCreationStep::ConfirmSsh);
    assert_eq!(
        dialog.approved_request(),
        Some(ClientRequest::CreateSshWorkspace {
            title: None,
            destination: "prod-east".to_owned(),
        })
    );
}
#[test]
fn assistant_workspace_request_trims_fields_and_expands_home() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.kind = WorkspaceCreationKind::Assistant;
    dialog.name = DialogTextEditor::with_text("  Research  ");
    dialog.working_dir = DialogTextEditor::with_text("  ~/Projects  ");
    dialog.instructions = DialogTextEditor::with_text("  answer in one sentence  ");
    let expected_working_dir = std::env::var("HOME").map_or_else(
        |_| "~/Projects".to_owned(),
        |home| format!("{home}/Projects"),
    );

    assert_eq!(
        dialog.approved_request(),
        Some(ClientRequest::CreateAssistantWorkspace {
            title: Some("Research".to_owned()),
            working_dir: Some(expected_working_dir),
            instructions: Some("answer in one sentence".to_owned()),
        })
    );
}

#[test]
fn assistant_workspace_fields_enforce_wire_limits() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.kind = WorkspaceCreationKind::Assistant;
    dialog.field = WorkspaceCreationField::WorkingDir;
    dialog.replace_text(None, &"x".repeat(MAX_WORKSPACE_DIR_BYTES + 1), false, None);
    assert_eq!(dialog.working_dir.text.len(), MAX_WORKSPACE_DIR_BYTES);

    dialog.field = WorkspaceCreationField::Instructions;
    dialog.replace_text(
        None,
        &"😀".repeat(MAX_ASSISTANT_INSTRUCTIONS_CHARS + 1),
        false,
        None,
    );
    assert_eq!(
        dialog.instructions.text.chars().count(),
        MAX_ASSISTANT_INSTRUCTIONS_CHARS
    );
}

#[test]
fn workspace_dialog_editor_inserts_and_deletes_at_the_visible_caret() {
    let mut editor = DialogTextEditor::with_text("Terminal App");
    editor.move_home(false);
    for _ in 0..8 {
        editor.move_right(false);
    }

    editor.replace(None, "-", 80, false, false, None);
    assert_eq!(editor.text, "Terminal- App");
    assert_eq!(editor.selected_range, 9..9);

    editor.delete_backward();
    assert_eq!(editor.text, "Terminal App");
    assert_eq!(editor.selected_range, 8..8);

    editor.delete_forward();
    assert_eq!(editor.text, "TerminalApp");
    assert_eq!(editor.selected_range, 8..8);
}
#[test]
fn workspace_dialog_selection_replacement_supports_normal_editing() {
    let mut editor = DialogTextEditor::with_text("tailscale-old");
    for _ in 0..3 {
        editor.move_left(true);
    }
    assert_eq!(editor.selected_text(), Some("old"));

    editor.replace(None, "node", MAX_SSH_INPUT_LEN, true, false, None);

    assert_eq!(editor.text, "tailscale-node");
    assert_eq!(editor.selected_range, editor.text.len()..editor.text.len());
}
#[test]
fn workspace_dialog_double_click_selection_replaces_a_name_or_destination_unit() {
    let mut name = DialogTextEditor::with_text("Build workstation");
    name.select_word_at(2);
    assert_eq!(name.selected_text(), Some("Build"));
    name.replace(None, "Deploy", 80, false, false, None);
    assert_eq!(name.text, "Deploy workstation");

    let mut destination = DialogTextEditor::with_text("admin@build-node");
    destination.select_word_at(7);
    assert_eq!(destination.selected_text(), Some("admin@build-node"));
    destination.replace(None, "ops@edge", MAX_SSH_INPUT_LEN, true, false, None);
    assert_eq!(destination.text, "ops@edge");
}
#[test]
fn workspace_dialog_select_all_supports_cut_or_replacement_without_terminal_input() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.name = DialogTextEditor::with_text("Default workstation name");
    dialog.name.select_all();
    assert_eq!(
        dialog.name.selected_text(),
        Some("Default workstation name")
    );

    dialog.replace_text(None, "My Mac", false, None);
    assert_eq!(dialog.name.text, "My Mac");
    assert!(dialog.name.selected_text().is_none());
}
#[test]
fn workspace_dialog_uses_utf16_ranges_without_splitting_unicode_text() {
    let mut editor = DialogTextEditor::with_text("box😀x");
    assert_eq!(editor.range_to_utf16(&(3..7)), 3..5);

    editor.replace(Some(&(3..5)), "🌐", 80, false, false, None);

    assert_eq!(editor.text, "box🌐x");
    assert_eq!(editor.range_to_utf16(&editor.selected_range), 5..5);
    editor.delete_backward();
    assert_eq!(editor.text, "boxx");
}
#[test]
fn workspace_dialog_fields_keep_independent_cursors_and_selection() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.name = DialogTextEditor::with_text("Build box");
    dialog.name.select_all();
    dialog.destination = DialogTextEditor::with_text("admin@node");
    dialog.field = WorkspaceCreationField::Name;
    dialog.replace_text(None, "Tailscale", false, None);
    dialog.field = WorkspaceCreationField::Destination;
    dialog.backspace();

    assert_eq!(dialog.name.text, "Tailscale");
    assert_eq!(dialog.destination.text, "admin@nod");
    assert_eq!(dialog.name.selected_range, 9..9);
    assert_eq!(dialog.destination.selected_range, 9..9);
}
#[test]
fn workspace_dialog_marked_text_tracks_gpui_relative_utf16_selection() {
    let mut editor = DialogTextEditor::with_text("host");
    editor.move_home(false);
    editor.replace(None, "候補", 80, false, true, Some(&(1..1)));

    assert_eq!(editor.text, "候補host");
    assert_eq!(editor.marked_range, Some(0..6));
    assert_eq!(editor.selected_range, 3..3);
    assert_eq!(editor.range_to_utf16(&editor.selected_range), 1..1);
}
#[test]
fn pasted_ssh_command_is_normalized_before_confirmation() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.kind = WorkspaceCreationKind::SystemSsh;
    dialog.field = WorkspaceCreationField::Destination;
    dialog.name = DialogTextEditor::with_text("Build box");

    dialog.paste("ssh tailscale_user@build-node\n");
    assert_eq!(dialog.destination.text, "ssh tailscale_user@build-node");
    assert_eq!(dialog.approved_request(), None);

    dialog.review();

    assert_eq!(dialog.step, WorkspaceCreationStep::ConfirmSsh);
    assert_eq!(dialog.destination.text, "tailscale_user@build-node");
    assert_eq!(
        dialog.approved_request(),
        Some(ClientRequest::CreateSshWorkspace {
            title: Some("Build box".to_owned()),
            destination: "tailscale_user@build-node".to_owned(),
        })
    );
}
#[test]
fn pasted_ssh_options_never_cross_the_confirmation_boundary() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.kind = WorkspaceCreationKind::SystemSsh;
    dialog.field = WorkspaceCreationField::Destination;
    dialog.paste("ssh -A build-node");

    dialog.review();

    assert_eq!(dialog.step, WorkspaceCreationStep::Details);
    assert!(dialog.error.is_some());
    assert_eq!(dialog.approved_request(), None);
}
#[test]
fn workspace_creation_modal_consumes_paste_instead_of_leaking_it_to_the_terminal() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.kind = WorkspaceCreationKind::SystemSsh;
    dialog.field = WorkspaceCreationField::Destination;
    assert!(route_workspace_creation_paste(
        Some(&mut dialog),
        "ssh admin@tailscale-node"
    ));
    assert_eq!(dialog.destination.text, "ssh admin@tailscale-node");

    dialog.review();
    assert!(route_workspace_creation_paste(
        Some(&mut dialog),
        "ssh other-node"
    ));
    assert_eq!(dialog.destination.text, "admin@tailscale-node");

    assert!(!route_workspace_creation_paste(
        None,
        "ordinary terminal paste"
    ));
}
#[test]
fn ssh_review_keeps_invalid_input_out_of_the_network_action_boundary() {
    let mut dialog = WorkspaceCreationDialog::new();
    dialog.kind = WorkspaceCreationKind::SystemSsh;
    dialog.destination = DialogTextEditor::with_text("-oProxyCommand=bad");

    dialog.review();

    assert_eq!(dialog.step, WorkspaceCreationStep::Details);
    assert!(dialog.error.is_some());
    assert_eq!(dialog.approved_request(), None);
}
#[test]
fn drag_hover_state_persists_updates_and_clears_for_leave_drop_or_cancel() {
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let mut hover = DragHoverState::default();

    hover.enter(DragDestination::Split {
        target_pane: first,
        placement: DropPlacement::Left,
    });
    assert_eq!(hover.split_for(first), Some(DropPlacement::Left));

    hover.enter(DragDestination::Split {
        target_pane: first,
        placement: DropPlacement::Bottom,
    });
    assert_eq!(hover.split_for(first), Some(DropPlacement::Bottom));

    hover.enter(DragDestination::Merge {
        target_pane: second,
    });
    assert!(hover.merges_into(second));
    assert_eq!(hover.split_for(first), None);

    hover.clear();
    assert_eq!(hover, DragHoverState::default());
}
#[test]
fn sidebar_resize_only_updates_while_the_left_button_is_held() {
    let mut resize = SidebarResizeLifecycle::default();

    assert_eq!(
        resize.pointer_move(Some(MouseButton::Left)),
        SidebarResizeMove::Ignore
    );

    resize.begin(240.0);
    assert_eq!(
        resize.pointer_move(Some(MouseButton::Left)),
        SidebarResizeMove::Update
    );
    assert_eq!(resize.pointer_move(None), SidebarResizeMove::Complete);
    assert!(!resize.is_active());
    assert_eq!(resize.pointer_move(None), SidebarResizeMove::Ignore);
}
#[test]
fn sidebar_resize_release_and_cancel_end_the_capture() {
    let mut resize = SidebarResizeLifecycle::default();

    resize.begin(275.0);
    assert!(resize.finish());
    assert!(!resize.finish());
    assert!(!resize.is_active());

    resize.begin(310.0);
    assert_eq!(resize.cancel(), Some(310.0));
    assert_eq!(resize.cancel(), None);
    assert!(!resize.is_active());
}

fn tmux_session(id: &TmuxSessionId, name: &str) -> TmuxSession {
    TmuxSession {
        id: id.clone(),
        name: name.to_owned(),
        windows: 2,
        attached_clients: 0,
    }
}

fn tmux_picker(sessions: Vec<TmuxSession>, open: HashSet<TmuxSessionId>) -> TmuxSessionPicker {
    TmuxSessionPicker {
        workspace_id: Uuid::nil(),
        scope: TmuxScanScope::Local,
        sessions,
        open_session_ids: open,
        no_server: false,
        selected_session_ids: HashSet::new(),
        status: None,
        error: None,
    }
}

#[test]
fn tmux_picker_selects_sessions_in_scan_order() {
    let first = TmuxSessionId::try_from("$1".to_owned()).unwrap();
    let second = TmuxSessionId::try_from("$2".to_owned()).unwrap();
    let third = TmuxSessionId::try_from("$3".to_owned()).unwrap();
    let mut picker = tmux_picker(
        vec![
            tmux_session(&first, "one"),
            tmux_session(&second, "two"),
            tmux_session(&third, "three"),
        ],
        HashSet::new(),
    );

    picker.toggle_session(&third);
    picker.toggle_session(&first);
    assert_eq!(
        picker
            .selected_session_ids_in_scan_order()
            .iter()
            .map(TmuxSessionId::as_str)
            .collect::<Vec<_>>(),
        vec!["$1", "$3"]
    );

    picker.toggle_session(&first);
    assert_eq!(
        picker.selected_session_ids_in_scan_order(),
        vec![third.clone()]
    );

    picker.select_all_sessions();
    assert_eq!(
        picker.selected_session_ids_in_scan_order(),
        vec![first, second, third]
    );

    picker.clear_all_sessions();
    assert!(picker.selected_session_ids_in_scan_order().is_empty());
}

#[test]
fn tmux_picker_never_offers_sessions_already_open_in_a_tab() {
    let open_id = TmuxSessionId::try_from("$1".to_owned()).unwrap();
    let free_id = TmuxSessionId::try_from("$2".to_owned()).unwrap();
    let mut picker = tmux_picker(
        vec![
            tmux_session(&open_id, "already-open"),
            tmux_session(&free_id, "free"),
        ],
        HashSet::from([open_id.clone()]),
    );

    assert!(picker.is_open(&open_id));
    picker.toggle_session(&open_id);
    assert!(picker.selected_session_ids_in_scan_order().is_empty());

    picker.select_all_sessions();
    assert_eq!(
        picker.selected_session_ids_in_scan_order(),
        vec![free_id.clone()]
    );

    picker.clear_all_sessions();
    picker.toggle_session(&free_id);
    assert_eq!(picker.selected_session_ids_in_scan_order(), vec![free_id]);
}
