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
use std::time::Duration;

use gpui::{
    AnyElement, App, Application, Bounds, Context, CursorStyle, FocusHandle, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, StrikethroughStyle,
    StyledText, TextRun, TitlebarOptions, UnderlineStyle, Window, WindowBounds, WindowOptions,
    actions, div, font, point, prelude::*, px, relative, rgb, rgba, size,
};
use rust_mux_desktop::request;
use rust_mux_protocol::{
    ClientRequest, DropPlacement, Pane, PaneLayout, ServiceResponse, SessionSnapshot, SplitAxis,
    TerminalAttributes, TerminalColor, TerminalLine, TerminalScreen, Workspace,
};
use uuid::Uuid;

mod theme;

use theme::AppTheme;

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
    ]
);

const SIDEBAR_WIDTH: f32 = 190.0;
const TITLEBAR_HEIGHT: f32 = 38.0;
const PANE_HEADER_HEIGHT: f32 = 29.0;
const TERMINAL_FONT_SIZE: f32 = 13.0;
const TERMINAL_LINE_HEIGHT: f32 = 18.0;
const TERMINAL_CELL_WIDTH: f32 = 7.83;
const THEME: AppTheme = AppTheme::HARBOR_NIGHT;

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

#[derive(Clone, Debug)]
struct CloseConfirmation {
    pane_id: Uuid,
    title: String,
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
    split_key: Uuid,
    axis: SplitAxis,
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
    snapshot: Option<SessionSnapshot>,
    screens: HashMap<Uuid, TerminalScreen>,
    active_workspace: Option<Uuid>,
    focused_pane: Option<Uuid>,
    split_ratios: HashMap<Uuid, f32>,
    resizing: Option<ResizeDrag>,
    last_sizes: HashMap<Uuid, (u16, u16)>,
    viewport: (u16, u16),
    connection_error: Option<String>,
    tab_menu: Option<TabMenu>,
    rename_editor: Option<RenameEditor>,
    close_confirmation: Option<CloseConfirmation>,
    dragging_pane: Option<Uuid>,
    drag_hover: DragHoverState,
}

