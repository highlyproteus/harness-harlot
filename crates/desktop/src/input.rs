//! Keyboard handling and command execution.

use crate::browser::browser_command_available;
use crate::commands::{AppCommand, palette_matches};
use crate::helpers::{append_rename_text, terminal_input_bytes};
use crate::view_models::{
    CommandPaletteState, Modal, RenameTarget, WorkspaceCreationField, WorkspaceCreationKind,
    WorkspaceCreationStep,
};
use crate::{COMMAND_PALETTE_LIMIT, HhApp};
use gpui::{
    Bounds, ClipboardItem, Context, EntityInputHandler, KeyDownEvent, Pixels, Point,
    UTF16Selection, Window, point, px, size,
};
use hh_protocol::{ClientRequest, SplitAxis};
use std::ops::Range;

pub(crate) fn browser_key_text(
    key: &str,
    key_char: Option<&str>,
    shift: bool,
    alt: bool,
) -> Option<String> {
    let mut text = key_char.map(ToOwned::to_owned).or_else(|| {
        if alt {
            return None;
        }
        match key {
            "space" => Some(" ".to_owned()),
            key if key.chars().count() == 1 && !key.chars().any(char::is_control) => {
                Some(key.to_owned())
            }
            _ => None,
        }
    })?;
    if shift {
        text.make_ascii_uppercase();
    }
    Some(text)
}

enum BrowserKeyRoute {
    NotEditing,
    Consumed,
    PassToInput,
}

impl HhApp {
    pub(crate) fn execute_command(&mut self, command: AppCommand, cx: &mut Context<Self>) {
        if !matches!(self.editor.modal, Modal::None | Modal::CommandPalette(_))
            && command != AppCommand::ShowCommandPalette
        {
            return;
        }
        if matches!(command, AppCommand::SplitRight | AppCommand::SplitDown)
            && self.layout.focused_pane.is_some_and(|pane_id| {
                self.pane_metadata(pane_id)
                    .is_some_and(|pane| pane.kind.is_browser())
            })
        {
            return;
        }
        self.editor.modal = Modal::None;
        match command {
            AppCommand::NewWorkspace => self.new_workspace(cx),
            AppCommand::ToggleSidebar => self.toggle_sidebar(cx),
            AppCommand::NewTab => self.new_tab(cx),
            AppCommand::NewBrowserTab => self.new_browser_tab(cx),
            AppCommand::TerminalZoomIn => self.adjust_terminal_zoom(1, cx),
            AppCommand::TerminalZoomOut => self.adjust_terminal_zoom(-1, cx),
            AppCommand::SplitRight => self.split(SplitAxis::Horizontal, cx),
            AppCommand::SplitDown => self.split(SplitAxis::Vertical, cx),
            AppCommand::FocusLeft | AppCommand::FocusUp => self.focus_direction(false, cx),
            AppCommand::FocusRight | AppCommand::FocusDown => self.focus_direction(true, cx),
            AppCommand::ShowCommandPalette => {
                self.editor.modal = Modal::CommandPalette(CommandPaletteState::default());
                cx.notify();
            }
            AppCommand::TogglePaneZoom => self.toggle_pane_zoom(cx),
            AppCommand::EqualizePanes => self.equalize_panes(cx),
            AppCommand::ReattachPane => {
                if let Some(pane_id) = self.layout.focused_pane {
                    self.reattach_pane(pane_id, cx);
                }
            }
            AppCommand::ShowNotifications => self.toggle_sidebar_activity(cx),
        }
    }

