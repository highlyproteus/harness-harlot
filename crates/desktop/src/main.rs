#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::unused_self
)]

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use gpui::{
    AnyElement, App, Application, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, GlobalElementId,
    InspectorElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, StrikethroughStyle, Style,
    StyledText, TextRun, TitlebarOptions, UTF16Selection, UnderlineStyle, Window, WindowBounds,
    WindowOptions, actions, div, point, prelude::*, px, relative, rgb, rgba, size,
};
use rust_mux_desktop::request;
use rust_mux_protocol::{
    ClientRequest, DropPlacement, MAX_SSH_HOST_LEN, Pane, PaneLayout, ServiceResponse,
    SessionSnapshot, SplitAxis, TerminalAttributes, TerminalColor, TerminalLine, TerminalModes,
    TerminalModifiers, TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalRun,
    TerminalScreen, TerminalSelection, TerminalSelectionKind, Workspace, validate_ssh_host,
};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

mod commands;
mod theme;
mod typography;

use commands::{
    AppCommand, AppConfig, ROOT_KEY_CONTEXT, ResolvedBinding, ResolvedKeymap, descriptor,
    palette_matches,
};
use theme::{AppTheme, BuiltInTheme};
use typography::TerminalFontProfile;

actions!(
    rust_mux,
    [
        NewWorkspace,
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

const SIDEBAR_WIDTH: f32 = 190.0;
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
const THEME: AppTheme = BuiltInTheme::HarborNight.theme();

#[derive(Clone, Debug)]
struct PaneDrag {
    pane_id: Uuid,
    title: String,
    position: Point<Pixels>,
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

#[derive(Clone, Copy, Debug)]
struct TerminalLineRender {
    row: usize,
    cursor: Option<rust_mux_protocol::TerminalCursor>,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SshConnectStep {
    Destination,
    Confirm,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SshConnectDialog {
    target_pane: Uuid,
    host: String,
    step: SshConnectStep,
    error: Option<String>,
}

impl SshConnectDialog {
    fn new(target_pane: Uuid) -> Self {
        Self {
            target_pane,
            host: String::new(),
            step: SshConnectStep::Destination,
            error: None,
        }
    }

    fn review(&mut self) {
        match validate_ssh_host(&self.host) {
            Ok(()) => {
                self.step = SshConnectStep::Confirm;
                self.error = None;
            }
            Err(message) => self.error = Some(message.to_owned()),
        }
    }

    fn approved_request(&self) -> Option<ClientRequest> {
        (self.step == SshConnectStep::Confirm && validate_ssh_host(&self.host).is_ok()).then(|| {
            ClientRequest::ConnectSsh {
                target_pane: self.target_pane,
                host: self.host.clone(),
            }
        })
    }
}

impl CloseConfirmation {
    fn for_pane(pane: &Pane) -> Self {
        Self {
            pane_id: pane.id,
            title: pane.title.clone(),
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
struct RustMux {
    focus_handle: FocusHandle,
    terminal_font: TerminalFontProfile,
    keymap: ResolvedKeymap,
    snapshot: Option<SessionSnapshot>,
    screens: HashMap<Uuid, TerminalScreen>,
    active_workspace: Option<Uuid>,
    focused_pane: Option<Uuid>,
    split_ratios: HashMap<SplitControlId, f32>,
    zoomed_pane: Option<Uuid>,
    command_palette: Option<CommandPaletteState>,
    resizing: Option<ResizeDrag>,
    last_sizes: HashMap<Uuid, (u16, u16)>,
    workspace_pixels: (f32, f32),
    connection_error: Option<String>,
    tab_menu: Option<TabMenu>,
    rename_editor: Option<RenameEditor>,
    search_editor: Option<SearchEditor>,
    close_confirmation: Option<CloseConfirmation>,
    ssh_connect: Option<SshConnectDialog>,
    dragging_pane: Option<Uuid>,
    drag_hover: DragHoverState,
    selection_drag: Option<SelectionDrag>,
    ime_preedit: String,
}

impl RustMux {
    fn new(window: &mut Window, keymap: ResolvedKeymap, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        let terminal_font = TerminalFontProfile::resolve(cx.text_system());
        let mut app = Self {
            focus_handle,
            terminal_font,
            keymap,
            snapshot: None,
            screens: HashMap::new(),
            active_workspace: None,
            focused_pane: None,
            split_ratios: HashMap::new(),
            zoomed_pane: None,
            command_palette: None,
            resizing: None,
            last_sizes: HashMap::new(),
            workspace_pixels: (0.0, 0.0),
            connection_error: None,
            tab_menu: None,
            rename_editor: None,
            search_editor: None,
            close_confirmation: None,
            ssh_connect: None,
            dragging_pane: None,
            drag_hover: DragHoverState::default(),
            selection_drag: None,
            ime_preedit: String::new(),
        };
        app.update_window_geometry(window);
        app.refresh_state();

        cx.observe_window_bounds(window, |this, window, cx| {
            if this.update_window_geometry(window) {
                this.sync_pty_sizes();
                cx.notify();
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(70)).await;
                if this
                    .update(cx, |this, cx| {
                        this.refresh_state();
                        this.sync_pty_sizes();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        app
    }

    fn refresh_state(&mut self) {
        match request(ClientRequest::GetState) {
            Ok(ServiceResponse::State { snapshot, screens }) => {
                if self.active_workspace.is_none()
                    || !snapshot
                        .workspaces
                        .iter()
                        .any(|workspace| Some(workspace.id) == self.active_workspace)
                {
                    self.active_workspace =
                        snapshot.workspaces.first().map(|workspace| workspace.id);
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
                    self.focused_pane = visible.first().copied();
                }
                self.screens = screens
                    .into_iter()
                    .map(|screen| (screen.pane_id, screen))
                    .collect();
                self.snapshot = Some(snapshot);
                self.connection_error = None;
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"))
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
    }

    fn active_workspace_in<'a>(&self, snapshot: &'a SessionSnapshot) -> Option<&'a Workspace> {
        let active = self.active_workspace?;
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == active)
    }

    fn send(&mut self, request_message: ClientRequest) {
        if let Err(error) = request(request_message) {
            self.connection_error = Some(format!("{error:#}"));
        }
        self.refresh_state();
    }

    fn new_workspace(&mut self, cx: &mut Context<Self>) {
        match request(ClientRequest::CreateWorkspace { title: None }) {
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            }) => {
                self.active_workspace = Some(workspace_id);
                self.focused_pane = Some(pane_id);
                self.refresh_state();
            }
            Ok(response) => {
                self.connection_error = Some(format!("unexpected response: {response:?}"))
            }
            Err(error) => self.connection_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn new_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(target_pane) = self.focused_pane {
            self.new_tab_at(target_pane, cx);
        }
    }

    fn new_tab_at(&mut self, target_pane: Uuid, cx: &mut Context<Self>) {
        self.focused_pane = Some(target_pane);
        match request(ClientRequest::CreateTab { target_pane }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => self.focused_pane = Some(pane_id),
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
        self.focused_pane = Some(target_pane);
        self.zoomed_pane = None;
        match request(ClientRequest::CreatePane { target_pane, axis }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => self.focused_pane = Some(pane_id),
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
        self.focused_pane = Some(pane_id);
        self.send(ClientRequest::ActivateTab { pane_id });
        cx.notify();
    }

    fn swap_panes(&mut self, source_pane: Uuid, target_pane: Uuid, cx: &mut Context<Self>) {
        if source_pane != target_pane {
            self.zoomed_pane = None;
            self.send(ClientRequest::SwapPanes {
                source_pane,
                target_pane,
            });
            self.focused_pane = Some(source_pane);
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
        self.focused_pane = Some(source_pane);
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
        self.focused_pane = Some(source_pane);
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
        self.focused_pane = Some(pane_id);
        self.send(ClientRequest::ActivateTab { pane_id });
        self.tab_menu = Some(TabMenu { pane_id, position });
        self.rename_editor = None;
        self.close_confirmation = None;
        cx.notify();
    }

    fn begin_rename(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focused_pane = Some(pane_id);
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

    fn begin_close(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focused_pane = Some(pane_id);
        if let Some(pane) = self.pane_metadata(pane_id) {
            self.close_confirmation = Some(CloseConfirmation::for_pane(&pane));
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

    fn begin_ssh_connect(&mut self, cx: &mut Context<Self>) {
        let Some(target_pane) = self.focused_pane else {
            return;
        };
        self.ssh_connect = Some(SshConnectDialog::new(target_pane));
        self.tab_menu = None;
        self.rename_editor = None;
        self.close_confirmation = None;
        cx.notify();
    }

    fn review_ssh_connect(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.ssh_connect.as_mut() {
            dialog.review();
            cx.notify();
        }
    }

    fn confirm_ssh_connect(&mut self, cx: &mut Context<Self>) {
        let Some(request_message) = self
            .ssh_connect
            .as_ref()
            .and_then(SshConnectDialog::approved_request)
        else {
            return;
        };
        match request(request_message) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focused_pane = Some(pane_id);
                self.ssh_connect = None;
                self.refresh_state();
            }
            Ok(response) => {
                if let Some(dialog) = self.ssh_connect.as_mut() {
                    dialog.error = Some(format!("unexpected response: {response:?}"));
                }
            }
            Err(error) => {
                if let Some(dialog) = self.ssh_connect.as_mut() {
                    dialog.error = Some(format!("{error:#}"));
                }
            }
        }
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
        self.focused_pane = Some(panes[next]);
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
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::BRACKETED_PASTE));
        match prepare_paste(&text, bracketed) {
            Ok(bytes) => self.send(ClientRequest::WriteInput { pane_id, bytes }),
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
        let Some(editor) = self.search_editor.as_mut() else {
            return;
        };
        if editor.query.is_empty() {
            editor.no_match = false;
            cx.notify();
            return;
        }
        match request(ClientRequest::SearchPane {
            pane_id,
            query: editor.query.clone(),
            forward,
        }) {
            Ok(ServiceResponse::SearchResult { found }) => {
                editor.no_match = !found;
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
            self.send(ClientRequest::WriteInput {
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
        self.focused_pane = Some(pane_id);
        self.focus_handle.focus(window);
        let mouse_reporting = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING));
        if mouse_reporting && !event.modifiers.shift {
            if let Some(button) = terminal_mouse_button(event.button) {
                self.send(ClientRequest::MouseInput {
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
            self.send(ClientRequest::BeginSelection {
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
            self.send(ClientRequest::UpdateSelection { pane_id, point });
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let mouse_motion = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_MOTION));
        if mouse_motion && let Some(button) = event.pressed_button.and_then(terminal_mouse_button) {
            self.send(ClientRequest::MouseInput {
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
                self.send(ClientRequest::ClearSelection { pane_id });
            } else {
                self.send(ClientRequest::UpdateSelection { pane_id, point });
            }
        } else if self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
            && let Some(button) = terminal_mouse_button(event.button)
        {
            self.send(ClientRequest::MouseInput {
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
        if self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
        {
            self.send(ClientRequest::MouseInput {
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
        } else {
            self.send(ClientRequest::ScrollPane { pane_id, lines });
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            self.handle_palette_key(event, cx);
            return;
        }
        let keystroke = &event.keystroke;
        if self.ssh_connect.is_some() {
            let step = self.ssh_connect.as_ref().map(|dialog| dialog.step);
            match keystroke.key.as_str() {
                "enter" if step == Some(SshConnectStep::Destination) => {
                    self.review_ssh_connect(cx);
                }
                "enter" => self.confirm_ssh_connect(cx),
                "escape" => {
                    self.ssh_connect = None;
                    cx.notify();
                }
                "backspace" if step == Some(SshConnectStep::Destination) => {
                    if let Some(dialog) = self.ssh_connect.as_mut() {
                        dialog.host.pop();
                        dialog.error = None;
                        cx.notify();
                    }
                }
                _ if step == Some(SshConnectStep::Destination)
                    && !keystroke.modifiers.platform
                    && !keystroke.modifiers.control =>
                {
                    if let Some(text) = &keystroke.key_char
                        && let Some(dialog) = self.ssh_connect.as_mut()
                        && dialog.host.len() + text.len() <= MAX_SSH_HOST_LEN
                        && !text.chars().any(char::is_control)
                    {
                        dialog.host.push_str(text);
                        dialog.error = None;
                        cx.notify();
                    }
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
                            self.send(ClientRequest::ClearSelection { pane_id });
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
            self.send(ClientRequest::WriteInput { pane_id, bytes });
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn handle_resize(&mut self, event: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
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
        let workspace_x = f32::from(event.position.x) - SIDEBAR_WIDTH;
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
        let next = workspace_pixel_size(
            f32::from(window.bounds().size.width),
            f32::from(window.bounds().size.height),
        );
        if self.workspace_pixels == next {
            return false;
        }
        self.workspace_pixels = next;
        self.last_sizes.clear();
        true
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
        let workspaces = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.workspaces.clone())
            .unwrap_or_default();
        div()
            .w(px(SIDEBAR_WIDTH))
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
                    .gap(px(14.0))
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child("▣")
                    .child("◌")
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
                    .pb(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Workspaces"),
                    )
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("~/Projects"),
                    ),
            )
            .children(
                workspaces
                    .into_iter()
                    .enumerate()
                    .map(|(index, workspace)| {
                        let active = Some(workspace.id) == self.active_workspace;
                        let workspace_id = workspace.id;
                        let pane_count = workspace
                            .tabs
                            .first()
                            .map_or(0, |tab| visible_panes(&tab.layout).len());
                        div()
                            .id(("workspace", element_key(workspace.id)))
                            .mx(px(7.0))
                            .mb(px(3.0))
                            .px(px(10.0))
                            .py(px(8.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .when(active, |element| element.bg(rgb(THEME.accent)))
                            .hover(|element| {
                                if active {
                                    element
                                } else {
                                    element.bg(rgb(THEME.surface))
                                }
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.active_workspace = Some(workspace_id);
                                this.focused_pane = workspace
                                    .tabs
                                    .first()
                                    .and_then(|tab| visible_panes(&tab.layout).first().copied());
                                this.last_sizes.clear();
                                cx.notify();
                            }))
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if active {
                                        rgb(0xffffff)
                                    } else {
                                        rgb(THEME.foreground)
                                    })
                                    .child(format!("{}  {}", index + 1, workspace.title)),
                            )
                            .child(
                                div()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(if active {
                                        rgb(0xe6f2ff)
                                    } else {
                                        rgb(THEME.dim)
                                    })
                                    .child(format!(
                                        "local · {pane_count} pane{}",
                                        if pane_count == 1 { "" } else { "s" }
                                    )),
                            )
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("connect-ssh")
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
                    .on_click(cx.listener(|this, _, _, cx| this.begin_ssh_connect(cx)))
                    .child("Connect with SSH…"),
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

    fn render_pane_header(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let merge_preview = self.drag_hover.merges_into(active);
        div()
            .id(("pane-tab-strip", element_key(active)))
            .h(px(PANE_HEADER_HEIGHT))
            .flex_none()
            .bg(rgb(THEME.surface))
            .border_b(if merge_preview { px(2.0) } else { px(1.0) })
            .border_color(if merge_preview {
                rgb(THEME.accent)
            } else {
                rgb(THEME.border)
            })
            .when(merge_preview, |element| {
                element.bg(rgba((THEME.accent << 8) | 0x18))
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
                        let selected = pane_id == active;
                        let close_tooltip = format!("Close {}…", pane.title);
                        let drag = PaneDrag {
                            pane_id,
                            title: pane.title.clone(),
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
                            .border_color(rgb(THEME.accent))
                            .border_r_1()
                            .border_color(rgb(THEME.border))
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
                                    .flex_none()
                                    .w(px(8.0))
                                    .h(px(8.0))
                                    .rounded(px(2.0))
                                    .border_1()
                                    .border_color(if selected {
                                        rgb(THEME.accent)
                                    } else {
                                        rgb(THEME.muted)
                                    }),
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
                                    .child(pane.title),
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
                RustMux::new_tab_at,
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
        let screen = self.screens.get(&active).cloned();
        let drop_target = self
            .dragging_pane
            .and_then(|source| split_target_for_drag(source, &panes, active));
        let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        let rendered_lines = screen
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
            .unwrap_or_default();
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
                this.focused_pane = Some(active);
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
                        rgb(THEME.focus_ring)
                    } else {
                        rgb(THEME.terminal)
                    })
                    .font(self.terminal_font.font(false, false))
                    .text_size(px(self.terminal_font.metrics.font_size))
                    .line_height(px(self.terminal_font.metrics.line_height))
                    .text_color(rgb(THEME.foreground))
                    .children(rendered_lines)
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
                                        .border_color(rgb(THEME.accent))
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
                            rgb(THEME.cursor)
                        } else {
                            rgb(THEME.muted)
                        })
                        .when(focused, |cursor| {
                            cursor.bg(rgba((THEME.cursor << 8) | 0x30))
                        }),
                )
            })
            .children((0..columns).map(|column| {
                let point = TerminalPoint {
                    row: u16::try_from(row).unwrap_or(u16::MAX),
                    column,
                };
                let span = metrics.span(column, 1);
                div()
                    .id((
                        "terminal-cell",
                        element_key(pane_id)
                            ^ (u64::try_from(row).unwrap_or(u64::MAX) << 16)
                            ^ u64::from(column),
                    ))
                    .absolute()
                    .left(px(span.x))
                    .top(px(0.0))
                    .w(px(span.width))
                    .h(px(span.height))
                    .cursor(CursorStyle::IBeam)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.begin_terminal_pointer(pane_id, point, event, window, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.begin_terminal_pointer(pane_id, point, event, window, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.begin_terminal_pointer(pane_id, point, event, window, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        this.move_terminal_pointer(pane_id, point, event, cx);
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            this.end_terminal_pointer(pane_id, point, event, cx);
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Middle,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            this.end_terminal_pointer(pane_id, point, event, cx);
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            this.end_terminal_pointer(pane_id, point, event, cx);
                        }),
                    )
                    .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                        this.scroll_terminal(pane_id, point, event, cx);
                    }))
            }))
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
                        .border_color(rgb(THEME.accent))
                        .bg(rgba((THEME.accent << 8) | 0x24))
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
            .w(px(170.0))
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

    fn render_ssh_dialog(&self, dialog: &SshConnectDialog, cx: &mut Context<Self>) -> AnyElement {
        let host = dialog.host.clone();
        let error = dialog.error.clone();
        let content = match dialog.step {
            SshConnectStep::Destination => div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(THEME.foreground))
                        .child("Connect with system OpenSSH"),
                )
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "Enter one host or alias. OpenSSH will resolve your existing SSH config, agent, keys, proxies, and known_hosts. No connection starts until you confirm the next screen.",
                        ),
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
                        .font_family("SF Mono")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .child(host)
                        .child("│"),
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
                                .id("cancel-ssh")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.ssh_connect = None;
                                    cx.notify();
                                }))
                                .child("Cancel"),
                        )
                        .child(
                            div()
                                .id("review-ssh")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .text_color(rgb(0xffffff))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.review_ssh_connect(cx)
                                }))
                                .child("Review"),
                        ),
                )
                .into_any_element(),
            SshConnectStep::Confirm => div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(THEME.foreground))
                        .child(format!("Connect to {host}?")),
                )
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "This explicit action starts the installed OpenSSH client in a new managed terminal. Rust Mux will not answer password or host-key prompts, forward an agent automatically, or change host-key policy.",
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
                                .id("back-ssh")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(dialog) = this.ssh_connect.as_mut() {
                                        dialog.step = SshConnectStep::Destination;
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                }))
                                .child("Back"),
                        )
                        .child(
                            div()
                                .id("confirm-ssh")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .text_color(rgb(0xffffff))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_ssh_connect(cx)
                                }))
                                .child("Connect with OpenSSH"),
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
                    .w(px(480.0))
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
                            .child(
                                "This will terminate this terminal and its running shell process. Other terminal tabs stay open.",
                            ),
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
        let canonical_layout = workspace.tabs.first().map(|tab| tab.layout.clone());
        let layout = canonical_layout.as_ref().map(|layout| {
            self.zoomed_pane
                .and_then(|pane_id| zoom_projection(layout, pane_id))
                .unwrap_or_else(|| layout.clone())
        });
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
                    .px(px(11.0))
                    .bg(rgb(THEME.surface))
                    .border_b_1()
                    .border_color(rgb(THEME.border))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.accent))
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
            .child(div().min_h(px(0.0)).flex_1().child(layout.map_or_else(
                || div().size_full().bg(rgb(THEME.terminal)).into_any_element(),
                |layout| {
                    self.render_layout(layout, self.workspace_pixels.0, self.workspace_pixels.1, cx)
                },
            )))
            .into_any_element()
    }
}

impl Render for RustMux {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_window_geometry(window);

        div()
            .key_context(if self.command_palette.is_some() {
                "RustMuxPalette"
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
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.handle_key(event, cx)),
            )
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
                cx.listener(|this, _, _, cx| {
                    this.resizing = None;
                    this.dragging_pane = None;
                    this.drag_hover.clear();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.tab_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &NewWorkspace, _, cx| {
                this.execute_command(AppCommand::NewWorkspace, cx);
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
                cx.listener(|_: &mut RustMux, _: &ConsumeChordPrefix, _, cx| {
                    cx.stop_propagation();
                }),
            )
            .on_action(cx.listener(RustMux::copy_terminal))
            .on_action(cx.listener(RustMux::paste_terminal))
            .on_action(cx.listener(RustMux::find_terminal))
            .on_action(cx.listener(RustMux::find_next_terminal))
            .child(
                div()
                    .absolute()
                    .w(px(1.0))
                    .h(px(1.0))
                    .child(TerminalInputElement { input: cx.entity() }),
            )
            .child(self.render_sidebar(cx))
            .child(self.render_workspace(cx))
            .when_some(self.tab_menu, |element, menu| {
                element.child(self.render_tab_menu(menu, cx))
            })
            .when_some(self.rename_editor.as_ref(), |element, editor| {
                element.child(self.render_rename_dialog(editor, cx))
            })
            .when_some(self.close_confirmation.as_ref(), |element, confirmation| {
                element.child(self.render_close_dialog(confirmation, cx))
            })
            .when_some(self.command_palette.as_ref(), |element, palette| {
                element.child(self.render_command_palette(palette, cx))
            })
            .when_some(self.ssh_connect.as_ref(), |element, dialog| {
                element.child(self.render_ssh_dialog(dialog, cx))
            })
    }
}

impl EntityInputHandler for RustMux {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        actual_range.replace(0..self.ime_preedit.encode_utf16().count());
        Some(self.ime_preedit.clone())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.ime_preedit.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.ime_preedit.is_empty()).then(|| 0..self.ime_preedit.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.ime_preedit.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_preedit.clear();
        self.commit_text(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        text.clone_into(&mut self.ime_preedit);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(
            bounds.bottom_left(),
            size(px(1.0), px(self.terminal_font.metrics.line_height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}

struct TerminalInputElement {
    input: Entity<RustMux>,
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
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
    }
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

fn workspace_pixel_size(window_width: f32, window_height: f32) -> (f32, f32) {
    (
        (window_width - SIDEBAR_WIDTH).max(1.0),
        (window_height - TITLEBAR_HEIGHT).max(1.0),
    )
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

fn main() {
    Application::new().run(|cx: &mut App| {
        let keymap = match AppConfig::load().and_then(|config| config.resolve_keymap()) {
            Ok(keymap) => keymap,
            Err(error) => {
                eprintln!("Rust Mux config ignored: {error}");
                AppConfig::default()
                    .resolve_keymap()
                    .expect("built-in keymap must be valid")
            }
        };
        let mut bindings = keymap.bindings.iter().map(gpui_binding).collect::<Vec<_>>();
        bindings.extend(
            keymap
                .chord_prefixes
                .iter()
                .map(|prefix| KeyBinding::new(prefix, ConsumeChordPrefix, Some(ROOT_KEY_CONTEXT))),
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
                    title: Some("Rust Mux".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(13.0), px(13.0))),
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.0), px(460.0))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| RustMux::new(window, keymap.clone(), cx)),
        )
        .expect("open Rust Mux window");
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
        };

        let confirmation = CloseConfirmation::for_pane(&pane);

        assert_eq!(confirmation.pane_id, pane.id);
        assert_eq!(confirmation.title, "build");
        assert_eq!(
            confirmation.request(),
            ClientRequest::ClosePane { pane_id: pane.id }
        );
    }

    #[test]
    fn ssh_draft_cannot_create_a_network_action_before_review_and_confirmation() {
        let target_pane = Uuid::from_u128(7);
        let mut dialog = SshConnectDialog::new(target_pane);
        dialog.host = "prod-east".to_owned();

        assert_eq!(dialog.approved_request(), None);

        dialog.review();

        assert_eq!(dialog.step, SshConnectStep::Confirm);
        assert_eq!(
            dialog.approved_request(),
            Some(ClientRequest::ConnectSsh {
                target_pane,
                host: "prod-east".to_owned(),
            })
        );
    }

    #[test]
    fn ssh_review_keeps_invalid_input_out_of_the_network_action_boundary() {
        let mut dialog = SshConnectDialog::new(Uuid::from_u128(8));
        dialog.host = "-oProxyCommand=bad".to_owned();

        dialog.review();

        assert_eq!(dialog.step, SshConnectStep::Destination);
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
    fn pane_geometry_tracks_narrow_medium_and_wide_windows_without_fixed_columns() {
        let pane = Pane {
            id: Uuid::from_u128(10),
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
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
                let workspace = workspace_pixel_size(window_width, window_height);
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
    fn split_geometry_accounts_for_the_divider_and_each_panes_chrome() {
        let first = Pane {
            id: Uuid::from_u128(21),
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
        };
        let second = Pane {
            id: Uuid::from_u128(22),
            title: "Terminal 2".to_owned(),
            shell: "zsh".to_owned(),
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
        let workspace = workspace_pixel_size(1280.0, 820.0);
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
        };
        let second = Pane {
            id: Uuid::from_u128(102),
            title: "two".to_owned(),
            shell: "zsh".to_owned(),
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
