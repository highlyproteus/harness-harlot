//! Workstation selection, ordering, directory editors, and tmux trio.

use crate::helpers::{
    append_rename_text, child_workstations, find_pane, resolved_terminal_accent,
    resolved_workspace_color, same_machine, visible_panes, workspace_tab_click_target,
    workspace_tab_standalone_pane,
};
use crate::notifications::SeenScope;
use crate::view_models::{
    CreationParent, DialogTextEditor, DirEditor, DirEditorTarget, Modal, TmuxSelectionChange,
    TmuxSessionPicker, WorkspaceConnectionInfo, WorkspaceCreationDialog, WorkspaceCreationField,
    WorkspaceCreationKind, WorkspaceCreationStep, WorkspaceDeleteConfirmation,
    WorkspaceDisconnectConfirmation, WorkspaceRenameEditor,
};
use crate::{DRAG_CLICK_SUPPRESSION_MS, HhApp};
use gpui::{Context, Pixels, Point, Window};
use hh_protocol::{
    AppearanceColor, ClientRequest, ServiceResponse, TmuxScanScope, Workspace, WorkspaceConnection,
    WorkspaceConnectionStatus, effective_working_dir, validate_workspace_dir,
    workstation_descendants,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) fn remote_completion_parts(value: &str) -> (String, String) {
    value.rfind('/').map_or_else(
        || ("/".to_owned(), value.to_owned()),
        |slash| {
            (
                value[..=slash].to_owned(),
                value[slash.saturating_add(1)..].to_owned(),
            )
        },
    )
}

pub(crate) fn longest_common_prefix(values: &[String]) -> String {
    let Some(first) = values.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for value in &values[1..] {
        while !value.starts_with(&prefix) {
            if prefix.pop().is_none() {
                return prefix;
            }
        }
    }
    prefix
}

