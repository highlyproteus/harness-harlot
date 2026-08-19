//! Pane lifecycle: create, split, move, close, input, and search.

use gpui::{
    ClipboardItem, Context, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ScrollWheelEvent, Window, px,
};
use hh_protocol::{
    ClientRequest, DropPlacement, HistoryPageDirection, HistoryPageFlags, Pane, ServiceResponse,
    SplitAxis, TerminalModes, TerminalMouseAction, TerminalMouseButton, TerminalPoint,
    TerminalSelectionKind,
};

use crate::helpers::{
    WorkspaceTabScope, append_rename_text, apply_layout_control_mutation, collect_terminal_tabs,
    constrained_sidebar_width, effective_split_ratio, find_pane, find_split_rect, prepare_paste,
    terminal_modifiers, terminal_mouse_button, visible_panes, workspace_tab_set,
};
use crate::typography::{TerminalCellMetrics, adjusted_terminal_zoom_level};
use crate::view_models::{
    ArchivedView, CloseConfirmation, GroupRenameEditor, LayoutControlMutation, Modal, PixelRect,
    RenameEditor, SearchEditor, SelectionDrag, SidebarResizeMove, TabCloseConfirmation,
    WorkspaceCreationStep, route_workspace_creation_paste,
};
use crate::{
    APP_CHROME_HEIGHT, CopyTerminal, FindNextTerminal, FindTerminal, HhApp, PasteTerminal,
};
use uuid::Uuid;

impl HhApp {
    pub(crate) fn new_tab(&mut self, cx: &mut Context<Self>) {
        let Some((workspace_id, scope, empty)) = self
            .session
            .snapshot
            .as_ref()
            .and_then(|snapshot| self.active_workspace_in(snapshot))
            .map(|workspace| {
                (
                    workspace.id,
                    workspace_tab_set(workspace, self.sidebar.workspace_tab_scope).scope,
                    workspace.tabs.is_empty(),
                )
            })
        else {
            return;
        };
        if empty {
            self.open_workspace_terminal(workspace_id, cx);
            return;
        }
        match scope {
            WorkspaceTabScope::Workstation => self.new_workspace_tab(workspace_id, cx),
            WorkspaceTabScope::Project(project_id) => {
                self.new_project_group(workspace_id, project_id, cx);
            }
        }
    }

    pub(crate) fn focus_created_pane(
        &mut self,
        workspace_id: Uuid,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.expanded_workspaces.insert(workspace_id);
        self.sidebar.active_workspace = Some(workspace_id);
        self.focus_pane_with_snapshot(pane_id, cx);
        cx.notify();
    }

    pub(crate) fn focus_created_pane_inferred(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        if let Some(workspace_id) = self.workspace_id_for_pane(pane_id) {
            self.sidebar.expanded_workspaces.insert(workspace_id);
            self.sidebar.active_workspace = Some(workspace_id);
        }
        self.focus_pane_with_snapshot(pane_id, cx);
        cx.notify();
    }

