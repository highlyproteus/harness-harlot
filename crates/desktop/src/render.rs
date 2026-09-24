//! The root Render implementation.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseMoveEvent, ParentElement, Pixels, Render, Styled, Window, div, px, rgb,
};

use crate::HhApp;
use crate::commands::{AppCommand, ROOT_KEY_CONTEXT};
use crate::elements::{
    SidebarResizeCaptureElement, TerminalInputElement, TerminalSelectionCaptureElement,
};
use crate::input::browser_url_editor_is_active;
use crate::view_models::{ColorTarget, DialogAction, Modal};
use crate::{
    ConsumeChordPrefix, EqualizePanes, FocusDown, FocusLeft, FocusRight, FocusUp, NewBrowserTab,
    NewGalleryTab, NewTab, NewWorkspace, PaneDrag, ReattachPane, RetryTerminalInput, ShowBots,
    ShowCommandPalette, ShowNotifications, ShowSettings, SplitDown, SplitRight, THEME,
    TerminalZoomIn, TerminalZoomOut, TogglePaneZoom, ToggleSidebar,
};

impl HhApp {
    fn render_modal(&self, menu_max_height: Pixels, cx: &mut Context<Self>) -> Option<AnyElement> {
        match &self.editor.modal {
            Modal::None | Modal::AppearanceSettings | Modal::Search(_) => None,
            Modal::CommandPalette(palette) => Some(self.render_command_palette(palette, cx)),
            Modal::WorkspaceCreation(dialog) => {
                Some(self.render_workspace_creation_dialog(dialog, cx))
            }
            Modal::WorkspaceRename(editor) => Some(self.render_rename_dialog(
                Some(("workspace-rename-input", editor.replace_on_type)),
                format!("{}{}", editor.value, self.editor.ime_preedit),
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
                if editor.bot {
                    "Rename bot"
                } else {
                    "Rename group"
                },
                "save-group-rename",
                DialogAction::RenameTab,
                cx,
            )),
            Modal::WorkspaceDelete(confirmation) => {
                Some(self.render_workspace_delete_dialog(confirmation, cx))
            }
            Modal::UpdateRestart(confirmation) => {
                Some(self.render_update_restart_dialog(confirmation, cx))
            }
            Modal::TmuxPicker(picker) => Some(self.render_tmux_session_picker(picker, cx)),
            Modal::WorkspaceDisconnect(confirmation) => {
                Some(self.render_workspace_disconnect_dialog(confirmation, cx))
            }
            Modal::Close(confirmation) => Some(self.render_close_dialog(confirmation, cx)),
            Modal::TabClose(confirmation) => Some(self.render_tab_close_dialog(confirmation, cx)),
            Modal::TabMenu(menu) => Some(self.render_tab_menu(*menu, menu_max_height, cx)),
            Modal::WorkspaceMenu(menu) => {
                Some(self.render_workspace_menu(*menu, menu_max_height, cx))
            }
            Modal::CreateMenu(menu) => Some(self.render_create_menu(*menu, cx)),
            Modal::GroupMenu(menu) => Some(self.render_group_menu(*menu, menu_max_height, cx)),
            Modal::BotMenu(menu) => Some(self.render_bot_menu(*menu, menu_max_height, cx)),
            Modal::WorkspaceConnectionInfo(info) => {
                Some(self.render_workspace_connection_info(info, cx))
            }
        }
    }
}

impl Render for HhApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_window_geometry(window);
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        self.schedule_browser_presentation(window, cx);

        // The workspace dialog has its own focus targets. A pointer click on
        // the sidebar button must not leave native text input attached to the
        // terminal behind the dialog.
        if let Some(dialog) = self.editor.modal.workspace_creation() {
            self.editor.workspace_input_focus[dialog.field.index()].focus(window);
        } else if browser_url_editor_is_active(
            self.editor
                .browser_url_editor
                .as_ref()
                .map(|editor| editor.pane_id),
            self.layout.focused_pane,
        ) || self.editor.modal.pane_rename().is_some()
            || self.editor.modal.workspace_rename().is_some()
            || self.editor.modal.group_rename().is_some()
            || self.editor.modal.dir_editor().is_some()
        {
            // Keep custom text editors on the root input route so native child
            // views cannot consume replacement typing.
            self.focus_handle.focus(window);
        }
        let menu_max_height = window.viewport_size().height - px(16.0);
        let modal_element = self.render_modal(menu_max_height, cx);

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
                    let next = Some(event.drag(cx).pane_id);
                    if this.layout.dragging_pane != next {
                        this.layout.dragging_pane = next;
                        this.layout.drag_hover.clear();
                        cx.notify();
                    }
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
                            | Modal::BotMenu(_)
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
            .on_action(cx.listener(|this, _: &NewGalleryTab, _, cx| {
                this.execute_command(AppCommand::NewGalleryTab, cx);
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
            .on_action(cx.listener(|this, _: &ShowBots, _, cx| {
                this.execute_command(AppCommand::ShowBots, cx);
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
            .when(self.layout.selection_drag.is_some(), |element| {
                element.child(
                    div()
                        .absolute()
                        .w(px(1.0))
                        .h(px(1.0))
                        .child(TerminalSelectionCaptureElement { input: cx.entity() }),
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