impl HhApp {
    pub(crate) fn new_workspace_terminal(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let Some(empty) = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| workspace.tabs.is_empty())
        }) else {
            return;
        };
        if empty {
            self.open_workspace_terminal(workspace_id, cx);
        } else {
            self.new_workspace_tab(workspace_id, cx);
        }
    }

    pub(crate) fn terminal_accent(&self, pane_id: Uuid) -> AppearanceColor {
        self.session
            .snapshot
            .as_ref()
            .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                resolved_terminal_accent(snapshot, pane_id)
            })
    }

    pub(crate) fn workspace_color(&self, workspace_id: Uuid) -> AppearanceColor {
        self.session
            .snapshot
            .as_ref()
            .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                resolved_workspace_color(snapshot, workspace_id)
            })
    }

    pub(crate) fn new_workspace(&mut self, cx: &mut Context<Self>) {
        self.begin_workspace_creation(cx);
    }

    /// Expands `workspace_id` and every workstation enclosing it, so its card
    /// is visible in the sidebar tree.
    pub(crate) fn reveal_workspace(&mut self, workspace_id: Uuid) {
        self.sidebar.expanded_workspaces.insert(workspace_id);
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return;
        };
        let mut current = Some(workspace_id);
        for _ in 0..snapshot.workspaces.len() {
            let Some(id) = current else {
                break;
            };
            self.sidebar.expanded_workspaces.insert(id);
            current = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .and_then(|workspace| workspace.parent_workstation);
        }
    }

    /// Shows workstation `workspace_id` on its first tab; `seen` says whether
    /// that counts as the user looking at it.
    pub(crate) fn select_workspace(
        &mut self,
        workspace_id: Uuid,
        seen: SeenScope,
        cx: &mut Context<Self>,
    ) {
        self.reveal_workspace(workspace_id);
        self.sidebar.active_workspace = Some(workspace_id);
        let first_pane = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .and_then(|workspace| workspace.tabs.first())
                .and_then(|tab| visible_panes(&tab.layout).first().copied())
        });
        if let Some(pane_id) = first_pane {
            self.mark_seen(pane_id, seen);
            self.focus_pane_with_snapshot(pane_id, cx);
        }
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn select_workspace_by_index(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(workspace_id) = self.session.snapshot.as_ref().and_then(|snapshot| {
            child_workstations(&snapshot.workspaces, None)
                .get(index)
                .map(|workspace| workspace.id)
        }) else {
            return false;
        };
        self.select_workspace(workspace_id, SeenScope::Tab, cx);
        self.sidebar.workstation_tab_scroll.scroll_to_item(index);
        true
    }

    pub(crate) fn select_workspace_tab(
        &mut self,
        workspace_id: Uuid,
        pane_id: Uuid,
        seen: SeenScope,
        cx: &mut Context<Self>,
    ) {
        let selected_tab_id = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .and_then(|workspace| {
                    workspace
                        .tabs
                        .iter()
                        .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                        .map(|tab| tab.id)
                })
        });
        self.mark_seen(pane_id, seen);
        self.dispatch_with(
            ClientRequest::ActivateTab { pane_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        this.sidebar.active_workspace = Some(workspace_id);
                        let refreshed_pane_id = selected_tab_id.map_or(Some(pane_id), |tab_id| {
                            this.session.snapshot.as_ref().and_then(|snapshot| {
                                snapshot
                                    .workspaces
                                    .iter()
                                    .find(|workspace| workspace.id == workspace_id)
                                    .and_then(|workspace| {
                                        workspace_tab_click_target(workspace, tab_id, Some(pane_id))
                                    })
                            })
                        });
                        if let Some(refreshed_pane_id) = refreshed_pane_id {
                            this.focus_pane_with_snapshot(refreshed_pane_id, cx);
                        }
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        self.layout.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    pub(crate) fn select_workspace_tab_root(
        &mut self,
        workspace_id: Uuid,
        tab_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .and_then(|workspace| {
                    workspace_tab_click_target(workspace, tab_id, self.layout.focused_pane)
                })
        }) else {
            return;
        };
        self.select_workspace_tab(workspace_id, pane_id, SeenScope::Tab, cx);
    }

    pub(crate) fn dismiss_workspace_tab(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let standalone_pane = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .and_then(workspace_tab_standalone_pane)
                .map(|pane| pane.id)
        });
        if let Some(pane_id) = standalone_pane {
            self.begin_close(pane_id, cx);
        } else {
            self.sidebar.dismissed_workspace_tabs.insert(tab_id);
            self.editor.modal = Modal::None;
            cx.notify();
        }
    }

    pub(crate) fn select_sidebar_pane(
        &mut self,
        workspace_id: Uuid,
        tab_id: Uuid,
        pane_id: Uuid,
        seen: SeenScope,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.dismissed_workspace_tabs.remove(&tab_id);
        self.select_workspace_tab(workspace_id, pane_id, seen, cx);
        cx.notify();
    }

    pub(crate) fn workspace_id_for_pane(&self, pane_id: Uuid) -> Option<Uuid> {
        self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| {
                    workspace
                        .tabs
                        .iter()
                        .any(|tab| find_pane(&tab.layout, pane_id).is_some())
                })
                .map(|workspace| workspace.id)
        })
    }

    pub(crate) fn begin_workspace_creation(&mut self, cx: &mut Context<Self>) {
        self.open_workspace_creation(WorkspaceCreationDialog::new(), cx);
    }

    /// Opens the New Workstation dialog for a workstation nested in
    /// `parent_id`, on the parent's machine.
    pub(crate) fn begin_nested_workspace_creation(
        &mut self,
        parent_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let parent = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == parent_id)
                .map(|workspace| CreationParent {
                    workspace_id: parent_id,
                    title: workspace.title.clone(),
                    remote: matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. }),
                })
        });
        if let Some(parent) = parent {
            self.open_workspace_creation(WorkspaceCreationDialog::nested(parent), cx);
        }
    }

    fn open_workspace_creation(&mut self, dialog: WorkspaceCreationDialog, cx: &mut Context<Self>) {
        self.editor.modal = Modal::WorkspaceCreation(dialog);
        self.editor.workspace_input_layouts = [None, None, None, None];
        self.editor.workspace_input_bounds = [None, None, None, None];
        cx.notify();
    }

    /// Fills the New Workstation dialog's root folder from the native folder
    /// chooser.
    pub(crate) fn choose_workspace_creation_folder(&mut self, cx: &mut Context<Self>) {
        self.prompt_local_directory(
            "Choose root folder",
            |this, dir, cx| {
                if let Some(dialog) = this.editor.modal.workspace_creation_mut() {
                    dialog.working_dir = DialogTextEditor::with_text(dir);
                    dialog.field = WorkspaceCreationField::WorkingDir;
                    dialog.error = None;
                }
                cx.notify();
            },
            cx,
        );
    }

    pub(crate) fn focus_workspace_creation_field(
        &mut self,
        field: WorkspaceCreationField,
        position: Option<Point<Pixels>>,
        extend_selection: bool,
        click_count: usize,
        window: &mut Window,
    ) {
        let index = field.index();
        let offset = position.and_then(|position| {
            let line = self.editor.workspace_input_layouts[index].as_ref()?;
            let bounds = self.editor.workspace_input_bounds[index]?;
            Some(line.closest_index_for_x(position.x - bounds.left()))
        });
        let Some(dialog) = self.editor.modal.workspace_creation_mut() else {
            return;
        };
        dialog.field = field;
        // Give the custom GPUI input the platform text focus immediately on
        // mouse-down. Waiting for the next render left click/keyboard routing
        // competing with the terminal behind the modal on macOS.
        self.editor.workspace_input_focus[index].focus(window);
        let editor = dialog.active_editor_mut();
        match offset {
            Some(_) if click_count >= 3 => editor.select_all(),
            Some(offset) if click_count == 2 => editor.select_word_at(offset),
            Some(offset) if extend_selection => editor.select_to(offset),
            Some(offset) => editor.move_to(offset),
            None => editor.move_end(false),
        }
    }

    pub(crate) fn submit_workspace_creation(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.editor.modal.workspace_creation_mut() else {
            return;
        };
        if dialog.kind == WorkspaceCreationKind::SystemSsh
            && dialog.step == WorkspaceCreationStep::Details
        {
            dialog.review();
            cx.notify();
            return;
        }
        let Some(request_message) = self
            .editor
            .modal
            .workspace_creation()
            .and_then(WorkspaceCreationDialog::approved_request)
        else {
            return;
        };
        let local_root = match &request_message {
            ClientRequest::CreateWorkspace {
                working_dir: Some(root),
                ..
            } => Some(root.clone()),
            _ => None,
        };
        self.dispatch_with(
            request_message,
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::WorkspaceCreated {
                        workspace_id,
                        pane_id,
                    }) => {
                        this.sidebar.active_workspace = Some(workspace_id);
                        this.reveal_workspace(workspace_id);
                        this.focus_pane_with_snapshot(pane_id, cx);
                        this.editor.modal = Modal::None;
                        if let Some(root) = local_root.clone() {
                            this.detect_and_set_workstation_icon(workspace_id, root, cx);
                        }
                    }
                    Ok(ServiceResponse::BotCreated {
                        workspace_id,
                        pane_id,
                        ..
                    }) => {
                        this.editor.modal = Modal::None;
                        this.show_bot_pane(workspace_id, pane_id, cx);
                    }
                    Ok(response) => {
                        if let Some(dialog) = this.editor.modal.workspace_creation_mut() {
                            dialog.error = Some(format!("unexpected response: {response:?}"));
                        }
                    }
                    Err(error) => {
                        if let Some(dialog) = this.editor.modal.workspace_creation_mut() {
                            dialog.error = Some(format!("{error:#}"));
                        }
                    }
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn begin_workspace_rename(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        if let Some(workspace) = workspace {
            self.editor.modal = Modal::WorkspaceRename(WorkspaceRenameEditor {
                workspace_id,
                value: workspace.title.clone(),
                replace_on_type: true,
                bot: workspace.is_bot(),
            });
            cx.notify();
        }
    }

    pub(crate) fn submit_workspace_rename(&mut self, cx: &mut Context<Self>) {
        let Modal::WorkspaceRename(editor) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        self.dispatch(ClientRequest::RenameWorkspace {
            workspace_id: editor.workspace_id,
            title: editor.value,
        });
        cx.notify();
    }

    pub(crate) fn begin_workspace_dir_edit(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| {
                    (
                        workspace.connection.clone(),
                        effective_working_dir(&snapshot.workspaces, workspace_id)
                            .map(str::to_owned),
                    )
                })
        });
        let Some((connection, working_dir)) = workspace else {
            self.editor.modal = Modal::None;
            cx.notify();
            return;
        };
        match connection {
            WorkspaceConnection::Local => {
                self.editor.modal = Modal::None;
                self.prompt_local_directory(
                    "Choose root folder",
                    move |this, dir, cx| {
                        this.dispatch(ClientRequest::SetWorkspaceWorkingDir {
                            workspace_id,
                            working_dir: Some(dir),
                        });
                        cx.notify();
                    },
                    cx,
                );
            }
            WorkspaceConnection::SystemSsh { .. } => {
                self.editor.modal = Modal::DirEditor(DirEditor {
                    target: DirEditorTarget::WorkspaceRoot(workspace_id),
                    value: working_dir.unwrap_or_else(|| "/".to_owned()),
                    replace_on_type: true,
                    suggestions: Vec::new(),
                });
                cx.notify();
            }
        }
    }

    pub(crate) fn submit_dir_editor(&mut self, cx: &mut Context<Self>) {
        let Modal::DirEditor(editor) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        if let Err(error) = validate_workspace_dir(&editor.value) {
            self.report(&anyhow::Error::from(error));
            self.editor.modal = Modal::DirEditor(DirEditor {
                replace_on_type: false,
                ..editor
            });
            cx.notify();
            return;
        }
        match editor.target {
            DirEditorTarget::WorkspaceRoot(workspace_id) => {
                self.dispatch(ClientRequest::SetWorkspaceWorkingDir {
                    workspace_id,
                    working_dir: Some(editor.value),
                });
            }
        }
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn complete_remote_directory(
        &mut self,
        workspace_id: Uuid,
        _cx: &mut Context<Self>,
    ) {
        let Some(value) = self
            .editor
            .modal
            .dir_editor()
            .map(|editor| editor.value.clone())
        else {
            return;
        };
        let (directory, partial) = remote_completion_parts(&value);
        self.dispatch_with(
            ClientRequest::ListRemoteDirectory {
                workspace_id,
                path: directory.clone(),
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::RemoteDirectory { entries, .. }) => {
                        let matches = entries
                            .into_iter()
                            .filter(|entry| entry.starts_with(&partial))
                            .collect::<Vec<_>>();
                        if let Some(editor) = this.editor.modal.dir_editor_mut() {
                            match matches.as_slice() {
                                [] => {
                                    editor.suggestions = vec!["(no matches)".to_owned()];
                                }
                                [only] => {
                                    editor.value = format!("{directory}{only}/");
                                    editor.replace_on_type = false;
                                    editor.suggestions.clear();
                                }
                                _ => {
                                    let prefix = longest_common_prefix(&matches);
                                    editor.value = format!("{directory}{prefix}");
                                    editor.replace_on_type = false;
                                    editor.suggestions = matches.into_iter().take(8).collect();
                                }
                            }
                        }
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    pub(crate) fn handle_dir_editor_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        if keystroke.key == "tab" {
            let workspace_id = self
                .editor
                .modal
                .dir_editor()
                .map(|editor| match editor.target {
                    DirEditorTarget::WorkspaceRoot(workspace_id) => workspace_id,
                });
            let ssh_workspace = workspace_id.filter(|workspace_id| {
                self.session.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.workspaces.iter().any(|workspace| {
                        workspace.id == *workspace_id
                            && matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. })
                    })
                })
            });
            if let Some(workspace_id) = ssh_workspace {
                self.complete_remote_directory(workspace_id, cx);
            }
            return;
        }
        let Some(editor) = self.editor.modal.dir_editor_mut() else {
            return;
        };
        if keystroke.modifiers.platform && keystroke.key.eq_ignore_ascii_case("a") {
            editor.replace_on_type = true;
        } else {
            match keystroke.key.as_str() {
                "enter" => {
                    self.submit_dir_editor(cx);
                    return;
                }
                "escape" => self.editor.modal = Modal::None,
                "backspace" => {
                    if editor.replace_on_type {
                        editor.value.clear();
                    } else {
                        editor.value.pop();
                    }
                    editor.replace_on_type = false;
                    editor.suggestions.clear();
                }
                _ if !keystroke.modifiers.platform
                    && !keystroke.modifiers.control
                    && !keystroke.modifiers.alt =>
                {
                    if let Some(text) = &keystroke.key_char {
                        append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
                        editor.suggestions.clear();
                    }
                }
                _ => {}
            }
        }
        cx.notify();
    }

    pub(crate) fn set_workspace_pinned(
        &mut self,
        workspace_id: Uuid,
        pinned: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(ClientRequest::SetWorkspacePinned {
            workspace_id,
            pinned,
        });
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn set_tab_pinned(&mut self, tab_id: Uuid, pinned: bool, cx: &mut Context<Self>) {
        self.dispatch(ClientRequest::SetTabPinned { tab_id, pinned });
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn reorder_workspace(
        &mut self,
        workspace_id: Uuid,
        target_workspace_id: Uuid,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(ClientRequest::ReorderWorkspace {
            workspace_id,
            target_workspace_id,
            after,
        });
        self.sidebar.dragging_workspace = None;
        self.sidebar.workspace_drop_preview = None;
        self.sidebar.suppress_workspace_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    pub(crate) fn reorder_workspace_tab(
        &mut self,
        tab_id: Uuid,
        target_tab_id: Uuid,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(ClientRequest::ReorderTab {
            tab_id,
            target_tab_id,
            after,
        });
        self.sidebar.tab_drop_preview = None;
        self.sidebar.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    /// Whether a tab dragged out of `source` may move into `target`: another
    /// non-bot workstation on the same machine.
    pub(crate) fn accepts_tab_move(&self, source: Uuid, target: Uuid) -> bool {
        if source == target {
            return false;
        }
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return false;
        };
        let find = |id: Uuid| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id && !workspace.is_bot())
        };
        find(source)
            .zip(find(target))
            .is_some_and(|(source, target)| same_machine(&source.connection, &target.connection))
    }

    pub(crate) fn move_tab_to_workstation(
        &mut self,
        tab_id: Uuid,
        workspace_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(ClientRequest::MoveTabToWorkstation {
            tab_id,
            workspace_id,
        });
        self.reveal_workspace(workspace_id);
        self.sidebar.tab_drop_preview = None;
        self.sidebar.tab_drop_workspace = None;
        self.sidebar.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    pub(crate) fn move_sidebar_pane_into_tab(
        &mut self,
        source_pane: Uuid,
        target_tab: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::MovePaneIntoTab {
                source_pane,
                target_tab,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        this.layout.focused_pane = Some(source_pane);
                        this.layout.last_sizes.clear();
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        self.sidebar.tab_drop_preview = None;
        self.sidebar.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    pub(crate) fn move_sidebar_pane_to_new_tab(
        &mut self,
        source_pane: Uuid,
        target_tab: Uuid,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::MovePaneToNewTab {
                source_pane,
                target_tab,
                after,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        this.layout.focused_pane = Some(source_pane);
                        this.layout.last_sizes.clear();
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        self.sidebar.tab_drop_preview = None;
        self.sidebar.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    pub(crate) fn disconnect_workspace(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::DisconnectWorkspace { workspace_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        if this.sidebar.active_workspace == Some(workspace_id) {
                            this.sidebar.active_workspace = None;
                            this.layout.focused_pane = None;
                        }
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn open_workspace_connection_info(
        &mut self,
        workspace_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor.modal = Modal::WorkspaceConnectionInfo(WorkspaceConnectionInfo {
            workspace_id,
            position,
        });
        cx.notify();
    }

    pub(crate) fn begin_workspace_disconnect(
        &mut self,
        workspace_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        let Some(Workspace {
            title,
            connection:
                WorkspaceConnection::SystemSsh {
                    destination,
                    status: WorkspaceConnectionStatus::Connected,
                },
            ..
        }) = workspace
        else {
            return;
        };
        self.editor.modal = Modal::WorkspaceDisconnect(WorkspaceDisconnectConfirmation {
            workspace_id,
            title: title.clone(),
            destination: destination.clone(),
        });
        cx.notify();
    }

    pub(crate) fn confirm_workspace_disconnect(&mut self, cx: &mut Context<Self>) {
        let Modal::WorkspaceDisconnect(confirmation) = std::mem::take(&mut self.editor.modal)
        else {
            return;
        };
        self.disconnect_workspace(confirmation.workspace_id, cx);
    }

    pub(crate) fn reconnect_workspace(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::ReconnectWorkspace { workspace_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::PaneCreated { pane_id }) => {
                        this.sidebar.active_workspace = Some(workspace_id);
                        this.focus_pane_with_snapshot(pane_id, cx);
                    }
                    Ok(response) => {
                        this.report_unexpected(&response);
                    }
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn begin_workspace_delete(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        if let Some((workspace, snapshot)) = workspace.zip(self.session.snapshot.as_ref())
            && !workspace.home
        {
            let nested = workstation_descendants(&snapshot.workspaces, workspace_id);
            let active_terminal_count = snapshot
                .workspaces
                .iter()
                .filter(|candidate| candidate.id == workspace_id || nested.contains(&candidate.id))
                .map(|candidate| candidate.active_terminal_count)
                .sum();
            self.editor.modal = Modal::WorkspaceDelete(WorkspaceDeleteConfirmation {
                workspace_id,
                title: workspace.title.clone(),
                active_terminal_count,
                nested_count: nested.len(),
                bot: workspace.is_bot(),
            });
            cx.notify();
        }
    }

    pub(crate) fn scan_tmux_sessions(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.editor.modal = Modal::None;
        self.dispatch_with(
            ClientRequest::ScanTmuxSessions { workspace_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::TmuxSessions {
                        scope,
                        sessions,
                        open_session_ids,
                        no_server,
                    }) => {
                        this.editor.modal = Modal::TmuxPicker(TmuxSessionPicker {
                            workspace_id,
                            scope,
                            sessions,
                            open_session_ids: open_session_ids.into_iter().collect(),
                            no_server,
                            selected_session_ids: HashSet::new(),
                            status: None,
                            error: None,
                        });
                        this.session.connection_error = None;
                    }
                    Ok(response) => {
                        this.editor.modal = Modal::TmuxPicker(TmuxSessionPicker {
                            workspace_id,
                            scope: TmuxScanScope::Local,
                            sessions: Vec::new(),
                            open_session_ids: HashSet::new(),
                            no_server: false,
                            selected_session_ids: HashSet::new(),
                            status: None,
                            error: Some(format!("unexpected scan response: {response:?}")),
                        });
                    }
                    Err(error) => {
                        this.editor.modal = Modal::TmuxPicker(TmuxSessionPicker {
                            workspace_id,
                            scope: TmuxScanScope::Local,
                            sessions: Vec::new(),
                            open_session_ids: HashSet::new(),
                            no_server: false,
                            selected_session_ids: HashSet::new(),
                            status: None,
                            error: Some(error.to_string()),
                        });
                    }
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn mutate_tmux_selection(
        &mut self,
        change: TmuxSelectionChange,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = self.editor.modal.tmux_picker_mut() {
            match change {
                TmuxSelectionChange::Session(session_id) => picker.toggle_session(&session_id),
                TmuxSelectionChange::All => picker.select_all_sessions(),
                TmuxSelectionChange::None => picker.clear_all_sessions(),
            }
            picker.status = None;
            picker.error = None;
            cx.notify();
        }
    }

    pub(crate) fn open_selected_tmux_sessions(&mut self, _cx: &mut Context<Self>) {
        let Some((workspace_id, session_ids)) = self.editor.modal.tmux_picker_mut().map(|picker| {
            (
                picker.workspace_id,
                picker.selected_session_ids_in_scan_order(),
            )
        }) else {
            return;
        };
        if session_ids.is_empty() {
            return;
        }
        self.dispatch_with(
            ClientRequest::AttachTmuxSessions {
                workspace_id,
                session_ids,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::TmuxSessionsAttached { pane_ids, skipped }) => {
                        let opened = pane_ids.len();
                        this.sidebar.active_workspace = Some(workspace_id);
                        if let Some(pane_id) = pane_ids.last().copied() {
                            this.focus_pane_with_snapshot(pane_id, cx);
                        }
                        if skipped.is_empty() {
                            this.editor.modal = Modal::None;
                        } else if let Some(picker) = this.editor.modal.tmux_picker_mut() {
                            picker.selected_session_ids = skipped
                                .iter()
                                .map(|issue| issue.session_id.clone())
                                .collect();
                            let detail = skipped
                                .iter()
                                .map(|issue| format!("{} ({})", issue.session_id, issue.message))
                                .collect::<Vec<_>>()
                                .join(", ");
                            picker.status = Some(if opened == 0 {
                                format!("No tmux tabs opened. Skipped: {detail}")
                            } else {
                                format!("Opened {opened} tmux tab(s). Skipped: {detail}")
                            });
                            picker.error = None;
                        }
                    }
                    Ok(response) => {
                        if let Some(picker) = this.editor.modal.tmux_picker_mut() {
                            picker.error =
                                Some(format!("unexpected tmux open response: {response:?}"));
                        }
                    }
                    Err(error) => {
                        if let Some(picker) = this.editor.modal.tmux_picker_mut() {
                            picker.error = Some(error.to_string());
                        }
                    }
                }
                cx.notify();
            }),
        );
    }

    pub(crate) fn confirm_workspace_delete(&mut self, cx: &mut Context<Self>) {
        let Modal::WorkspaceDelete(confirmation) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        let workspace_id = confirmation.workspace_id;
        self.dispatch_with(
            ClientRequest::DeleteWorkspace { workspace_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        if this.sidebar.active_workspace == Some(workspace_id) {
                            this.sidebar.active_workspace = None;
                            this.layout.focused_pane = None;
                        }
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{longest_common_prefix, remote_completion_parts};

    #[test]
    fn remote_completion_splits_paths_and_extends_common_prefix() {
        assert_eq!(
            remote_completion_parts("/srv/pro"),
            ("/srv/".to_owned(), "pro".to_owned())
        );
        assert_eq!(
            remote_completion_parts("/srv/"),
            ("/srv/".to_owned(), String::new())
        );
        assert_eq!(
            longest_common_prefix(&["project-a".to_owned(), "project-b".to_owned()]),
            "project-"
        );
    }
}
