#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::unused_self
)]

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Application, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle,
    DispatchPhase, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, KeyBinding,
    KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, ScrollWheelEvent, ShapedLine, StrikethroughStyle, Style, StyledText, TextRun,
    TitlebarOptions, UTF16Selection, UnderlineStyle, Window, WindowBounds, WindowOptions, actions,
    div, fill, img, point, prelude::*, px, relative, rgb, rgba, size, svg,
};
use nah_desktop::request;
use nah_protocol::{
    AppearanceColor, ClientRequest, DropPlacement, HistoryArchiveStatus, HistoryCleanupPolicy,
    HistoryClearScope, HistoryPageDirection, HistoryPageFlags, HistoryRetention, HistorySettings,
    HistoryWarning, MAX_SSH_INPUT_LEN, Pane, PaneLayout, PaneRevisionCursor, PaneStreamState,
    ServiceResponse, SessionSnapshot, SplitAxis, StreamDiagnostics, TerminalAttributes,
    TerminalColor, TerminalHistoryPage, TerminalLine, TerminalModes, TerminalModifiers,
    TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalProfile, TerminalRun,
    TerminalScreen, TerminalSelection, TerminalSelectionKind, Workspace, WorkspaceConnection,
    WorkspaceConnectionStatus, WorkspacePinMove, normalize_ssh_input, validate_ssh_host,
};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

mod agent_icons;
mod commands;
mod theme;
mod typography;
mod ui_state;

use agent_icons::{AgentIconAssets, AgentIconFormat, agent_icon_definition};
use commands::{
    AppCommand, AppConfig, ROOT_KEY_CONTEXT, ResolvedBinding, ResolvedKeymap, descriptor,
    palette_matches,
};
use theme::{AppTheme, BuiltInTheme};
use typography::TerminalFontProfile;
use ui_state::UiStateStore;

actions!(
    nah_app,
    [
        NewWorkspace,
        ToggleSidebar,
        NewTab,
        SplitRight,
        SplitDown,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        ShowCommandPalette,
        TogglePaneZoom,
        EqualizePanes,
        ConsumeChordPrefix,
        CopyTerminal,
        PasteTerminal,
        FindTerminal,
        FindNextTerminal,
    ]
);

const DEFAULT_SIDEBAR_WIDTH: f32 = 190.0;
const MIN_SIDEBAR_WIDTH: f32 = 150.0;
const MAX_SIDEBAR_WIDTH: f32 = 420.0;
const MIN_TERMINAL_AREA_WIDTH: f32 = 320.0;
const SIDEBAR_RESIZE_HANDLE_WIDTH: f32 = 12.0;
const TITLEBAR_HEIGHT: f32 = 38.0;
const PANE_HEADER_HEIGHT: f32 = 29.0;
const SPLIT_DIVIDER_SIZE: f32 = 4.0;
const TERMINAL_HORIZONTAL_PADDING: f32 = 18.0;
const TERMINAL_VERTICAL_PADDING: f32 = 12.0;
const TERMINAL_FOCUS_BORDER_WIDTH: f32 = 1.0;
const MIN_PANE_WIDTH: f32 = 140.0;
const MIN_PANE_HEIGHT: f32 = 90.0;
const COMMAND_PALETTE_LIMIT: usize = 32;
const MAX_PASTE_BYTES: usize = 64 * 1024;
const ACTIVE_TERMINAL_POLL_MS: u64 = 33;
const IDLE_TERMINAL_POLL_MS: u64 = 250;
const COLD_PANE_AFTER: Duration = Duration::from_mins(1);
const THEME: AppTheme = BuiltInTheme::HarborNight.theme();
const APPEARANCE_PRESETS: [AppearanceColor; 8] = [
    AppearanceColor::new(0x62, 0xad, 0xff),
    AppearanceColor::new(0x67, 0xc8, 0xc6),
    AppearanceColor::new(0x95, 0xcc, 0x7f),
    AppearanceColor::new(0xe4, 0xbd, 0x72),
    AppearanceColor::new(0xef, 0x71, 0x7a),
    AppearanceColor::new(0xc9, 0x90, 0xe5),
    AppearanceColor::new(0xf0, 0x8a, 0xc0),
    AppearanceColor::new(0x9a, 0xa2, 0xaf),
];

