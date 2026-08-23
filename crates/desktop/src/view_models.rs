use gpui::{IntoElement, ParentElement, Pixels, Point, Render, Styled, Window, div, px, rgb};
use hh_protocol::{
    ClientRequest, DropPlacement, MAX_SSH_INPUT_LEN, Pane, SplitAxis, TerminalHistoryPage,
    TerminalPoint, TerminalProfile, TerminalSelection, TerminalSelectionKind, TmuxScanScope,
    TmuxSession, TmuxSessionId, normalize_ssh_input, validate_ssh_host,
};
use std::collections::HashSet;
use std::ops::Range;
use uuid::Uuid;

use gpui::{Context, MouseButton};

use crate::THEME;

#[derive(Clone, Debug)]
pub(super) struct PaneDrag {
    pub(super) pane_id: Uuid,
    pub(super) title: String,
    pub(super) position: Point<Pixels>,
}

#[derive(Clone, Debug)]
pub(super) struct WorkspaceDrag {
    pub(super) workspace_id: Uuid,
    pub(super) pinned: bool,
    pub(super) title: String,
    pub(super) position: Point<Pixels>,
}

#[derive(Clone, Debug)]
pub(super) struct TabDrag {
    pub(super) workspace_id: Uuid,
    pub(super) tab_id: Uuid,
    pub(super) pane_id: Option<Uuid>,
    pub(super) from_group: bool,
    pub(super) title: String,
    pub(super) position: Point<Pixels>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TabDropPreview {
    pub(super) target_tab_id: Uuid,
    pub(super) after: bool,
    pub(super) into_group: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceDropPreview {
    pub(super) target_workspace_id: Uuid,
    pub(super) after: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TabIdentityPresentation {
    pub(super) label: String,
    pub(super) profile: TerminalProfile,
    pub(super) detail: String,
}

impl Render for PaneDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        drag_ghost(&self.title, self.position, true)
    }
}

impl Render for WorkspaceDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        drag_ghost(&self.title, self.position, false)
    }
}

impl Render for TabDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        drag_ghost(&self.title, self.position, false)
    }
}

/// One shared drag-ghost pill. Terminal ghosts are smaller, monospace, and
/// square-cornered; workspace and tab ghosts are rounded and system-font.
fn drag_ghost(title: &str, position: gpui::Point<Pixels>, terminal: bool) -> impl IntoElement {
    let (width, half_width, border, font) = if terminal {
        (140.0, 70.0, rgb(THEME.border_strong), "SF Mono")
    } else {
        (164.0, 82.0, rgb(THEME.accent), ".SystemUIFont")
    };
    let element = div()
        .absolute()
        .left(position.x - px(half_width))
        .top(position.y - px(14.0))
        .w(px(width))
        .h(px(28.0))
        .bg(rgb(THEME.elevated))
        .border_1()
        .border_color(border)
        .flex()
        .items_center()
        .justify_center()
        .font_family(font);
    let element = if terminal {
        element.text_xs()
    } else {
        element.text_sm().rounded(px(5.0))
    };
    element
        .text_color(rgb(THEME.foreground))
        .child(title.to_owned())
}

#[derive(Clone, Debug)]
pub(super) struct TooltipView {
    pub(super) text: String,
}

