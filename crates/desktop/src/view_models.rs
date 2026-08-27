use gpui::{IntoElement, ParentElement, Pixels, Point, Render, Styled, Window, div, px, rgb};
use hh_protocol::{
    ClientRequest, DropPlacement, MAX_SSH_INPUT_LEN, MAX_WORKSPACE_DIR_BYTES, Pane, SplitAxis,
    TerminalHistoryPage, TerminalPoint, TerminalProfile, TerminalSelection, TerminalSelectionKind,
    TmuxScanScope, TmuxSession, TmuxSessionId, normalize_ssh_input, validate_ssh_host,
};
use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use uuid::Uuid;

use gpui::{Context, MouseButton};

use crate::{THEME, helpers::expand_home};

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
pub(super) struct ComposerAttachment {
    pub(super) filename: String,
    pub(super) data_url: String,
    pub(super) path: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct AssistantComposer {
    pub(super) pane_id: Uuid,
    pub(super) text: String,
    pub(super) selection: Option<Range<usize>>,
    pub(super) attachment: Option<ComposerAttachment>,
}

impl AssistantComposer {
    pub(super) fn insert(&mut self, text: &str) {
        if let Some(selection) = self.selection.take() {
            self.text.replace_range(selection, text);
        } else {
            self.text.push_str(text);
        }
    }

    pub(super) fn backspace(&mut self) {
        if let Some(selection) = self.selection.take() {
            self.text.replace_range(selection, "");
        } else {
            self.text.pop();
        }
    }

    pub(super) fn select_all(&mut self) {
        self.selection = (!self.text.is_empty()).then_some(0..self.text.len());
    }

    pub(super) fn selected_text(&self) -> Option<&str> {
        self.selection
            .as_ref()
            .and_then(|selection| self.text.get(selection.clone()))
    }

    pub(super) fn cut_selection(&mut self) -> Option<String> {
        let selection = self.selection.take()?;
        let selected = self.text.get(selection.clone())?.to_owned();
        self.text.replace_range(selection, "");
        Some(selected)
    }

    pub(super) fn all_selected(&self) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|selection| selection.start == 0 && selection.end == self.text.len())
    }
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
    pub(super) icon_picker_open: bool,
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
    pub(super) kind: CloseConfirmationKind,
}

/// Drives the close-dialog copy for the pane kind being closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CloseConfirmationKind {
    Terminal,
    Browser,
    Assistant,
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

const MAX_ASSISTANT_INSTRUCTIONS_CHARS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceCreationKind {
    Local,
    SystemSsh,
    Assistant,
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
    WorkingDir,
    Instructions,
}

impl WorkspaceCreationField {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Destination => 1,
            Self::WorkingDir => 2,
            Self::Instructions => 3,
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
    pub(super) working_dir: DialogTextEditor,
    pub(super) instructions: DialogTextEditor,
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
            working_dir: DialogTextEditor::default(),
            instructions: DialogTextEditor::default(),
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
            WorkspaceCreationField::WorkingDir => &self.working_dir,
            WorkspaceCreationField::Instructions => &self.instructions,
        }
    }

    pub(super) fn active_editor_mut(&mut self) -> &mut DialogTextEditor {
        match self.field {
            WorkspaceCreationField::Name => &mut self.name,
            WorkspaceCreationField::Destination => &mut self.destination,
            WorkspaceCreationField::WorkingDir => &mut self.working_dir,
            WorkspaceCreationField::Instructions => &mut self.instructions,
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
            WorkspaceCreationField::WorkingDir => (MAX_WORKSPACE_DIR_BYTES, true),
            WorkspaceCreationField::Instructions => (MAX_ASSISTANT_INSTRUCTIONS_CHARS, false),
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
            WorkspaceCreationKind::Assistant if self.step == WorkspaceCreationStep::Details => {
                let working_dir = (!self.working_dir.text.trim().is_empty())
                    .then(|| expand_home(self.working_dir.text.trim()));
                let instructions = (!self.instructions.text.trim().is_empty())
                    .then(|| self.instructions.text.trim().to_owned());
                Some(ClientRequest::CreateAssistantWorkspace {
                    title,
                    working_dir,
                    instructions,
                })
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
            WorkspaceCreationKind::Local
            | WorkspaceCreationKind::SystemSsh
            | WorkspaceCreationKind::Assistant => None,
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
        let kind = if pane.kind.is_browser() {
            CloseConfirmationKind::Browser
        } else if pane.kind.is_assistant() {
            CloseConfirmationKind::Assistant
        } else {
            CloseConfirmationKind::Terminal
        };
        Self {
            pane_id: pane.id,
            title: pane.title.clone(),
            leaves_workspace_empty,
            kind,
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
#[path = "view_models_tests.rs"]
mod tests;