#[derive(Clone, Debug)]
struct PaneDrag {
    pane_id: Uuid,
    title: String,
    position: Point<Pixels>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TabIdentityPresentation {
    label: String,
    profile: TerminalProfile,
    detail: String,
}

impl Render for PaneDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .left(self.position.x - px(70.0))
            .top(self.position.y - px(14.0))
            .w(px(140.0))
            .h(px(28.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .flex()
            .items_center()
            .justify_center()
            .font_family("SF Mono")
            .text_xs()
            .text_color(rgb(THEME.foreground))
            .child(self.title.clone())
    }
}

#[derive(Clone, Debug)]
struct TooltipView {
    text: String,
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
struct TabMenu {
    pane_id: Uuid,
    position: Point<Pixels>,
}

#[derive(Clone, Copy, Debug)]
struct WorkspaceMenu {
    workspace_id: Uuid,
    position: Point<Pixels>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColorTarget {
    DefaultTerminal,
    DefaultWorkspace,
    Pane(Uuid),
    Workspace(Uuid),
}

#[derive(Clone, Debug)]
struct ColorPickerState {
    target: ColorTarget,
    hex: String,
    replace_on_type: bool,
    invalid: bool,
}

#[derive(Clone, Debug)]
struct RenameEditor {
    pane_id: Uuid,
    value: String,
    replace_on_type: bool,
}

#[derive(Clone, Debug, Default)]
struct SearchEditor {
    query: String,
    no_match: bool,
}

#[derive(Clone, Debug)]
struct ArchivedView {
    page: TerminalHistoryPage,
    first_line: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryEditField {
    RetentionDays,
    QuotaGib,
}

#[derive(Clone, Debug)]
struct HistoryEditor {
    field: HistoryEditField,
    text: String,
    replace_on_type: bool,
    invalid: bool,
}

#[derive(Clone, Copy, Debug)]
struct TerminalLineRender {
    row: usize,
    cursor: Option<nah_protocol::TerminalCursor>,
    focused: bool,
    pane_id: Uuid,
    columns: u16,
    selection: Option<TerminalSelection>,
}

#[derive(Clone, Copy, Debug)]
struct SelectionDrag {
    pane_id: Uuid,
    anchor: TerminalPoint,
    preserve_single_cell: bool,
}

#[derive(Clone, Debug)]
struct CloseConfirmation {
    pane_id: Uuid,
    title: String,
    leaves_workspace_empty: bool,
}

#[derive(Clone, Debug)]
struct WorkspaceRenameEditor {
    workspace_id: Uuid,
    value: String,
    replace_on_type: bool,
}

#[derive(Clone, Debug)]
struct WorkspaceDeleteConfirmation {
    workspace_id: Uuid,
    title: String,
    active_terminal_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceCreationKind {
    Local,
    SystemSsh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceCreationStep {
    Details,
    ConfirmSsh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceCreationField {
    Name,
    Destination,
}

impl WorkspaceCreationField {
    const fn index(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Destination => 1,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DialogTextEditor {
    text: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
}

impl DialogTextEditor {
    fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            selected_range: end..end,
            selection_reversed: false,
            marked_range: None,
        }
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
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

    fn offset_to_utf16(&self, offset: usize) -> usize {
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

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.text.len());
        self.text[offset..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(index, _)| offset + index)
    }

    fn move_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.marked_range = None;
    }

    fn select_to(&mut self, offset: usize) {
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

    fn move_left(&mut self, selecting: bool) {
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

    fn move_right(&mut self, selecting: bool) {
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

    fn move_home(&mut self, selecting: bool) {
        if selecting {
            self.select_to(0);
        } else {
            self.move_to(0);
        }
    }

    fn move_end(&mut self, selecting: bool) {
        if selecting {
            self.select_to(self.text.len());
        } else {
            self.move_to(self.text.len());
        }
    }

    fn select_all(&mut self) {
        self.selected_range = 0..self.text.len();
        self.selection_reversed = false;
        self.marked_range = None;
    }

    fn selected_text(&self) -> Option<&str> {
        (!self.selected_range.is_empty()).then(|| &self.text[self.selected_range.clone()])
    }

    fn replacement_range(&self, range_utf16: Option<&Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone())
    }

    fn replace(
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

    fn utf16_offset_in(text: &str, offset: usize) -> usize {
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

    fn delete_backward(&mut self) {
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            self.selected_range = self.previous_boundary(cursor)..cursor;
        }
        self.replace(None, "", usize::MAX, true, false, None);
    }

    fn delete_forward(&mut self) {
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            self.selected_range = cursor..self.next_boundary(cursor);
        }
        self.replace(None, "", usize::MAX, true, false, None);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceCreationDialog {
    kind: WorkspaceCreationKind,
    name: DialogTextEditor,
    destination: DialogTextEditor,
    field: WorkspaceCreationField,
    step: WorkspaceCreationStep,
    error: Option<String>,
}

impl WorkspaceCreationDialog {
    fn new() -> Self {
        Self {
            kind: WorkspaceCreationKind::Local,
            name: DialogTextEditor::default(),
            destination: DialogTextEditor::default(),
            field: WorkspaceCreationField::Name,
            step: WorkspaceCreationStep::Details,
            error: None,
        }
    }

    fn review(&mut self) {
        match normalize_ssh_input(&self.destination.text) {
            Ok(destination) => {
                self.destination = DialogTextEditor::with_text(destination);
                self.step = WorkspaceCreationStep::ConfirmSsh;
                self.error = None;
            }
            Err(message) => self.error = Some(message.to_owned()),
        }
    }

    fn active_editor(&self) -> &DialogTextEditor {
        match self.field {
            WorkspaceCreationField::Name => &self.name,
            WorkspaceCreationField::Destination => &self.destination,
        }
    }

    fn active_editor_mut(&mut self) -> &mut DialogTextEditor {
        match self.field {
            WorkspaceCreationField::Name => &mut self.name,
            WorkspaceCreationField::Destination => &mut self.destination,
        }
    }

    fn replace_text(
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

    fn paste(&mut self, text: &str) {
        self.replace_text(None, text, false, None);
    }

    fn backspace(&mut self) {
        self.active_editor_mut().delete_backward();
        self.error = None;
    }

    fn delete(&mut self) {
        self.active_editor_mut().delete_forward();
        self.error = None;
    }

    fn approved_request(&self) -> Option<ClientRequest> {
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

fn route_workspace_creation_paste(
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
    fn for_pane(pane: &Pane, leaves_workspace_empty: bool) -> Self {
        Self {
            pane_id: pane.id,
            title: pane.title.clone(),
            leaves_workspace_empty,
        }
    }

    fn request(&self) -> ClientRequest {
        ClientRequest::ClosePane {
            pane_id: self.pane_id,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum PaneControlIcon {
    Add,
    SplitRight,
    SplitDown,
}

#[derive(Clone, Copy, Debug)]
struct ResizeDrag {
    split_id: SplitControlId,
    axis: SplitAxis,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum SidebarResizeLifecycle {
    #[default]
    Idle,
    Dragging {
        initial_width: f32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidebarResizeMove {
    Ignore,
    Update,
    Complete,
}

impl SidebarResizeLifecycle {
    fn begin(&mut self, initial_width: f32) {
        *self = Self::Dragging { initial_width };
    }

    fn pointer_move(&mut self, pressed_button: Option<MouseButton>) -> SidebarResizeMove {
        match (*self, pressed_button) {
            (Self::Idle, _) => SidebarResizeMove::Ignore,
            (Self::Dragging { .. }, Some(MouseButton::Left)) => SidebarResizeMove::Update,
            (Self::Dragging { .. }, _) => {
                *self = Self::Idle;
                SidebarResizeMove::Complete
            }
        }
    }

    fn finish(&mut self) -> bool {
        if matches!(self, Self::Idle) {
            return false;
        }
        *self = Self::Idle;
        true
    }

    fn cancel(&mut self) -> Option<f32> {
        let Self::Dragging { initial_width } = *self else {
            return None;
        };
        *self = Self::Idle;
        Some(initial_width)
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Dragging { .. })
    }
}

/// Client-local split identity. The current protocol has no split IDs, so this
/// wraps its deterministic compatibility key behind one boundary. A future
/// protocol `SplitId` can replace the field without changing layout controls.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SplitControlId {
    first: Uuid,
    second: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayoutControlMutation {
    Equalize,
}

#[derive(Clone, Debug, Default)]
struct CommandPaletteState {
    query: String,
    selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PixelRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragDestination {
    Split {
        target_pane: Uuid,
        placement: DropPlacement,
    },
    Merge {
        target_pane: Uuid,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DragHoverState {
    destination: Option<DragDestination>,
}

impl DragHoverState {
    fn enter(&mut self, destination: DragDestination) {
        self.destination = Some(destination);
    }

    fn clear(&mut self) {
        self.destination = None;
    }

    fn split_for(self, target_pane: Uuid) -> Option<DropPlacement> {
        match self.destination {
            Some(DragDestination::Split {
                target_pane: target,
                placement,
            }) if target == target_pane => Some(placement),
            _ => None,
        }
    }

    fn merges_into(self, target_pane: Uuid) -> bool {
        matches!(
            self.destination,
            Some(DragDestination::Merge { target_pane: target }) if target == target_pane
        )
    }
}

#[derive(Debug)]
struct NahApp {
    focus_handle: FocusHandle,
    terminal_font: TerminalFontProfile,
    keymap: ResolvedKeymap,
    snapshot: Option<SessionSnapshot>,
    screens: HashMap<Uuid, TerminalScreen>,
    pane_states: HashMap<Uuid, PaneStreamState>,
    pane_attention: HashMap<Uuid, Instant>,
    stream_diagnostics: StreamDiagnostics,
    active_workspace: Option<Uuid>,
    expanded_workspaces: HashSet<Uuid>,
    focused_pane: Option<Uuid>,
    split_ratios: HashMap<SplitControlId, f32>,
    zoomed_pane: Option<Uuid>,
    command_palette: Option<CommandPaletteState>,
    resizing: Option<ResizeDrag>,
    sidebar_resize: SidebarResizeLifecycle,
    ui_state_store: Option<UiStateStore>,
    preferred_sidebar_width: f32,
    sidebar_visible: bool,
    sidebar_pixels: f32,
    last_sizes: HashMap<Uuid, (u16, u16)>,
    workspace_pixels: (f32, f32),
    connection_error: Option<String>,
    tab_menu: Option<TabMenu>,
    workspace_menu: Option<WorkspaceMenu>,
    appearance_settings_open: bool,
    history_status: Option<HistoryArchiveStatus>,
    archived_views: HashMap<Uuid, ArchivedView>,
    history_editor: Option<HistoryEditor>,
    history_clear_confirmation: Option<HistoryClearScope>,
    color_picker: Option<ColorPickerState>,
    rename_editor: Option<RenameEditor>,
    workspace_rename_editor: Option<WorkspaceRenameEditor>,
    search_editor: Option<SearchEditor>,
    close_confirmation: Option<CloseConfirmation>,
    workspace_delete_confirmation: Option<WorkspaceDeleteConfirmation>,
    workspace_creation: Option<WorkspaceCreationDialog>,
    dragging_pane: Option<Uuid>,
    drag_hover: DragHoverState,
    selection_drag: Option<SelectionDrag>,
    ime_preedit: String,
    workspace_input_focus: [FocusHandle; 2],
    workspace_input_layouts: [Option<ShapedLine>; 2],
    workspace_input_bounds: [Option<Bounds<Pixels>>; 2],
}

impl NahApp {
    fn new(window: &mut Window, keymap: ResolvedKeymap, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        let workspace_input_focus = [cx.focus_handle(), cx.focus_handle()];
        let terminal_font = TerminalFontProfile::resolve(cx.text_system());
        let ui_state_store = match UiStateStore::from_default_path() {
            Ok(store) => Some(store),
            Err(error) => {
                eprintln!("Not a Harness UI state unavailable: {error:#}");
                None
            }
        };
        let preferred_sidebar_width = ui_state_store
            .as_ref()
            .and_then(|store| match store.load_workspace_sidebar_width() {
                Ok(width) => width,
                Err(error) => {
                    eprintln!("Not a Harness UI state ignored: {error:#}");
                    None
                }
            })
            .unwrap_or(DEFAULT_SIDEBAR_WIDTH);
        let mut app = Self {
            focus_handle,
            terminal_font,
            keymap,
            snapshot: None,
            screens: HashMap::new(),
            pane_states: HashMap::new(),
            pane_attention: HashMap::new(),
            stream_diagnostics: StreamDiagnostics::default(),
            active_workspace: None,
            expanded_workspaces: HashSet::new(),
            focused_pane: None,
            split_ratios: HashMap::new(),
            zoomed_pane: None,
            command_palette: None,
            resizing: None,
            sidebar_resize: SidebarResizeLifecycle::default(),
            ui_state_store,
            preferred_sidebar_width,
            sidebar_visible: true,
            sidebar_pixels: DEFAULT_SIDEBAR_WIDTH,
            last_sizes: HashMap::new(),
            workspace_pixels: (0.0, 0.0),
            connection_error: None,
            tab_menu: None,
            workspace_menu: None,
            appearance_settings_open: false,
            history_status: None,
            archived_views: HashMap::new(),
            history_editor: None,
            history_clear_confirmation: None,
            color_picker: None,
            rename_editor: None,
            workspace_rename_editor: None,
            search_editor: None,
            close_confirmation: None,
            workspace_delete_confirmation: None,
            workspace_creation: None,
            dragging_pane: None,
            drag_hover: DragHoverState::default(),
            selection_drag: None,
            ime_preedit: String::new(),
            workspace_input_focus,
            workspace_input_layouts: [None, None],
            workspace_input_bounds: [None, None],
        };
        app.update_window_geometry(window);
        app.refresh_state();
        if app.focused_pane.is_some() && app.screens.is_empty() {
            app.refresh_state();
        }

        cx.observe_window_bounds(window, |this, window, cx| {
            if this.update_window_geometry(window) {
                this.sync_pty_sizes();
                cx.notify();
            }
        })
        .detach();

        cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.cancel_sidebar_resize(window, cx);
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let mut poll_delay_ms = ACTIVE_TERMINAL_POLL_MS;
            loop {
                gpui::Timer::after(Duration::from_millis(poll_delay_ms)).await;
                let Ok(update_request) = this.update(cx, |this, _| this.pane_update_request())
                else {
                    break;
                };
                let response = cx
                    .background_spawn(async move { request(update_request) })
                    .await;
                let Ok(state_changed) = this.update(cx, |this, cx| {
                    let state_changed = this.apply_update_result(response);
                    this.sync_pty_sizes();
                    if state_changed {
                        cx.notify();
                    }
                    state_changed
                }) else {
                    break;
                };
                poll_delay_ms = next_terminal_poll_delay_ms(poll_delay_ms, state_changed);
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_secs(5)).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    if this.refresh_history_status() {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
        app
    }

    fn refresh_state(&mut self) -> bool {
        self.apply_update_result(request(self.pane_update_request()))
    }

    fn pane_update_request(&self) -> ClientRequest {
        let now = Instant::now();
        let pane_revisions = self
            .screens
            .values()
            .map(|screen| PaneRevisionCursor {
                pane_id: screen.pane_id,
                revision: screen.revision,
            })
            .collect();
        let subscribed_panes = responsive_panes(now, self.focused_pane, &self.pane_attention);
        ClientRequest::GetUpdates {
            snapshot_revision: self.snapshot.as_ref().map(|snapshot| snapshot.revision),
            pane_revisions,
            subscribed_panes,
        }
    }

    fn apply_update_result(&mut self, result: anyhow::Result<ServiceResponse>) -> bool {
        match result {
            Ok(ServiceResponse::Updates {
                session_revision,
                snapshot,
                screens,
                pane_states,
                diagnostics,
            }) => {
                let apply_started = Instant::now();
                let current_session_revision =
                    self.snapshot.as_ref().map(|snapshot| snapshot.revision);
                let topology_is_current =
                    current_session_revision.is_none_or(|current| session_revision >= current);
                let mut snapshot_changed = false;
                let mut screens_applied = 0;
                let mut focus_resync = None;
                if let Some(snapshot) = snapshot
                    && current_session_revision.is_none_or(|current| snapshot.revision >= current)
                {
                    snapshot_changed = self.snapshot.as_ref() != Some(&snapshot);
                    if self.active_workspace.is_none()
                        || !snapshot.workspaces.iter().any(|workspace| {
                            Some(workspace.id) == self.active_workspace
                                && workspace_is_selectable(workspace)
                        })
                    {
                        self.active_workspace = snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace_is_selectable(workspace))
                            .map(|workspace| workspace.id);
                    }
                    let visible = self
                        .active_workspace_in(&snapshot)
                        .and_then(|workspace| workspace.tabs.first())
                        .map(|tab| visible_panes(&tab.layout))
                        .unwrap_or_default();
                    if self
                        .zoomed_pane
                        .is_some_and(|pane| !visible.contains(&pane))
                    {
                        self.zoomed_pane = None;
                        self.last_sizes.clear();
                    }
                    if self.focused_pane.is_none()
                        || !visible.iter().any(|pane| Some(*pane) == self.focused_pane)
                    {
                        focus_resync = visible.first().copied();
                        if focus_resync.is_none() {
                            self.focused_pane = None;
                        }
                    }
                    self.snapshot = Some(snapshot);
                }
                for screen in screens {
                    let is_newer = self
                        .screens
                        .get(&screen.pane_id)
                        .is_none_or(|current| screen.revision > current.revision);
                    if is_newer {
                        self.screens.insert(screen.pane_id, screen);
                        screens_applied += 1;
                    }
                }
                if topology_is_current {
                    let live_panes = pane_states
                        .iter()
                        .map(|state| state.pane_id)
                        .collect::<std::collections::HashSet<_>>();
                    self.screens
                        .retain(|pane_id, _| live_panes.contains(pane_id));
                    self.pane_attention
                        .retain(|pane_id, _| live_panes.contains(pane_id));
                    self.pane_states = pane_states
                        .into_iter()
                        .map(|state| (state.pane_id, state))
                        .collect();
                }
                self.stream_diagnostics = diagnostics;
                let connection_changed = self.connection_error.take().is_some();
                self.connection_error = None;
                let mut state_changed =
                    pane_update_requires_repaint(snapshot_changed, screens_applied)
                        || connection_changed;
                if let Some(pane_id) = focus_resync {
                    state_changed |= self.focus_pane_with_snapshot(pane_id);
                }
                self.stream_diagnostics.desktop_apply_micros =
                    u64::try_from(apply_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                state_changed
            }
            Ok(response) => {
                let error = format!("unexpected response: {response:?}");
                let changed = self.connection_error.as_deref() != Some(error.as_str());
                self.connection_error = Some(error);
                changed
            }
            Err(error) => {
                let error = format!("{error:#}");
                let changed = self.connection_error.as_deref() != Some(error.as_str());
                self.connection_error = Some(error);
                changed
            }
        }
    }

    fn focus_pane_with_snapshot(&mut self, pane_id: Uuid) -> bool {
        if self.focused_pane == Some(pane_id) {
            self.pane_attention.insert(pane_id, Instant::now());
            return false;
        }
        match request(ClientRequest::GetPaneSnapshot { pane_id }) {
            Ok(ServiceResponse::PaneSnapshot {
                screen,
                diagnostics,
            }) => {
                let attended_at = Instant::now();
                let changed = self.focused_pane != Some(pane_id)
                    || self
                        .screens
                        .get(&pane_id)
                        .is_none_or(|current| current.revision != screen.revision);
                if self.focused_pane != Some(pane_id)
                    && let Some(previous) = self.focused_pane
                {
                    self.pane_attention.insert(previous, attended_at);
                }
                self.pane_states.insert(
                    pane_id,
                    PaneStreamState {
                        pane_id,
                        revision: screen.revision,
                        subscribed: true,
                        dirty: false,
                    },
                );
                self.screens.insert(pane_id, screen);
                self.focused_pane = Some(pane_id);
                self.pane_attention.insert(pane_id, attended_at);
                self.stream_diagnostics = diagnostics;
                self.connection_error = None;
                changed
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
                false
            }
            Err(error) => {
                self.connection_error = Some(format!("{error:#}"));
                false
            }
        }
    }

    fn active_workspace_in<'a>(&self, snapshot: &'a SessionSnapshot) -> Option<&'a Workspace> {
        let active = self.active_workspace?;
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == active)
    }

    fn terminal_accent(&self, pane_id: Uuid) -> AppearanceColor {
        self.snapshot
            .as_ref()
            .map_or(AppearanceColor::HARBOR_BLUE, |snapshot| {
                resolved_terminal_accent(snapshot, pane_id)
            })
    }

    fn workspace_color(&self, workspace_id: Uuid) -> AppearanceColor {
        self.snapshot
            .as_ref()
            .map_or(AppearanceColor::HARBOR_BLUE, |snapshot| {
                resolved_workspace_color(snapshot, workspace_id)
            })
    }

    fn appearance_choices(&self) -> Vec<AppearanceColor> {
        let mut colors = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.appearance.recent_colors.clone())
            .unwrap_or_default();
        for preset in APPEARANCE_PRESETS {
            if !colors.contains(&preset) {
                colors.push(preset);
            }
        }
        colors.truncate(12);
        colors
    }

    fn color_for_target(&self, target: ColorTarget) -> AppearanceColor {
        match target {
            ColorTarget::DefaultTerminal => self
                .snapshot
                .as_ref()
                .map_or(AppearanceColor::HARBOR_BLUE, |snapshot| {
                    snapshot.appearance.default_terminal_accent
                }),
            ColorTarget::DefaultWorkspace => self
                .snapshot
                .as_ref()
                .map_or(AppearanceColor::HARBOR_BLUE, |snapshot| {
                    snapshot.appearance.default_workspace_color
                }),
            ColorTarget::Pane(pane_id) => self.terminal_accent(pane_id),
            ColorTarget::Workspace(workspace_id) => self.workspace_color(workspace_id),
        }
    }

    fn apply_color(
        &mut self,
        target: ColorTarget,
        color: Option<AppearanceColor>,
        cx: &mut Context<Self>,
    ) {
        let request = match (target, color) {
            (ColorTarget::DefaultTerminal, Some(color)) => {
                ClientRequest::SetDefaultTerminalAccent { color }
            }
            (ColorTarget::DefaultWorkspace, Some(color)) => {
                ClientRequest::SetDefaultWorkspaceColor { color }
            }
            (ColorTarget::Pane(pane_id), color) => ClientRequest::SetPaneColor { pane_id, color },
            (ColorTarget::Workspace(workspace_id), color) => ClientRequest::SetWorkspaceColor {
                workspace_id,
                color,
            },
            (ColorTarget::DefaultTerminal | ColorTarget::DefaultWorkspace, None) => return,
        };
        self.send(request);
        self.tab_menu = None;
        self.workspace_menu = None;
        self.color_picker = None;
        cx.notify();
    }

    fn open_color_picker(&mut self, target: ColorTarget, cx: &mut Context<Self>) {
        let current = self.color_for_target(target).as_rgb();
        self.color_picker = Some(ColorPickerState {
            target,
            hex: format!("{current:06X}"),
            replace_on_type: true,
            invalid: false,
        });
        self.tab_menu = None;
        self.workspace_menu = None;
        cx.notify();
    }

    fn submit_color_picker(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.color_picker.as_ref() else {
            return;
        };
        let target = picker.target;
        let color = parse_hex_color(&picker.hex);
        if let Some(color) = color {
            self.apply_color(target, Some(color), cx);
        } else if let Some(picker) = self.color_picker.as_mut() {
            picker.invalid = true;
            cx.notify();
        }
    }

    fn open_appearance_settings(&mut self, cx: &mut Context<Self>) {
        self.appearance_settings_open = true;
        self.tab_menu = None;
        self.workspace_menu = None;
        self.color_picker = None;
        self.history_editor = None;
        self.history_clear_confirmation = None;
        let _ = self.refresh_history_status();
        cx.notify();
    }

    fn refresh_history_status(&mut self) -> bool {
        let previous = self.history_status.clone();
        match request(ClientRequest::GetHistoryStatus) {
            Ok(ServiceResponse::HistoryStatus { status }) => {
                self.history_status = Some(status);
                self.connection_error = None;
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        self.history_status != previous
    }

    fn apply_history_settings(&mut self, settings: HistorySettings, cx: &mut Context<Self>) {
        match request(ClientRequest::SetHistorySettings { settings }) {
            Ok(ServiceResponse::HistoryStatus { status }) => {
                self.history_status = Some(status);
                self.history_editor = None;
                self.connection_error = None;
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn mutate_history_settings(
        &mut self,
        update: impl FnOnce(&mut HistorySettings),
        cx: &mut Context<Self>,
    ) {
        let Some(mut settings) = self
            .history_status
            .as_ref()
            .map(|status| status.settings.clone())
        else {
            let _ = self.refresh_history_status();
            cx.notify();
            return;
        };
        update(&mut settings);
        self.apply_history_settings(settings, cx);
    }

    fn clear_history(&mut self, scope: HistoryClearScope, cx: &mut Context<Self>) {
        if self.history_clear_confirmation != Some(scope) {
            self.history_clear_confirmation = Some(scope);
            cx.notify();
            return;
        }
        match request(ClientRequest::ClearHistory { scope }) {
            Ok(ServiceResponse::HistoryStatus { status }) => {
                self.history_status = Some(status);
                self.history_clear_confirmation = None;
                self.archived_views.clear();
                self.connection_error = None;
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn begin_history_edit(&mut self, field: HistoryEditField, cx: &mut Context<Self>) {
        let text = match (field, self.history_status.as_ref()) {
            (
                HistoryEditField::RetentionDays,
                Some(HistoryArchiveStatus {
                    settings:
                        HistorySettings {
                            retention: HistoryRetention::Days { days },
                            ..
                        },
                    ..
                }),
            ) => days.to_string(),
            (HistoryEditField::RetentionDays, _) => "30".to_owned(),
            (HistoryEditField::QuotaGib, Some(status)) => {
                (status.settings.quota_bytes / 1024 / 1024 / 1024).to_string()
            }
            (HistoryEditField::QuotaGib, None) => "5".to_owned(),
        };
        self.history_editor = Some(HistoryEditor {
            field,
            text,
            replace_on_type: true,
            invalid: false,
        });
        cx.notify();
    }

    fn submit_history_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.history_editor.as_ref() else {
            return;
        };
        let field = editor.field;
        let Ok(value) = editor.text.parse::<u64>() else {
            if let Some(editor) = self.history_editor.as_mut() {
                editor.invalid = true;
            }
            cx.notify();
            return;
        };
        match field {
            HistoryEditField::RetentionDays if (1..=3_650).contains(&value) => {
                self.mutate_history_settings(
                    |settings| {
                        settings.retention = HistoryRetention::Days {
                            days: u32::try_from(value).unwrap_or(3_650),
                        };
                    },
                    cx,
                );
            }
            HistoryEditField::QuotaGib if (1..=4_096).contains(&value) => {
                self.mutate_history_settings(
                    |settings| {
                        settings.quota_bytes = value * 1024 * 1024 * 1024;
                    },
                    cx,
                );
            }
            _ => {
                if let Some(editor) = self.history_editor.as_mut() {
                    editor.invalid = true;
                }
                cx.notify();
            }
        }
    }

    fn send(&mut self, request_message: ClientRequest) {
        self.send_control(request_message);
        self.refresh_state();
    }

    fn send_control(&mut self, request_message: ClientRequest) {
        if let Err(error) = request(request_message) {
            self.connection_error = Some(format!("{error:#}"));
        }
    }

    fn new_workspace(&mut self, cx: &mut Context<Self>) {
        self.begin_workspace_creation(cx);
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        if self.sidebar_resize.finish() {
            self.persist_sidebar_width();
        }
        let window_width = self.workspace_pixels.0 + self.sidebar_pixels;
        self.sidebar_pixels = sidebar_width_for_visibility(
            self.preferred_sidebar_width,
            window_width,
            self.sidebar_visible,
        );
        self.workspace_pixels.0 = (window_width - self.sidebar_pixels).max(1.0);
        self.last_sizes.clear();
        self.sync_pty_sizes();
        cx.notify();
    }

    fn toggle_workspace_expanded(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        if !self.expanded_workspaces.remove(&workspace_id) {
            self.expanded_workspaces.insert(workspace_id);
        }
        cx.notify();
    }

    fn select_workspace_tab(&mut self, workspace_id: Uuid, pane_id: Uuid, cx: &mut Context<Self>) {
        self.active_workspace = Some(workspace_id);
        self.activate_tab(pane_id, cx);
        self.last_sizes.clear();
    }

    fn new_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(target_pane) = self.focused_pane {
            self.new_tab_at(target_pane, cx);
        } else if let Some(workspace_id) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| self.active_workspace_in(snapshot))
            .filter(|workspace| workspace.tabs.is_empty())
            .map(|workspace| workspace.id)
        {
            self.open_workspace_terminal(workspace_id, cx);
        }
    }

    fn open_workspace_terminal(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        match request(ClientRequest::CreateWorkspaceTerminal { workspace_id }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_pane_with_snapshot(pane_id);
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        self.refresh_state();
        self.last_sizes.clear();
        cx.notify();
    }

    fn new_tab_at(&mut self, target_pane: Uuid, cx: &mut Context<Self>) {
        match request(ClientRequest::CreateTab { target_pane }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_pane_with_snapshot(pane_id);
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"))
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        self.refresh_state();
        cx.notify();
    }

    fn split(&mut self, axis: SplitAxis, cx: &mut Context<Self>) {
        if let Some(target_pane) = self.focused_pane {
            self.split_at(target_pane, axis, cx);
        }
    }

    fn split_at(&mut self, target_pane: Uuid, axis: SplitAxis, cx: &mut Context<Self>) {
        self.zoomed_pane = None;
        match request(ClientRequest::CreatePane { target_pane, axis }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_pane_with_snapshot(pane_id);
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"))
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        self.refresh_state();
        self.last_sizes.clear();
        cx.notify();
    }

    fn activate_tab(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        match request(ClientRequest::ActivateTab { pane_id }) {
            Ok(ServiceResponse::Ack) => {
                self.focus_pane_with_snapshot(pane_id);
                self.refresh_state();
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn swap_panes(&mut self, source_pane: Uuid, target_pane: Uuid, cx: &mut Context<Self>) {
        if source_pane != target_pane {
            self.zoomed_pane = None;
            self.send(ClientRequest::SwapPanes {
                source_pane,
                target_pane,
            });
            self.focus_pane_with_snapshot(source_pane);
            self.last_sizes.clear();
            cx.notify();
        }
    }

    fn move_pane_to_split(
        &mut self,
        source_pane: Uuid,
        target_pane: Uuid,
        placement: DropPlacement,
        cx: &mut Context<Self>,
    ) {
        self.dragging_pane = None;
        self.drag_hover.clear();
        self.zoomed_pane = None;
        self.send(ClientRequest::MovePaneToSplit {
            source_pane,
            target_pane,
            placement,
        });
        self.focus_pane_with_snapshot(source_pane);
        self.last_sizes.clear();
        cx.notify();
    }

    fn move_pane_to_tab(&mut self, source_pane: Uuid, target_pane: Uuid, cx: &mut Context<Self>) {
        self.dragging_pane = None;
        self.drag_hover.clear();
        self.zoomed_pane = None;
        self.send(ClientRequest::MovePaneToTab {
            source_pane,
            target_pane,
        });
        self.focus_pane_with_snapshot(source_pane);
        self.last_sizes.clear();
        cx.notify();
    }

    fn pane_metadata(&self, pane_id: Uuid) -> Option<Pane> {
        self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find_map(|tab| find_pane(&tab.layout, pane_id).cloned())
        })
    }

    fn open_tab_menu(&mut self, pane_id: Uuid, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Err(error) = request(ClientRequest::ActivateTab { pane_id }) {
            self.connection_error = Some(format!("{error:#}"));
        }
        self.focus_pane_with_snapshot(pane_id);
        self.refresh_state();
        self.tab_menu = Some(TabMenu { pane_id, position });
        self.workspace_menu = None;
        self.rename_editor = None;
        self.close_confirmation = None;
        cx.notify();
    }

    fn open_workspace_menu(
        &mut self,
        workspace_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.workspace_menu = Some(WorkspaceMenu {
            workspace_id,
            position,
        });
        self.tab_menu = None;
        self.last_sizes.clear();
        cx.notify();
    }

    fn begin_rename(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focus_pane_with_snapshot(pane_id);
        if let Some(pane) = self.pane_metadata(pane_id) {
            self.rename_editor = Some(RenameEditor {
                pane_id,
                value: pane.title,
                replace_on_type: true,
            });
            self.tab_menu = None;
            cx.notify();
        }
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.rename_editor.take() else {
            return;
        };
        self.send(ClientRequest::RenamePane {
            pane_id: editor.pane_id,
            title: editor.value,
        });
        cx.notify();
    }

    fn set_pane_profile(
        &mut self,
        pane_id: Uuid,
        profile: Option<TerminalProfile>,
        cx: &mut Context<Self>,
    ) {
        self.send(ClientRequest::SetPaneProfile { pane_id, profile });
        self.tab_menu = None;
        cx.notify();
    }

    fn reset_pane_identity(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.send(ClientRequest::ResetPaneIdentity { pane_id });
        self.tab_menu = None;
        cx.notify();
    }

    fn begin_close(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focus_pane_with_snapshot(pane_id);
        if let Some(pane) = self.pane_metadata(pane_id) {
            let leaves_workspace_empty = self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.workspaces.iter().any(|workspace| {
                    let panes = workspace
                        .tabs
                        .iter()
                        .flat_map(|tab| visible_panes(&tab.layout))
                        .collect::<Vec<_>>();
                    panes.len() == 1 && panes[0] == pane_id
                })
            });
            self.close_confirmation =
                Some(CloseConfirmation::for_pane(&pane, leaves_workspace_empty));
            self.tab_menu = None;
            cx.notify();
        }
    }

    fn confirm_close(&mut self, cx: &mut Context<Self>) {
        let Some(confirmation) = self.close_confirmation.take() else {
            return;
        };
        self.send(confirmation.request());
        self.last_sizes.clear();
        cx.notify();
    }

    fn begin_workspace_creation(&mut self, cx: &mut Context<Self>) {
        self.workspace_creation = Some(WorkspaceCreationDialog::new());
        self.workspace_input_layouts = [None, None];
        self.workspace_input_bounds = [None, None];
        self.tab_menu = None;
        self.workspace_menu = None;
        self.rename_editor = None;
        self.close_confirmation = None;
        cx.notify();
    }

    fn focus_workspace_creation_field(
        &mut self,
        field: WorkspaceCreationField,
        position: Option<Point<Pixels>>,
        extend_selection: bool,
    ) {
        let index = field.index();
        let offset = position.and_then(|position| {
            let line = self.workspace_input_layouts[index].as_ref()?;
            let bounds = self.workspace_input_bounds[index]?;
            Some(line.closest_index_for_x(position.x - bounds.left()))
        });
        let Some(dialog) = self.workspace_creation.as_mut() else {
            return;
        };
        dialog.field = field;
        let editor = dialog.active_editor_mut();
        match offset {
            Some(offset) if extend_selection => editor.select_to(offset),
            Some(offset) => editor.move_to(offset),
            None => editor.move_end(false),
        }
    }

    fn submit_workspace_creation(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.workspace_creation.as_mut() else {
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
            .workspace_creation
            .as_ref()
            .and_then(WorkspaceCreationDialog::approved_request)
        else {
            return;
        };
        match request(request_message) {
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            }) => {
                self.active_workspace = Some(workspace_id);
                self.focus_pane_with_snapshot(pane_id);
                self.workspace_creation = None;
                self.refresh_state();
            }
            Ok(response) => {
                if let Some(dialog) = self.workspace_creation.as_mut() {
                    dialog.error = Some(format!("unexpected response: {response:?}"));
                }
            }
            Err(error) => {
                if let Some(dialog) = self.workspace_creation.as_mut() {
                    dialog.error = Some(format!("{error:#}"));
                }
            }
        }
        cx.notify();
    }

    fn begin_workspace_rename(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        if let Some(workspace) = workspace {
            self.workspace_rename_editor = Some(WorkspaceRenameEditor {
                workspace_id,
                value: workspace.title.clone(),
                replace_on_type: true,
            });
            self.workspace_menu = None;
            cx.notify();
        }
    }

    fn submit_workspace_rename(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.workspace_rename_editor.take() else {
            return;
        };
        self.send(ClientRequest::RenameWorkspace {
            workspace_id: editor.workspace_id,
            title: editor.value,
        });
        cx.notify();
    }

    fn set_workspace_pinned(&mut self, workspace_id: Uuid, pinned: bool, cx: &mut Context<Self>) {
        self.send(ClientRequest::SetWorkspacePinned {
            workspace_id,
            pinned,
        });
        self.workspace_menu = None;
        cx.notify();
    }

    fn move_pinned_workspace(
        &mut self,
        workspace_id: Uuid,
        direction: WorkspacePinMove,
        cx: &mut Context<Self>,
    ) {
        self.send(ClientRequest::MovePinnedWorkspace {
            workspace_id,
            direction,
        });
        self.workspace_menu = None;
        cx.notify();
    }

    fn disconnect_workspace(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.send_control(ClientRequest::DisconnectWorkspace { workspace_id });
        if self.connection_error.is_none() && self.active_workspace == Some(workspace_id) {
            self.active_workspace = None;
            self.focused_pane = None;
        }
        self.refresh_state();
        cx.notify();
    }

    fn reconnect_workspace(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        match request(ClientRequest::ReconnectWorkspace { workspace_id }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.active_workspace = Some(workspace_id);
                self.focus_pane_with_snapshot(pane_id);
                self.refresh_state();
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn begin_workspace_delete(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        if let Some(workspace) = workspace {
            self.workspace_delete_confirmation = Some(WorkspaceDeleteConfirmation {
                workspace_id,
                title: workspace.title.clone(),
                active_terminal_count: workspace.active_terminal_count,
            });
            self.workspace_menu = None;
            cx.notify();
        }
    }

    fn confirm_workspace_delete(&mut self, cx: &mut Context<Self>) {
        let Some(confirmation) = self.workspace_delete_confirmation.take() else {
            return;
        };
        self.send_control(ClientRequest::DeleteWorkspace {
            workspace_id: confirmation.workspace_id,
        });
        if self.connection_error.is_none()
            && self.active_workspace == Some(confirmation.workspace_id)
        {
            self.active_workspace = None;
            self.focused_pane = None;
        }
        self.refresh_state();
        cx.notify();
    }

    fn focus_direction(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(workspace) = self.active_workspace_in(snapshot) else {
            return;
        };
        let Some(tab) = workspace.tabs.first() else {
            return;
        };
        let panes = visible_panes(&tab.layout);
        let Some(current) = self.focused_pane else {
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
        self.focus_pane_with_snapshot(panes[next]);
        if self.zoomed_pane.is_some() {
            self.zoomed_pane = self.focused_pane;
            self.last_sizes.clear();
            self.sync_pty_sizes();
        }
        cx.notify();
    }

    fn execute_command(&mut self, command: AppCommand, cx: &mut Context<Self>) {
        self.command_palette = None;
        match command {
            AppCommand::NewWorkspace => self.new_workspace(cx),
            AppCommand::ToggleSidebar => self.toggle_sidebar(cx),
            AppCommand::NewTab => self.new_tab(cx),
            AppCommand::SplitRight => self.split(SplitAxis::Horizontal, cx),
            AppCommand::SplitDown => self.split(SplitAxis::Vertical, cx),
            AppCommand::FocusLeft | AppCommand::FocusUp => self.focus_direction(false, cx),
            AppCommand::FocusRight | AppCommand::FocusDown => self.focus_direction(true, cx),
            AppCommand::ShowCommandPalette => {
                self.command_palette = Some(CommandPaletteState::default());
                cx.notify();
            }
            AppCommand::TogglePaneZoom => self.toggle_pane_zoom(cx),
            AppCommand::EqualizePanes => self.equalize_panes(cx),
        }
    }

    fn toggle_pane_zoom(&mut self, cx: &mut Context<Self>) {
        let Some(focused) = self.focused_pane else {
            return;
        };
        self.zoomed_pane = if self.zoomed_pane == Some(focused) {
            None
        } else {
            Some(focused)
        };
        self.last_sizes.clear();
        self.sync_pty_sizes();
        cx.notify();
    }

    fn equalize_panes(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self
            .active_workspace_in(snapshot)
            .and_then(|workspace| workspace.tabs.first())
            .map(|tab| tab.layout.clone())
        else {
            return;
        };
        if apply_layout_control_mutation(
            &layout,
            &mut self.split_ratios,
            LayoutControlMutation::Equalize,
        ) > 0
        {
            self.last_sizes.clear();
            self.sync_pty_sizes();
            cx.notify();
        }
    }

    fn handle_palette_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mut execute = None;
        let mut close = false;
        if let Some(palette) = self.command_palette.as_mut() {
            let result_count = palette_matches(&palette.query, COMMAND_PALETTE_LIMIT).len();
            match keystroke.key.as_str() {
                "escape" => close = true,
                "enter" => {
                    execute = palette_matches(&palette.query, COMMAND_PALETTE_LIMIT)
                        .get(palette.selected)
                        .map(|item| item.command);
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
            self.command_palette = None;
            cx.notify();
        } else if let Some(command) = execute {
            self.execute_command(command, cx);
        }
        // Palette keystrokes are modal and can never become PTY input.
        cx.stop_propagation();
    }

    fn copy_terminal(&mut self, _: &CopyTerminal, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self
            .workspace_creation
            .as_ref()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
            .and_then(|dialog| dialog.active_editor().selected_text())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
            return;
        }
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        match request(ClientRequest::CopySelection { pane_id }) {
            Ok(ServiceResponse::SelectionText { text: Some(text) }) if !text.is_empty() => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.connection_error = None;
            }
            Ok(ServiceResponse::SelectionText { .. }) => {}
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn paste_terminal(&mut self, _: &PasteTerminal, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if route_workspace_creation_paste(self.workspace_creation.as_mut(), &text) {
            cx.notify();
            return;
        }
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        let bracketed = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::BRACKETED_PASTE));
        match prepare_paste(&text, bracketed) {
            Ok(bytes) => self.send_control(ClientRequest::WriteInput { pane_id, bytes }),
            Err(message) => self.connection_error = Some(message.to_owned()),
        }
        cx.notify();
    }

    fn find_terminal(&mut self, _: &FindTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.search_editor = Some(SearchEditor::default());
        self.ime_preedit.clear();
        cx.notify();
    }

    fn find_next_terminal(&mut self, _: &FindNextTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.run_search(true, cx);
    }

    fn run_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        let Some(editor) = self.search_editor.as_ref() else {
            return;
        };
        if editor.query.is_empty() {
            if let Some(editor) = self.search_editor.as_mut() {
                editor.no_match = false;
            }
            cx.notify();
            return;
        }
        let query = editor.query.clone();
        match request(ClientRequest::SearchPane {
            pane_id,
            query: query.clone(),
            forward,
        }) {
            Ok(ServiceResponse::SearchResult { found }) => {
                if !found {
                    let before = self
                        .archived_views
                        .get(&pane_id)
                        .map(|view| view.page.cursor);
                    match request(ClientRequest::SearchArchivedHistory {
                        pane_id,
                        query: query.clone(),
                        before,
                    }) {
                        Ok(ServiceResponse::HistorySearchResult { page: Some(page) }) => {
                            let rows = self
                                .screens
                                .get(&pane_id)
                                .map_or(30, |screen| usize::from(screen.rows));
                            let first_line = page
                                .lines
                                .iter()
                                .position(|line| line.contains(&query))
                                .unwrap_or(0)
                                .min(page.lines.len().saturating_sub(rows));
                            self.archived_views.clear();
                            self.archived_views
                                .insert(pane_id, ArchivedView { page, first_line });
                            if let Some(editor) = self.search_editor.as_mut() {
                                editor.no_match = false;
                            }
                        }
                        Ok(ServiceResponse::HistorySearchResult { page: None }) => {
                            if let Some(editor) = self.search_editor.as_mut() {
                                editor.no_match = true;
                            }
                        }
                        Ok(response) => {
                            self.connection_error =
                                Some(format!("unexpected response: {response:?}"));
                        }
                        Err(error) => self.connection_error = Some(format!("{error:#}")),
                    }
                } else if let Some(editor) = self.search_editor.as_mut() {
                    editor.no_match = false;
                    self.archived_views.remove(&pane_id);
                }
                self.connection_error = None;
                self.refresh_state();
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() || text.chars().any(|character| character == '\0') {
            return;
        }
        if let Some(picker) = self.color_picker.as_mut() {
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
        if let Some(editor) = self.history_editor.as_mut() {
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
        if let Some(dialog) = self.workspace_creation.as_mut() {
            if dialog.step == WorkspaceCreationStep::Details {
                dialog.replace_text(None, text, false, None);
            }
            cx.notify();
            return;
        }
        if let Some(editor) = self.workspace_rename_editor.as_mut() {
            if editor.replace_on_type {
                editor.value.clear();
            }
            let remaining = 80_usize.saturating_sub(editor.value.chars().count());
            editor
                .value
                .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
            editor.replace_on_type = false;
            cx.notify();
            return;
        }
        if let Some(editor) = self.rename_editor.as_mut() {
            if editor.replace_on_type {
                editor.value.clear();
            }
            let remaining = 80_usize.saturating_sub(editor.value.chars().count());
            editor
                .value
                .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
            editor.replace_on_type = false;
            cx.notify();
            return;
        }
        if let Some(editor) = self.search_editor.as_mut() {
            let remaining = 256_usize.saturating_sub(editor.query.chars().count());
            editor
                .query
                .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
            editor.no_match = false;
            self.run_search(true, cx);
            return;
        }
        if let Some(pane_id) = self.focused_pane {
            self.send_control(ClientRequest::WriteInput {
                pane_id,
                bytes: text.as_bytes().to_vec(),
            });
            cx.notify();
        }
    }

    fn begin_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane_with_snapshot(pane_id);
        self.focus_handle.focus(window);
        let mouse_reporting = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING));
        if mouse_reporting && !event.modifiers.shift {
            if let Some(button) = terminal_mouse_button(event.button) {
                self.send_control(ClientRequest::MouseInput {
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
            self.selection_drag = Some(SelectionDrag {
                pane_id,
                anchor: point,
                preserve_single_cell: matches!(
                    kind,
                    TerminalSelectionKind::Semantic | TerminalSelectionKind::Lines
                ),
            });
            self.send_control(ClientRequest::BeginSelection {
                pane_id,
                point,
                kind,
            });
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn move_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if self
            .selection_drag
            .is_some_and(|selection| selection.pane_id == pane_id)
            && event.dragging()
        {
            self.send_control(ClientRequest::UpdateSelection { pane_id, point });
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let mouse_motion = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_MOTION));
        if mouse_motion && let Some(button) = event.pressed_button.and_then(terminal_mouse_button) {
            self.send_control(ClientRequest::MouseInput {
                pane_id,
                point,
                button,
                action: TerminalMouseAction::Move,
                modifiers: terminal_modifiers(event.modifiers),
            });
            cx.stop_propagation();
        }
    }

    fn end_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseUpEvent,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = self
            .selection_drag
            .take()
            .filter(|selection| selection.pane_id == pane_id)
        {
            if point == selection.anchor && !selection.preserve_single_cell {
                self.send_control(ClientRequest::ClearSelection { pane_id });
            } else {
                self.send_control(ClientRequest::UpdateSelection { pane_id, point });
            }
        } else if self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
            && let Some(button) = terminal_mouse_button(event.button)
        {
            self.send_control(ClientRequest::MouseInput {
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

    fn scroll_terminal(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let pixels = event
            .delta
            .pixel_delta(px(self.terminal_font.metrics.line_height));
        let lines = (f32::from(pixels.y) / self.terminal_font.metrics.line_height).round() as i32;
        let lines = if lines == 0 {
            if pixels.y < px(0.0) { -1 } else { 1 }
        } else {
            lines
        };
        if self.archived_views.contains_key(&pane_id) {
            self.scroll_archived_view(pane_id, lines, cx);
            cx.stop_propagation();
            return;
        }
        if self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
        {
            self.send_control(ClientRequest::MouseInput {
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
            && self.screens.get(&pane_id).is_some_and(|screen| {
                screen.display_offset >= screen.history_size && screen.history_size > 0
            })
        {
            self.load_archived_page(pane_id, None, HistoryPageDirection::Older, cx);
        } else {
            self.send_control(ClientRequest::ScrollPane { pane_id, lines });
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn load_archived_page(
        &mut self,
        pane_id: Uuid,
        cursor: Option<nah_protocol::HistoryCursor>,
        direction: HistoryPageDirection,
        cx: &mut Context<Self>,
    ) {
        match request(ClientRequest::LoadHistoryPage {
            pane_id,
            cursor,
            direction,
        }) {
            Ok(ServiceResponse::HistoryPage { page: Some(page) }) => {
                let rows = self
                    .screens
                    .get(&pane_id)
                    .map_or(30, |screen| usize::from(screen.rows));
                let first_line = match direction {
                    HistoryPageDirection::Older => page.lines.len().saturating_sub(rows),
                    HistoryPageDirection::Newer => 0,
                };
                self.archived_views.clear();
                self.archived_views
                    .insert(pane_id, ArchivedView { page, first_line });
                self.connection_error = None;
            }
            Ok(ServiceResponse::HistoryPage { page: None }) => {
                if direction == HistoryPageDirection::Newer {
                    self.archived_views.remove(&pane_id);
                }
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"));
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn scroll_archived_view(&mut self, pane_id: Uuid, lines: i32, cx: &mut Context<Self>) {
        let rows = self
            .screens
            .get(&pane_id)
            .map_or(30, |screen| usize::from(screen.rows));
        let Some(view) = self.archived_views.get_mut(&pane_id) else {
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
            self.archived_views.remove(&pane_id);
            cx.notify();
        }
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.sidebar_resize.is_active() {
            self.cancel_sidebar_resize(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.command_palette.is_some() {
            self.handle_palette_key(event, cx);
            return;
        }
        let keystroke = &event.keystroke;
        if let Some(picker) = self.color_picker.as_mut() {
            match keystroke.key.as_str() {
                "enter" => self.submit_color_picker(cx),
                "escape" => {
                    self.color_picker = None;
                    cx.notify();
                }
                "backspace" => {
                    if picker.replace_on_type {
                        picker.hex.clear();
                    } else {
                        picker.hex.pop();
                    }
                    picker.replace_on_type = false;
                    picker.invalid = false;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if let Some(editor) = self.history_editor.as_mut() {
            match keystroke.key.as_str() {
                "enter" => self.submit_history_edit(cx),
                "escape" => {
                    self.history_editor = None;
                    cx.notify();
                }
                "backspace" => {
                    if editor.replace_on_type {
                        editor.text.clear();
                    } else {
                        editor.text.pop();
                    }
                    editor.replace_on_type = false;
                    editor.invalid = false;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if self.appearance_settings_open {
            if keystroke.key == "escape" {
                self.appearance_settings_open = false;
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        if self.workspace_creation.is_some() {
            let step = self.workspace_creation.as_ref().map(|dialog| dialog.step);
            if step == Some(WorkspaceCreationStep::Details)
                && keystroke.modifiers.platform
                && keystroke.key.eq_ignore_ascii_case("a")
            {
                if let Some(dialog) = self.workspace_creation.as_mut() {
                    dialog.active_editor_mut().select_all();
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            if step == Some(WorkspaceCreationStep::Details)
                && keystroke.modifiers.platform
                && keystroke.key.eq_ignore_ascii_case("x")
            {
                if let Some(dialog) = self.workspace_creation.as_mut()
                    && let Some(text) = dialog.active_editor().selected_text().map(str::to_owned)
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    dialog.replace_text(None, "", false, None);
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            match keystroke.key.as_str() {
                "enter" => self.submit_workspace_creation(cx),
                "escape" => {
                    self.workspace_creation = None;
                    cx.notify();
                }
                "tab" if step == Some(WorkspaceCreationStep::Details) => {
                    if let Some(dialog) = self.workspace_creation.as_mut() {
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
                    if let Some(dialog) = self.workspace_creation.as_mut() {
                        dialog.backspace();
                        cx.notify();
                    }
                }
                "delete" if step == Some(WorkspaceCreationStep::Details) => {
                    if let Some(dialog) = self.workspace_creation.as_mut() {
                        dialog.delete();
                        cx.notify();
                    }
                }
                "left" if step == Some(WorkspaceCreationStep::Details) => {
                    if let Some(dialog) = self.workspace_creation.as_mut() {
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
                    if let Some(dialog) = self.workspace_creation.as_mut() {
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
                    if let Some(dialog) = self.workspace_creation.as_mut() {
                        dialog
                            .active_editor_mut()
                            .move_home(keystroke.modifiers.shift);
                        cx.notify();
                    }
                }
                "end" if step == Some(WorkspaceCreationStep::Details) => {
                    if let Some(dialog) = self.workspace_creation.as_mut() {
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
                    // Most text arrives through EntityInputHandler, including
                    // IME composition. Some native key paths, however, only
                    // provide `key_char`; accept that ordinary text here so a
                    // modal never looks focused while silently rejecting the
                    // user's keyboard. This remains modal and cannot reach a
                    // terminal behind the dialog.
                    if let Some(text) = &keystroke.key_char
                        && !text.chars().any(char::is_control)
                        && let Some(dialog) = self.workspace_creation.as_mut()
                    {
                        dialog.replace_text(None, text, false, None);
                        cx.notify();
                    }
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if let Some(editor) = self.workspace_rename_editor.as_mut() {
            if keystroke.modifiers.platform && keystroke.key.eq_ignore_ascii_case("a") {
                editor.replace_on_type = true;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            match keystroke.key.as_str() {
                "enter" => self.submit_workspace_rename(cx),
                "escape" => {
                    self.workspace_rename_editor = None;
                    cx.notify();
                }
                "backspace" => {
                    if editor.replace_on_type {
                        editor.value.clear();
                    } else {
                        editor.value.pop();
                    }
                    editor.replace_on_type = false;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if let Some(editor) = self.rename_editor.as_mut() {
            if keystroke.modifiers.platform && keystroke.key.eq_ignore_ascii_case("a") {
                editor.replace_on_type = true;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            match keystroke.key.as_str() {
                "enter" => self.submit_rename(cx),
                "escape" => {
                    self.rename_editor = None;
                    cx.notify();
                }
                "backspace" => {
                    if editor.replace_on_type {
                        editor.value.clear();
                    } else {
                        editor.value.pop();
                    }
                    editor.replace_on_type = false;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if let Some(editor) = self.search_editor.as_mut() {
            match keystroke.key.as_str() {
                "enter" => self.run_search(!keystroke.modifiers.shift, cx),
                "escape" => {
                    self.search_editor = None;
                    self.ime_preedit.clear();
                    cx.notify();
                }
                "backspace" => {
                    editor.query.pop();
                    editor.no_match = false;
                    if editor.query.is_empty() {
                        if let Some(pane_id) = self.focused_pane {
                            self.send_control(ClientRequest::ClearSelection { pane_id });
                        }
                    } else {
                        self.run_search(true, cx);
                    }
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if self.workspace_delete_confirmation.is_some() {
            match keystroke.key.as_str() {
                "enter" => self.confirm_workspace_delete(cx),
                "escape" => {
                    self.workspace_delete_confirmation = None;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if self.close_confirmation.is_some() {
            match keystroke.key.as_str() {
                "enter" => self.confirm_close(cx),
                "escape" => {
                    self.close_confirmation = None;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if self.tab_menu.is_some() && keystroke.key == "escape" {
            self.tab_menu = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.workspace_menu.is_some() && keystroke.key == "escape" {
            self.workspace_menu = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.dragging_pane.is_some() && keystroke.key == "escape" {
            self.dragging_pane = None;
            self.drag_hover.clear();
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
        if let (Some(pane_id), Some(bytes)) = (self.focused_pane, bytes) {
            self.send_control(ClientRequest::WriteInput { pane_id, bytes });
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn handle_resize(&mut self, event: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        match self.sidebar_resize.pointer_move(event.pressed_button) {
            SidebarResizeMove::Ignore => {}
            SidebarResizeMove::Update => {
                let window_width = f32::from(window.bounds().size.width);
                let next = constrained_sidebar_width(f32::from(event.position.x), window_width);
                if (self.preferred_sidebar_width - next).abs() > f32::EPSILON {
                    self.preferred_sidebar_width = next;
                    self.update_window_geometry(window);
                    self.last_sizes.clear();
                    self.sync_pty_sizes();
                    cx.notify();
                }
                return;
            }
            SidebarResizeMove::Complete => {
                self.persist_sidebar_width();
                cx.notify();
                return;
            }
        }
        let Some(drag) = self.resizing else { return };
        self.update_window_geometry(window);
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self
            .active_workspace_in(snapshot)
            .and_then(|workspace| workspace.tabs.first())
            .map(|tab| &tab.layout)
        else {
            return;
        };
        let root = PixelRect {
            x: 0.0,
            y: 0.0,
            width: self.workspace_pixels.0,
            height: self.workspace_pixels.1,
        };
        let Some(split) = find_split_rect(layout, drag.split_id, root, &self.split_ratios) else {
            return;
        };
        let workspace_x = f32::from(event.position.x) - self.sidebar_pixels;
        let workspace_y = f32::from(event.position.y) - TITLEBAR_HEIGHT;
        let ratio = match drag.axis {
            SplitAxis::Horizontal => (workspace_x - split.x) / split.width.max(1.0),
            SplitAxis::Vertical => (workspace_y - split.y) / split.height.max(1.0),
        };
        self.split_ratios.insert(
            drag.split_id,
            effective_split_ratio(drag.axis, split.width, split.height, ratio),
        );
        self.last_sizes.clear();
        self.sync_pty_sizes();
        cx.notify();
    }

    fn update_window_geometry(&mut self, window: &Window) -> bool {
        let window_width = f32::from(window.bounds().size.width);
        let sidebar_pixels = sidebar_width_for_visibility(
            self.preferred_sidebar_width,
            window_width,
            self.sidebar_visible,
        );
        let next = workspace_pixel_size(
            window_width,
            f32::from(window.bounds().size.height),
            sidebar_pixels,
        );
        if self.workspace_pixels == next
            && (self.sidebar_pixels - sidebar_pixels).abs() < f32::EPSILON
        {
            return false;
        }
        self.sidebar_pixels = sidebar_pixels;
        self.workspace_pixels = next;
        self.last_sizes.clear();
        true
    }

    fn persist_sidebar_width(&self) {
        if let Some(store) = &self.ui_state_store
            && let Err(error) = store.save_workspace_sidebar_width(self.preferred_sidebar_width)
        {
            eprintln!("Not a Harness sidebar width was not persisted: {error:#}");
        }
    }

    fn cancel_sidebar_resize(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(initial_width) = self.sidebar_resize.cancel() else {
            return;
        };
        self.preferred_sidebar_width = initial_width;
        self.update_window_geometry(window);
        self.last_sizes.clear();
        self.sync_pty_sizes();
        cx.notify();
    }

    fn finish_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_resize.finish() {
            self.persist_sidebar_width();
        }
        self.resizing = None;
        self.dragging_pane = None;
        self.drag_hover.clear();
        cx.notify();
    }

    fn sync_pty_sizes(&mut self) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let Some(workspace) = self.active_workspace_in(&snapshot) else {
            return;
        };
        let Some(tab) = workspace.tabs.first() else {
            return;
        };
        let mut sizes = Vec::new();
        let projected = self
            .zoomed_pane
            .and_then(|pane_id| zoom_projection(&tab.layout, pane_id));
        collect_pane_sizes(
            projected.as_ref().unwrap_or(&tab.layout),
            self.workspace_pixels.0,
            self.workspace_pixels.1,
            self.terminal_font.metrics,
            &self.split_ratios,
            &mut sizes,
        );
        for (pane_id, columns, rows) in sizes {
            if self.last_sizes.get(&pane_id) == Some(&(columns, rows)) {
                continue;
            }
            match request(ClientRequest::ResizePane {
                pane_id,
                columns,
                rows,
            }) {
                Ok(ServiceResponse::Ack) => {
                    self.last_sizes.insert(pane_id, (columns, rows));
                }
                Ok(response) => {
                    self.connection_error = Some(format!(
                        "unexpected resize response for {pane_id}: {response:?}"
                    ));
                }
                Err(error) => self.connection_error = Some(format!("{error:#}")),
            }
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut workspaces = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.workspaces.clone())
            .unwrap_or_default();
        workspaces.sort_by_key(|workspace| {
            (
                !workspace.pinned,
                if workspace.pinned {
                    workspace.pin_order
                } else {
                    u32::MAX
                },
            )
        });
        let history_needs_attention = self
            .history_status
            .as_ref()
            .is_some_and(|status| status.warning.is_some());
        div()
            .w(px(self.sidebar_pixels - SIDEBAR_RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex_none()
            .bg(rgb(THEME.sidebar))
            .border_r_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(TITLEBAR_HEIGHT))
                    .flex_none()
                    .pl(px(79.0))
                    .pr(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child(
                        div()
                            .id("hide-workspace-sidebar")
                            .w(px(20.0))
                            .h(px(20.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|element| {
                                element
                                    .bg(rgb(THEME.elevated))
                                    .text_color(rgb(THEME.foreground))
                            })
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Hide workspace sidebar (⌘B)".to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
                            .child(render_sidebar_toggle_icon(true)),
                    )
                    .child(
                        div()
                            .id("appearance-settings")
                            .relative()
                            .cursor_pointer()
                            .hover(|element| element.text_color(rgb(THEME.foreground)))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Settings".to_owned(),
                                })
                                .into()
                            })
                            .on_click(
                                cx.listener(|this, _, _, cx| this.open_appearance_settings(cx)),
                            )
                            .child("⚙")
                            .when(history_needs_attention, |element| {
                                element.child(
                                    div()
                                        .absolute()
                                        .top(px(-2.0))
                                        .right(px(-4.0))
                                        .w(px(5.0))
                                        .h(px(5.0))
                                        .rounded_full()
                                        .bg(rgb(THEME.danger)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("new-workspace")
                            .cursor_pointer()
                            .hover(|element| element.text_color(rgb(THEME.foreground)))
                            .on_click(cx.listener(|this, _, _, cx| this.new_workspace(cx)))
                            .child("＋"),
                    ),
            )
            .child(
                div()
                    .px(px(10.0))
                    .pt(px(10.0))
                    .pb(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Workspaces"),
                    ),
            )
            .children(
                workspaces
                    .into_iter()
                    .enumerate()
                    .map(|(index, workspace)| {
                        let active = Some(workspace.id) == self.active_workspace;
                        let workspace_id = workspace.id;
                        let offline = matches!(
                            workspace.connection,
                            WorkspaceConnection::SystemSsh {
                                status: WorkspaceConnectionStatus::Offline,
                                ..
                            }
                        );
                        let connected = matches!(
                            workspace.connection,
                            WorkspaceConnection::SystemSsh {
                                status: WorkspaceConnectionStatus::Connected,
                                ..
                            }
                        );
                        let pinned = workspace.pinned;
                        let workspace_title = workspace.title.clone();
                        let terminal_tabs = workspace_terminal_tabs(&workspace)
                            .into_iter()
                            .cloned()
                            .collect::<Vec<_>>();
                        let first_pane = workspace
                            .tabs
                            .first()
                            .and_then(|tab| visible_panes(&tab.layout).first().copied());
                        let terminal_count = terminal_tabs.len();
                        let expanded = self.expanded_workspaces.contains(&workspace_id);
                        let workspace_color = self.workspace_color(workspace_id).as_rgb();
                        let card_color = if connected {
                            0x234b38
                        } else if offline {
                            0x40252a
                        } else {
                            workspace_color
                        };
                        let active_text = readable_text_color(card_color);
                        let connection_status = match &workspace.connection {
                            WorkspaceConnection::Local => None,
                            WorkspaceConnection::SystemSsh {
                                destination,
                                status: WorkspaceConnectionStatus::Connected,
                            } => Some(format!("Connected · {destination}")),
                            WorkspaceConnection::SystemSsh {
                                destination,
                                status: WorkspaceConnectionStatus::Offline,
                            } => Some(format!("Offline · {destination}")),
                        };
                        div()
                            .id(("workspace-section", element_key(workspace.id)))
                            .mx(px(7.0))
                            .mb(px(3.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .id(("workspace", element_key(workspace.id)))
                                    .h(px(31.0))
                                    .px(px(8.0))
                                    .rounded(px(6.0))
                                    .when(!offline || terminal_count == 0, |element| {
                                        element.cursor_pointer()
                                    })
                                    .when(offline, |element| element.bg(rgb(card_color)))
                                    .when(active || connected, |element| {
                                        element.bg(rgb(card_color))
                                    })
                                    .hover(|element| {
                                        if active || connected || offline {
                                            element
                                        } else {
                                            element.bg(rgb(THEME.surface))
                                        }
                                    })
                                    .when(!offline || terminal_count == 0, |element| {
                                        element.on_click(cx.listener(move |this, _, _, cx| {
                                            this.active_workspace = Some(workspace_id);
                                            if let Some(pane_id) = first_pane {
                                                this.focus_pane_with_snapshot(pane_id);
                                            }
                                            this.last_sizes.clear();
                                            cx.notify();
                                        }))
                                    })
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                            this.open_workspace_menu(
                                                workspace_id,
                                                event.position,
                                                cx,
                                            );
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .child(
                                        div()
                                            .id((
                                                "toggle-workspace-tabs",
                                                element_key(workspace_id),
                                            ))
                                            .flex_none()
                                            .w(px(14.0))
                                            .h(px(18.0))
                                            .cursor_pointer()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .font_family(".SystemUIFont")
                                            .text_sm()
                                            .text_color(if active || connected || offline {
                                                rgb(active_text)
                                            } else {
                                                rgb(THEME.muted)
                                            })
                                            .tooltip(move |_, cx| {
                                                cx.new(|_| TooltipView {
                                                    text: if expanded {
                                                        "Collapse terminal tabs".to_owned()
                                                    } else {
                                                        "Expand terminal tabs".to_owned()
                                                    },
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.toggle_workspace_expanded(workspace_id, cx);
                                                cx.stop_propagation();
                                            }))
                                            .child(if expanded { "⌄" } else { "›" }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(if active || connected || offline {
                                                rgb(active_text)
                                            } else {
                                                rgb(THEME.foreground)
                                            })
                                            .child(format!("{}  {workspace_title}", index + 1)),
                                    )
                                    .child(
                                        div()
                                            .id(("workspace-tab-count", element_key(workspace_id)))
                                            .flex_none()
                                            .min_w(px(18.0))
                                            .h(px(17.0))
                                            .px(px(5.0))
                                            .rounded_full()
                                            .bg(rgba(if active || connected || offline {
                                                0xffffff20
                                            } else {
                                                0xffffff0c
                                            }))
                                            .font_family("SF Mono")
                                            .text_size(px(9.5))
                                            .text_color(if active || connected || offline {
                                                rgb(active_text)
                                            } else {
                                                rgb(THEME.muted)
                                            })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .tooltip(move |_, cx| {
                                                cx.new(|_| TooltipView {
                                                    text: terminal_tab_count_label(terminal_count),
                                                })
                                                .into()
                                            })
                                            .child(terminal_count.to_string()),
                                    )
                                    .child(
                                        div()
                                            .id(("pin-workspace", element_key(workspace_id)))
                                            .cursor_pointer()
                                            .font_family("SF Mono")
                                            .text_xs()
                                            .text_color(rgb(if pinned {
                                                THEME.ansi[3]
                                            } else {
                                                THEME.muted
                                            }))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.set_workspace_pinned(
                                                    workspace_id,
                                                    !pinned,
                                                    cx,
                                                );
                                                cx.stop_propagation();
                                            }))
                                            .child(if pinned { "◆" } else { "◇" }),
                                    )
                                    .child(
                                        div()
                                            .id(("delete-workspace", element_key(workspace_id)))
                                            .cursor_pointer()
                                            .font_family("SF Mono")
                                            .text_xs()
                                            .text_color(rgb(THEME.danger))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.begin_workspace_delete(workspace_id, cx);
                                                cx.stop_propagation();
                                            }))
                                            .child("×"),
                                    ),
                            )
                            .when_some(connection_status, |element, status| {
                                element.child(
                                    div()
                                        .ml(px(21.0))
                                        .mr(px(4.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_sm()
                                                .font_family("SF Mono")
                                                .text_xs()
                                                .text_color(rgb(THEME.dim))
                                                .child(status),
                                        )
                                        .when(connected, |element| {
                                            element.child(
                                                div()
                                                    .id((
                                                        "disconnect-workspace",
                                                        element_key(workspace_id),
                                                    ))
                                                    .cursor_pointer()
                                                    .text_xs()
                                                    .text_color(rgb(THEME.foreground))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.disconnect_workspace(workspace_id, cx);
                                                        cx.stop_propagation();
                                                    }))
                                                    .child("Disconnect"),
                                            )
                                        })
                                        .when(offline, |element| {
                                            element.child(
                                                div()
                                                    .id((
                                                        "reconnect-workspace",
                                                        element_key(workspace_id),
                                                    ))
                                                    .cursor_pointer()
                                                    .text_xs()
                                                    .text_color(rgb(THEME.ansi[2]))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.reconnect_workspace(workspace_id, cx);
                                                        cx.stop_propagation();
                                                    }))
                                                    .child("Reconnect"),
                                            )
                                        }),
                                )
                            })
                            .when(expanded, |element| {
                                if terminal_tabs.is_empty() {
                                    element.child(
                                        div()
                                            .ml(px(28.0))
                                            .mr(px(4.0))
                                            .py(px(5.0))
                                            .font_family(".SystemUIFont")
                                            .text_xs()
                                            .text_color(rgb(THEME.dim))
                                            .child("No open terminal tabs"),
                                    )
                                } else {
                                    element.children(terminal_tabs.into_iter().map(|pane| {
                                        let pane_id = pane.id;
                                        let selected = self.focused_pane == Some(pane_id);
                                        let identity = tab_identity_presentation(&pane);
                                        let identity_detail = identity.detail.clone();
                                        let pane_accent = self.terminal_accent(pane_id).as_rgb();
                                        div()
                                            .id(("workspace-tab", element_key(pane_id)))
                                            .ml(px(20.0))
                                            .mr(px(4.0))
                                            .px(px(7.0))
                                            .h(px(27.0))
                                            .rounded(px(4.0))
                                            .cursor_pointer()
                                            .flex()
                                            .items_center()
                                            .gap(px(7.0))
                                            .when(selected, |element| {
                                                element.bg(rgb(THEME.surface))
                                            })
                                            .hover(|element| element.bg(rgb(THEME.elevated)))
                                            .tooltip(move |_, cx| {
                                                cx.new(|_| TooltipView {
                                                    text: identity_detail.clone(),
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.select_workspace_tab(
                                                    workspace_id,
                                                    pane_id,
                                                    cx,
                                                );
                                                cx.stop_propagation();
                                            }))
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .w(px(18.0))
                                                    .h(px(18.0))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(render_terminal_profile_mark(
                                                        identity.profile,
                                                        if selected {
                                                            THEME.foreground
                                                        } else {
                                                            THEME.muted
                                                        },
                                                        if selected {
                                                            pane_accent
                                                        } else {
                                                            THEME.muted
                                                        },
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .min_w(px(0.0))
                                                    .flex_1()
                                                    .truncate()
                                                    .font_family(".SystemUIFont")
                                                    .text_xs()
                                                    .font_weight(if selected {
                                                        gpui::FontWeight::MEDIUM
                                                    } else {
                                                        gpui::FontWeight::NORMAL
                                                    })
                                                    .text_color(if selected {
                                                        rgb(THEME.foreground)
                                                    } else {
                                                        rgb(THEME.muted)
                                                    })
                                                    .child(identity.label),
                                            )
                                    }))
                                }
                            })
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("new-workspace-bottom")
                    .mx(px(9.0))
                    .mb(px(8.0))
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.surface))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.border_color(rgb(THEME.accent)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_click(cx.listener(|this, _, _, cx| this.new_workspace(cx)))
                    .child("＋ New Workspace"),
            )
            .child(
                div()
                    .px(px(11.0))
                    .pb(px(9.0))
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("⌘N new workspace"),
            )
            .into_any_element()
    }

    fn render_sidebar_resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("workspace-sidebar-resize-handle")
            .relative()
            .w(px(SIDEBAR_RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex_none()
            .cursor(CursorStyle::ResizeLeftRight)
            .flex()
            .justify_center()
            .bg(rgba(0x00000000))
            .hover(|element| element.bg(rgba(0xffffff10)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.resizing = None;
                    this.sidebar_resize.begin(this.preferred_sidebar_width);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(2.0))
                    .h(px(38.0))
                    .rounded_full()
                    .bg(rgb(THEME.border_strong)),
            )
            .into_any_element()
    }

    fn render_pane_header(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let merge_preview = self.drag_hover.merges_into(active);
        let active_accent = self.terminal_accent(active).as_rgb();
        div()
            .id(("pane-tab-strip", element_key(active)))
            .h(px(PANE_HEADER_HEIGHT))
            .flex_none()
            .bg(rgb(THEME.surface))
            .border_b(if merge_preview { px(2.0) } else { px(1.0) })
            .border_color(if merge_preview {
                rgb(active_accent)
            } else {
                rgb(THEME.border)
            })
            .when(merge_preview, |element| {
                element.bg(rgba((active_accent << 8) | 0x18))
            })
            .flex()
            .items_center()
            .on_drag_move::<PaneDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        this.dragging_pane = Some(event.drag(cx).pane_id);
                        this.drag_hover.enter(DragDestination::Merge {
                            target_pane: active,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(move |this, info: &PaneDrag, _, cx| {
                this.move_pane_to_tab(info.pane_id, active, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .min_w(px(0.0))
                    .h_full()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .children(panes.into_iter().map(|pane| {
                        let pane_id = pane.id;
                        let identity = tab_identity_presentation(&pane);
                        let identity_detail = identity.detail.clone();
                        let selected = pane_id == active;
                        let pane_accent = pane
                            .color
                            .unwrap_or_else(|| self.terminal_accent(pane_id))
                            .as_rgb();
                        let close_tooltip = format!("Close {}…", identity.label);
                        let drag = PaneDrag {
                            pane_id,
                            title: identity.label.clone(),
                            position: Point::default(),
                        };
                        div()
                            .id(("pane-tab", element_key(pane_id)))
                            .h_full()
                            .min_w(px(54.0))
                            .max_w(px(220.0))
                            .flex_shrink()
                            .overflow_hidden()
                            .pl(px(8.0))
                            .pr(px(4.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .border_t(if selected { px(2.0) } else { px(0.0) })
                            .border_r_1()
                            .border_color(if selected {
                                rgb(pane_accent)
                            } else {
                                rgb(THEME.border)
                            })
                            .when(selected, |element| element.bg(rgb(THEME.selection)))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.activate_tab(pane_id, cx)),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.open_tab_menu(pane_id, event.position, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .on_drag(drag, |info: &PaneDrag, position, _, cx| {
                                cx.new(|_| PaneDrag {
                                    position,
                                    ..info.clone()
                                })
                            })
                            .child(
                                div()
                                    .id(("identity-badge", element_key(pane_id)))
                                    .flex_none()
                                    .w(px(IDENTITY_MARK_SIZE))
                                    .h(px(IDENTITY_MARK_SIZE))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| TooltipView {
                                            text: identity_detail.clone(),
                                        })
                                        .into()
                                    })
                                    .child(render_terminal_profile_mark(
                                        identity.profile,
                                        if selected {
                                            THEME.foreground
                                        } else {
                                            THEME.muted
                                        },
                                        if selected { pane_accent } else { THEME.muted },
                                    )),
                            )
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex_1()
                                    .truncate()
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .font_weight(if selected {
                                        gpui::FontWeight::MEDIUM
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(if selected {
                                        rgb(THEME.foreground)
                                    } else {
                                        rgb(THEME.muted)
                                    })
                                    .child(identity.label),
                            )
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex_shrink()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .font_family("SF Mono")
                                    .text_size(px(9.5))
                                    .text_color(rgb(THEME.dim))
                                    .child(pane.shell),
                            )
                            .child(
                                div()
                                    .id(("close-tab", element_key(pane_id)))
                                    .ml(px(1.0))
                                    .flex_none()
                                    .w(px(18.0))
                                    .h(px(18.0))
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .font_family(".SystemUIFont")
                                    .text_sm()
                                    .line_height(px(14.0))
                                    .text_color(rgb(THEME.dim))
                                    .hover(|element| {
                                        element
                                            .bg(rgb(THEME.elevated))
                                            .text_color(rgb(THEME.foreground))
                                    })
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| TooltipView {
                                            text: close_tooltip.clone(),
                                        })
                                        .into()
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_close(pane_id, cx);
                                        cx.stop_propagation();
                                    }))
                                    .child("×"),
                            )
                    })),
            )
            .child(self.pane_control(
                active,
                "new-tab",
                PaneControlIcon::Add,
                "New terminal tab (⌘T)",
                cx,
                NahApp::new_tab_at,
            ))
            .child(self.pane_control(
                active,
                "split-right",
                PaneControlIcon::SplitRight,
                "Split right (⌘D)",
                cx,
                |this, pane_id, cx| this.split_at(pane_id, SplitAxis::Horizontal, cx),
            ))
            .child(self.pane_control(
                active,
                "split-down",
                PaneControlIcon::SplitDown,
                "Split down (⇧⌘D)",
                cx,
                |this, pane_id, cx| this.split_at(pane_id, SplitAxis::Vertical, cx),
            ))
            .into_any_element()
    }

    fn pane_control(
        &self,
        pane_id: Uuid,
        id: &'static str,
        icon: PaneControlIcon,
        tooltip: &'static str,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, Uuid, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id((id, element_key(pane_id)))
            .flex_none()
            .w(px(27.0))
            .h_full()
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(THEME.muted))
            .hover(|element| {
                element
                    .bg(rgb(THEME.elevated))
                    .text_color(rgb(THEME.foreground))
            })
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: tooltip.to_owned(),
                })
                .into()
            })
            .on_click(cx.listener(move |this, _, _, cx| handler(this, pane_id, cx)))
            .child(self.render_control_icon(icon))
            .into_any_element()
    }

    fn render_control_icon(&self, icon: PaneControlIcon) -> AnyElement {
        match icon {
            PaneControlIcon::Add => div()
                .font_family(".SystemUIFont")
                .text_base()
                .line_height(px(14.0))
                .child("+")
                .into_any_element(),
            PaneControlIcon::SplitRight | PaneControlIcon::SplitDown => {
                let vertical = matches!(icon, PaneControlIcon::SplitRight);
                div()
                    .relative()
                    .w(px(14.0))
                    .h(px(11.0))
                    .rounded(px(2.0))
                    .border_1()
                    .border_color(rgb(THEME.muted))
                    .child(
                        div()
                            .absolute()
                            .when(vertical, |element| {
                                element.left(px(6.0)).top(px(0.0)).w(px(1.0)).h_full()
                            })
                            .when(!vertical, |element| {
                                element.left(px(0.0)).top(px(4.5)).w_full().h(px(1.0))
                            })
                            .bg(rgb(THEME.muted)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_terminal(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.focused_pane == Some(active);
        let terminal_accent = self.terminal_accent(active).as_rgb();
        let screen = self.screens.get(&active).cloned();
        let archived = self.archived_views.get(&active);
        let drop_target = self
            .dragging_pane
            .and_then(|source| split_target_for_drag(source, &panes, active));
        let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        let rendered_lines = if let (Some(view), Some(screen)) = (archived, screen.as_ref()) {
            view.page
                .lines
                .iter()
                .skip(view.first_line)
                .take(usize::from(screen.rows))
                .map(|line| plain_history_line(line))
                .enumerate()
                .map(|(row, line)| {
                    self.render_terminal_line(
                        line,
                        TerminalLineRender {
                            row,
                            cursor: None,
                            focused,
                            pane_id: active,
                            columns: screen.columns,
                            selection: None,
                        },
                        cx,
                    )
                })
                .collect::<Vec<_>>()
        } else {
            screen
                .as_ref()
                .map(|screen| {
                    screen
                        .lines
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(row, line)| {
                            self.render_terminal_line(
                                line,
                                TerminalLineRender {
                                    row,
                                    cursor: screen.cursor,
                                    focused,
                                    pane_id: active,
                                    columns: screen.columns,
                                    selection: screen.selection,
                                },
                                cx,
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        div()
            .id(("terminal", element_key(active)))
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus_pane_with_snapshot(active);
                this.focus_handle.focus(window);
                cx.notify();
            }))
            .on_drop(cx.listener(move |this, info: &PaneDrag, _, cx| {
                this.swap_panes(info.pane_id, active, cx);
            }))
            .on_drag_move::<PaneDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let source = event.drag(cx).pane_id;
                    this.dragging_pane = Some(source);
                    if let Some(target_pane) = split_target_for_drag_ids(source, &pane_ids, active)
                        && let Some(placement) =
                            split_placement_at(event.event.position, event.bounds)
                    {
                        this.drag_hover.enter(DragDestination::Split {
                            target_pane,
                            placement,
                        });
                    }
                    cx.notify();
                },
            ))
            .child(self.render_pane_header(panes, active, cx))
            .child(
                div()
                    .relative()
                    .min_h(px(0.0))
                    .flex_1()
                    .px(px(9.0))
                    .py(px(6.0))
                    .border_l_1()
                    .border_color(if focused {
                        rgb(terminal_accent)
                    } else {
                        rgb(THEME.terminal)
                    })
                    .font(self.terminal_font.font(false, false))
                    .text_size(px(self.terminal_font.metrics.font_size))
                    .line_height(px(self.terminal_font.metrics.line_height))
                    .text_color(rgb(THEME.foreground))
                    .children(rendered_lines)
                    .when_some(archived, |element, view| {
                        let notice = if view.page.flags.contains(HistoryPageFlags::CORRUPT) {
                            "LOCAL HISTORY · CORRUPT CHUNK · gap preserved"
                        } else if view.page.flags.contains(HistoryPageFlags::GAP_BEFORE)
                            || view.page.flags.contains(HistoryPageFlags::GAP_AFTER)
                        {
                            "LOCAL HISTORY · archive gap · live terminal unaffected"
                        } else {
                            "LOCAL HISTORY · disk-backed page · scroll down for live"
                        };
                        element.child(
                            div()
                                .absolute()
                                .top(px(3.0))
                                .right(px(8.0))
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(4.0))
                                .bg(rgb(THEME.elevated))
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(THEME.muted))
                                .child(notice),
                        )
                    })
                    .when(
                        focused
                            && self.search_editor.is_none()
                            && self.rename_editor.is_none()
                            && !self.ime_preedit.is_empty(),
                        |element| {
                            let cursor = screen.as_ref().and_then(|screen| screen.cursor);
                            element.when_some(cursor, |element, cursor| {
                                let span = self.terminal_font.metrics.span(cursor.column, 1);
                                element.child(
                                    div()
                                        .absolute()
                                        .left(px(span.x))
                                        .top(px(f32::from(cursor.row)
                                            * self.terminal_font.metrics.line_height))
                                        .font(self.terminal_font.font(false, false))
                                        .text_size(px(self.terminal_font.metrics.font_size))
                                        .text_color(rgb(THEME.foreground))
                                        .border_b_1()
                                        .border_color(rgb(terminal_accent))
                                        .child(self.ime_preedit.clone()),
                                )
                            })
                        },
                    )
                    .when_some(
                        self.search_editor.as_ref().filter(|_| focused),
                        |element, editor| element.child(self.render_search_bar(editor)),
                    )
                    .when_some(drop_target, |element, target| {
                        element.child(self.render_drop_layer(target, cx))
                    }),
            )
            .into_any_element()
    }

    fn render_terminal_line(
        &self,
        line: TerminalLine,
        render: TerminalLineRender,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let TerminalLineRender {
            row,
            cursor,
            focused,
            pane_id,
            columns,
            selection,
        } = render;
        let mut start_column = 0_u16;
        let styled_runs = line
            .runs
            .into_iter()
            .map(|mut style| {
                let columns = terminal_run_columns(&style, start_column);
                if style.text.contains('\t') {
                    style.text = terminal_run_display_text(&style, start_column);
                }
                let element = self.render_terminal_run(style, start_column, columns);
                start_column = start_column.saturating_add(columns);
                element
            })
            .collect::<Vec<_>>();
        let cursor_column = cursor
            .filter(|cursor| usize::from(cursor.row) == row)
            .map(|cursor| cursor.column);
        let metrics = self.terminal_font.metrics;
        let pane_accent = self.terminal_accent(pane_id).as_rgb();
        div()
            .relative()
            .h(px(metrics.line_height))
            .flex_none()
            .overflow_hidden()
            .when_some(
                selection.and_then(|selection| selection_span(selection, row, columns)),
                |element, (start, width)| {
                    let span = metrics.span(start, width);
                    element.child(
                        div()
                            .absolute()
                            .left(px(span.x))
                            .top(px(0.0))
                            .w(px(span.width))
                            .h(px(span.height))
                            .bg(rgb(THEME.selection)),
                    )
                },
            )
            .children(styled_runs)
            .when_some(cursor_column, |element, column| {
                let cursor = metrics.span(column, 1);
                element.child(
                    div()
                        .absolute()
                        .left(px(cursor.x))
                        .top(px(0.0))
                        .w(px(cursor.width))
                        .h(px(cursor.height))
                        .rounded(px(1.0))
                        .border_1()
                        .border_color(if focused {
                            rgb(pane_accent)
                        } else {
                            rgb(THEME.muted)
                        })
                        .when(focused, |cursor| cursor.bg(rgba((pane_accent << 8) | 0x30))),
                )
            })
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .size_full()
                    .child(TerminalPointerElement {
                        input: cx.entity(),
                        pane_id,
                        row: u16::try_from(row).unwrap_or(u16::MAX),
                        columns,
                        cell_width: metrics.cell_width,
                    }),
            )
            .into_any_element()
    }

    fn render_search_bar(&self, editor: &SearchEditor) -> AnyElement {
        div()
            .absolute()
            .right(px(8.0))
            .top(px(7.0))
            .w(px(280.0))
            .h(px(34.0))
            .px(px(9.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(if editor.no_match {
                rgb(THEME.danger)
            } else {
                rgb(THEME.border_strong)
            })
            .shadow_lg()
            .flex()
            .items_center()
            .gap(px(7.0))
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .child("Find")
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .font(self.terminal_font.font(false, false))
                    .child(if editor.query.is_empty() && self.ime_preedit.is_empty() {
                        "Type to search…".to_owned()
                    } else {
                        format!("{}{}", editor.query, self.ime_preedit)
                    }),
            )
            .child(if editor.no_match {
                "No match"
            } else {
                "↵ next"
            })
            .into_any_element()
    }

    fn render_terminal_run(
        &self,
        style: TerminalRun,
        start_column: u16,
        columns: u16,
    ) -> AnyElement {
        let bold = style.attributes.contains(TerminalAttributes::BOLD);
        let dim = style.attributes.contains(TerminalAttributes::DIM);
        let italic = style.attributes.contains(TerminalAttributes::ITALIC);
        let underline = style.attributes.contains(TerminalAttributes::UNDERLINE);
        let strikethrough = style.attributes.contains(TerminalAttributes::STRIKETHROUGH);
        let foreground = THEME.terminal_color(style.foreground, bold, dim);
        let background = THEME.terminal_color(style.background, false, false);
        let metrics = self.terminal_font.metrics;
        let span = metrics.span(start_column, columns);
        let glyph_top = (metrics.baseline - metrics.ascent).max(0.0);
        let glyph_height = metrics.ascent + metrics.descent;
        let text_len = style.text.len();
        div()
            .absolute()
            .left(px(span.x))
            .top(px(0.0))
            .w(px(span.width))
            .h(px(span.height))
            .overflow_hidden()
            .when(
                style.background != TerminalColor::DefaultBackground,
                |element| element.bg(rgb(background)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(glyph_top))
                    .w_full()
                    .h(px(glyph_height))
                    .whitespace_nowrap()
                    .font(self.terminal_font.font(bold, italic))
                    .text_size(px(metrics.font_size))
                    .line_height(px(glyph_height))
                    .child(StyledText::new(style.text).with_runs(vec![TextRun {
                        len: text_len,
                        font: self.terminal_font.font(bold, italic),
                        color: rgb(foreground).into(),
                        background_color: None,
                        underline: underline.then_some(UnderlineStyle {
                            thickness: px(1.0),
                            color: Some(rgb(foreground).into()),
                            wavy: false,
                        }),
                        strikethrough: strikethrough.then_some(StrikethroughStyle {
                            thickness: px(1.0),
                            color: Some(rgb(foreground).into()),
                        }),
                    }])),
            )
            .into_any_element()
    }

    fn render_drop_layer(&self, target_pane: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let preview = self.drag_hover.split_for(target_pane);
        let pane_accent = self.terminal_accent(target_pane).as_rgb();
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .when_some(preview, |element, placement| {
                element.child(
                    div()
                        .absolute()
                        .border_2()
                        .border_color(rgb(pane_accent))
                        .bg(rgba((pane_accent << 8) | 0x24))
                        .when(
                            matches!(placement, DropPlacement::Left | DropPlacement::Right),
                            |element| element.w(relative(0.5)).h_full(),
                        )
                        .when(
                            matches!(placement, DropPlacement::Top | DropPlacement::Bottom),
                            |element| element.h(relative(0.5)).w_full(),
                        )
                        .when(matches!(placement, DropPlacement::Right), |element| {
                            element.right(px(0.0))
                        })
                        .when(matches!(placement, DropPlacement::Bottom), |element| {
                            element.bottom(px(0.0))
                        }),
                )
            })
            .children([
                self.render_drop_zone(target_pane, DropPlacement::Left, cx),
                self.render_drop_zone(target_pane, DropPlacement::Right, cx),
                self.render_drop_zone(target_pane, DropPlacement::Top, cx),
                self.render_drop_zone(target_pane, DropPlacement::Bottom, cx),
            ])
            .into_any_element()
    }

    fn render_drop_zone(
        &self,
        target_pane: Uuid,
        placement: DropPlacement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let placement_id = match placement {
            DropPlacement::Left => "left",
            DropPlacement::Right => "right",
            DropPlacement::Top => "top",
            DropPlacement::Bottom => "bottom",
        };
        div()
            .id((placement_id, element_key(target_pane)))
            .absolute()
            .when(matches!(placement, DropPlacement::Left), |element| {
                element
                    .left(px(0.0))
                    .top(px(0.0))
                    .w(relative(0.25))
                    .h_full()
            })
            .when(matches!(placement, DropPlacement::Right), |element| {
                element
                    .right(px(0.0))
                    .top(px(0.0))
                    .w(relative(0.25))
                    .h_full()
            })
            .when(matches!(placement, DropPlacement::Top), |element| {
                element
                    .top(px(0.0))
                    .left(relative(0.25))
                    .w(relative(0.5))
                    .h(relative(0.5))
            })
            .when(matches!(placement, DropPlacement::Bottom), |element| {
                element
                    .bottom(px(0.0))
                    .left(relative(0.25))
                    .w(relative(0.5))
                    .h(relative(0.5))
            })
            .on_drop(cx.listener(move |this, info: &PaneDrag, _, cx| {
                this.move_pane_to_split(info.pane_id, target_pane, placement, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_tab_menu(&self, menu: TabMenu, cx: &mut Context<Self>) -> AnyElement {
        let pane_id = menu.pane_id;
        div()
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(232.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .id(("rename-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_rename(pane_id, cx)))
                    .child("Rename…"),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .mx(px(8.0))
                    .pt(px(7.0))
                    .border_t_1()
                    .border_color(rgb(THEME.border))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Terminal identity"),
            )
            .child(self.render_profile_choices(pane_id, cx))
            .child(
                div()
                    .id(("reset-identity-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.reset_pane_identity(pane_id, cx)),
                    )
                    .child("Reset name and identity"),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .mx(px(8.0))
                    .pt(px(7.0))
                    .border_t_1()
                    .border_color(rgb(THEME.border))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Terminal color"),
            )
            .child(self.render_color_choices(ColorTarget::Pane(pane_id), "tab-menu", cx))
            .child(
                div()
                    .id(("default-color-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_color(ColorTarget::Pane(pane_id), None, cx)
                    }))
                    .child("Use default"),
            )
            .child(
                div()
                    .id(("pick-color-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_color_picker(ColorTarget::Pane(pane_id), cx)
                    }))
                    .child("Pick color…"),
            )
            .child(
                div()
                    .id(("close-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_close(pane_id, cx)))
                    .child("Close Terminal…"),
            )
            .into_any_element()
    }

    fn render_profile_choices(&self, pane_id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let selected = self
            .pane_metadata(pane_id)
            .and_then(|pane| pane.profile_override);
        let choices = std::iter::once(("Auto".to_owned(), None)).chain(
            TerminalProfile::ALL
                .into_iter()
                .map(|profile| (profile.display_name().to_owned(), Some(profile))),
        );
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(5.0))
            .children(choices.enumerate().map(|(index, (label, profile))| {
                let active = selected == profile;
                div()
                    .id(("identity-profile", index))
                    .px(px(7.0))
                    .py(px(5.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(if active {
                        rgb(THEME.foreground)
                    } else {
                        rgb(THEME.muted)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_pane_profile(pane_id, profile, cx)
                    }))
                    .children(profile.map(|profile| {
                        render_terminal_profile_mark(
                            profile,
                            if active {
                                THEME.foreground
                            } else {
                                THEME.muted
                            },
                            if active { THEME.accent } else { THEME.muted },
                        )
                    }))
                    .child(label)
            }))
            .into_any_element()
    }

    fn render_workspace_menu(&self, menu: WorkspaceMenu, cx: &mut Context<Self>) -> AnyElement {
        let workspace_id = menu.workspace_id;
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        let pinned = workspace.is_some_and(|workspace| workspace.pinned);
        let connection = workspace.map(|workspace| workspace.connection.clone());
        div()
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(232.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .id(("rename-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_rename(workspace_id, cx)
                    }))
                    .child("Rename workspace…"),
            )
            .child(
                div()
                    .id(("pin-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_workspace_pinned(workspace_id, !pinned, cx)
                    }))
                    .child(if pinned {
                        "Unpin workspace"
                    } else {
                        "Pin workspace"
                    }),
            )
            .when(pinned, |element| {
                element
                    .child(
                        div()
                            .id(("move-workspace-up", element_key(workspace_id)))
                            .mx(px(5.0))
                            .px(px(9.0))
                            .py(px(7.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .hover(|item| item.bg(rgb(THEME.accent_soft)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.move_pinned_workspace(workspace_id, WorkspacePinMove::Up, cx)
                            }))
                            .child("Move pinned workspace up"),
                    )
                    .child(
                        div()
                            .id(("move-workspace-down", element_key(workspace_id)))
                            .mx(px(5.0))
                            .px(px(9.0))
                            .py(px(7.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .hover(|item| item.bg(rgb(THEME.accent_soft)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.move_pinned_workspace(workspace_id, WorkspacePinMove::Down, cx)
                            }))
                            .child("Move pinned workspace down"),
                    )
            })
            .when_some(connection, |element, connection| match connection {
                WorkspaceConnection::Local => element,
                WorkspaceConnection::SystemSsh {
                    status: WorkspaceConnectionStatus::Connected,
                    ..
                } => element.child(
                    div()
                        .id(("disconnect-workspace-menu", element_key(workspace_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|item| item.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.disconnect_workspace(workspace_id, cx)
                        }))
                        .child("Disconnect and keep workspace"),
                ),
                WorkspaceConnection::SystemSsh {
                    status: WorkspaceConnectionStatus::Offline,
                    ..
                } => element.child(
                    div()
                        .id(("reconnect-workspace-menu", element_key(workspace_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.ansi[2]))
                        .hover(|item| item.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reconnect_workspace(workspace_id, cx)
                        }))
                        .child("Reconnect with system OpenSSH"),
                ),
            })
            .child(
                div()
                    .mx(px(9.0))
                    .py(px(5.0))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Workspace color"),
            )
            .child(self.render_color_choices(
                ColorTarget::Workspace(workspace_id),
                "workspace-menu",
                cx,
            ))
            .child(
                div()
                    .id(("workspace-default-color", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_color(ColorTarget::Workspace(workspace_id), None, cx)
                    }))
                    .child("Use default"),
            )
            .child(
                div()
                    .id(("workspace-pick-color", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_color_picker(ColorTarget::Workspace(workspace_id), cx)
                    }))
                    .child("Pick color…"),
            )
            .child(
                div()
                    .id(("delete-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .mt(px(4.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_delete(workspace_id, cx)
                    }))
                    .child("Delete workspace…"),
            )
            .into_any_element()
    }

    fn render_color_choices(
        &self,
        target: ColorTarget,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(
                self.appearance_choices()
                    .into_iter()
                    .enumerate()
                    .map(|(index, color)| {
                        let rgb_value = color.as_rgb();
                        div()
                            .id((id_prefix, index))
                            .w(px(20.0))
                            .h(px(20.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(rgb_value))
                            .border_1()
                            .border_color(rgb(THEME.border_strong))
                            .hover(|element| element.border_color(rgb(THEME.foreground)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| TooltipView {
                                    text: format!("#{rgb_value:06X}"),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_color(target, Some(color), cx)
                            }))
                    }),
            )
            .into_any_element()
    }

    fn render_appearance_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let appearance = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.appearance.clone())
            .unwrap_or_default();
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(680.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .font_family(".SystemUIFont")
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(THEME.foreground))
                                    .child("Settings"),
                            )
                            .child(
                                div()
                                    .id("close-appearance")
                                    .w(px(26.0))
                                    .h(px(26.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| {
                                        element
                                            .bg(rgb(THEME.surface))
                                            .text_color(rgb(THEME.foreground))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.appearance_settings_open = false;
                                        cx.notify();
                                    }))
                                    .child("×"),
                            ),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Appearance"),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("Global defaults stay independent. Terminal accents never recolor workspaces, and workspace colors never recolor terminals."),
                    )
                    .child(self.render_appearance_row(
                        "Default terminal accent",
                        "Focus rail, active tab, cursor, and terminal focus treatment",
                        ColorTarget::DefaultTerminal,
                        appearance.default_terminal_accent,
                        cx,
                    ))
                    .child(self.render_appearance_row(
                        "Default workspace color",
                        "Selected workspace and workspace marker in the left rail",
                        ColorTarget::DefaultWorkspace,
                        appearance.default_workspace_color,
                        cx,
                    ))
                    .child(
                        div()
                            .pt(px(2.0))
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Saved locally with session layout · no network or telemetry"),
                    )
                    .child(self.render_history_settings(cx)),
            )
            .into_any_element()
    }

    fn render_history_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.history_status.clone().unwrap_or(HistoryArchiveStatus {
            settings: HistorySettings::default(),
            live_scrollback_lines: 2_000,
            archived_bytes: 0,
            retained_sessions: 0,
            oldest_started_ms: None,
            dropped_bytes: 0,
            warning: None,
        });
        let settings = status.settings.clone();
        let oldest = status
            .oldest_started_ms
            .map_or_else(|| "none yet".to_owned(), format_history_date);
        let warning = history_warning_text(status.warning, status.dropped_bytes);
        let active_workspace = self.active_workspace;
        let focused_pane = self.focused_pane;
        div()
            .pt(px(4.0))
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .pt(px(6.0))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("History Storage"),
                    )
                    .child(
                        div()
                            .id("history-enabled")
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(if settings.enabled {
                                THEME.accent_soft
                            } else {
                                THEME.surface
                            }))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.mutate_history_settings(
                                    |settings| settings.enabled = !settings.enabled,
                                    cx,
                                );
                            }))
                            .child(if settings.enabled {
                                "On · local only"
                            } else {
                                "Off"
                            }),
                    ),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child(format!(
                        "Live memory: {} lines · Local archive: {} · {} sessions · oldest {}",
                        status.live_scrollback_lines,
                        format_bytes(status.archived_bytes),
                        status.retained_sessions,
                        oldest
                    )),
            )
            .when_some(warning, |element, warning| {
                element.child(
                    div()
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(5.0))
                        .bg(rgb(THEME.surface))
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.danger))
                        .child(warning),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(history_label("Retention"))
                    .children(
                        [
                            ("Forever", HistoryRetention::Indefinite),
                            ("7d", HistoryRetention::Days { days: 7 }),
                            ("30d", HistoryRetention::Days { days: 30 }),
                            ("90d", HistoryRetention::Days { days: 90 }),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, retention))| {
                            let selected = settings.retention == retention;
                            div()
                                .id(("history-retention", index))
                                .px(px(7.0))
                                .py(px(3.0))
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .bg(rgb(if selected {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mutate_history_settings(
                                        |settings| settings.retention = retention,
                                        cx,
                                    );
                                }))
                                .child(label)
                        }),
                    )
                    .child(self.render_history_custom_field(
                        HistoryEditField::RetentionDays,
                        "Custom days",
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(history_label("Quota"))
                    .children(
                        [
                            ("1 GiB", 1_u64),
                            ("5 GiB", 5),
                            ("10 GiB", 10),
                            ("50 GiB", 50),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, gib))| {
                            let bytes = gib * 1024 * 1024 * 1024;
                            let selected = settings.quota_bytes == bytes;
                            div()
                                .id(("history-quota", index))
                                .px(px(7.0))
                                .py(px(3.0))
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .bg(rgb(if selected {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mutate_history_settings(
                                        |settings| settings.quota_bytes = bytes,
                                        cx,
                                    );
                                }))
                                .child(label)
                        }),
                    )
                    .child(self.render_history_custom_field(
                        HistoryEditField::QuotaGib,
                        "Custom GiB",
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(history_label("At capacity"))
                    .child(
                        div()
                            .id("history-pause-policy")
                            .px(px(7.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .bg(rgb(
                                if settings.cleanup_policy == HistoryCleanupPolicy::PauseWhenFull {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                },
                            ))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mutate_history_settings(
                                    |settings| {
                                        settings.cleanup_policy =
                                            HistoryCleanupPolicy::PauseWhenFull;
                                    },
                                    cx,
                                );
                            }))
                            .child("Pause + warn (safe)"),
                    )
                    .child(
                        div()
                            .id("history-delete-oldest-policy")
                            .px(px(7.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .bg(rgb(
                                if settings.cleanup_policy == HistoryCleanupPolicy::DeleteOldest {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                },
                            ))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mutate_history_settings(
                                    |settings| {
                                        settings.cleanup_policy =
                                            HistoryCleanupPolicy::DeleteOldest;
                                    },
                                    cx,
                                );
                            }))
                            .child("Auto-delete oldest (opt-in)"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(history_label("Clear"))
                    .when_some(focused_pane, |element, pane_id| {
                        element.child(self.render_clear_history_button(
                            "Terminal",
                            HistoryClearScope::Terminal { pane_id },
                            cx,
                        ))
                    })
                    .when_some(active_workspace, |element, workspace_id| {
                        element.child(self.render_clear_history_button(
                            "Workspace",
                            HistoryClearScope::Workspace { workspace_id },
                            cx,
                        ))
                    })
                    .child(self.render_clear_history_button(
                        "All history",
                        HistoryClearScope::All,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_right()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Future output only · older sessions cannot be recovered"),
                    ),
            )
            .into_any_element()
    }

    fn render_history_custom_field(
        &self,
        field: HistoryEditField,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self
            .history_editor
            .as_ref()
            .filter(|editor| editor.field == field);
        let label = editor.map_or_else(
            || placeholder.to_owned(),
            |editor| {
                if editor.invalid {
                    format!("{} · invalid", editor.text)
                } else {
                    format!("{} ↵", editor.text)
                }
            },
        );
        div()
            .id(match field {
                HistoryEditField::RetentionDays => "custom-retention",
                HistoryEditField::QuotaGib => "custom-quota",
            })
            .px(px(7.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if editor.is_some() {
                THEME.accent
            } else {
                THEME.border
            }))
            .font_family("SF Mono")
            .text_xs()
            .text_color(rgb(if editor.is_some_and(|editor| editor.invalid) {
                THEME.danger
            } else {
                THEME.muted
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.begin_history_edit(field, cx)))
            .child(label)
            .into_any_element()
    }

    fn render_clear_history_button(
        &self,
        label: &'static str,
        scope: HistoryClearScope,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let confirming = self.history_clear_confirmation == Some(scope);
        div()
            .id(("clear-history", history_scope_key(scope)))
            .px(px(7.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if confirming {
                THEME.danger
            } else {
                THEME.border
            }))
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(if confirming {
                THEME.danger
            } else {
                THEME.muted
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.clear_history(scope, cx)))
            .child(if confirming {
                format!("Confirm {label}")
            } else {
                label.to_owned()
            })
            .into_any_element()
    }

    fn render_appearance_row(
        &self,
        label: &'static str,
        description: &'static str,
        target: ColorTarget,
        color: AppearanceColor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rgb_value = color.as_rgb();
        div()
            .p(px(12.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.surface))
            .border_1()
            .border_color(rgb(THEME.border))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .w(px(28.0))
                    .h(px(28.0))
                    .rounded(px(7.0))
                    .bg(rgb(rgb_value))
                    .border_1()
                    .border_color(rgb(THEME.border_strong)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(THEME.foreground))
                            .child(label),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .child(description),
                    ),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(format!("#{rgb_value:06X}")),
            )
            .child(
                div()
                    .id(match target {
                        ColorTarget::DefaultTerminal => "pick-default-terminal",
                        ColorTarget::DefaultWorkspace => "pick-default-workspace",
                        ColorTarget::Pane(_) => "pick-pane",
                        ColorTarget::Workspace(_) => "pick-workspace",
                    })
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.border_color(rgb(rgb_value)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_color_picker(target, cx)))
                    .child("Pick color…"),
            )
            .into_any_element()
    }

    fn render_color_picker(&self, picker: &ColorPickerState, cx: &mut Context<Self>) -> AnyElement {
        let target = picker.target;
        let (title, can_reset) = match target {
            ColorTarget::DefaultTerminal => ("Pick default terminal accent", false),
            ColorTarget::DefaultWorkspace => ("Pick default workspace color", false),
            ColorTarget::Pane(_) => ("Pick terminal color", true),
            ColorTarget::Workspace(_) => ("Pick workspace color", true),
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0faa))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(340.0))
                    .p(px(16.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(11.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(title),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .child("Recent colors first, followed by Harbor Night presets."),
                    )
                    .child(self.render_color_choices(target, "color-picker", cx))
                    .child(
                        div()
                            .h(px(36.0))
                            .px(px(10.0))
                            .rounded(px(6.0))
                            .bg(rgb(THEME.terminal))
                            .border_1()
                            .border_color(if picker.invalid {
                                rgb(THEME.danger)
                            } else {
                                rgb(THEME.border_strong)
                            })
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child("#")
                            .child(
                                div()
                                    .when(picker.replace_on_type, |element| {
                                        element.bg(rgb(THEME.selection))
                                    })
                                    .child(picker.hex.clone()),
                            )
                            .child("│")
                            .when(picker.invalid, |element| {
                                element.child(
                                    div()
                                        .ml(px(6.0))
                                        .font_family(".SystemUIFont")
                                        .text_xs()
                                        .text_color(rgb(THEME.danger))
                                        .child("Enter six hex digits"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .when(can_reset, |element| {
                                element.child(
                                    div()
                                        .id("picker-use-default")
                                        .px(px(11.0))
                                        .py(px(7.0))
                                        .rounded(px(5.0))
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(rgb(THEME.foreground))
                                        .hover(|element| element.bg(rgb(THEME.surface)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.apply_color(target, None, cx)
                                        }))
                                        .child("Use default"),
                                )
                            })
                            .child(
                                div()
                                    .id("cancel-color-picker")
                                    .px(px(11.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.surface)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.color_picker = None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("apply-color-picker")
                                    .px(px(11.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.accent_soft))
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .hover(|element| element.bg(rgb(THEME.selection)))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.submit_color_picker(cx)),
                                    )
                                    .child("Apply"),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_rename_dialog(&self, editor: &RenameEditor, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(390.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Rename terminal"),
                    )
                    .child(
                        div()
                            .h(px(36.0))
                            .px(px(10.0))
                            .rounded(px(6.0))
                            .bg(rgb(THEME.terminal))
                            .border_1()
                            .border_color(rgb(THEME.accent))
                            .flex()
                            .items_center()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child(
                                div()
                                    .when(editor.replace_on_type, |element| {
                                        element.bg(rgb(THEME.selection))
                                    })
                                    .child(format!("{}{}", editor.value, self.ime_preedit)),
                            )
                            .child("│"),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-rename")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.surface)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.rename_editor = None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("save-rename")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.accent))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx)))
                                    .child("Rename"),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_workspace_creation_dialog(
        &self,
        dialog: &WorkspaceCreationDialog,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = dialog.kind;
        let field = dialog.field;
        let destination = dialog.destination.text.clone();
        let error = dialog.error.clone();
        let name_input_focus =
            self.workspace_input_focus[WorkspaceCreationField::Name.index()].clone();
        let destination_input_focus =
            self.workspace_input_focus[WorkspaceCreationField::Destination.index()].clone();
        let content = match dialog.step {
            WorkspaceCreationStep::Details => div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(THEME.foreground))
                        .child("New Workspace"),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("new-workspace-local")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .border_1()
                                .border_color(rgb(if kind == WorkspaceCreationKind::Local {
                                    THEME.accent
                                } else {
                                    THEME.border_strong
                                }))
                                .bg(rgb(if kind == WorkspaceCreationKind::Local {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_sm()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(dialog) = this.workspace_creation.as_mut() {
                                        dialog.kind = WorkspaceCreationKind::Local;
                                        dialog.field = WorkspaceCreationField::Name;
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                }))
                                .child("Local shell"),
                        )
                        .child(
                            div()
                                .id("new-workspace-ssh")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .border_1()
                                .border_color(rgb(if kind == WorkspaceCreationKind::SystemSsh {
                                    THEME.accent
                                } else {
                                    THEME.border_strong
                                }))
                                .bg(rgb(if kind == WorkspaceCreationKind::SystemSsh {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_sm()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(dialog) = this.workspace_creation.as_mut() {
                                        dialog.kind = WorkspaceCreationKind::SystemSsh;
                                        dialog.field = WorkspaceCreationField::Destination;
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                }))
                                .child("System SSH"),
                        ),
                )
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child("Workspace name (optional)"),
                )
                .child(
                    div()
                        .id("workspace-name-input")
                        .track_focus(&name_input_focus)
                        .h(px(36.0))
                        .px(px(10.0))
                        .rounded(px(6.0))
                        .bg(rgb(THEME.terminal))
                        .border_1()
                        .border_color(rgb(if field == WorkspaceCreationField::Name {
                            THEME.accent
                        } else {
                            THEME.border_strong
                        }))
                        .overflow_hidden()
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                            this.focus_workspace_creation_field(
                                WorkspaceCreationField::Name,
                                event.mouse_position(),
                                event.modifiers().shift,
                            );
                            cx.notify();
                        }))
                        .child(WorkspaceTextInputElement {
                            input: cx.entity(),
                            field: WorkspaceCreationField::Name,
                            placeholder: "Workspace name",
                        }),
                )
                .when(kind == WorkspaceCreationKind::SystemSsh, |element| {
                    element
                        .child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child("SSH destination or exact ssh command"),
                        )
                        .child(
                            div()
                                .id("workspace-ssh-input")
                                .track_focus(&destination_input_focus)
                                .h(px(36.0))
                                .px(px(10.0))
                                .rounded(px(6.0))
                                .bg(rgb(THEME.terminal))
                                .border_1()
                                .border_color(rgb(
                                    if field == WorkspaceCreationField::Destination {
                                        THEME.accent
                                    } else {
                                        THEME.border_strong
                                    },
                                ))
                                .overflow_hidden()
                                .cursor_pointer()
                                .flex()
                                .items_center()
                                .font_family("SF Mono")
                                .text_sm()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                    this.focus_workspace_creation_field(
                                        WorkspaceCreationField::Destination,
                                        event.mouse_position(),
                                        event.modifiers().shift,
                                    );
                                    cx.notify();
                                }))
                                .child(WorkspaceTextInputElement {
                                    input: cx.entity(),
                                    field: WorkspaceCreationField::Destination,
                                    placeholder: "ssh user@host-or-alias",
                                }),
                        )
                })
                .when(kind == WorkspaceCreationKind::SystemSsh, |element| {
                    element.child(
                        div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "The workspace connects immediately after confirmation and saves only its name, destination, pin/order, and offline/connected intent locally. System OpenSSH keeps authority over config, agent, keys, proxies, and known_hosts. Not a Harness stores no credentials or SSH config contents.",
                        ),
                    )
                })
                .when_some(error, |element, message| {
                    element.child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.danger))
                            .child(message),
                    )
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("cancel-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.workspace_creation = None;
                                    cx.notify();
                                }))
                                .child("Cancel"),
                        )
                        .child(
                            div()
                                .id("submit-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .text_color(rgb(0xffffff))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit_workspace_creation(cx)
                                }))
                                .child(if kind == WorkspaceCreationKind::SystemSsh {
                                    "Review connection"
                                } else {
                                    "Create workspace"
                                }),
                        ),
                )
                .into_any_element(),
            WorkspaceCreationStep::ConfirmSsh => div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(THEME.foreground))
                        .child(format!("Connect and save {destination}?")),
                )
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "This starts the installed OpenSSH client now and saves safe workspace metadata locally for later reconnect. Not a Harness adds no SSH options, stores no credentials, and does not change your config, agent, forwarding, or host-key policy.",
                        ),
                )
                .when_some(error, |element, message| {
                    element.child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.danger))
                            .child(message),
                    )
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("back-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(dialog) = this.workspace_creation.as_mut() {
                                        dialog.step = WorkspaceCreationStep::Details;
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                }))
                                .child("Back"),
                        )
                        .child(
                            div()
                                .id("confirm-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .text_color(rgb(0xffffff))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit_workspace_creation(cx)
                                }))
                                .child("Connect and save"),
                        ),
                )
                .into_any_element(),
        };

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(520.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_workspace_rename_dialog(
        &self,
        editor: &WorkspaceRenameEditor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(390.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Rename workspace"),
                    )
                    .child(
                        div()
                            .h(px(36.0))
                            .px(px(10.0))
                            .rounded(px(6.0))
                            .bg(rgb(THEME.terminal))
                            .border_1()
                            .border_color(rgb(THEME.accent))
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child(editor.value.clone())
                            .child("│"),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-workspace-rename")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_rename_editor = None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("save-workspace-rename")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.accent))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_workspace_rename(cx)
                                    }))
                                    .child("Rename"),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_workspace_delete_dialog(
        &self,
        confirmation: &WorkspaceDeleteConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = if confirmation.active_terminal_count == 0 {
            "This removes the saved workspace metadata from this machine. No active terminal process will be ended.".to_owned()
        } else {
            format!(
                "This permanently removes the workspace and ends {} active terminal process{}. Disconnecting is the non-destructive choice for a saved SSH workspace.",
                confirmation.active_terminal_count,
                if confirmation.active_terminal_count == 1 {
                    ""
                } else {
                    "es"
                }
            )
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(440.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(format!("Delete {}?", confirmation.title)),
                    )
                    .child(div().text_sm().text_color(rgb(THEME.muted)).child(message))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-workspace-delete")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_delete_confirmation = None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("confirm-workspace-delete")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.danger))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_workspace_delete(cx)
                                    }))
                                    .child("Delete workspace"),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_close_dialog(
        &self,
        confirmation: &CloseConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(410.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(9.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(format!("Close {}?", confirmation.title)),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child(if confirmation.leaves_workspace_empty {
                                "This will terminate the last terminal and leave the saved workspace empty. You can open a new terminal from its empty state."
                            } else {
                                "This will terminate this terminal and its running shell process. Other terminal tabs stay open."
                            }),
                    )
                    .child(
                        div()
                            .mt(px(7.0))
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-close")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.surface)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.close_confirmation = None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("confirm-close")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.danger))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(|this, _, _, cx| this.confirm_close(cx)))
                                    .child("Close Terminal"),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_layout(
        &self,
        layout: PaneLayout,
        width: f32,
        height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match layout {
            PaneLayout::Leaf { pane } => {
                let active = pane.id;
                self.render_terminal(vec![pane], active, cx)
            }
            PaneLayout::Stack { panes, active } => self.render_terminal(panes, active, cx),
            PaneLayout::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let split_id = split_control_id(&first, &second);
                let ratio = effective_split_ratio(
                    axis,
                    width,
                    height,
                    self.split_ratios.get(&split_id).copied().unwrap_or(ratio),
                );
                let vertical = axis == SplitAxis::Vertical;
                let (first_width, first_height, second_width, second_height) =
                    split_child_dimensions(axis, width, height, ratio);
                div()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .flex()
                    .when(vertical, |element| element.flex_col())
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .when(vertical, |element| element.h(relative(ratio)).w_full())
                            .when(!vertical, |element| element.w(relative(ratio)).h_full())
                            .child(self.render_layout(*first, first_width, first_height, cx)),
                    )
                    .child(self.render_divider(split_id, axis, cx))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex_1()
                            .child(self.render_layout(*second, second_width, second_height, cx)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_divider(
        &self,
        split_id: SplitControlId,
        axis: SplitAxis,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let vertical = axis == SplitAxis::Vertical;
        div()
            .id(("divider", split_element_key(split_id)))
            .flex_none()
            .when(vertical, |element| {
                element
                    .w_full()
                    .h(px(4.0))
                    .cursor(CursorStyle::ResizeUpDown)
            })
            .when(!vertical, |element| {
                element
                    .h_full()
                    .w(px(4.0))
                    .cursor(CursorStyle::ResizeLeftRight)
            })
            .bg(rgb(THEME.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.resizing = Some(ResizeDrag { split_id, axis });
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn binding_label(&self, command: AppCommand) -> String {
        self.keymap
            .bindings
            .iter()
            .filter(|binding| binding.command == command)
            .map(|binding| binding.sequence.as_str())
            .collect::<Vec<_>>()
            .join("  ")
    }

    fn render_command_palette(
        &self,
        palette: &CommandPaletteState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let matches = palette_matches(&palette.query, COMMAND_PALETTE_LIMIT);
        let query = if palette.query.is_empty() {
            "Type a command…".to_owned()
        } else {
            palette.query.clone()
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x00000070))
            .flex()
            .justify_center()
            .pt(px(92.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.command_palette = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("command-palette")
                    .w(px(620.0))
                    .h_auto()
                    .max_h(relative(0.75))
                    .overflow_y_scroll()
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.elevated))
                    .shadow_lg()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        div()
                            .h(px(48.0))
                            .px(px(15.0))
                            .border_b_1()
                            .border_color(rgb(THEME.border))
                            .flex()
                            .items_center()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(if palette.query.is_empty() {
                                rgb(THEME.dim)
                            } else {
                                rgb(THEME.foreground)
                            })
                            .child(query),
                    )
                    .children(matches.into_iter().enumerate().map(|(index, item)| {
                        let command = item.command;
                        let metadata = descriptor(command);
                        let selected = index == palette.selected;
                        div()
                            .id(("palette-command", index))
                            .h(px(44.0))
                            .px(px(13.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .when(selected, |element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.execute_command(command, cx);
                                cx.stop_propagation();
                            }))
                            .child(
                                div()
                                    .w(px(210.0))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.dim))
                                    .child(format!("{} · {}", metadata.category, metadata.id)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .font_family(".SystemUIFont")
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .child(metadata.title),
                            )
                            .child(
                                div()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(rgb(THEME.muted))
                                    .child(self.binding_label(command)),
                            )
                    })),
            )
            .into_any_element()
    }

    fn render_workspace(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = &self.snapshot else {
            return div()
                .size_full()
                .bg(rgb(THEME.terminal))
                .flex()
                .items_center()
                .justify_center()
                .font_family("SF Mono")
                .text_sm()
                .text_color(rgb(THEME.muted))
                .child("session service unavailable")
                .into_any_element();
        };
        let Some(workspace) = self.active_workspace_in(snapshot) else {
            return div().size_full().bg(rgb(THEME.terminal)).into_any_element();
        };
        let workspace_id = workspace.id;
        let empty_workspace_uses_ssh =
            matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. });
        let open_terminal_binding = self.binding_label(AppCommand::NewTab);
        let workspace_color = self.workspace_color(workspace.id).as_rgb();
        let canonical_layout = workspace.tabs.first().map(|tab| tab.layout.clone());
        let layout = canonical_layout.as_ref().map(|layout| {
            self.zoomed_pane
                .and_then(|pane_id| zoom_projection(layout, pane_id))
                .unwrap_or_else(|| layout.clone())
        });
        let workspace_content = if let Some(layout) = layout {
            self.render_layout(layout, self.workspace_pixels.0, self.workspace_pixels.1, cx)
        } else {
            div()
                .size_full()
                .bg(rgb(THEME.terminal))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(420.0))
                        .p(px(24.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(rgb(THEME.border_strong))
                        .bg(rgb(THEME.surface))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            div()
                                .font_family("SF Mono")
                                .text_lg()
                                .text_color(rgb(THEME.foreground))
                                .child(">_"),
                        )
                        .child(
                            div()
                                .font_family(".SystemUIFont")
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(THEME.foreground))
                                .child("No terminals open"),
                        )
                        .child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .text_center()
                                .child(if empty_workspace_uses_ssh {
                                    "Open a fresh remote terminal with this workspace's saved system OpenSSH destination."
                                } else {
                                    "This workspace is saved and ready when you want another local shell."
                                }),
                        )
                        .child(
                            div()
                                .id("open-empty-workspace-terminal")
                                .mt(px(4.0))
                                .px(px(16.0))
                                .py(px(9.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(0xffffff))
                                .hover(|element| element.bg(rgb(THEME.ansi[4])))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_workspace_terminal(workspace_id, cx)
                                }))
                                .child("Open Terminal"),
                        )
                        .child(
                            div()
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child(format!(
                                    "Press {open_terminal_binding} to open a terminal"
                                )),
                        ),
                )
                .into_any_element()
        };
        div()
            .min_w(px(0.0))
            .h_full()
            .flex_1()
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(TITLEBAR_HEIGHT))
                    .flex_none()
                    .pl(px(if self.sidebar_visible { 11.0 } else { 79.0 }))
                    .pr(px(11.0))
                    .bg(rgb(THEME.surface))
                    .border_b_1()
                    .border_color(rgb(THEME.border))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(!self.sidebar_visible, |element| {
                        element.child(
                            div()
                                .id("show-workspace-sidebar")
                                .flex_none()
                                .w(px(24.0))
                                .h(px(24.0))
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(THEME.muted))
                                .hover(|element| {
                                    element
                                        .bg(rgb(THEME.elevated))
                                        .text_color(rgb(THEME.foreground))
                                })
                                .tooltip(|_, cx| {
                                    cx.new(|_| TooltipView {
                                        text: "Show workspace sidebar (⌘B)".to_owned(),
                                    })
                                    .into()
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
                                .child(render_sidebar_toggle_icon(false)),
                        )
                    })
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(workspace_color))
                            .child("▰"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(workspace.title.clone()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child(format!(
                                "{} · {}{}  ·  ⇧⌘P commands",
                                THEME.name,
                                self.terminal_font.family,
                                if self.zoomed_pane.is_some() {
                                    " · ZOOMED"
                                } else {
                                    ""
                                }
                            )),
                    ),
            )
            .child(div().min_h(px(0.0)).flex_1().child(workspace_content))
            .into_any_element()
    }
}

impl Render for NahApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_window_geometry(window);

        // The workspace dialog has its own focus targets. A pointer click on
        // the sidebar button must not leave native text input attached to the
        // terminal behind the dialog.
        if let Some(dialog) = self.workspace_creation.as_ref() {
            self.workspace_input_focus[dialog.field.index()].focus(window);
        }

        div()
            .key_context(if self.command_palette.is_some() {
                "NahPalette"
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx)
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.handle_resize(event, window, cx)
            }))
            .on_drag_move::<PaneDrag>(cx.listener(
                |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    this.dragging_pane = Some(event.drag(cx).pane_id);
                    this.drag_hover.clear();
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
                    let dismissed_tab = this.tab_menu.take().is_some();
                    let dismissed_workspace = this.workspace_menu.take().is_some();
                    if dismissed_tab || dismissed_workspace {
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
            .on_action(
                cx.listener(|_: &mut NahApp, _: &ConsumeChordPrefix, _, cx| {
                    cx.stop_propagation();
                }),
            )
            .on_action(cx.listener(NahApp::copy_terminal))
            .on_action(cx.listener(NahApp::paste_terminal))
            .on_action(cx.listener(NahApp::find_terminal))
            .on_action(cx.listener(NahApp::find_next_terminal))
            .child(
                div()
                    .absolute()
                    .w(px(1.0))
                    .h(px(1.0))
                    .child(TerminalInputElement { input: cx.entity() }),
            )
            .when(self.sidebar_resize.is_active(), |element| {
                element.child(
                    div()
                        .absolute()
                        .w(px(1.0))
                        .h(px(1.0))
                        .child(SidebarResizeCaptureElement { input: cx.entity() }),
                )
            })
            .when(self.sidebar_visible, |element| {
                element
                    .child(self.render_sidebar(cx))
                    .child(self.render_sidebar_resize_handle(cx))
            })
            .child(self.render_workspace(cx))
            .when_some(self.tab_menu, |element, menu| {
                element.child(self.render_tab_menu(menu, cx))
            })
            .when_some(self.workspace_menu, |element, menu| {
                element.child(self.render_workspace_menu(menu, cx))
            })
            .when_some(self.rename_editor.as_ref(), |element, editor| {
                element.child(self.render_rename_dialog(editor, cx))
            })
            .when_some(self.workspace_rename_editor.as_ref(), |element, editor| {
                element.child(self.render_workspace_rename_dialog(editor, cx))
            })
            .when_some(self.close_confirmation.as_ref(), |element, confirmation| {
                element.child(self.render_close_dialog(confirmation, cx))
            })
            .when_some(
                self.workspace_delete_confirmation.as_ref(),
                |element, confirmation| {
                    element.child(self.render_workspace_delete_dialog(confirmation, cx))
                },
            )
            .when_some(self.command_palette.as_ref(), |element, palette| {
                element.child(self.render_command_palette(palette, cx))
            })
            .when_some(self.workspace_creation.as_ref(), |element, dialog| {
                element.child(self.render_workspace_creation_dialog(dialog, cx))
            })
            .when(self.appearance_settings_open, |element| {
                element.child(self.render_appearance_settings(cx))
            })
            .when_some(self.color_picker.as_ref(), |element, picker| {
                element.child(self.render_color_picker(picker, cx))
            })
    }
}

impl EntityInputHandler for NahApp {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if let Some(dialog) = self
            .workspace_creation
            .as_ref()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            let byte_range = editor.range_from_utf16(&range);
            actual_range.replace(editor.range_to_utf16(&byte_range));
            return Some(editor.text[byte_range].to_owned());
        }
        actual_range.replace(0..self.ime_preedit.encode_utf16().count());
        Some(self.ime_preedit.clone())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(dialog) = self
            .workspace_creation
            .as_ref()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            return Some(UTF16Selection {
                range: editor.range_to_utf16(&editor.selected_range),
                reversed: editor.selection_reversed,
            });
        }
        let end = self.ime_preedit.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        if let Some(dialog) = self
            .workspace_creation
            .as_ref()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            return editor
                .marked_range
                .as_ref()
                .map(|range| editor.range_to_utf16(range));
        }
        (!self.ime_preedit.is_empty()).then(|| 0..self.ime_preedit.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self
            .workspace_creation
            .as_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.active_editor_mut().marked_range = None;
            cx.notify();
            return;
        }
        self.ime_preedit.clear();
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
            .workspace_creation
            .as_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.replace_text(range.as_ref(), text, false, None);
            cx.notify();
            return;
        }
        self.ime_preedit.clear();
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
            .workspace_creation
            .as_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.replace_text(range.as_ref(), text, true, selected_range.as_ref());
            cx.notify();
            return;
        }
        text.clone_into(&mut self.ime_preedit);
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
            .workspace_creation
            .as_ref()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let index = dialog.field.index();
            let (Some(line), Some(input_bounds)) = (
                self.workspace_input_layouts[index].as_ref(),
                self.workspace_input_bounds[index],
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
        Some(Bounds::new(
            bounds.bottom_left(),
            size(px(1.0), px(self.terminal_font.metrics.line_height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if let Some(dialog) = self
            .workspace_creation
            .as_ref()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let index = dialog.field.index();
            let (Some(line), Some(bounds)) = (
                self.workspace_input_layouts[index].as_ref(),
                self.workspace_input_bounds[index],
            ) else {
                return None;
            };
            let byte_index = line.closest_index_for_x(point.x - bounds.left());
            return Some(dialog.active_editor().offset_to_utf16(byte_index));
        }
        Some(0)
    }
}

struct WorkspaceTextInputElement {
    input: Entity<NahApp>,
    field: WorkspaceCreationField,
    placeholder: &'static str,
}

struct WorkspaceTextPrepaintState {
    line: ShapedLine,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    text_bounds: Bounds<Pixels>,
    active: bool,
}

impl IntoElement for WorkspaceTextInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WorkspaceTextInputElement {
    type RequestLayoutState = ();
    type PrepaintState = WorkspaceTextPrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let app = self.input.read(cx);
        let dialog = app.workspace_creation.as_ref();
        let active = dialog.is_some_and(|dialog| {
            dialog.step == WorkspaceCreationStep::Details && dialog.field == self.field
        });
        let editor = dialog.map(|dialog| match self.field {
            WorkspaceCreationField::Name => &dialog.name,
            WorkspaceCreationField::Destination => &dialog.destination,
        });
        let content = editor.map_or("", |editor| editor.text.as_str());
        let display_text = if content.is_empty() {
            self.placeholder
        } else {
            content
        };
        let style = window.text_style();
        let base_run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: if content.is_empty() {
                rgb(THEME.dim).into()
            } else {
                style.color
            },
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs =
            if let Some(marked_range) = editor.and_then(|editor| editor.marked_range.as_ref()) {
                vec![
                    TextRun {
                        len: marked_range.start,
                        ..base_run.clone()
                    },
                    TextRun {
                        len: marked_range.len(),
                        underline: Some(UnderlineStyle {
                            color: Some(base_run.color),
                            thickness: px(1.0),
                            wavy: false,
                        }),
                        ..base_run.clone()
                    },
                    TextRun {
                        len: content.len().saturating_sub(marked_range.end),
                        ..base_run
                    },
                ]
                .into_iter()
                .filter(|run| run.len > 0)
                .collect()
            } else {
                vec![base_run]
            };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line =
            window
                .text_system()
                .shape_line(display_text.to_owned().into(), font_size, &runs, None);
        let selected_range = editor.map_or(0..0, |editor| editor.selected_range.clone());
        let cursor_offset = editor.map_or(0, DialogTextEditor::cursor_offset);
        let cursor_x = line.x_for_index(cursor_offset);
        let scroll_x = if active {
            (cursor_x - (bounds.size.width - px(2.0))).max(px(0.0))
        } else {
            px(0.0)
        };
        let text_bounds = Bounds::new(point(bounds.left() - scroll_x, bounds.top()), bounds.size);
        let (selection, cursor) = if active && !selected_range.is_empty() {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            text_bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            text_bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x62adff40),
                )),
                None,
            )
        } else if active {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(
                            text_bounds.left() + line.x_for_index(cursor_offset),
                            bounds.top(),
                        ),
                        size(px(1.5), bounds.bottom() - bounds.top()),
                    ),
                    rgb(THEME.accent),
                )),
            )
        } else {
            (None, None)
        };

        WorkspaceTextPrepaintState {
            line,
            cursor,
            selection,
            text_bounds,
            active,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if state.active {
            let focus_handle =
                self.input.read(cx).workspace_input_focus[self.field.index()].clone();
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
        if let Some(selection) = state.selection.take() {
            window.paint_quad(selection);
        }
        state
            .line
            .paint(state.text_bounds.origin, window.line_height(), window, cx)
            .expect("workspace input text should paint");
        if let Some(cursor) = state.cursor.take() {
            window.paint_quad(cursor);
        }
        let line = state.line.clone();
        let field_index = self.field.index();
        self.input.update(cx, |app, _| {
            app.workspace_input_layouts[field_index] = Some(line);
            app.workspace_input_bounds[field_index] = Some(state.text_bounds);
        });
    }
}

struct TerminalInputElement {
    input: Entity<NahApp>,
}

impl IntoElement for TerminalInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalInputElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let app = self.input.read(cx);
        if app.workspace_creation.is_none() {
            window.handle_input(
                &app.focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
    }
}

/// Registers window-level listeners while the sidebar divider owns an active
/// pointer gesture. GPUI's normal element listeners are hover-scoped, while a
/// resize capture must continue to receive drag and release events outside the
/// divider (and even outside the window bounds when the platform delivers them).
struct SidebarResizeCaptureElement {
    input: Entity<NahApp>,
}

impl IntoElement for SidebarResizeCaptureElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SidebarResizeCaptureElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let input = self.input.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                input.update(cx, |this, cx| this.handle_resize(event, window, cx));
                cx.stop_propagation();
            }
        });

        let input = self.input.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                input.update(cx, |this, cx| this.finish_resize(cx));
                cx.stop_propagation();
            }
        });
    }
}

/// One hit surface per terminal row keeps pointer semantics exact without
/// forcing GPUI/Taffy to lay out an element for every visible grid cell.
struct TerminalPointerElement {
    input: Entity<NahApp>,
    pane_id: Uuid,
    row: u16,
    columns: u16,
    cell_width: f32,
}

impl IntoElement for TerminalPointerElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalPointerElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, HitboxBehavior::BlockMouse)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        let pointer_hitbox = hitbox.clone();
        let input = self.input.clone();
        let pane_id = self.pane_id;
        let row = self.row;
        let columns = self.columns;
        let cell_width = self.cell_width;
        let hitbox = pointer_hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.begin_terminal_pointer(pane_id, point, event, window, cx);
                });
            }
        });

        let input = self.input.clone();
        let hitbox = pointer_hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.move_terminal_pointer(pane_id, point, event, cx);
                });
            }
        });

        let input = self.input.clone();
        let hitbox = pointer_hitbox.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.end_terminal_pointer(pane_id, point, event, cx);
                });
            }
        });

        let input = self.input.clone();
        let hitbox = pointer_hitbox;
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.scroll_terminal(pane_id, point, event, cx);
                });
            }
        });
    }
}