impl Render for TooltipView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(5.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(THEME.foreground))
            .child(self.text.clone())
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TabMenu {
    pub(super) pane_id: Uuid,
    pub(super) position: Point<Pixels>,
    pub(super) identity_picker_open: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct WorkspaceMenu {
    pub(super) workspace_id: Uuid,
    pub(super) position: Point<Pixels>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct GroupMenu {
    pub(super) tab_id: Uuid,
    pub(super) position: Point<Pixels>,
    pub(super) icon_picker_open: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CreateMenuTarget {
    Global,
    TabStrip {
        workspace_id: Uuid,
        target_tab: Option<Uuid>,
    },
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CreateMenu {
    pub(super) position: Point<Pixels>,
    pub(super) target: CreateMenuTarget,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum DirEditorTarget {
    WorkspaceDefault(Uuid),
    NewProject(Uuid),
    ProjectDir(Uuid),
}

#[derive(Clone, Debug)]
pub(super) struct DirEditor {
    pub(super) target: DirEditorTarget,
    pub(super) value: String,
    pub(super) replace_on_type: bool,
    pub(super) suggestions: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ColorTarget {
    DefaultTerminal,
    DefaultWorkspace,
    Pane(Uuid),
    Workspace(Uuid),
    Tab(Uuid),
}

#[derive(Clone, Debug)]
pub(super) struct ColorPickerState {
    pub(super) target: ColorTarget,
    pub(super) hex: String,
    pub(super) hue: f32,
    pub(super) saturation: f32,
    pub(super) value: f32,
    pub(super) replace_on_type: bool,
    pub(super) invalid: bool,
}

#[derive(Clone, Debug)]
pub(super) struct RenameEditor {
    pub(super) pane_id: Uuid,
    pub(super) value: String,
    pub(super) replace_on_type: bool,
}

#[derive(Clone, Debug)]
pub(super) struct GroupRenameEditor {
    pub(super) tab_id: Uuid,
    pub(super) value: String,
    pub(super) replace_on_type: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RenameTarget {
    Pane,
    Workspace,
    Group,
}

#[derive(Clone, Debug, Default)]
pub(super) struct SearchEditor {
    pub(super) query: String,
    pub(super) no_match: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ArchivedView {
    pub(super) page: TerminalHistoryPage,
    pub(super) first_line: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HistoryEditField {
    RetentionDays,
    QuotaGib,
}

#[derive(Clone, Debug)]
pub(super) struct HistoryEditor {
    pub(super) field: HistoryEditField,
    pub(super) text: String,
    pub(super) replace_on_type: bool,
    pub(super) invalid: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TerminalLineRender {
    pub(super) row: usize,
    pub(super) cursor: Option<hh_protocol::TerminalCursor>,
    pub(super) focused: bool,
    pub(super) pane_id: Uuid,
    pub(super) pane_accent: u32,
    pub(super) columns: u16,
    pub(super) selection: Option<TerminalSelection>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SelectionDrag {
    pub(super) pane_id: Uuid,
    pub(super) anchor: TerminalPoint,
    pub(super) kind: TerminalSelectionKind,
    pub(super) deferred_mouse_click: bool,
    pub(super) preserve_single_cell: bool,
}

#[derive(Clone, Debug)]
pub(super) struct CloseConfirmation {
    pub(super) pane_id: Uuid,
    pub(super) title: String,
    pub(super) leaves_workspace_empty: bool,
    pub(super) is_browser: bool,
}
#[derive(Clone, Debug)]
pub(super) struct TabCloseConfirmation {
    pub(super) tab_id: Uuid,
    pub(super) title: String,
    pub(super) is_project: bool,
    pub(super) child_count: usize,
    pub(super) terminal_count: usize,
}

#[derive(Clone, Debug)]
pub(super) struct WorkspaceRenameEditor {
    pub(super) workspace_id: Uuid,
    pub(super) value: String,
    pub(super) replace_on_type: bool,
}

#[derive(Clone, Debug)]
pub(super) struct WorkspaceDeleteConfirmation {
    pub(super) workspace_id: Uuid,
    pub(super) title: String,
    pub(super) active_terminal_count: u32,
}

#[derive(Clone, Debug)]
pub(super) struct WorkspaceConnectionInfo {
    pub(super) workspace_id: Uuid,
    pub(super) position: Point<Pixels>,
}

#[derive(Clone, Debug)]
pub(super) struct WorkspaceDisconnectConfirmation {
    pub(super) workspace_id: Uuid,
    pub(super) title: String,
    pub(super) destination: String,
}

#[derive(Clone, Debug)]
pub(super) struct TmuxSessionPicker {
    pub(super) workspace_id: Uuid,
    pub(super) scope: TmuxScanScope,
    pub(super) sessions: Vec<TmuxSession>,
    pub(super) open_session_ids: HashSet<TmuxSessionId>,
    pub(super) no_server: bool,
    pub(super) selected_session_ids: HashSet<TmuxSessionId>,
    pub(super) status: Option<String>,
    pub(super) error: Option<String>,
}

impl TmuxSessionPicker {
    /// Sessions already shown in a tab are presentation-only: offering them
    /// again would produce a selection the service can only skip.
    pub(super) fn is_open(&self, session_id: &TmuxSessionId) -> bool {
        self.open_session_ids.contains(session_id)
    }

    pub(super) fn toggle_session(&mut self, session_id: &TmuxSessionId) {
        if self.is_open(session_id) {
            return;
        }
        if !self.selected_session_ids.insert(session_id.clone()) {
            self.selected_session_ids.remove(session_id);
        }
    }

    pub(super) fn select_all_sessions(&mut self) {
        self.selected_session_ids = self
            .sessions
            .iter()
            .filter(|session| !self.is_open(&session.id))
            .map(|session| session.id.clone())
            .collect();
    }

    pub(super) fn clear_all_sessions(&mut self) {
        self.selected_session_ids.clear();
    }

    pub(super) fn selected_session_ids_in_scan_order(&self) -> Vec<TmuxSessionId> {
        self.sessions
            .iter()
            .filter(|session| self.selected_session_ids.contains(&session.id))
            .map(|session| session.id.clone())
            .collect()
    }
}

pub(super) enum TmuxSelectionChange {
    Session(TmuxSessionId),
    All,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceCreationKind {
    Local,
    SystemSsh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceCreationStep {
    Details,
    ConfirmSsh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceCreationField {
    Name,
    Destination,
}

impl WorkspaceCreationField {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Destination => 1,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct DialogTextEditor {
    pub(super) text: String,
    pub(super) selected_range: Range<usize>,
    pub(super) selection_reversed: bool,
    pub(super) marked_range: Option<Range<usize>>,
}

impl DialogTextEditor {
    pub(super) fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            selected_range: end..end,
            selection_reversed: false,
            marked_range: None,
        }
    }

    pub(super) fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(super) fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for character in self.text.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += character.len_utf16();
            utf8_offset += character.len_utf8();
        }
        utf8_offset.min(self.text.len())
    }

    pub(super) fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for character in self.text.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += character.len_utf8();
            utf16_offset += character.len_utf16();
        }
        utf16_offset
    }

    pub(super) fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    pub(super) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub(super) fn previous_boundary(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    pub(super) fn next_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.text.len());
        self.text[offset..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(index, _)| offset + index)
    }

    pub(super) fn move_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.marked_range = None;
    }

    pub(super) fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.marked_range = None;
    }

    pub(super) fn move_left(&mut self, selecting: bool) {
        let target = if !selecting && !self.selected_range.is_empty() {
            self.selected_range.start
        } else {
            self.previous_boundary(self.cursor_offset())
        };
        if selecting {
            self.select_to(target);
        } else {
            self.move_to(target);
        }
    }

    pub(super) fn move_right(&mut self, selecting: bool) {
        let target = if !selecting && !self.selected_range.is_empty() {
            self.selected_range.end
        } else {
            self.next_boundary(self.cursor_offset())
        };
        if selecting {
            self.select_to(target);
        } else {
            self.move_to(target);
        }
    }

    pub(super) fn move_home(&mut self, selecting: bool) {
        if selecting {
            self.select_to(0);
        } else {
            self.move_to(0);
        }
    }

    pub(super) fn move_end(&mut self, selecting: bool) {
        if selecting {
            self.select_to(self.text.len());
        } else {
            self.move_to(self.text.len());
        }
    }

    pub(super) fn select_all(&mut self) {
        self.selected_range = 0..self.text.len();
        self.selection_reversed = false;
        self.marked_range = None;
    }

    /// Select the word under a double-click using the same practical
    /// boundaries people expect in workstation names and SSH destinations.
    /// Keep connection punctuation together so `user@build-node` is useful as
    /// a single editable unit rather than a series of tiny selections.
    pub(super) fn select_word_at(&mut self, offset: usize) {
        if self.text.is_empty() {
            self.move_to(0);
            return;
        }

        let mut cursor = offset.min(self.text.len());
        if cursor == self.text.len() {
            cursor = self.previous_boundary(cursor);
        }
        let is_word = |character: char| {
            character.is_alphanumeric()
                || matches!(character, '_' | '-' | '.' | '@' | '/' | ':' | '~')
        };
        let Some(character) = self.text[cursor..].chars().next() else {
            self.move_to(cursor);
            return;
        };
        let word = is_word(character);

        let mut start = cursor;
        while start > 0 {
            let previous = self.previous_boundary(start);
            let Some(previous_character) = self.text[previous..].chars().next() else {
                break;
            };
            if is_word(previous_character) != word {
                break;
            }
            start = previous;
        }

        let mut end = cursor + character.len_utf8();
        while end < self.text.len() {
            let Some(next_character) = self.text[end..].chars().next() else {
                break;
            };
            if is_word(next_character) != word {
                break;
            }
            end += next_character.len_utf8();
        }
        self.selected_range = start..end;
        self.selection_reversed = false;
        self.marked_range = None;
    }

    pub(super) fn selected_text(&self) -> Option<&str> {
        (!self.selected_range.is_empty()).then(|| &self.text[self.selected_range.clone()])
    }

    pub(super) fn replacement_range(&self, range_utf16: Option<&Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone())
    }

    pub(super) fn replace(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        new_text: &str,
        maximum: usize,
        limit_is_bytes: bool,
        mark: bool,
        marked_selection_utf16: Option<&Range<usize>>,
    ) {
        let range = self.replacement_range(range_utf16);
        let mut insertion: String = new_text
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        loop {
            let result_len = if limit_is_bytes {
                self.text.len() - range.len() + insertion.len()
            } else {
                self.text[..range.start].chars().count()
                    + insertion.chars().count()
                    + self.text[range.end..].chars().count()
            };
            if result_len <= maximum || insertion.is_empty() {
                break;
            }
            insertion.pop();
        }

        self.text.replace_range(range.clone(), &insertion);
        let inserted = range.start..range.start + insertion.len();
        self.marked_range = mark
            .then(|| inserted.clone())
            .filter(|range| !range.is_empty());
        if mark {
            if let Some(selection) = marked_selection_utf16 {
                let relative_start = Self::utf16_offset_in(&insertion, selection.start);
                let relative_end = Self::utf16_offset_in(&insertion, selection.end);
                self.selected_range = range.start + relative_start..range.start + relative_end;
            } else {
                self.selected_range = inserted.end..inserted.end;
            }
        } else {
            self.selected_range = inserted.end..inserted.end;
        }
        self.selection_reversed = false;
    }

    pub(super) fn utf16_offset_in(text: &str, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for character in text.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += character.len_utf16();
            utf8_offset += character.len_utf8();
        }
        utf8_offset.min(text.len())
    }

    pub(super) fn delete_backward(&mut self) {
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            self.selected_range = self.previous_boundary(cursor)..cursor;
        }
        self.replace(None, "", usize::MAX, true, false, None);
    }

    pub(super) fn delete_forward(&mut self) {
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            self.selected_range = cursor..self.next_boundary(cursor);
        }
        self.replace(None, "", usize::MAX, true, false, None);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceCreationDialog {
    pub(super) kind: WorkspaceCreationKind,
    pub(super) name: DialogTextEditor,
    pub(super) destination: DialogTextEditor,
    pub(super) field: WorkspaceCreationField,
    pub(super) step: WorkspaceCreationStep,
    pub(super) error: Option<String>,
}

impl WorkspaceCreationDialog {
    pub(super) fn new() -> Self {
        Self {
            kind: WorkspaceCreationKind::Local,
            name: DialogTextEditor::default(),
            destination: DialogTextEditor::default(),
            field: WorkspaceCreationField::Name,
            step: WorkspaceCreationStep::Details,
            error: None,
        }
    }

    pub(super) fn review(&mut self) {
        match normalize_ssh_input(&self.destination.text) {
            Ok(destination) => {
                self.destination = DialogTextEditor::with_text(destination);
                self.step = WorkspaceCreationStep::ConfirmSsh;
                self.error = None;
            }
            Err(message) => self.error = Some(message.to_string()),
        }
    }

    pub(super) fn active_editor(&self) -> &DialogTextEditor {
        match self.field {
            WorkspaceCreationField::Name => &self.name,
            WorkspaceCreationField::Destination => &self.destination,
        }
    }

    pub(super) fn active_editor_mut(&mut self) -> &mut DialogTextEditor {
        match self.field {
            WorkspaceCreationField::Name => &mut self.name,
            WorkspaceCreationField::Destination => &mut self.destination,
        }
    }

    pub(super) fn replace_text(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        text: &str,
        mark: bool,
        marked_selection_utf16: Option<&Range<usize>>,
    ) {
        let (maximum, limit_is_bytes) = match self.field {
            WorkspaceCreationField::Name => (80, false),
            WorkspaceCreationField::Destination => (MAX_SSH_INPUT_LEN, true),
        };
        self.active_editor_mut().replace(
            range_utf16,
            text,
            maximum,
            limit_is_bytes,
            mark,
            marked_selection_utf16,
        );
        self.error = None;
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.replace_text(None, text, false, None);
    }

    pub(super) fn backspace(&mut self) {
        self.active_editor_mut().delete_backward();
        self.error = None;
    }

    pub(super) fn delete(&mut self) {
        self.active_editor_mut().delete_forward();
        self.error = None;
    }

    pub(super) fn approved_request(&self) -> Option<ClientRequest> {
        let title = (!self.name.text.trim().is_empty()).then(|| self.name.text.trim().to_owned());
        match self.kind {
            WorkspaceCreationKind::Local if self.step == WorkspaceCreationStep::Details => {
                Some(ClientRequest::CreateWorkspace { title })
            }
            WorkspaceCreationKind::SystemSsh
                if self.step == WorkspaceCreationStep::ConfirmSsh
                    && validate_ssh_host(&self.destination.text).is_ok() =>
            {
                Some(ClientRequest::CreateSshWorkspace {
                    title,
                    destination: self.destination.text.clone(),
                })
            }
            WorkspaceCreationKind::Local | WorkspaceCreationKind::SystemSsh => None,
        }
    }
}

pub(super) fn route_workspace_creation_paste(
    dialog: Option<&mut WorkspaceCreationDialog>,
    text: &str,
) -> bool {
    let Some(dialog) = dialog else {
        return false;
    };
    if dialog.step == WorkspaceCreationStep::Details {
        dialog.paste(text);
    }
    true
}

impl CloseConfirmation {
    pub(super) fn for_pane(pane: &Pane, leaves_workspace_empty: bool) -> Self {
        Self {
            pane_id: pane.id,
            title: pane.title.clone(),
            is_browser: pane.kind.is_browser(),
            leaves_workspace_empty,
        }
    }

    pub(super) fn request(&self) -> ClientRequest {
        ClientRequest::ClosePane {
            pane_id: self.pane_id,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum PaneControlIcon {
    Add,
    SplitRight,
    SplitDown,
    Web,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ResizeDrag {
    pub(super) split_id: SplitControlId,
    pub(super) axis: SplitAxis,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum SidebarResizeLifecycle {
    #[default]
    Idle,
    Dragging {
        initial_width: f32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarResizeMove {
    Ignore,
    Update,
    Complete,
}

impl SidebarResizeLifecycle {
    pub(super) fn begin(&mut self, initial_width: f32) {
        *self = Self::Dragging { initial_width };
    }

    pub(super) fn pointer_move(
        &mut self,
        pressed_button: Option<MouseButton>,
    ) -> SidebarResizeMove {
        match (*self, pressed_button) {
            (Self::Idle, _) => SidebarResizeMove::Ignore,
            (Self::Dragging { .. }, Some(MouseButton::Left)) => SidebarResizeMove::Update,
            (Self::Dragging { .. }, _) => {
                *self = Self::Idle;
                SidebarResizeMove::Complete
            }
        }
    }

    pub(super) fn finish(&mut self) -> bool {
        if matches!(self, Self::Idle) {
            return false;
        }
        *self = Self::Idle;
        true
    }

    pub(super) fn cancel(&mut self) -> Option<f32> {
        let Self::Dragging { initial_width } = *self else {
            return None;
        };
        *self = Self::Idle;
        Some(initial_width)
    }

    pub(super) fn is_active(self) -> bool {
        matches!(self, Self::Dragging { .. })
    }
}

/// Client-local split identity. The current protocol has no split IDs, so this
/// wraps its deterministic compatibility key behind one boundary. A future
/// protocol `SplitId` can replace the field without changing layout controls.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SplitControlId {
    pub(crate) first: Uuid,
    pub(crate) second: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LayoutControlMutation {
    Equalize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CommandPaletteState {
    pub(super) query: String,
    pub(super) selected: usize,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum DialogTone {
    Accent,
    Danger,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum DialogAction {
    RenamePane,
    RenameWorkspace,
    RenameTab,
    DeleteWorkspace,
    DisconnectWorkspace,
    ClosePane,
    ConfirmDirEditor,
    CloseTab,
}
pub(super) struct DialogSpec {
    pub(super) title: String,
    pub(super) confirm_label: &'static str,
    pub(super) confirm_tone: DialogTone,
    pub(super) confirm_id: &'static str,
    pub(super) action: DialogAction,
}

#[derive(Clone, Debug, Default)]
pub(super) enum Modal {
    #[default]
    None,
    CommandPalette(CommandPaletteState),
    WorkspaceCreation(WorkspaceCreationDialog),
    WorkspaceRename(WorkspaceRenameEditor),
    DirEditor(DirEditor),
    PaneRename(RenameEditor),
    GroupRename(GroupRenameEditor),
    Search(SearchEditor),
    WorkspaceDelete(WorkspaceDeleteConfirmation),
    TmuxPicker(TmuxSessionPicker),
    WorkspaceDisconnect(WorkspaceDisconnectConfirmation),
    Close(CloseConfirmation),
    TabClose(TabCloseConfirmation),
    TabMenu(TabMenu),
    WorkspaceMenu(WorkspaceMenu),
    CreateMenu(CreateMenu),
    GroupMenu(GroupMenu),
    WorkspaceConnectionInfo(WorkspaceConnectionInfo),
    AppearanceSettings,
}

impl Modal {
    pub(super) fn command_palette(&self) -> Option<&CommandPaletteState> {
        let Self::CommandPalette(palette) = self else {
            return None;
        };
        Some(palette)
    }

    pub(super) fn command_palette_mut(&mut self) -> Option<&mut CommandPaletteState> {
        let Self::CommandPalette(palette) = self else {
            return None;
        };
        Some(palette)
    }

    pub(super) fn workspace_creation(&self) -> Option<&WorkspaceCreationDialog> {
        let Self::WorkspaceCreation(dialog) = self else {
            return None;
        };
        Some(dialog)
    }

    pub(super) fn workspace_creation_mut(&mut self) -> Option<&mut WorkspaceCreationDialog> {
        let Self::WorkspaceCreation(dialog) = self else {
            return None;
        };
        Some(dialog)
    }

    pub(super) fn workspace_rename_mut(&mut self) -> Option<&mut WorkspaceRenameEditor> {
        let Self::WorkspaceRename(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn workspace_rename(&self) -> Option<&WorkspaceRenameEditor> {
        let Self::WorkspaceRename(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn dir_editor(&self) -> Option<&DirEditor> {
        let Self::DirEditor(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn dir_editor_mut(&mut self) -> Option<&mut DirEditor> {
        let Self::DirEditor(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn pane_rename_mut(&mut self) -> Option<&mut RenameEditor> {
        let Self::PaneRename(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn pane_rename(&self) -> Option<&RenameEditor> {
        let Self::PaneRename(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn group_rename_mut(&mut self) -> Option<&mut GroupRenameEditor> {
        let Self::GroupRename(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn group_rename(&self) -> Option<&GroupRenameEditor> {
        let Self::GroupRename(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn search(&self) -> Option<&SearchEditor> {
        let Self::Search(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn search_mut(&mut self) -> Option<&mut SearchEditor> {
        let Self::Search(editor) = self else {
            return None;
        };
        Some(editor)
    }

    pub(super) fn tmux_picker_mut(&mut self) -> Option<&mut TmuxSessionPicker> {
        let Self::TmuxPicker(picker) = self else {
            return None;
        };
        Some(picker)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PixelRect {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) width: f32,
    pub(super) height: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DragDestination {
    Split {
        target_pane: Uuid,
        placement: DropPlacement,
    },
    Merge {
        target_pane: Uuid,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DragHoverState {
    pub(super) destination: Option<DragDestination>,
}

impl DragHoverState {
    pub(super) fn enter(&mut self, destination: DragDestination) {
        self.destination = Some(destination);
    }

    pub(super) fn clear(&mut self) {
        self.destination = None;
    }

    pub(super) fn split_for(self, target_pane: Uuid) -> Option<DropPlacement> {
        match self.destination {
            Some(DragDestination::Split {
                target_pane: target,
                placement,
            }) if target == target_pane => Some(placement),
            _ => None,
        }
    }

    pub(super) fn merges_into(self, target_pane: Uuid) -> bool {
        matches!(
            self.destination,
            Some(DragDestination::Merge { target_pane: target }) if target == target_pane
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClientRequest, CloseConfirmation, DialogTextEditor, DragDestination, DragHoverState,
        DropPlacement, HashSet, MAX_SSH_INPUT_LEN, MouseButton, Pane, SidebarResizeLifecycle,
        SidebarResizeMove, TmuxScanScope, TmuxSession, TmuxSessionId, TmuxSessionPicker, Uuid,
        WorkspaceCreationDialog, WorkspaceCreationField, WorkspaceCreationKind,
        WorkspaceCreationStep, route_workspace_creation_paste,
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
            custom_title: Some("build".to_owned()),
            profile_override: None,
            custom_icon: None,
        };

        let confirmation = CloseConfirmation::for_pane(&pane, true);

        assert_eq!(confirmation.pane_id, pane.id);
        assert_eq!(confirmation.title, "build");
        assert!(!confirmation.is_browser);
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
}
