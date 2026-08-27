//! The root Render implementation.
use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseMoveEvent,
    ParentElement, Render, Styled, Window, div, px, rgb,
};

use crate::HhApp;
use crate::commands::{AppCommand, ROOT_KEY_CONTEXT};
use crate::elements::{SidebarResizeCaptureElement, TerminalInputElement};
use crate::view_models::{ColorTarget, DialogAction, Modal};
use crate::{
    ConsumeChordPrefix, EqualizePanes, FocusDown, FocusLeft, FocusRight, FocusUp, NewBrowserTab,
    NewTab, NewWorkspace, PaneDrag, ReattachPane, RetryTerminalInput, ShowCommandPalette,
    ShowNotifications, ShowSettings, SplitDown, SplitRight, THEME, TerminalZoomIn, TerminalZoomOut,
    TogglePaneZoom, ToggleSidebar, ToggleVoiceMic,
};

impl Render for HhApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_window_geometry(window);

        // The workspace dialog has its own focus targets. A pointer click on
        // the sidebar button must not leave native text input attached to the
        // terminal behind the dialog.
        if let Some(dialog) = self.editor.modal.workspace_creation() {
            self.editor.workspace_input_focus[dialog.field.index()].focus(window);
        } else if self.editor.browser_url_editor.is_some()
            || self.editor.modal.pane_rename().is_some()
            || self.editor.modal.workspace_rename().is_some()
            || self.editor.modal.group_rename().is_some()
            || self.editor.modal.dir_editor().is_some()
        {
            // Keep custom text editors on the root input route so native child
            // views cannot consume replacement typing.
            self.focus_handle.focus(window);
        }
        let modal_element = match &self.editor.modal {
            Modal::None | Modal::AppearanceSettings | Modal::Search(_) => None,
            Modal::CommandPalette(palette) => Some(self.render_command_palette(palette, cx)),
            Modal::WorkspaceCreation(dialog) => {
                Some(self.render_workspace_creation_dialog(dialog, cx))
            }
            Modal::WorkspaceRename(editor) => Some(self.render_rename_dialog(
                None,
                editor.value.clone(),
                "Rename workstation",
                "save-workspace-rename",
                DialogAction::RenameWorkspace,
                cx,
            )),
            Modal::DirEditor(editor) => Some(self.render_dir_editor_dialog(editor, cx)),
            Modal::PaneRename(editor) => Some(self.render_rename_dialog(
                Some(("terminal-rename-input", editor.replace_on_type)),
                format!("{}{}", editor.value, self.editor.ime_preedit),
                "Rename terminal",
                "save-rename",
                DialogAction::RenamePane,
                cx,
            )),
            Modal::GroupRename(editor) => Some(self.render_rename_dialog(
                Some(("group-rename-input", editor.replace_on_type)),
                format!("{}{}", editor.value, self.editor.ime_preedit),
                "Rename group",
                "save-group-rename",
                DialogAction::RenameTab,
                cx,
            )),
            Modal::WorkspaceDelete(confirmation) => {
                Some(self.render_workspace_delete_dialog(confirmation, cx))
            }
            Modal::TmuxPicker(picker) => Some(self.render_tmux_session_picker(picker, cx)),
            Modal::WorkspaceDisconnect(confirmation) => {
                Some(self.render_workspace_disconnect_dialog(confirmation, cx))
            }
            Modal::Close(confirmation) => Some(self.render_close_dialog(confirmation, cx)),
            Modal::TabClose(confirmation) => Some(self.render_tab_close_dialog(confirmation, cx)),
            Modal::TabMenu(menu) => Some(self.render_tab_menu(*menu, cx)),
            Modal::WorkspaceMenu(menu) => Some(self.render_workspace_menu(*menu, cx)),
            Modal::CreateMenu(menu) => Some(self.render_create_menu(*menu, cx)),
            Modal::GroupMenu(menu) => Some(self.render_group_menu(*menu, cx)),
            Modal::WorkspaceConnectionInfo(info) => {
                Some(self.render_workspace_connection_info(info, cx))
            }
        };

        div()
            .key_context(if self.editor.modal.command_palette().is_some() {
                "HhPalette"
            } else {
                ROOT_KEY_CONTEXT
            })
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .min_w(px(720.0))
            .min_h(px(460.0))
            .bg(rgb(THEME.window))
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx)
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.handle_resize(event, window, cx)
            }))
            .on_drag_move::<PaneDrag>(cx.listener(
                |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    this.layout.dragging_pane = Some(event.drag(cx).pane_id);
                    this.layout.drag_hover.clear();
                    cx.notify();
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_resize(cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if matches!(
                        this.editor.modal,
                        Modal::TabMenu(_)
                            | Modal::WorkspaceMenu(_)
                            | Modal::CreateMenu(_)
                            | Modal::GroupMenu(_)
                            | Modal::WorkspaceConnectionInfo(_)
                    ) {
                        this.editor.modal = Modal::None;
                        cx.notify();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &NewWorkspace, _, cx| {
                this.execute_command(AppCommand::NewWorkspace, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.execute_command(AppCommand::ToggleSidebar, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| {
                this.execute_command(AppCommand::NewTab, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &NewBrowserTab, _, cx| {
                this.execute_command(AppCommand::NewBrowserTab, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &TerminalZoomIn, _, cx| {
                this.execute_command(AppCommand::TerminalZoomIn, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &TerminalZoomOut, _, cx| {
                this.execute_command(AppCommand::TerminalZoomOut, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| {
                this.execute_command(AppCommand::SplitRight, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                this.execute_command(AppCommand::SplitDown, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusLeft, _, cx| {
                this.execute_command(AppCommand::FocusLeft, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusUp, _, cx| {
                this.execute_command(AppCommand::FocusUp, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusRight, _, cx| {
                this.execute_command(AppCommand::FocusRight, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusDown, _, cx| {
                this.execute_command(AppCommand::FocusDown, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ShowCommandPalette, _, cx| {
                this.execute_command(AppCommand::ShowCommandPalette, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &TogglePaneZoom, _, cx| {
                this.execute_command(AppCommand::TogglePaneZoom, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &EqualizePanes, _, cx| {
                this.execute_command(AppCommand::EqualizePanes, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ReattachPane, _, cx| {
                this.execute_command(AppCommand::ReattachPane, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &RetryTerminalInput, _, cx| {
                this.execute_command(AppCommand::RetryTerminalInput, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ShowNotifications, _, cx| {
                this.execute_command(AppCommand::ShowNotifications, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ToggleVoiceMic, _, cx| {
                this.execute_command(AppCommand::ToggleVoiceMic, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ShowSettings, _, cx| {
                this.execute_command(AppCommand::ShowSettings, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|_: &mut HhApp, _: &ConsumeChordPrefix, _, cx| {
                cx.stop_propagation();
            }))
            .on_action(cx.listener(HhApp::copy_terminal))
            .on_action(cx.listener(HhApp::paste_terminal))
            .on_action(cx.listener(HhApp::find_terminal))
            .on_action(cx.listener(HhApp::find_next_terminal))
            .child(
                div()
                    .absolute()
                    .w(px(1.0))
                    .h(px(1.0))
                    .child(TerminalInputElement { input: cx.entity() }),
            )
            .when(self.sidebar.sidebar_resize.is_active(), |element| {
                element.child(
                    div()
                        .absolute()
                        .w(px(1.0))
                        .h(px(1.0))
                        .child(SidebarResizeCaptureElement { input: cx.entity() }),
                )
            })
            // The global navigation shares the macOS titlebar row. The rail
            // begins directly beneath it instead of rendering under traffic
            // lights or under a redundant second bar.
            .child(self.render_global_navigation(cx))
            .child(
                div()
                    .relative()
                    .min_h(px(0.0))
                    .flex_1()
                    .flex()
                    .when(self.sidebar.sidebar_visible, |element| {
                        element
                            .child(self.render_sidebar(cx))
                            .child(self.render_sidebar_resize_handle(cx))
                    })
                    .child(self.render_workspace(cx)),
            )
            .when_some(modal_element, |element, modal| element.child(modal))
            .when_some(
                self.editor.color_picker.as_ref().filter(|picker| {
                    matches!(
                        picker.target,
                        ColorTarget::DefaultTerminal | ColorTarget::DefaultWorkspace
                    )
                }),
                |element, picker| element.child(self.render_color_picker(picker, cx)),
            )
    }
}