fn terminal_point_at(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    row: u16,
    columns: u16,
    cell_width: f32,
) -> TerminalPoint {
    let relative_x = f32::from(position.x - bounds.origin.x).max(0.0);
    let column = if columns == 0 || cell_width <= f32::EPSILON {
        0
    } else {
        (relative_x / cell_width).floor() as u16
    };
    TerminalPoint {
        row,
        column: column.min(columns.saturating_sub(1)),
    }
}

fn next_terminal_poll_delay_ms(current: u64, state_changed: bool) -> u64 {
    if state_changed {
        ACTIVE_TERMINAL_POLL_MS
    } else {
        current.saturating_mul(2).min(IDLE_TERMINAL_POLL_MS)
    }
}

fn pane_update_requires_repaint(snapshot_delivered: bool, screens_delivered: usize) -> bool {
    snapshot_delivered || screens_delivered > 0
}

fn responsive_panes(
    now: Instant,
    focused_pane: Option<Uuid>,
    pane_attention: &HashMap<Uuid, Instant>,
) -> Vec<Uuid> {
    let mut panes = pane_attention
        .iter()
        .filter_map(|(pane_id, attended)| {
            (Some(*pane_id) == focused_pane
                || now.saturating_duration_since(*attended) < COLD_PANE_AFTER)
                .then_some(*pane_id)
        })
        .collect::<Vec<_>>();
    if let Some(focused) = focused_pane
        && !panes.contains(&focused)
    {
        panes.push(focused);
    }
    panes.sort_unstable();
    panes
}