impl RustMux {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        let mut app = Self {
            focus_handle,
            snapshot: None,
            screens: HashMap::new(),
            active_workspace: None,
            focused_pane: None,
            split_ratios: HashMap::new(),
            resizing: None,
            last_sizes: HashMap::new(),
            viewport: (100, 30),
            connection_error: None,
            tab_menu: None,
            rename_editor: None,
            close_confirmation: None,
            dragging_pane: None,
            drag_hover: DragHoverState::default(),
        };
        app.refresh_state();

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
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
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
                _ if !keystroke.modifiers.platform && !keystroke.modifiers.control => {
                    if let Some(text) = &keystroke.key_char
                        && editor.value.chars().count() < 80
                        && !text.chars().any(char::is_control)
                    {
                        if editor.replace_on_type {
                            editor.value.clear();
                        }
                        editor.value.push_str(text);
                        editor.replace_on_type = false;
                        cx.notify();
                    }
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
        if keystroke.modifiers.platform {
            return;
        }
        let bytes = if keystroke.modifiers.control && keystroke.key.len() == 1 {
            keystroke
                .key
                .as_bytes()
                .first()
                .map(|byte| vec![byte.to_ascii_lowercase() & 0x1f])
        } else {
            match keystroke.key.as_str() {
                "enter" => Some(vec![b'\r']),
                "backspace" => Some(vec![0x7f]),
                "tab" => Some(vec![b'\t']),
                "escape" => Some(vec![0x1b]),
                "left" => Some(b"\x1b[D".to_vec()),
                "right" => Some(b"\x1b[C".to_vec()),
                "up" => Some(b"\x1b[A".to_vec()),
                "down" => Some(b"\x1b[B".to_vec()),
                "home" => Some(b"\x1b[H".to_vec()),
                "end" => Some(b"\x1b[F".to_vec()),
                "delete" => Some(b"\x1b[3~".to_vec()),
                _ => keystroke.key_char.as_ref().map(|text| {
                    let mut bytes = text.as_bytes().to_vec();
                    if keystroke.modifiers.alt {
                        bytes.insert(0, 0x1b);
                    }
                    bytes
                }),
            }
        };
        if let (Some(pane_id), Some(bytes)) = (self.focused_pane, bytes) {
            self.send(ClientRequest::WriteInput { pane_id, bytes });
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn handle_resize(&mut self, event: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(drag) = self.resizing else { return };
        let width = f32::from(window.bounds().size.width).max(SIDEBAR_WIDTH + 100.0);
        let height = f32::from(window.bounds().size.height).max(TITLEBAR_HEIGHT + 100.0);
        let ratio = match drag.axis {
            SplitAxis::Horizontal => {
                (f32::from(event.position.x) - SIDEBAR_WIDTH) / (width - SIDEBAR_WIDTH)
            }
            SplitAxis::Vertical => {
                (f32::from(event.position.y) - TITLEBAR_HEIGHT) / (height - TITLEBAR_HEIGHT)
            }
        };
        self.split_ratios
            .insert(drag.split_key, ratio.clamp(0.18, 0.82));
        self.last_sizes.clear();
        self.sync_pty_sizes();
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
        collect_sizes(
            &tab.layout,
            self.viewport.0,
            self.viewport.1,
            &self.split_ratios,
            &mut sizes,
        );
        for (pane_id, columns, rows) in sizes {
            if self.last_sizes.get(&pane_id) == Some(&(columns, rows)) {
                continue;
            }
            if request(ClientRequest::ResizePane {
                pane_id,
                columns,
                rows,
            })
            .is_ok()
            {
                self.last_sizes.insert(pane_id, (columns, rows));
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
                    .max_w(px(220.0))
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
                    .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(pane_id, cx)))
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
                            .overflow_hidden()
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
                            .font_family("SF Mono")
                            .text_size(px(9.5))
                            .text_color(rgb(THEME.dim))
                            .child(pane.shell),
                    )
                    .child(
                        div()
                            .id(("close-tab", element_key(pane_id)))
                            .ml(px(1.0))
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
            }))
            .child(div().flex_1())
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
                    .border_l(if focused { px(2.0) } else { px(0.0) })
                    .border_color(rgb(THEME.accent))
                    .font_family("SF Mono")
                    .text_size(px(TERMINAL_FONT_SIZE))
                    .line_height(px(TERMINAL_LINE_HEIGHT))
                    .text_color(rgb(THEME.foreground))
                    .children(screen.clone().into_iter().flat_map(|screen| {
                        let cursor = screen.cursor;
                        screen
                            .lines
                            .into_iter()
                            .enumerate()
                            .map(move |(row, line)| {
                                self.render_terminal_line(line, row, cursor, focused)
                            })
                    }))
                    .when_some(drop_target, |element, target| {
                        element.child(self.render_drop_layer(target, cx))
                    }),
            )
            .into_any_element()
    }

    fn render_terminal_line(
        &self,
        line: TerminalLine,
        row: usize,
        cursor: Option<rust_mux_protocol::TerminalCursor>,
        focused: bool,
    ) -> AnyElement {
        let mut text = String::new();
        let mut runs = Vec::new();
        for style in line.runs {
            let bold = style.attributes.contains(TerminalAttributes::BOLD);
            let dim = style.attributes.contains(TerminalAttributes::DIM);
            let italic = style.attributes.contains(TerminalAttributes::ITALIC);
            let underline = style.attributes.contains(TerminalAttributes::UNDERLINE);
            let strikethrough = style.attributes.contains(TerminalAttributes::STRIKETHROUGH);
            let foreground = THEME.terminal_color(style.foreground, bold, dim);
            let background = THEME.terminal_color(style.background, false, false);
            let mut run_font = font("SF Mono");
            if bold {
                run_font = run_font.bold();
            }
            if italic {
                run_font = run_font.italic();
            }
            let len = style.text.len();
            text.push_str(&style.text);
            runs.push(TextRun {
                len,
                font: run_font,
                color: rgb(foreground).into(),
                background_color: (style.background != TerminalColor::DefaultBackground)
                    .then(|| rgb(background).into()),
                underline: underline.then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(rgb(foreground).into()),
                    wavy: false,
                }),
                strikethrough: strikethrough.then_some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(rgb(foreground).into()),
                }),
            });
        }
        if text.is_empty() {
            text.push(' ');
            runs.push(TextRun {
                len: 1,
                font: font("SF Mono"),
                color: rgb(THEME.foreground).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
        let cursor_column = cursor
            .filter(|cursor| focused && usize::from(cursor.row) == row)
            .map(|cursor| cursor.column);
        div()
            .relative()
            .h(px(TERMINAL_LINE_HEIGHT))
            .flex_none()
            .overflow_hidden()
            .child(StyledText::new(text).with_runs(runs))
            .when_some(cursor_column, |element, column| {
                element.child(
                    div()
                        .absolute()
                        .left(px(f32::from(column) * TERMINAL_CELL_WIDTH))
                        .top(px(1.0))
                        .w(px(TERMINAL_CELL_WIDTH))
                        .h(px(TERMINAL_LINE_HEIGHT - 2.0))
                        .rounded(px(1.0))
                        .bg(rgba((THEME.cursor << 8) | 0x88)),
                )
            })
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
                                    .child(editor.value.clone()),
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

    fn render_layout(&self, layout: PaneLayout, cx: &mut Context<Self>) -> AnyElement {
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
                let split_key = first_visible_pane(&first);
                let ratio = self.split_ratios.get(&split_key).copied().unwrap_or(ratio);
                let vertical = axis == SplitAxis::Vertical;
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
                            .child(self.render_layout(*first, cx)),
                    )
                    .child(self.render_divider(split_key, axis, cx))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex_1()
                            .child(self.render_layout(*second, cx)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_divider(
        &self,
        split_key: Uuid,
        axis: SplitAxis,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let vertical = axis == SplitAxis::Vertical;
        div()
            .id(("divider", element_key(split_key)))
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
                    this.resizing = Some(ResizeDrag { split_key, axis });
                    cx.notify();
                }),
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
        let layout = workspace.tabs.first().map(|tab| tab.layout.clone());
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
                            .child(format!("{}  ·  ⌘T tab   ⌘D split   ⇧⌘D down", THEME.name)),
                    ),
            )
            .child(div().min_h(px(0.0)).flex_1().child(layout.map_or_else(
                || div().size_full().bg(rgb(THEME.terminal)).into_any_element(),
                |layout| self.render_layout(layout, cx),
            )))
            .into_any_element()
    }
}