    pub(crate) fn handle_palette_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mut execute = None;
        let mut close = false;
        if let Some(palette) = self.editor.modal.command_palette_mut() {
            let matches = palette_matches(&palette.query, COMMAND_PALETTE_LIMIT)
                .into_iter()
                .filter(|item| {
                    item.command != AppCommand::NewBrowserTab || browser_command_available()
                })
                .collect::<Vec<_>>();
            let result_count = matches.len();
            match keystroke.key.as_str() {
                "escape" => close = true,
                "enter" => {
                    execute = matches.get(palette.selected).map(|item| item.command);
                }
                "up" => {
                    palette.selected = palette.selected.saturating_sub(1);
                    cx.notify();
                }
                "down" => {
                    palette.selected = (palette.selected + 1).min(result_count.saturating_sub(1));
                    cx.notify();
                }
                "backspace" => {
                    palette.query.pop();
                    palette.selected = 0;
                    cx.notify();
                }
                _ if !keystroke.modifiers.platform
                    && !keystroke.modifiers.control
                    && !keystroke.modifiers.alt =>
                {
                    if let Some(text) = &keystroke.key_char
                        && !text.chars().any(char::is_control)
                    {
                        palette.query.push_str(text);
                        palette.selected = 0;
                        cx.notify();
                    }
                }
                _ => {}
            }
        }
        if close {
            self.editor.modal = Modal::None;
            cx.notify();
        } else if let Some(command) = execute {
            self.execute_command(command, cx);
        }
        // Palette keystrokes are modal and can never become PTY input.
        cx.stop_propagation();
    }

    pub(crate) fn handle_workspace_creation_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        let step = self
            .editor
            .modal
            .workspace_creation()
            .map(|dialog| dialog.step);
        if step == Some(WorkspaceCreationStep::Details)
            && keystroke.modifiers.platform
            && keystroke.key.eq_ignore_ascii_case("a")
        {
            if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                dialog.active_editor_mut().select_all();
                cx.notify();
            }
            return;
        }
        if step == Some(WorkspaceCreationStep::Details)
            && keystroke.modifiers.platform
            && keystroke.key.eq_ignore_ascii_case("x")
        {
            if let Some(dialog) = self.editor.modal.workspace_creation_mut()
                && let Some(text) = dialog.active_editor().selected_text().map(str::to_owned)
            {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                dialog.replace_text(None, "", false, None);
                cx.notify();
            }
            return;
        }
        match keystroke.key.as_str() {
            "enter" => self.submit_workspace_creation(cx),
            "escape" => {
                self.editor.modal = Modal::None;
                cx.notify();
            }
            "tab" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                    dialog.field = match (dialog.kind, dialog.field) {
                        (WorkspaceCreationKind::SystemSsh, WorkspaceCreationField::Name) => {
                            WorkspaceCreationField::Destination
                        }
                        _ => WorkspaceCreationField::Name,
                    };
                    cx.notify();
                }
            }
            "backspace" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                    dialog.backspace();
                    cx.notify();
                }
            }
            "delete" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                    dialog.delete();
                    cx.notify();
                }
            }
            "left" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                    if keystroke.modifiers.platform {
                        dialog
                            .active_editor_mut()
                            .move_home(keystroke.modifiers.shift);
                    } else {
                        dialog
                            .active_editor_mut()
                            .move_left(keystroke.modifiers.shift);
                    }
                    cx.notify();
                }
            }
            "right" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                    if keystroke.modifiers.platform {
                        dialog
                            .active_editor_mut()
                            .move_end(keystroke.modifiers.shift);
                    } else {
                        dialog
                            .active_editor_mut()
                            .move_right(keystroke.modifiers.shift);
                    }
                    cx.notify();
                }
            }
            "home" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                    dialog
                        .active_editor_mut()
                        .move_home(keystroke.modifiers.shift);
                    cx.notify();
                }
            }
            "end" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
                    dialog
                        .active_editor_mut()
                        .move_end(keystroke.modifiers.shift);
                    cx.notify();
                }
            }
            _ if step == Some(WorkspaceCreationStep::Details)
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.control
                && !keystroke.modifiers.alt =>
            {
                if let Some(text) = &keystroke.key_char
                    && !text.chars().any(char::is_control)
                    && let Some(dialog) = self.editor.modal.workspace_creation_mut()
                {
                    dialog.replace_text(None, text, false, None);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    pub(crate) fn handle_rename_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        target: RenameTarget,
        cx: &mut Context<Self>,
    ) {
        let (value, replace_on_type) = match (&mut self.editor.modal, target) {
            (Modal::WorkspaceRename(editor), RenameTarget::Workspace) => {
                (&mut editor.value, &mut editor.replace_on_type)
            }
            (Modal::PaneRename(editor), RenameTarget::Pane) => {
                (&mut editor.value, &mut editor.replace_on_type)
            }
            (Modal::GroupRename(editor), RenameTarget::Group) => {
                (&mut editor.value, &mut editor.replace_on_type)
            }
            _ => return,
        };
        if keystroke.modifiers.platform && keystroke.key.eq_ignore_ascii_case("a") {
            *replace_on_type = true;
            cx.notify();
            return;
        }
        match keystroke.key.as_str() {
            "enter" => match target {
                RenameTarget::Pane => self.submit_rename(cx),
                RenameTarget::Workspace => self.submit_workspace_rename(cx),
                RenameTarget::Group => self.submit_group_rename(cx),
            },
            "escape" => {
                self.editor.modal = Modal::None;
                cx.notify();
            }
            "backspace" => {
                if *replace_on_type {
                    value.clear();
                } else {
                    value.pop();
                }
                *replace_on_type = false;
                cx.notify();
            }
            _ if !keystroke.modifiers.platform
                && !keystroke.modifiers.control
                && !keystroke.modifiers.alt =>
            {
                if let Some(text) = &keystroke.key_char {
                    append_rename_text(value, replace_on_type, text);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    pub(crate) fn handle_search_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        match keystroke.key.as_str() {
            "enter" => self.run_search(!keystroke.modifiers.shift, cx),
            "escape" => {
                self.editor.modal = Modal::None;
                self.editor.ime_preedit.clear();
                cx.notify();
            }
            "backspace" => {
                let Some(editor) = self.editor.modal.search_mut() else {
                    return;
                };
                editor.query.pop();
                editor.no_match = false;
                let empty = editor.query.is_empty();
                if empty {
                    if let Some(pane_id) = self.layout.focused_pane {
                        self.dispatch_control(ClientRequest::ClearSelection { pane_id });
                    }
                } else {
                    self.run_search(true, cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// Backspace semantics shared by every inline one-field editor: a pending
    /// replace-on-type selection clears the whole field, otherwise one
    /// character is removed, and the transient flags reset.
    fn apply_inline_backspace(buffer: &mut String, replace_on_type: &mut bool, invalid: &mut bool) {
        if *replace_on_type {
            buffer.clear();
        } else {
            buffer.pop();
        }
        *replace_on_type = false;
        *invalid = false;
    }

    fn handle_browser_url_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> BrowserKeyRoute {
        if self.editor.browser_url_editor.is_none() {
            return BrowserKeyRoute::NotEditing;
        }

        let route_to_input_context = match event.keystroke.key.as_str() {
            "enter" => {
                self.submit_browser_url(cx);
                false
            }
            "escape" => {
                self.editor.browser_url_editor = None;
                self.editor.ime_preedit.clear();
                cx.notify();
                false
            }
            "backspace" => {
                if let Some(editor) = self.editor.browser_url_editor.as_mut() {
                    Self::apply_inline_backspace(
                        &mut editor.text,
                        &mut editor.replace_on_type,
                        &mut editor.invalid,
                    );
                }
                cx.notify();
                false
            }
            "a" if event.keystroke.modifiers.platform => {
                if let Some(editor) = self.editor.browser_url_editor.as_mut() {
                    editor.replace_on_type = true;
                }
                cx.notify();
                false
            }
            _ if event.keystroke.modifiers.platform => true,
            _ if !event.keystroke.modifiers.platform && !event.keystroke.modifiers.control => {
                let text = browser_key_text(
                    &event.keystroke.key,
                    event.keystroke.key_char.as_deref(),
                    event.keystroke.modifiers.shift,
                    event.keystroke.modifiers.alt,
                );
                if let Some(text) = text {
                    self.append_browser_url_text(&text);
                    cx.notify();
                    false
                } else {
                    // IME/composition keys still need AppKit's input context.
                    true
                }
            }
            _ => false,
        };

        if route_to_input_context {
            BrowserKeyRoute::PassToInput
        } else {
            BrowserKeyRoute::Consumed
        }
    }

    pub(crate) fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" && self.sidebar.sidebar_resize.is_active() {
            self.cancel_sidebar_resize(window, cx);
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.sidebar.sidebar_activity {
            self.sidebar.sidebar_activity = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        match self.handle_browser_url_key(event, cx) {
            BrowserKeyRoute::NotEditing => {}
            BrowserKeyRoute::Consumed => {
                cx.stop_propagation();
                return;
            }
            BrowserKeyRoute::PassToInput => return,
        }
        if self.editor.modal.command_palette().is_some() {
            self.handle_palette_key(event, cx);
            return;
        }
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform && !keystroke.modifiers.control && !keystroke.modifiers.alt
        {
            let workspace_shortcut = match keystroke.key.as_str() {
                "1" => Some(0),
                "2" => Some(1),
                "3" => Some(2),
                "4" => Some(3),
                "5" => Some(4),
                "6" => Some(5),
                "7" => Some(6),
                "8" => Some(7),
                "9" => Some(8),
                _ => None,
            };
            if workspace_shortcut.is_some_and(|index| self.select_workspace_by_index(index, cx)) {
                cx.stop_propagation();
                return;
            }
        }
        if let Some(picker) = self.editor.color_picker.as_mut() {
            match keystroke.key.as_str() {
                "enter" => self.submit_color_picker(cx),
                "escape" => {
                    self.editor.color_picker = None;
                    cx.notify();
                }
                "backspace" => {
                    Self::apply_inline_backspace(
                        &mut picker.hex,
                        &mut picker.replace_on_type,
                        &mut picker.invalid,
                    );
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if let Some(editor) = self.editor.history_editor.as_mut() {
            match keystroke.key.as_str() {
                "enter" => self.submit_history_edit(cx),
                "escape" => {
                    self.editor.history_editor = None;
                    cx.notify();
                }
                "backspace" => {
                    Self::apply_inline_backspace(
                        &mut editor.text,
                        &mut editor.replace_on_type,
                        &mut editor.invalid,
                    );
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        match &self.editor.modal {
            Modal::None => {}
            Modal::CommandPalette(_) => {
                self.handle_palette_key(event, cx);
                return;
            }
            Modal::AppearanceSettings => {
                if keystroke.key == "escape" {
                    self.editor.modal = Modal::None;
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceCreation(_) => {
                self.handle_workspace_creation_key(keystroke, cx);
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceRename(_) => {
                self.handle_rename_key(keystroke, RenameTarget::Workspace, cx);
                cx.stop_propagation();
                return;
            }
            Modal::DirEditor(_) => {
                self.handle_dir_editor_key(keystroke, cx);
                cx.stop_propagation();
                return;
            }
            Modal::PaneRename(_) => {
                self.handle_rename_key(keystroke, RenameTarget::Pane, cx);
                cx.stop_propagation();
                return;
            }
            Modal::GroupRename(_) => {
                self.handle_rename_key(keystroke, RenameTarget::Group, cx);
                cx.stop_propagation();
                return;
            }
            Modal::Search(_) => {
                self.handle_search_key(keystroke, cx);
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceDelete(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_workspace_delete(cx),
                    "escape" => {
                        self.editor.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::TabClose(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_tab_close(cx),
                    "escape" => {
                        self.editor.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::TmuxPicker(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.open_selected_tmux_sessions(cx),
                    "escape" => {
                        self.editor.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceDisconnect(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_workspace_disconnect(cx),
                    "escape" => {
                        self.editor.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::Close(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_close(cx),
                    "escape" => {
                        self.editor.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::TabMenu(_)
            | Modal::WorkspaceMenu(_)
            | Modal::CreateMenu(_)
            | Modal::GroupMenu(_)
            | Modal::WorkspaceConnectionInfo(_) => {
                if keystroke.key == "escape" {
                    self.editor.modal = Modal::None;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
            }
        }
        if self.layout.dragging_pane.is_some() && keystroke.key == "escape" {
            self.layout.dragging_pane = None;
            self.layout.drag_hover.clear();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let bytes = terminal_input_bytes(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            keystroke.modifiers.control,
            keystroke.modifiers.alt,
            keystroke.modifiers.platform,
        );
        if let (Some(pane_id), Some(bytes)) = (self.layout.focused_pane, bytes) {
            self.dispatch_control(ClientRequest::WriteInput { pane_id, bytes });
            cx.stop_propagation();
            cx.notify();
        }
    }
}

impl EntityInputHandler for HhApp {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            let byte_range = editor.range_from_utf16(&range);
            actual_range.replace(editor.range_to_utf16(&byte_range));
            return Some(editor.text[byte_range].to_owned());
        }
        actual_range.replace(0..self.editor.ime_preedit.encode_utf16().count());
        Some(self.editor.ime_preedit.clone())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            return Some(UTF16Selection {
                range: editor.range_to_utf16(&editor.selected_range),
                reversed: editor.selection_reversed,
            });
        }
        let end = self.editor.ime_preedit.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            return editor
                .marked_range
                .as_ref()
                .map(|range| editor.range_to_utf16(range));
        }
        (!self.editor.ime_preedit.is_empty())
            .then(|| 0..self.editor.ime_preedit.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.active_editor_mut().marked_range = None;
            cx.notify();
            return;
        }
        self.editor.ime_preedit.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.replace_text(range.as_ref(), text, false, None);
            cx.notify();
            return;
        }
        self.editor.ime_preedit.clear();
        self.commit_text(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.replace_text(range.as_ref(), text, true, selected_range.as_ref());
            cx.notify();
            return;
        }
        text.clone_into(&mut self.editor.ime_preedit);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let index = dialog.field.index();
            let (Some(line), Some(input_bounds)) = (
                self.editor.workspace_input_layouts[index].as_ref(),
                self.editor.workspace_input_bounds[index],
            ) else {
                return None;
            };
            let byte_range = dialog.active_editor().range_from_utf16(&range);
            return Some(Bounds::from_corners(
                point(
                    input_bounds.left() + line.x_for_index(byte_range.start),
                    input_bounds.top(),
                ),
                point(
                    input_bounds.left() + line.x_for_index(byte_range.end),
                    input_bounds.bottom(),
                ),
            ));
        }
        let line_height = self
            .layout
            .focused_pane
            .map_or(self.terminal_font.metrics.line_height, |pane_id| {
                self.terminal_metrics(pane_id).line_height
            });
        Some(Bounds::new(
            bounds.bottom_left(),
            size(px(1.0), px(line_height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if let Some(dialog) = self
            .editor
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let index = dialog.field.index();
            let (Some(line), Some(bounds)) = (
                self.editor.workspace_input_layouts[index].as_ref(),
                self.editor.workspace_input_bounds[index],
            ) else {
                return None;
            };
            let byte_index = line.closest_index_for_x(point.x - bounds.left());
            return Some(dialog.active_editor().offset_to_utf16(byte_index));
        }
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::browser_key_text;

    #[test]
    fn browser_url_key_text_falls_back_to_printable_physical_keys() {
        assert_eq!(
            browser_key_text("a", None, false, false).as_deref(),
            Some("a")
        );
        assert_eq!(
            browser_key_text("a", None, true, false).as_deref(),
            Some("A")
        );
        assert_eq!(
            browser_key_text("space", None, false, false).as_deref(),
            Some(" ")
        );
        assert_eq!(browser_key_text("left", None, false, false), None);
        assert_eq!(browser_key_text("a", None, false, true), None);
    }
}