fn terminal_mouse_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

fn terminal_modifiers(modifiers: gpui::Modifiers) -> TerminalModifiers {
    TerminalModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
    }
}

fn selection_span(selection: TerminalSelection, row: usize, columns: u16) -> Option<(u16, u16)> {
    let row = u16::try_from(row).ok()?;
    if row < selection.start.row || row > selection.end.row || columns == 0 {
        return None;
    }
    let start = if selection.is_block || row == selection.start.row {
        selection.start.column.min(columns - 1)
    } else {
        0
    };
    let end = if selection.is_block || row == selection.end.row {
        selection.end.column.min(columns - 1)
    } else {
        columns - 1
    };
    (end >= start).then_some((start, end - start + 1))
}

fn prepare_paste(text: &str, bracketed: bool) -> Result<Vec<u8>, &'static str> {
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r");
    let sanitized = normalized.replace(['\0', '\u{1b}'], "");
    let wrapper_size = if bracketed { 12 } else { 0 };
    if sanitized.len().saturating_add(wrapper_size) > MAX_PASTE_BYTES {
        return Err("paste rejected: clipboard text exceeds 64 KiB");
    }
    if bracketed {
        let mut bytes = Vec::with_capacity(sanitized.len() + wrapper_size);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(sanitized.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        Ok(bytes)
    } else {
        Ok(sanitized.into_bytes())
    }
}