impl Render for RustMux {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = (f32::from(window.bounds().size.width) - SIDEBAR_WIDTH).max(160.0);
        let height = (f32::from(window.bounds().size.height) - TITLEBAR_HEIGHT).max(100.0);
        self.viewport = (
            ((width / 7.6).floor() as u16).clamp(20, 300),
            ((height / 16.0).floor() as u16).clamp(5, 120),
        );

        div()
            .key_context("RustMux")
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
            .on_action(cx.listener(|this, _: &NewWorkspace, _, cx| this.new_workspace(cx)))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.new_tab(cx)))
            .on_action(
                cx.listener(|this, _: &SplitRight, _, cx| this.split(SplitAxis::Horizontal, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SplitDown, _, cx| this.split(SplitAxis::Vertical, cx)),
            )
            .on_action(cx.listener(|this, _: &FocusLeft, _, cx| this.focus_direction(false, cx)))
            .on_action(cx.listener(|this, _: &FocusUp, _, cx| this.focus_direction(false, cx)))
            .on_action(cx.listener(|this, _: &FocusRight, _, cx| this.focus_direction(true, cx)))
            .on_action(cx.listener(|this, _: &FocusDown, _, cx| this.focus_direction(true, cx)))
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

fn first_visible_pane(layout: &PaneLayout) -> Uuid {
    match layout {
        PaneLayout::Leaf { pane } => pane.id,
        PaneLayout::Stack { active, .. } => *active,
        PaneLayout::Split { first, .. } => first_visible_pane(first),
    }
}

fn collect_sizes(
    layout: &PaneLayout,
    columns: u16,
    rows: u16,
    ratios: &HashMap<Uuid, f32>,
    output: &mut Vec<(Uuid, u16, u16)>,
) {
    match layout {
        PaneLayout::Leaf { pane } => {
            output.push((pane.id, columns.max(20), rows.saturating_sub(2).max(5)));
        }
        PaneLayout::Stack { active, .. } => {
            output.push((*active, columns.max(20), rows.saturating_sub(2).max(5)));
        }
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = ratios
                .get(&first_visible_pane(first))
                .copied()
                .unwrap_or(*ratio)
                .clamp(0.18, 0.82);
            match axis {
                SplitAxis::Horizontal => {
                    let first_columns = (f32::from(columns) * ratio).floor() as u16;
                    collect_sizes(first, first_columns, rows, ratios, output);
                    collect_sizes(
                        second,
                        columns.saturating_sub(first_columns),
                        rows,
                        ratios,
                        output,
                    );
                }
                SplitAxis::Vertical => {
                    let first_rows = (f32::from(rows) * ratio).floor() as u16;
                    collect_sizes(first, columns, first_rows, ratios, output);
                    collect_sizes(
                        second,
                        columns,
                        rows.saturating_sub(first_rows),
                        ratios,
                        output,
                    );
                }
            }
        }
    }
}

fn element_key(id: Uuid) -> u64 {
    let (high, low) = id.as_u64_pair();
    high ^ low
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-n", NewWorkspace, Some("RustMux")),
            KeyBinding::new("cmd-t", NewTab, Some("RustMux")),
            KeyBinding::new("cmd-d", SplitRight, Some("RustMux")),
            KeyBinding::new("cmd-shift-d", SplitDown, Some("RustMux")),
            KeyBinding::new("cmd-alt-left", FocusLeft, Some("RustMux")),
            KeyBinding::new("cmd-alt-right", FocusRight, Some("RustMux")),
            KeyBinding::new("cmd-alt-up", FocusUp, Some("RustMux")),
            KeyBinding::new("cmd-alt-down", FocusDown, Some("RustMux")),
        ]);
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
            |window, cx| cx.new(|cx| RustMux::new(window, cx)),
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
}