    pub(crate) fn open_workspace_terminal(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::CreateWorkspaceTerminal { workspace_id },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    this.focus_created_pane(workspace_id, pane_id, cx);
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn new_workspace_tab(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::CreateWorkspaceTab { workspace_id },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    this.focus_created_pane(workspace_id, pane_id, cx);
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        self.layout.last_sizes.clear();
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn new_workspace_group(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::CreateWorkspaceGroup {
                workspace_id,
                parent_tab: None,
            },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    this.focus_created_pane(workspace_id, pane_id, cx);
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        self.layout.last_sizes.clear();
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn new_tab_at(&mut self, target_pane: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::CreateGroupTerminal { target_pane },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    this.focus_created_pane_inferred(pane_id, cx);
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        cx.notify();
    }

    pub(crate) fn split(&mut self, axis: SplitAxis, cx: &mut Context<Self>) {
        if let Some(target_pane) = self.layout.focused_pane {
            self.split_at(target_pane, axis, cx);
        }
    }

    pub(crate) fn split_at(&mut self, target_pane: Uuid, axis: SplitAxis, cx: &mut Context<Self>) {
        self.layout.zoomed_pane = None;
        self.dispatch_with(
            ClientRequest::CreatePane { target_pane, axis },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    this.focus_created_pane_inferred(pane_id, cx);
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn activate_tab(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::ActivateTab { pane_id },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::Ack) => {
                    // Commit the click intent before applying any in-flight snapshot.
                    // Otherwise a stale stack snapshot reasserts the previously
                    // focused sibling and the clicked inner tab immediately reverts.
                    this.layout.focused_pane = Some(pane_id);
                    this.focus_pane_with_snapshot(pane_id, cx);
                }
                Ok(response) => {
                    this.report_unexpected(&response);
                }
                Err(error) => this.report(&error),
            }),
        );
        self.layout.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    pub(crate) fn swap_panes(
        &mut self,
        source_pane: Uuid,
        target_pane: Uuid,
        cx: &mut Context<Self>,
    ) {
        if source_pane != target_pane {
            self.layout.zoomed_pane = None;
            self.dispatch(ClientRequest::SwapPanes {
                source_pane,
                target_pane,
            });
            self.focus_pane_with_snapshot(source_pane, cx);
            self.layout.last_sizes.clear();
            cx.notify();
        }
    }

    pub(crate) fn move_pane_to_split(
        &mut self,
        source_pane: Uuid,
        target_pane: Uuid,
        placement: DropPlacement,
        cx: &mut Context<Self>,
    ) {
        self.layout.dragging_pane = None;
        self.layout.drag_hover.clear();
        self.layout.zoomed_pane = None;
        self.dispatch(ClientRequest::MovePaneToSplit {
            source_pane,
            target_pane,
            placement,
        });
        self.focus_pane_with_snapshot(source_pane, cx);
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn move_pane_to_tab(
        &mut self,
        source_pane: Uuid,
        target_pane: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.layout.dragging_pane = None;
        self.layout.drag_hover.clear();
        self.layout.zoomed_pane = None;
        self.dispatch(ClientRequest::MovePaneToTab {
            source_pane,
            target_pane,
        });
        self.focus_pane_with_snapshot(source_pane, cx);
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn pane_metadata(&self, pane_id: Uuid) -> Option<Pane> {
        self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find_map(|tab| find_pane(&tab.layout, pane_id).cloned())
        })
    }

    pub(crate) fn group_metadata(&self, tab_id: Uuid) -> Option<(String, Uuid)> {
        self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| {
                    let pane_id = visible_panes(&tab.layout).first().copied().or_else(|| {
                        let mut panes = Vec::new();
                        collect_terminal_tabs(&tab.layout, &mut panes);
                        panes.first().map(|pane| pane.id)
                    })?;
                    Some((
                        tab.custom_title
                            .clone()
                            .unwrap_or_else(|| tab.title.clone()),
                        pane_id,
                    ))
                })
        })
    }

    pub(crate) fn new_group_terminal(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let target_pane = self.group_metadata(tab_id).map(|(_, pane_id)| pane_id);
        self.editor.modal = Modal::None;
        if let Some(target_pane) = target_pane {
            self.new_tab_at(target_pane, cx);
        } else {
            cx.notify();
        }
    }

    pub(crate) fn new_project_group(
        &mut self,
        workspace_id: Uuid,
        tab_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::CreateWorkspaceGroup {
                workspace_id,
                parent_tab: Some(tab_id),
            },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    this.focus_created_pane(workspace_id, pane_id, cx);
                    this.sidebar.collapsed_groups.remove(&tab_id);
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        self.editor.modal = Modal::None;
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn begin_group_rename(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        if let Some((label, _)) = self.group_metadata(tab_id) {
            self.editor.modal = Modal::GroupRename(GroupRenameEditor {
                tab_id,
                value: label,
                replace_on_type: true,
            });
            cx.notify();
        }
    }

    pub(crate) fn begin_rename(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focus_pane_with_snapshot(pane_id, cx);
        if let Some(pane) = self.pane_metadata(pane_id) {
            self.editor.modal = Modal::PaneRename(RenameEditor {
                pane_id,
                value: pane.title,
                replace_on_type: true,
            });
            cx.notify();
        }
    }

    pub(crate) fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Modal::PaneRename(editor) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        self.dispatch(ClientRequest::RenamePane {
            pane_id: editor.pane_id,
            title: editor.value,
        });
        cx.notify();
    }

    pub(crate) fn submit_group_rename(&mut self, cx: &mut Context<Self>) {
        let Modal::GroupRename(editor) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        self.dispatch(ClientRequest::RenameTab {
            tab_id: editor.tab_id,
            title: editor.value,
        });
        cx.notify();
    }

    pub(crate) fn begin_close(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focus_pane_with_snapshot(pane_id, cx);
        if let Some(pane) = self.pane_metadata(pane_id) {
            let leaves_workspace_empty = self.session.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.workspaces.iter().any(|workspace| {
                    let panes = workspace
                        .tabs
                        .iter()
                        .flat_map(|tab| visible_panes(&tab.layout))
                        .collect::<Vec<_>>();
                    panes.len() == 1 && panes[0] == pane_id
                })
            });
            self.editor.modal =
                Modal::Close(CloseConfirmation::for_pane(&pane, leaves_workspace_empty));
            cx.notify();
        }
    }

    pub(crate) fn begin_tab_close(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let confirmation = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot.workspaces.iter().find_map(|workspace| {
                let tab = workspace.tabs.iter().find(|tab| tab.id == tab_id)?;
                let child_count = workspace
                    .tabs
                    .iter()
                    .filter(|candidate| candidate.parent_tab == Some(tab_id))
                    .count();
                let mut panes = Vec::new();
                for candidate in workspace.tabs.iter().filter(|candidate| {
                    candidate.id == tab_id || candidate.parent_tab == Some(tab_id)
                }) {
                    collect_terminal_tabs(&candidate.layout, &mut panes);
                }
                Some(TabCloseConfirmation {
                    tab_id,
                    title: tab
                        .custom_title
                        .clone()
                        .unwrap_or_else(|| tab.title.clone()),
                    is_project: tab.project_dir.is_some(),
                    child_count,
                    terminal_count: panes.len(),
                })
            })
        });
        if let Some(confirmation) = confirmation {
            self.editor.modal = Modal::TabClose(confirmation);
            cx.notify();
        }
    }

    pub(crate) fn confirm_tab_close(&mut self, cx: &mut Context<Self>) {
        let Modal::TabClose(confirmation) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        self.dispatch_with(
            ClientRequest::CloseTab {
                tab_id: confirmation.tab_id,
            },
            Box::new(|this, cx, result| match result {
                Ok(ServiceResponse::Ack) => {
                    this.layout.last_sizes.clear();
                    cx.notify();
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        cx.notify();
    }

    pub(crate) fn confirm_close(&mut self, cx: &mut Context<Self>) {
        let Modal::Close(confirmation) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        self.dispatch(confirmation.request());
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn focus_direction(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.session.snapshot else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return;
        };
        let panes = visible_panes(layout);
        let Some(current) = self.layout.focused_pane else {
            return;
        };
        let Some(index) = panes.iter().position(|pane| *pane == current) else {
            return;
        };
        let next = if forward {
            (index + 1) % panes.len()
        } else if index == 0 {
            panes.len() - 1
        } else {
            index - 1
        };
        self.focus_pane_with_snapshot(panes[next], cx);
        if self.layout.zoomed_pane.is_some() {
            self.layout.zoomed_pane = self.layout.focused_pane;
            self.layout.last_sizes.clear();
            self.sync_pty_sizes(cx);
        }
        cx.notify();
    }

    pub(crate) fn terminal_metrics(&self, pane_id: Uuid) -> TerminalCellMetrics {
        self.terminal_font.metrics_for_zoom_level(
            self.terminal_zoom_levels
                .get(&pane_id)
                .copied()
                .unwrap_or_default(),
        )
    }

    pub(crate) fn adjust_terminal_zoom(&mut self, delta: i8, cx: &mut Context<Self>) {
        let Some(pane_id) = self.layout.focused_pane else {
            return;
        };
        let current = self
            .terminal_zoom_levels
            .get(&pane_id)
            .copied()
            .unwrap_or_default();
        let next = adjusted_terminal_zoom_level(current, delta);
        if next == current {
            return;
        }
        if next == 0 {
            self.terminal_zoom_levels.remove(&pane_id);
        } else {
            self.terminal_zoom_levels.insert(pane_id, next);
        }
        self.layout.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    pub(crate) fn toggle_pane_zoom(&mut self, cx: &mut Context<Self>) {
        let Some(focused) = self.layout.focused_pane else {
            return;
        };
        self.layout.zoomed_pane = if self.layout.zoomed_pane == Some(focused) {
            None
        } else {
            Some(focused)
        };
        self.layout.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    /// Respawns an exited terminal in place. For a tmux tab this re-attaches
    /// the same session; the PTY starts at the service's default size, so the
    /// pane geometry must be pushed again for tmux to redraw at full size.
    pub(crate) fn reattach_pane(&mut self, pane_id: Uuid, _cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::ReattachPane { pane_id },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => {
                        if let Some(state) = this.session.pane_states.get_mut(&pane_id) {
                            state.exited = false;
                        }
                        this.focus_pane_with_snapshot(pane_id, cx);
                        this.layout.last_sizes.clear();
                        this.sync_pty_sizes(cx);
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    pub(crate) fn equalize_panes(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot).cloned() else {
            return;
        };
        if apply_layout_control_mutation(
            &layout,
            &mut self.layout.split_ratios,
            LayoutControlMutation::Equalize,
        ) > 0
        {
            self.layout.last_sizes.clear();
            self.sync_pty_sizes(cx);
            cx.notify();
        }
    }

    pub(crate) fn copy_terminal(
        &mut self,
        _: &CopyTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self
            .editor
            .browser_url_editor
            .as_ref()
            .filter(|editor| editor.replace_on_type)
        {
            cx.write_to_clipboard(ClipboardItem::new_string(editor.text.clone()));
            return;
        }
        if let Some(text) = self
            .editor
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
            .and_then(|dialog| dialog.active_editor().selected_text())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
            return;
        }
        let Some(pane_id) = self.layout.focused_pane else {
            return;
        };
        self.dispatch_with(
            ClientRequest::CopySelection { pane_id },
            Box::new(|this, cx, result| {
                match result {
                    Ok(ServiceResponse::SelectionText { text: Some(text) }) if !text.is_empty() => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        this.session.connection_error = None;
                    }
                    Ok(ServiceResponse::SelectionText { .. }) => {}
                    Ok(response) => {
                        this.report_unexpected(&response);
                    }
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    pub(crate) fn paste_terminal(
        &mut self,
        _: &PasteTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if route_workspace_creation_paste(self.editor.modal.workspace_creation_mut(), &text) {
            cx.notify();
            return;
        }
        if self.editor.browser_url_editor.is_some() {
            self.append_browser_url_text(&text);
            cx.notify();
            return;
        }
        let Some(pane_id) = self.layout.focused_pane else {
            return;
        };
        let bracketed = self
            .session
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::BRACKETED_PASTE));
        match prepare_paste(&text, bracketed) {
            Ok(bytes) => self.dispatch_control(ClientRequest::WriteInput { pane_id, bytes }),
            Err(message) => self.session.connection_error = Some(message.to_owned()),
        }
        cx.notify();
    }

    pub(crate) fn find_terminal(
        &mut self,
        _: &FindTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.editor.modal, Modal::None | Modal::Search(_)) {
            return;
        }
        self.editor.modal = Modal::Search(SearchEditor::default());
        self.editor.ime_preedit.clear();
        cx.notify();
    }

    pub(crate) fn find_next_terminal(
        &mut self,
        _: &FindNextTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_search(true, cx);
    }

    pub(crate) fn run_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(pane_id) = self.layout.focused_pane else {
            return;
        };
        let Some(editor) = self.editor.modal.search() else {
            return;
        };
        if editor.query.is_empty() {
            if let Some(editor) = self.editor.modal.search_mut() {
                editor.no_match = false;
            }
            cx.notify();
            return;
        }
        let query = editor.query.clone();
        self.dispatch_with(
            ClientRequest::SearchPane {
                pane_id,
                query: query.clone(),
                forward,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::SearchResult { found: false }) => {
                        this.search_archived_history(pane_id, query);
                        this.session.connection_error = None;
                    }
                    Ok(ServiceResponse::SearchResult { found: true }) => {
                        if let Some(editor) = this.editor.modal.search_mut() {
                            editor.no_match = false;
                            this.editor.archived_views.remove(&pane_id);
                        }
                        this.session.connection_error = None;
                    }
                    Ok(response) => {
                        this.report_unexpected(&response);
                    }
                    Err(error) => {
                        this.report(&error);
                        this.search_archived_history(pane_id, query);
                    }
                }
                cx.notify();
            }),
        );
    }

    fn search_archived_history(&mut self, pane_id: Uuid, query: String) {
        let before = self
            .editor
            .archived_views
            .get(&pane_id)
            .map(|view| view.page.cursor);
        self.dispatch_stream_with(
            ClientRequest::SearchArchivedHistory {
                pane_id,
                query: query.clone(),
                before,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::HistorySearchResult { page: Some(page) }) => {
                        let rows = this
                            .session
                            .screens
                            .get(&pane_id)
                            .map_or(30, |screen| usize::from(screen.rows));
                        let first_line = page
                            .lines
                            .iter()
                            .position(|line| line.contains(&query))
                            .unwrap_or(0)
                            .min(page.lines.len().saturating_sub(rows));
                        this.editor.archived_views.clear();
                        this.editor
                            .archived_views
                            .insert(pane_id, ArchivedView { page, first_line });
                        if let Some(editor) = this.editor.modal.search_mut() {
                            editor.no_match = false;
                        }
                        this.session.connection_error = None;
                    }
                    Ok(ServiceResponse::HistorySearchResult { page: None }) => {
                        if let Some(editor) = this.editor.modal.search_mut() {
                            editor.no_match = true;
                        }
                        this.session.connection_error = None;
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() || text.chars().any(|character| character == '\0') {
            return;
        }
        if let Some(picker) = self.editor.color_picker.as_mut() {
            if picker.replace_on_type {
                picker.hex.clear();
            }
            let remaining = 6_usize.saturating_sub(picker.hex.len());
            picker.hex.extend(
                text.chars()
                    .filter(char::is_ascii_hexdigit)
                    .map(|character| character.to_ascii_uppercase())
                    .take(remaining),
            );
            picker.replace_on_type = false;
            picker.invalid = false;
            cx.notify();
            return;
        }
        if let Some(editor) = self.editor.history_editor.as_mut() {
            if editor.replace_on_type {
                editor.text.clear();
            }
            let remaining = 4_usize.saturating_sub(editor.text.len());
            editor
                .text
                .extend(text.chars().filter(char::is_ascii_digit).take(remaining));
            editor.replace_on_type = false;
            editor.invalid = false;
            cx.notify();
            return;
        }
        if self.append_browser_url_text(text) {
            cx.notify();
            return;
        }
        if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
            if dialog.step == WorkspaceCreationStep::Details {
                dialog.replace_text(None, text, false, None);
            }
            cx.notify();
            return;
        }
        if let Some(editor) = self.editor.modal.workspace_rename_mut() {
            append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
            cx.notify();
            return;
        }
        if let Some(editor) = self.editor.modal.pane_rename_mut() {
            append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
            cx.notify();
            return;
        }
        if let Some(editor) = self.editor.modal.group_rename_mut() {
            append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
            cx.notify();
            return;
        }
        if let Some(editor) = self.editor.modal.search_mut() {
            let remaining = 256_usize.saturating_sub(editor.query.chars().count());
            editor
                .query
                .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
            editor.no_match = false;
            cx.notify();
            self.run_search(true, cx);
            return;
        }
        if let Some(pane_id) = self.layout.focused_pane {
            self.dispatch_control(ClientRequest::WriteInput {
                pane_id,
                bytes: text.as_bytes().to_vec(),
            });
            cx.notify();
        }
    }

    pub(crate) fn begin_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane_with_snapshot(pane_id, cx);
        self.focus_handle.focus(window);
        let mouse_reporting = self
            .session
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING));
        if mouse_reporting && !event.modifiers.shift {
            if let Some(button) = terminal_mouse_button(event.button) {
                self.dispatch_control(ClientRequest::MouseInput {
                    pane_id,
                    point,
                    button,
                    action: TerminalMouseAction::Press,
                    modifiers: terminal_modifiers(event.modifiers),
                });
            }
        } else if event.button == MouseButton::Left {
            let kind = if event.modifiers.alt {
                TerminalSelectionKind::Block
            } else if event.click_count >= 3 {
                TerminalSelectionKind::Lines
            } else if event.click_count == 2 {
                TerminalSelectionKind::Semantic
            } else {
                TerminalSelectionKind::Simple
            };
            self.layout.selection_drag = Some(SelectionDrag {
                pane_id,
                anchor: point,
                preserve_single_cell: matches!(
                    kind,
                    TerminalSelectionKind::Semantic | TerminalSelectionKind::Lines
                ),
            });
            self.dispatch_control(ClientRequest::BeginSelection {
                pane_id,
                point,
                kind,
            });
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn move_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if self
            .layout
            .selection_drag
            .is_some_and(|selection| selection.pane_id == pane_id)
            && event.dragging()
        {
            self.dispatch_control(ClientRequest::UpdateSelection { pane_id, point });
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let mouse_motion = self
            .session
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_MOTION));
        if mouse_motion && let Some(button) = event.pressed_button.and_then(terminal_mouse_button) {
            self.dispatch_control(ClientRequest::MouseInput {
                pane_id,
                point,
                button,
                action: TerminalMouseAction::Move,
                modifiers: terminal_modifiers(event.modifiers),
            });
            cx.stop_propagation();
        }
    }

    pub(crate) fn end_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseUpEvent,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = self
            .layout
            .selection_drag
            .take()
            .filter(|selection| selection.pane_id == pane_id)
        {
            if point == selection.anchor && !selection.preserve_single_cell {
                self.dispatch_control(ClientRequest::ClearSelection { pane_id });
            } else {
                self.dispatch_control(ClientRequest::UpdateSelection { pane_id, point });
            }
        } else if self
            .session
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
            && let Some(button) = terminal_mouse_button(event.button)
        {
            self.dispatch_control(ClientRequest::MouseInput {
                pane_id,
                point,
                button,
                action: TerminalMouseAction::Release,
                modifiers: terminal_modifiers(event.modifiers),
            });
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn scroll_terminal(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let metrics = self.terminal_metrics(pane_id);
        let pixels = event.delta.pixel_delta(px(metrics.line_height));
        let lines = (f32::from(pixels.y) / metrics.line_height).round() as i32;
        let lines = if lines == 0 {
            if pixels.y < px(0.0) { -1 } else { 1 }
        } else {
            lines
        };
        if self.editor.archived_views.contains_key(&pane_id) {
            self.scroll_archived_view(pane_id, lines, cx);
            cx.stop_propagation();
            return;
        }
        if self
            .session
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
        {
            self.dispatch_control(ClientRequest::MouseInput {
                pane_id,
                point,
                button: if lines > 0 {
                    TerminalMouseButton::WheelUp
                } else {
                    TerminalMouseButton::WheelDown
                },
                action: TerminalMouseAction::Press,
                modifiers: terminal_modifiers(event.modifiers),
            });
        } else if lines > 0
            && self.session.screens.get(&pane_id).is_some_and(|screen| {
                screen.display_offset >= screen.history_size && screen.history_size > 0
            })
        {
            self.load_archived_page(pane_id, None, HistoryPageDirection::Older, cx);
        } else {
            self.dispatch_control(ClientRequest::ScrollPane { pane_id, lines });
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn load_archived_page(
        &mut self,
        pane_id: Uuid,
        cursor: Option<hh_protocol::HistoryCursor>,
        direction: HistoryPageDirection,
        _cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::LoadHistoryPage {
                pane_id,
                cursor,
                direction,
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::HistoryPage { page: Some(page) }) => {
                        let rows = this
                            .session
                            .screens
                            .get(&pane_id)
                            .map_or(30, |screen| usize::from(screen.rows));
                        let first_line = match direction {
                            HistoryPageDirection::Older => page.lines.len().saturating_sub(rows),
                            HistoryPageDirection::Newer => 0,
                        };
                        this.editor.archived_views.clear();
                        this.editor
                            .archived_views
                            .insert(pane_id, ArchivedView { page, first_line });
                        this.session.connection_error = None;
                    }
                    Ok(ServiceResponse::HistoryPage { page: None }) => {
                        if direction == HistoryPageDirection::Newer {
                            this.editor.archived_views.remove(&pane_id);
                        }
                    }
                    Ok(response) => {
                        this.report_unexpected(&response);
                    }
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    pub(crate) fn scroll_archived_view(
        &mut self,
        pane_id: Uuid,
        lines: i32,
        cx: &mut Context<Self>,
    ) {
        let rows = self
            .session
            .screens
            .get(&pane_id)
            .map_or(30, |screen| usize::from(screen.rows));
        let Some(view) = self.editor.archived_views.get_mut(&pane_id) else {
            return;
        };
        if lines > 0 {
            let amount = usize::try_from(lines).unwrap_or(usize::MAX);
            if view.first_line > 0 {
                view.first_line = view.first_line.saturating_sub(amount);
                cx.notify();
                return;
            }
            if view.page.flags.contains(HistoryPageFlags::HAS_OLDER) {
                let cursor = view.page.cursor;
                self.load_archived_page(pane_id, Some(cursor), HistoryPageDirection::Older, cx);
            }
            return;
        }
        let amount = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        let maximum = view.page.lines.len().saturating_sub(rows);
        if view.first_line < maximum {
            view.first_line = view.first_line.saturating_add(amount).min(maximum);
            cx.notify();
            return;
        }
        if view.page.flags.contains(HistoryPageFlags::HAS_NEWER) {
            let cursor = view.page.cursor;
            self.load_archived_page(pane_id, Some(cursor), HistoryPageDirection::Newer, cx);
        } else {
            self.editor.archived_views.remove(&pane_id);
            cx.notify();
        }
    }

    pub(crate) fn handle_resize(
        &mut self,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        match self
            .sidebar
            .sidebar_resize
            .pointer_move(event.pressed_button)
        {
            SidebarResizeMove::Ignore => {}
            SidebarResizeMove::Update => {
                let window_width = f32::from(window.bounds().size.width);
                let next = constrained_sidebar_width(f32::from(event.position.x), window_width);
                if (self.sidebar.preferred_sidebar_width - next).abs() > f32::EPSILON {
                    self.sidebar.preferred_sidebar_width = next;
                    self.update_window_geometry(window);
                    self.layout.last_sizes.clear();
                    self.sync_pty_sizes(cx);
                    cx.notify();
                }
                return;
            }
            SidebarResizeMove::Complete => {
                self.persist_sidebar_width(cx);
                cx.notify();
                return;
            }
        }
        let Some(drag) = self.layout.resizing else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.layout.resizing = None;
            cx.notify();
            return;
        }
        self.update_window_geometry(window);
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return;
        };
        let root = PixelRect {
            x: 0.0,
            y: 0.0,
            width: self.layout.workspace_pixels.0,
            height: self.layout.workspace_pixels.1,
        };
        let Some(split) = find_split_rect(layout, drag.split_id, root, &self.layout.split_ratios)
        else {
            return;
        };
        let workspace_x = f32::from(event.position.x) - self.sidebar.sidebar_pixels;
        let workspace_y = f32::from(event.position.y) - APP_CHROME_HEIGHT;
        let ratio = match drag.axis {
            SplitAxis::Horizontal => (workspace_x - split.x) / split.width.max(1.0),
            SplitAxis::Vertical => (workspace_y - split.y) / split.height.max(1.0),
        };
        self.layout.split_ratios.insert(
            drag.split_id,
            effective_split_ratio(drag.axis, split.width, split.height, ratio),
        );
        self.layout.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    pub(crate) fn cancel_sidebar_resize(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(initial_width) = self.sidebar.sidebar_resize.cancel() else {
            return;
        };
        self.sidebar.preferred_sidebar_width = initial_width;
        self.update_window_geometry(window);
        self.layout.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    pub(crate) fn finish_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar.sidebar_resize.finish() {
            self.persist_sidebar_width(cx);
        }
        self.layout.resizing = None;
        self.layout.dragging_pane = None;
        self.layout.drag_hover.clear();
        cx.notify();
    }
}