fn visible_panes(layout: &PaneLayout) -> Vec<Uuid> {
    match layout {
        PaneLayout::Leaf { pane } => vec![pane.id],
        PaneLayout::Stack { active, .. } => vec![*active],
        PaneLayout::Split { first, second, .. } => {
            let mut panes = visible_panes(first);
            panes.extend(visible_panes(second));
            panes
        }
    }
}

fn split_target_for_drag(source: Uuid, panes: &[Pane], active: Uuid) -> Option<Uuid> {
    let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
    split_target_for_drag_ids(source, &pane_ids, active)
}

fn split_target_for_drag_ids(source: Uuid, pane_ids: &[Uuid], active: Uuid) -> Option<Uuid> {
    if source == active {
        pane_ids
            .iter()
            .copied()
            .find(|pane| *pane != source)
            .or_else(|| (pane_ids.len() == 1).then_some(active))
    } else {
        Some(active)
    }
}

fn split_placement_at(position: Point<Pixels>, bounds: Bounds<Pixels>) -> Option<DropPlacement> {
    if !bounds.contains(&position) {
        return None;
    }
    let x = f32::from(position.x - bounds.origin.x);
    let y = f32::from(position.y - bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if y < PANE_HEADER_HEIGHT || width <= 0.0 || height <= PANE_HEADER_HEIGHT {
        return None;
    }
    if x <= width * 0.25 {
        Some(DropPlacement::Left)
    } else if x >= width * 0.75 {
        Some(DropPlacement::Right)
    } else if y - PANE_HEADER_HEIGHT <= (height - PANE_HEADER_HEIGHT) * 0.5 {
        Some(DropPlacement::Top)
    } else {
        Some(DropPlacement::Bottom)
    }
}

fn history_label(label: &'static str) -> AnyElement {
    div()
        .w(px(76.0))
        .font_family(".SystemUIFont")
        .text_xs()
        .text_color(rgb(THEME.muted))
        .child(label)
        .into_any_element()
}

fn history_scope_key(scope: HistoryClearScope) -> usize {
    match scope {
        HistoryClearScope::Terminal { .. } => 0,
        HistoryClearScope::Workspace { .. } => 1,
        HistoryClearScope::All => 2,
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{}.{} GiB", bytes / GIB, (bytes % GIB) * 10 / GIB)
    } else {
        format!("{}.{} MiB", bytes / MIB, (bytes % MIB) * 10 / MIB)
    }
}

fn format_history_date(milliseconds: u64) -> String {
    let days = i64::try_from(milliseconds / 1_000 / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

fn history_warning_text(warning: Option<HistoryWarning>, dropped_bytes: u64) -> Option<String> {
    match warning {
        Some(HistoryWarning::ApproachingCapacity) => Some(
            "Archive is nearing its quota. Increase the limit or clear selected history before it fills."
                .to_owned(),
        ),
        Some(HistoryWarning::PausedAtCapacity) => Some(format!(
            "Archive is full and paused; the terminal is still live. {} could not be archived. Increase the quota or clear selected history.",
            format_bytes(dropped_bytes)
        )),
        Some(HistoryWarning::QueueOverflow) => Some(format!(
            "The storage queue could not keep up; {} is marked as an archive gap. Terminal input and output continued normally.",
            format_bytes(dropped_bytes)
        )),
        Some(HistoryWarning::CorruptChunk) => Some(
            "A local archive chunk failed integrity checks. It is shown as a gap; other chunks remain available."
                .to_owned(),
        ),
        None => None,
    }
}

fn plain_history_line(text: &str) -> TerminalLine {
    TerminalLine {
        runs: if text.is_empty() {
            Vec::new()
        } else {
            vec![TerminalRun {
                text: text.to_owned(),
                columns: text.chars().fold(0_u16, |columns, character| {
                    columns.saturating_add(
                        u16::try_from(character.width().unwrap_or(0)).unwrap_or(u16::MAX),
                    )
                }),
                foreground: TerminalColor::DefaultForeground,
                background: TerminalColor::DefaultBackground,
                attributes: TerminalAttributes::default(),
            }]
        },
    }
}

fn terminal_run_columns(run: &TerminalRun, start_column: u16) -> u16 {
    if run.columns == 0 {
        legacy_text_columns(&run.text, start_column)
    } else {
        run.columns
    }
}

fn legacy_text_columns(text: &str, start_column: u16) -> u16 {
    const TAB_WIDTH: u16 = 8;
    let mut column = start_column;
    for character in text.chars() {
        if character == '\t' {
            let remainder = column % TAB_WIDTH;
            column = column.saturating_add(TAB_WIDTH - remainder);
        } else {
            let width = u16::try_from(character.width().unwrap_or(0)).unwrap_or(u16::MAX);
            column = column.saturating_add(width);
        }
    }
    column.saturating_sub(start_column)
}

fn expand_terminal_tabs(text: &str, start_column: u16) -> String {
    const TAB_WIDTH: u16 = 8;
    let mut column = start_column;
    let mut expanded = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\t' {
            let spaces = TAB_WIDTH - (column % TAB_WIDTH);
            expanded.extend(std::iter::repeat_n(' ', usize::from(spaces)));
            column = column.saturating_add(spaces);
        } else {
            expanded.push(character);
            let width = u16::try_from(character.width().unwrap_or(0)).unwrap_or(u16::MAX);
            column = column.saturating_add(width);
        }
    }
    expanded
}

fn terminal_run_display_text(run: &TerminalRun, start_column: u16) -> String {
    if run.columns == 0 {
        expand_terminal_tabs(&run.text, start_column)
    } else {
        // The terminal model already represents every occupied grid cell,
        // including the cells skipped by a tab. Render its tab cell as one
        // blank cell instead of asking GPUI to apply proportional tab stops.
        run.text.replace('\t', " ")
    }
}

fn find_pane(layout: &PaneLayout, pane_id: Uuid) -> Option<&Pane> {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => Some(pane),
        PaneLayout::Leaf { .. } => None,
        PaneLayout::Stack { panes, .. } => panes.iter().find(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            find_pane(first, pane_id).or_else(|| find_pane(second, pane_id))
        }
    }
}

fn collect_terminal_tabs<'a>(layout: &'a PaneLayout, panes: &mut Vec<&'a Pane>) {
    match layout {
        PaneLayout::Leaf { pane } => panes.push(pane),
        PaneLayout::Stack { panes: stacked, .. } => panes.extend(stacked),
        PaneLayout::Split { first, second, .. } => {
            collect_terminal_tabs(first, panes);
            collect_terminal_tabs(second, panes);
        }
    }
}

fn workspace_terminal_tabs(workspace: &Workspace) -> Vec<&Pane> {
    let mut panes = Vec::new();
    for tab in &workspace.tabs {
        collect_terminal_tabs(&tab.layout, &mut panes);
    }
    panes
}

fn terminal_tab_count_label(count: usize) -> String {
    format!("{count} terminal tab{}", if count == 1 { "" } else { "s" })
}

fn tab_identity_presentation(pane: &Pane) -> TabIdentityPresentation {
    let detection_detail = match pane.identity.source {
        nah_protocol::TerminalIdentitySource::UserRename => "Custom terminal name",
        nah_protocol::TerminalIdentitySource::UserProfile => "User-selected local profile",
        nah_protocol::TerminalIdentitySource::TerminalTitle => {
            "Detected from a bounded terminal-title signal; terminal content is not inspected"
        }
        nah_protocol::TerminalIdentitySource::Command => {
            "Detected from a bounded local child-process name; terminal content is not inspected"
        }
        nah_protocol::TerminalIdentitySource::Fallback => "Ordinary terminal",
    };
    let definition = agent_icon_definition(pane.identity.profile);
    let asset_detail = if definition.asset.is_some() {
        "Bundled official artwork is shown unchanged for referential identification only; no affiliation or endorsement is implied"
    } else if pane.identity.profile == TerminalProfile::GitHubCopilot {
        "The official CLI package exposes no standalone icon asset, so Not a Harness uses the neutral terminal glyph"
    } else {
        "Not a Harness uses the neutral terminal glyph"
    };
    TabIdentityPresentation {
        label: pane.title.clone(),
        profile: pane.identity.profile,
        detail: format!(
            "{} — {detection_detail}. {asset_detail}.",
            definition.accessible_name
        ),
    }
}

const IDENTITY_MARK_SIZE: f32 = 22.0;
const OFFICIAL_IDENTITY_ICON_SIZE: f32 = 20.0;
const FALLBACK_IDENTITY_ICON_SIZE: f32 = 14.0;
const FALLBACK_IDENTITY_FRAME_SIZE: f32 = 18.0;

fn terminal_profile_icon_is_framed(profile: TerminalProfile) -> bool {
    agent_icon_definition(profile).asset.is_none()
}

fn terminal_profile_icon_size(profile: TerminalProfile) -> f32 {
    if terminal_profile_icon_is_framed(profile) {
        FALLBACK_IDENTITY_ICON_SIZE
    } else {
        OFFICIAL_IDENTITY_ICON_SIZE
    }
}

fn render_terminal_profile_mark(
    profile: TerminalProfile,
    fallback_color: u32,
    fallback_border_color: u32,
) -> AnyElement {
    let icon =
        render_terminal_profile_icon(profile, fallback_color, terminal_profile_icon_size(profile));
    if terminal_profile_icon_is_framed(profile) {
        div()
            .w(px(FALLBACK_IDENTITY_FRAME_SIZE))
            .h(px(FALLBACK_IDENTITY_FRAME_SIZE))
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(fallback_border_color))
            .flex()
            .items_center()
            .justify_center()
            .child(icon)
            .into_any_element()
    } else {
        icon
    }
}

fn render_sidebar_toggle_icon(sidebar_visible: bool) -> AnyElement {
    div()
        .relative()
        .w(px(15.0))
        .h(px(13.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(rgb(THEME.muted))
        .child(
            div()
                .absolute()
                .left(px(4.0))
                .top(px(0.0))
                .w(px(1.0))
                .h_full()
                .bg(rgb(THEME.muted)),
        )
        .child(
            div()
                .absolute()
                .left(px(if sidebar_visible { 1.5 } else { 7.0 }))
                .top(px(3.0))
                .w(px(if sidebar_visible { 1.5 } else { 4.0 }))
                .h(px(5.0))
                .rounded(px(1.0))
                .bg(rgb(THEME.muted)),
        )
        .into_any_element()
}

fn render_terminal_profile_icon(
    profile: TerminalProfile,
    fallback_color: u32,
    icon_size: f32,
) -> AnyElement {
    let definition = agent_icon_definition(profile);
    let icon = match definition.asset {
        Some(asset) if asset.format == AgentIconFormat::Svg => svg()
            .path(asset.path)
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(rgb(fallback_color))
            .into_any_element(),
        Some(asset) => img(asset.path)
            .w(px(icon_size))
            .h(px(icon_size))
            .object_fit(gpui::ObjectFit::Contain)
            .into_any_element(),
        None => div()
            .font_family("SF Mono")
            .text_size(px(7.5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(rgb(fallback_color))
            .child(profile.fallback_glyph())
            .into_any_element(),
    };
    div()
        .w(px(icon_size))
        .h(px(icon_size))
        .flex()
        .items_center()
        .justify_center()
        .child(icon)
        .into_any_element()
}

fn resolved_terminal_accent(snapshot: &SessionSnapshot, pane_id: Uuid) -> AppearanceColor {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find_map(|tab| find_pane(&tab.layout, pane_id))
        .and_then(|pane| pane.color)
        .unwrap_or(snapshot.appearance.default_terminal_accent)
}

fn resolved_workspace_color(snapshot: &SessionSnapshot, workspace_id: Uuid) -> AppearanceColor {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .and_then(|workspace| workspace.color)
        .unwrap_or(snapshot.appearance.default_workspace_color)
}

fn workspace_is_selectable(workspace: &Workspace) -> bool {
    workspace.tabs.is_empty()
        || !matches!(
            workspace.connection,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            }
        )
}

fn stable_representative_pane(layout: &PaneLayout) -> Uuid {
    match layout {
        PaneLayout::Leaf { pane } => pane.id,
        PaneLayout::Stack { panes, active } => panes.first().map_or(*active, |pane| pane.id),
        PaneLayout::Split { first, .. } => stable_representative_pane(first),
    }
}

fn split_control_id(first: &PaneLayout, second: &PaneLayout) -> SplitControlId {
    SplitControlId {
        first: stable_representative_pane(first),
        second: stable_representative_pane(second),
    }
}

fn zoom_projection(layout: &PaneLayout, pane_id: Uuid) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf { pane } => (pane.id == pane_id).then(|| layout.clone()),
        PaneLayout::Stack { panes, .. } => panes
            .iter()
            .any(|pane| pane.id == pane_id)
            .then(|| layout.clone()),
        PaneLayout::Split { first, second, .. } => {
            zoom_projection(first, pane_id).or_else(|| zoom_projection(second, pane_id))
        }
    }
}

fn apply_layout_control_mutation(
    layout: &PaneLayout,
    ratios: &mut HashMap<SplitControlId, f32>,
    mutation: LayoutControlMutation,
) -> usize {
    match layout {
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => 0,
        PaneLayout::Split { first, second, .. } => {
            match mutation {
                LayoutControlMutation::Equalize => {
                    ratios.insert(split_control_id(first, second), 0.5);
                }
            }
            1 + apply_layout_control_mutation(first, ratios, mutation)
                + apply_layout_control_mutation(second, ratios, mutation)
        }
    }
}

fn constrained_sidebar_width(preferred_width: f32, window_width: f32) -> f32 {
    let preferred_width = if preferred_width.is_finite() {
        preferred_width
    } else {
        DEFAULT_SIDEBAR_WIDTH
    };
    let maximum_for_window =
        (window_width - MIN_TERMINAL_AREA_WIDTH).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    preferred_width.clamp(MIN_SIDEBAR_WIDTH, maximum_for_window)
}

fn sidebar_width_for_visibility(preferred_width: f32, window_width: f32, visible: bool) -> f32 {
    if visible {
        constrained_sidebar_width(preferred_width, window_width)
    } else {
        0.0
    }
}

fn workspace_pixel_size(window_width: f32, window_height: f32, sidebar_width: f32) -> (f32, f32) {
    (
        (window_width - sidebar_width).max(1.0),
        (window_height - TITLEBAR_HEIGHT).max(1.0),
    )
}

fn readable_text_color(background: u32) -> u32 {
    let red = (background >> 16) & 0xff;
    let green = (background >> 8) & 0xff;
    let blue = background & 0xff;
    if red * 299 + green * 587 + blue * 114 > 150_000 {
        0x111318
    } else {
        0xffffff
    }
}

fn parse_hex_color(value: &str) -> Option<AppearanceColor> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = u32::from_str_radix(value, 16).ok()?;
    Some(AppearanceColor::new(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    ))
}

fn effective_split_ratio(axis: SplitAxis, width: f32, height: f32, ratio: f32) -> f32 {
    let extent = match axis {
        SplitAxis::Horizontal => width,
        SplitAxis::Vertical => height,
    }
    .max(1.0);
    let minimum = match axis {
        SplitAxis::Horizontal => MIN_PANE_WIDTH,
        SplitAxis::Vertical => MIN_PANE_HEIGHT,
    };
    if extent < minimum * 2.0 + SPLIT_DIVIDER_SIZE {
        return 0.5;
    }
    let low = minimum / extent;
    let high = (extent - SPLIT_DIVIDER_SIZE - minimum) / extent;
    ratio.clamp(low, high)
}

fn split_child_dimensions(
    axis: SplitAxis,
    width: f32,
    height: f32,
    ratio: f32,
) -> (f32, f32, f32, f32) {
    match axis {
        SplitAxis::Horizontal => {
            let first_width = (width * ratio).floor().max(1.0);
            let second_width = (width - first_width - SPLIT_DIVIDER_SIZE).max(1.0);
            (first_width, height, second_width, height)
        }
        SplitAxis::Vertical => {
            let first_height = (height * ratio).floor().max(1.0);
            let second_height = (height - first_height - SPLIT_DIVIDER_SIZE).max(1.0);
            (width, first_height, width, second_height)
        }
    }
}

fn find_split_rect(
    layout: &PaneLayout,
    target_split_id: SplitControlId,
    rect: PixelRect,
    ratios: &HashMap<SplitControlId, f32>,
) -> Option<PixelRect> {
    let PaneLayout::Split {
        axis,
        ratio,
        first,
        second,
    } = layout
    else {
        return None;
    };
    let split_id = split_control_id(first, second);
    if split_id == target_split_id {
        return Some(rect);
    }
    let ratio = effective_split_ratio(
        *axis,
        rect.width,
        rect.height,
        ratios.get(&split_id).copied().unwrap_or(*ratio),
    );
    let (first_width, first_height, second_width, second_height) =
        split_child_dimensions(*axis, rect.width, rect.height, ratio);
    let first_rect = PixelRect {
        width: first_width,
        height: first_height,
        ..rect
    };
    let second_rect = match axis {
        SplitAxis::Horizontal => PixelRect {
            x: rect.x + first_width + SPLIT_DIVIDER_SIZE,
            y: rect.y,
            width: second_width,
            height: second_height,
        },
        SplitAxis::Vertical => PixelRect {
            x: rect.x,
            y: rect.y + first_height + SPLIT_DIVIDER_SIZE,
            width: second_width,
            height: second_height,
        },
    };
    find_split_rect(first, target_split_id, first_rect, ratios)
        .or_else(|| find_split_rect(second, target_split_id, second_rect, ratios))
}

fn collect_pane_sizes(
    layout: &PaneLayout,
    width: f32,
    height: f32,
    metrics: typography::TerminalCellMetrics,
    ratios: &HashMap<SplitControlId, f32>,
    output: &mut Vec<(Uuid, u16, u16)>,
) {
    match layout {
        PaneLayout::Leaf { pane } => {
            let (columns, rows) = terminal_grid_for_pane(width, height, metrics);
            output.push((pane.id, columns, rows));
        }
        PaneLayout::Stack { active, .. } => {
            let (columns, rows) = terminal_grid_for_pane(width, height, metrics);
            output.push((*active, columns, rows));
        }
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = effective_split_ratio(
                *axis,
                width,
                height,
                ratios
                    .get(&split_control_id(first, second))
                    .copied()
                    .unwrap_or(*ratio),
            );
            let (first_width, first_height, second_width, second_height) =
                split_child_dimensions(*axis, width, height, ratio);
            collect_pane_sizes(first, first_width, first_height, metrics, ratios, output);
            collect_pane_sizes(second, second_width, second_height, metrics, ratios, output);
        }
    }
}

fn terminal_input_bytes(
    key: &str,
    key_char: Option<&str>,
    control: bool,
    alt: bool,
    platform: bool,
) -> Option<Vec<u8>> {
    // Command/Super is an application modifier, not a PTY modifier. Unmatched
    // platform shortcuts remain available to the OS instead of becoming text.
    if platform {
        return None;
    }
    if control && key.len() == 1 {
        return key
            .as_bytes()
            .first()
            .map(|byte| vec![byte.to_ascii_lowercase() & 0x1f]);
    }
    let mut bytes = match key {
        "enter" => vec![b'\r'],
        "backspace" => vec![0x7f],
        "tab" => vec![b'\t'],
        "escape" => vec![0x1b],
        "left" => b"\x1b[D".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        _ => key_char?.as_bytes().to_vec(),
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn terminal_grid_for_pane(
    pane_width: f32,
    pane_height: f32,
    metrics: typography::TerminalCellMetrics,
) -> (u16, u16) {
    let content_width =
        (pane_width - TERMINAL_HORIZONTAL_PADDING - TERMINAL_FOCUS_BORDER_WIDTH).max(1.0);
    let content_height = (pane_height - PANE_HEADER_HEIGHT - TERMINAL_VERTICAL_PADDING).max(1.0);
    (
        metrics.columns_for_width(content_width),
        metrics.rows_for_height(content_height),
    )
}

fn element_key(id: Uuid) -> u64 {
    let (high, low) = id.as_u64_pair();
    high ^ low
}

fn split_element_key(id: SplitControlId) -> u64 {
    element_key(id.first).rotate_left(17) ^ element_key(id.second)
}

fn gpui_binding(binding: &ResolvedBinding) -> KeyBinding {
    match binding.command {
        AppCommand::NewWorkspace => {
            KeyBinding::new(&binding.sequence, NewWorkspace, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ToggleSidebar => {
            KeyBinding::new(&binding.sequence, ToggleSidebar, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::NewTab => KeyBinding::new(&binding.sequence, NewTab, Some(ROOT_KEY_CONTEXT)),
        AppCommand::SplitRight => {
            KeyBinding::new(&binding.sequence, SplitRight, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::SplitDown => {
            KeyBinding::new(&binding.sequence, SplitDown, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::FocusLeft => {
            KeyBinding::new(&binding.sequence, FocusLeft, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::FocusRight => {
            KeyBinding::new(&binding.sequence, FocusRight, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::FocusUp => KeyBinding::new(&binding.sequence, FocusUp, Some(ROOT_KEY_CONTEXT)),
        AppCommand::FocusDown => {
            KeyBinding::new(&binding.sequence, FocusDown, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ShowCommandPalette => KeyBinding::new(
            &binding.sequence,
            ShowCommandPalette,
            Some(ROOT_KEY_CONTEXT),
        ),
        AppCommand::TogglePaneZoom => {
            KeyBinding::new(&binding.sequence, TogglePaneZoom, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::EqualizePanes => {
            KeyBinding::new(&binding.sequence, EqualizePanes, Some(ROOT_KEY_CONTEXT))
        }
    }
}

/// Starts the bundled session service only when no compatible local service is
/// reachable. The service is deliberately detached from the desktop lifetime:
/// closing or replacing the app UI never asks it to stop, preserving active
/// terminal sessions. A future updater must instead defer until the service is
/// explicitly quiescent (see `docs/macos-release.md`).
fn ensure_bundled_session_service() {
    if std::env::var_os("NAH_DISABLE_BUNDLED_SERVICE").is_some()
        || request(ClientRequest::GetSnapshot).is_ok()
    {
        return;
    }

    let Some(service) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join("nah-service")))
    else {
        return;
    };
    if !service.is_file() {
        return;
    }
    if let Err(error) = Command::new(service).spawn() {
        eprintln!("Not a Harness could not start its bundled session service: {error}");
        return;
    }

    for _ in 0..20 {
        thread::sleep(Duration::from_millis(50));
        if request(ClientRequest::GetSnapshot).is_ok() {
            return;
        }
    }
    eprintln!("Not a Harness session service did not become ready within one second");
}

/// Sets the live Dock icon explicitly. `AppKit` otherwise retains the generic
/// placeholder selected while a development bundle is being rebuilt in place.
#[cfg(target_os = "macos")]
fn install_macos_dock_icon() {
    nah_macos_icon::install_dock_icon();
}

#[cfg(not(target_os = "macos"))]
fn install_macos_dock_icon() {}

fn main() {
    ensure_bundled_session_service();
    Application::new()
        .with_assets(AgentIconAssets)
        .run(|cx: &mut App| {
            install_macos_dock_icon();
            let keymap = match AppConfig::load().and_then(|config| config.resolve_keymap()) {
                Ok(keymap) => keymap,
                Err(error) => {
                    eprintln!("Not a Harness config ignored: {error}");
                    AppConfig::default()
                        .resolve_keymap()
                        .expect("built-in keymap must be valid")
                }
            };
            let mut bindings = keymap.bindings.iter().map(gpui_binding).collect::<Vec<_>>();
            bindings.extend(
                keymap.chord_prefixes.iter().map(|prefix| {
                    KeyBinding::new(prefix, ConsumeChordPrefix, Some(ROOT_KEY_CONTEXT))
                }),
            );
            bindings.extend([
                KeyBinding::new("cmd-c", CopyTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("cmd-v", PasteTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("cmd-f", FindTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("cmd-g", FindNextTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("ctrl-shift-c", CopyTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("ctrl-shift-v", PasteTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("ctrl-shift-f", FindTerminal, Some(ROOT_KEY_CONTEXT)),
            ]);
            cx.bind_keys(bindings);
            let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitlebarOptions {
                        title: Some("Not a Harness".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(13.0), px(13.0))),
                    }),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(720.0), px(460.0))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| NahApp::new(window, keymap.clone(), cx)),
            )
            .expect("open Not a Harness window");
            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_tab_close_requires_an_explicit_confirmation_for_the_exact_terminal() {
        let pane = Pane {
            id: Uuid::new_v4(),
            title: "build".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: Some("build".to_owned()),
            profile_override: None,
        };

        let confirmation = CloseConfirmation::for_pane(&pane, true);

        assert_eq!(confirmation.pane_id, pane.id);
        assert_eq!(confirmation.title, "build");
        assert!(confirmation.leaves_workspace_empty);
        assert_eq!(
            confirmation.request(),
            ClientRequest::ClosePane { pane_id: pane.id }
        );
    }

    #[test]
    fn empty_offline_ssh_workspace_is_selectable_only_for_its_reopen_affordance() {
        let mut snapshot = SessionSnapshot::seeded();
        let workspace = &mut snapshot.workspaces[0];
        workspace.connection = WorkspaceConnection::SystemSsh {
            destination: "tailnet-host".to_owned(),
            status: WorkspaceConnectionStatus::Offline,
        };
        assert!(!workspace_is_selectable(workspace));

        workspace.tabs.clear();

        assert!(workspace_is_selectable(workspace));
    }

    #[test]
    fn native_tab_identity_label_and_icon_registry_smoke_test() {
        let cases = [
            (TerminalProfile::Terminal, false),
            (TerminalProfile::Hermes, true),
            (TerminalProfile::Codex, true),
            (TerminalProfile::Claude, true),
            (TerminalProfile::Droid, true),
            (TerminalProfile::KiloCode, true),
            (TerminalProfile::Cursor, true),
            (TerminalProfile::OpenCode, true),
            (TerminalProfile::Aider, true),
            (TerminalProfile::GitHubCopilot, false),
            (TerminalProfile::Gemini, true),
        ];
        for (profile, has_official_asset) in cases {
            let label = profile.display_name();
            let pane = Pane {
                id: Uuid::new_v4(),
                title: label.to_owned(),
                shell: "zsh".to_owned(),
                color: None,
                identity: nah_protocol::TerminalIdentity {
                    profile,
                    source: if profile == TerminalProfile::Terminal {
                        nah_protocol::TerminalIdentitySource::Fallback
                    } else {
                        nah_protocol::TerminalIdentitySource::Command
                    },
                },
                custom_title: None,
                profile_override: None,
            };

            let presentation = tab_identity_presentation(&pane);
            assert_eq!(presentation.label, label);
            assert_eq!(presentation.profile, profile);
            assert!(presentation.detail.contains(label));
            assert_eq!(
                agent_icon_definition(profile).asset.is_some(),
                has_official_asset
            );
            assert_eq!(
                terminal_profile_icon_is_framed(profile),
                !has_official_asset
            );
            let expected_size = if has_official_asset {
                OFFICIAL_IDENTITY_ICON_SIZE
            } else {
                FALLBACK_IDENTITY_ICON_SIZE
            };
            assert!((terminal_profile_icon_size(profile) - expected_size).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn workspace_rail_lists_every_terminal_tab_across_stacks_and_splits() {
        let make_pane = |id: u128, title: &str, profile: TerminalProfile| Pane {
            id: Uuid::from_u128(id),
            title: title.to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity {
                profile,
                source: nah_protocol::TerminalIdentitySource::Command,
            },
            custom_title: None,
            profile_override: None,
        };
        let codex = make_pane(1, "Codex review", TerminalProfile::Codex);
        let droid = make_pane(2, "Droid build", TerminalProfile::Droid);
        let terminal = make_pane(3, "Logs", TerminalProfile::Terminal);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs[0].layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Stack {
                panes: vec![codex.clone(), droid.clone()],
                active: droid.id,
            }),
            second: Box::new(PaneLayout::Leaf {
                pane: terminal.clone(),
            }),
        };

        let tabs = workspace_terminal_tabs(&workspace);

        assert_eq!(
            tabs.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            vec![codex.id, droid.id, terminal.id]
        );
        assert_eq!(
            tabs.iter()
                .map(|pane| tab_identity_presentation(pane).profile)
                .collect::<Vec<_>>(),
            vec![
                TerminalProfile::Codex,
                TerminalProfile::Droid,
                TerminalProfile::Terminal
            ]
        );
        assert_eq!(terminal_tab_count_label(tabs.len()), "3 terminal tabs");
    }

    #[test]
    fn workspace_rail_empty_state_and_tab_count_labels_are_explicit() {
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs.clear();

        assert!(workspace_terminal_tabs(&workspace).is_empty());
        assert_eq!(terminal_tab_count_label(0), "0 terminal tabs");
        assert_eq!(terminal_tab_count_label(1), "1 terminal tab");
    }

    #[test]
    fn appearance_color_precedence_keeps_terminal_and_workspace_scopes_independent() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = visible_panes(&snapshot.workspaces[0].tabs[0].layout)[0];
        let workspace_id = snapshot.workspaces[0].id;
        let terminal_default = AppearanceColor::new(0x95, 0xcc, 0x7f);
        let workspace_default = AppearanceColor::new(0xc9, 0x90, 0xe5);
        let terminal_override = AppearanceColor::new(0xef, 0x71, 0x7a);
        let workspace_override = AppearanceColor::new(0xe4, 0xbd, 0x72);
        snapshot.appearance.default_terminal_accent = terminal_default;
        snapshot.appearance.default_workspace_color = workspace_default;

        assert_eq!(
            resolved_terminal_accent(&snapshot, pane_id),
            terminal_default
        );
        assert_eq!(
            resolved_workspace_color(&snapshot, workspace_id),
            workspace_default
        );

        let PaneLayout::Leaf { pane } = &mut snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected leaf");
        };
        pane.color = Some(terminal_override);
        snapshot.workspaces[0].color = Some(workspace_override);

        assert_eq!(
            resolved_terminal_accent(&snapshot, pane_id),
            terminal_override
        );
        assert_eq!(
            resolved_workspace_color(&snapshot, workspace_id),
            workspace_override
        );
        snapshot.workspaces[0].color = None;
        assert_eq!(
            resolved_terminal_accent(&snapshot, pane_id),
            terminal_override
        );
        assert_eq!(
            resolved_workspace_color(&snapshot, workspace_id),
            workspace_default
        );
    }

    #[test]
    fn color_picker_accepts_exact_hex_and_rejects_partial_or_non_hex_input() {
        assert_eq!(
            parse_hex_color("#67C8C6"),
            Some(AppearanceColor::new(0x67, 0xc8, 0xc6))
        );
        assert_eq!(
            parse_hex_color("62adff"),
            Some(AppearanceColor::HARBOR_BLUE)
        );
        assert_eq!(parse_hex_color("FFF"), None);
        assert_eq!(parse_hex_color("GGADFF"), None);
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
    fn pointer_local_split_zones_exclude_the_tab_strip_and_cover_each_half() {
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(100.0)),
        };

        assert_eq!(split_placement_at(point(px(50.0), px(10.0)), bounds), None);
        assert_eq!(
            split_placement_at(point(px(10.0), px(50.0)), bounds),
            Some(DropPlacement::Left)
        );
        assert_eq!(
            split_placement_at(point(px(90.0), px(50.0)), bounds),
            Some(DropPlacement::Right)
        );
        assert_eq!(
            split_placement_at(point(px(50.0), px(40.0)), bounds),
            Some(DropPlacement::Top)
        );
        assert_eq!(
            split_placement_at(point(px(50.0), px(90.0)), bounds),
            Some(DropPlacement::Bottom)
        );
        assert_eq!(split_placement_at(point(px(101.0), px(50.0)), bounds), None);
    }

    #[test]
    fn multi_column_terminal_rows_keep_spaces_and_wide_cells_on_one_grid() {
        let listing = TerminalRun {
            text: "Applications                         Music                 Work".to_owned(),
            columns: 0,
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            attributes: TerminalAttributes::default(),
        };
        assert_eq!(
            terminal_run_columns(&listing, 0),
            u16::try_from(listing.text.chars().count()).unwrap()
        );

        let wide = TerminalRun {
            text: "A界B".to_owned(),
            columns: 4,
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            attributes: TerminalAttributes::default(),
        };
        assert_eq!(terminal_run_columns(&wide, 0), 4);

        let combining = TerminalRun {
            text: "e\u{301}".to_owned(),
            columns: 1,
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            attributes: TerminalAttributes::default(),
        };
        assert_eq!(terminal_run_columns(&combining, 0), 1);

        let tabbed = TerminalRun {
            text: "Applications\tMusic\tWork".to_owned(),
            columns: 0,
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            attributes: TerminalAttributes::default(),
        };
        assert_eq!(terminal_run_columns(&tabbed, 0), 28);
        assert_eq!(legacy_text_columns("\tWork", 32), 12);
        assert_eq!(
            expand_terminal_tabs(&tabbed.text, 0),
            "Applications    Music   Work"
        );
        assert!(!expand_terminal_tabs(&tabbed.text, 0).contains('\t'));

        let modeled_cells = TerminalRun {
            text: "A\t  B".to_owned(),
            columns: 5,
            ..tabbed
        };
        assert_eq!(terminal_run_display_text(&modeled_cells, 0), "A   B");
        assert_eq!(terminal_run_columns(&modeled_cells, 0), 5);
    }

    #[test]
    fn one_row_hit_surface_maps_pointer_positions_to_terminal_cells() {
        let bounds = Bounds {
            origin: point(px(100.0), px(40.0)),
            size: size(px(80.0), px(18.0)),
        };

        assert_eq!(
            terminal_point_at(point(px(100.0), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 0 }
        );
        assert_eq!(
            terminal_point_at(point(px(139.9), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 4 }
        );
        assert_eq!(
            terminal_point_at(point(px(190.0), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 9 }
        );
    }

    #[test]
    fn terminal_polling_is_fast_while_output_changes_and_backs_off_when_idle() {
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, true),
            ACTIVE_TERMINAL_POLL_MS
        );
        assert_eq!(
            next_terminal_poll_delay_ms(ACTIVE_TERMINAL_POLL_MS, false),
            ACTIVE_TERMINAL_POLL_MS * 2
        );
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, false),
            IDLE_TERMINAL_POLL_MS
        );
    }

    #[test]
    fn attention_policy_keeps_focused_and_recent_panes_but_cools_old_inactive_panes() {
        let now = Instant::now();
        let focused = Uuid::from_u128(1);
        let recent = Uuid::from_u128(2);
        let cold = Uuid::from_u128(3);
        let attention = HashMap::from([
            (focused, now.checked_sub(Duration::from_mins(2)).unwrap()),
            (recent, now.checked_sub(Duration::from_secs(59)).unwrap()),
            (cold, now.checked_sub(Duration::from_secs(61)).unwrap()),
        ]);

        assert_eq!(
            responsive_panes(now, Some(focused), &attention),
            vec![focused, recent]
        );
    }

    #[test]
    fn revision_metadata_alone_does_not_repaint_inactive_panes() {
        assert!(!pane_update_requires_repaint(false, 0));
        assert!(pane_update_requires_repaint(false, 1));
        assert!(pane_update_requires_repaint(true, 0));
    }

    #[test]
    fn pane_geometry_tracks_narrow_medium_and_wide_windows_without_fixed_columns() {
        let pane = Pane {
            id: Uuid::from_u128(10),
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
        };
        let layout = PaneLayout::Leaf { pane };
        let metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let ratios = HashMap::new();

        let dimensions = [(720.0, 460.0), (1280.0, 820.0), (1800.0, 1000.0)]
            .into_iter()
            .map(|(window_width, window_height)| {
                let workspace =
                    workspace_pixel_size(window_width, window_height, DEFAULT_SIDEBAR_WIDTH);
                let mut sizes = Vec::new();
                collect_pane_sizes(
                    &layout,
                    workspace.0,
                    workspace.1,
                    metrics,
                    &ratios,
                    &mut sizes,
                );
                sizes[0]
            })
            .collect::<Vec<_>>();

        assert_eq!(dimensions[0], (Uuid::from_u128(10), 63, 20));
        assert_eq!(dimensions[1], (Uuid::from_u128(10), 133, 39));
        assert_eq!(dimensions[2], (Uuid::from_u128(10), 198, 48));
        assert!(
            dimensions
                .windows(2)
                .all(|pair| { pair[0].1 < pair[1].1 && pair[0].2 < pair[1].2 })
        );
    }

    #[test]
    fn sidebar_width_is_bounded_without_forgetting_a_wider_preference() {
        assert!((constrained_sidebar_width(80.0, 1280.0) - 150.0).abs() < 0.0001);
        assert!((constrained_sidebar_width(900.0, 1280.0) - 420.0).abs() < 0.0001);

        let preferred = 390.0;
        let compact = constrained_sidebar_width(preferred, 640.0);
        assert!((compact - 320.0).abs() < 0.0001);
        assert!((workspace_pixel_size(640.0, 460.0, compact).0 - 320.0).abs() < 0.0001);
        assert!((constrained_sidebar_width(preferred, 1280.0) - preferred).abs() < 0.0001);
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

    #[test]
    fn hidden_sidebar_gives_the_workspace_the_full_window_width() {
        let visible = sidebar_width_for_visibility(260.0, 1280.0, true);
        let hidden = sidebar_width_for_visibility(260.0, 1280.0, false);

        assert!((visible - 260.0).abs() < f32::EPSILON);
        assert!(hidden.abs() < f32::EPSILON);
        assert!((workspace_pixel_size(1280.0, 820.0, hidden).0 - 1280.0).abs() < f32::EPSILON);
    }

    #[test]
    fn widest_sidebar_still_leaves_two_constrained_split_panes_in_the_minimum_window() {
        let sidebar = constrained_sidebar_width(MAX_SIDEBAR_WIDTH, 720.0);
        let workspace = workspace_pixel_size(720.0, 460.0, sidebar);
        let ratio = effective_split_ratio(SplitAxis::Horizontal, workspace.0, workspace.1, 0.95);
        let (first_width, _, second_width, _) =
            split_child_dimensions(SplitAxis::Horizontal, workspace.0, workspace.1, ratio);

        assert!((sidebar - 400.0).abs() < 0.0001);
        assert!((workspace.0 - MIN_TERMINAL_AREA_WIDTH).abs() < 0.0001);
        assert!(first_width >= MIN_PANE_WIDTH);
        assert!(second_width >= MIN_PANE_WIDTH);
    }

    #[test]
    fn split_geometry_accounts_for_the_divider_and_each_panes_chrome() {
        let first = Pane {
            id: Uuid::from_u128(21),
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
        };
        let second = Pane {
            id: Uuid::from_u128(22),
            title: "Terminal 2".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf {
                pane: first.clone(),
            }),
            second: Box::new(PaneLayout::Leaf { pane: second }),
        };
        let metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let workspace = workspace_pixel_size(1280.0, 820.0, DEFAULT_SIDEBAR_WIDTH);
        let mut sizes = Vec::new();
        collect_pane_sizes(
            &layout,
            workspace.0,
            workspace.1,
            metrics,
            &HashMap::new(),
            &mut sizes,
        );

        assert_eq!(
            sizes,
            vec![(first.id, 65, 39), (Uuid::from_u128(22), 65, 39)]
        );
        let used_pixel_width = 545.0 + SPLIT_DIVIDER_SIZE + 541.0;
        assert!((used_pixel_width - workspace.0).abs() < 0.0001);
    }

    #[test]
    fn split_ratio_respects_practical_pane_constraints_at_each_window_size() {
        let narrow = effective_split_ratio(SplitAxis::Horizontal, 530.0, 422.0, 0.05);
        let wide = effective_split_ratio(SplitAxis::Horizontal, 1610.0, 962.0, 0.05);
        assert!((narrow - (MIN_PANE_WIDTH / 530.0)).abs() < 0.0001);
        assert!((wide - (MIN_PANE_WIDTH / 1610.0)).abs() < 0.0001);

        let too_short = effective_split_ratio(SplitAxis::Vertical, 530.0, 150.0, 0.9);
        assert!((too_short - 0.5).abs() < 0.0001);
    }

    #[test]
    fn terminal_input_encodes_unmatched_keys_once_with_control_and_alt_semantics() {
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, false),
            Some(vec![b'x'])
        );
        assert_eq!(
            terminal_input_bytes("c", Some("c"), true, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, true, false),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            terminal_input_bytes("up", None, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, true),
            None
        );
    }

    #[test]
    fn bracketed_paste_normalizes_newlines_and_cannot_inject_an_early_end_marker() {
        let bytes = prepare_paste("one\n\x1b[201~two\r\n", true).unwrap();
        assert_eq!(bytes, b"\x1b[200~one\r[201~two\r\x1b[201~");
        assert_eq!(
            bytes
                .windows(b"\x1b[201~".len())
                .filter(|window| *window == b"\x1b[201~")
                .count(),
            1
        );
    }

    #[test]
    fn zoom_is_a_projection_that_does_not_mutate_canonical_layout() {
        let first = Pane {
            id: Uuid::from_u128(101),
            title: "one".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
        };
        let second = Pane {
            id: Uuid::from_u128(102),
            title: "two".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.3,
            first: Box::new(PaneLayout::Leaf {
                pane: first.clone(),
            }),
            second: Box::new(PaneLayout::Stack {
                panes: vec![second.clone()],
                active: second.id,
            }),
        };
        let before = layout.clone();

        assert_eq!(
            zoom_projection(&layout, second.id),
            Some(PaneLayout::Stack {
                panes: vec![second.clone()],
                active: second.id
            })
        );
        assert_eq!(layout, before);
        assert_eq!(zoom_projection(&layout, Uuid::from_u128(999)), None);
    }

    #[test]
    fn equalize_is_a_controlled_mutation_over_all_current_split_identities() {
        let pane = |id| Pane {
            id: Uuid::from_u128(id),
            title: format!("pane {id}"),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
        };
        let nested = PaneLayout::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.8,
            first: Box::new(PaneLayout::Leaf { pane: pane(2) }),
            second: Box::new(PaneLayout::Leaf { pane: pane(3) }),
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.2,
            first: Box::new(PaneLayout::Leaf { pane: pane(1) }),
            second: Box::new(nested),
        };
        let mut ratios = HashMap::from([
            (
                SplitControlId {
                    first: Uuid::from_u128(1),
                    second: Uuid::from_u128(2),
                },
                0.1,
            ),
            (
                SplitControlId {
                    first: Uuid::from_u128(2),
                    second: Uuid::from_u128(3),
                },
                0.9,
            ),
        ]);

        let changed =
            apply_layout_control_mutation(&layout, &mut ratios, LayoutControlMutation::Equalize);

        assert_eq!(changed, 2);
        assert!(
            (ratios[&SplitControlId {
                first: Uuid::from_u128(1),
                second: Uuid::from_u128(2)
            }] - 0.5)
                .abs()
                < f32::EPSILON
        );
        assert!(
            (ratios[&SplitControlId {
                first: Uuid::from_u128(2),
                second: Uuid::from_u128(3)
            }] - 0.5)
                .abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn oversized_paste_is_rejected_before_it_reaches_the_protocol() {
        let text = "x".repeat(MAX_PASTE_BYTES + 1);
        assert_eq!(
            prepare_paste(&text, false),
            Err("paste rejected: clipboard text exceeds 64 KiB")
        );
    }

    #[test]
    fn selection_highlight_spans_exact_grid_cells_across_rows() {
        let selection = TerminalSelection {
            start: TerminalPoint { row: 1, column: 3 },
            end: TerminalPoint { row: 2, column: 4 },
            is_block: false,
        };
        assert_eq!(selection_span(selection, 0, 10), None);
        assert_eq!(selection_span(selection, 1, 10), Some((3, 7)));
        assert_eq!(selection_span(selection, 2, 10), Some((0, 5)));
    }
}
