//! Custom gpui elements for terminals, input, and resize capture.

use gpui::{
    App, Bounds, CursorStyle, DispatchPhase, Element, ElementId, ElementInputHandler, Entity,
    GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, ScrollWheelEvent,
    ShapedLine, Style, TextRun, UnderlineStyle, Window, fill, point, px, relative, rgb, rgba, size,
};
use hh_protocol::AppearanceColor;

use crate::helpers::terminal_point_at;
use crate::view_models::{DialogTextEditor, WorkspaceCreationField, WorkspaceCreationStep};
use crate::{HhApp, THEME};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SidebarPaneRowContext {
    pub(crate) workspace_id: Uuid,
    pub(crate) tab_id: Option<Uuid>,
    pub(crate) tab_color: Option<AppearanceColor>,
    pub(crate) from_group: bool,
    pub(crate) indent: f32,
}

pub(crate) struct WorkspaceTextInputElement {
    pub(crate) input: Entity<HhApp>,
    pub(crate) field: WorkspaceCreationField,
    pub(crate) placeholder: &'static str,
}

pub(crate) struct WorkspaceTextPrepaintState {
    pub(crate) line: ShapedLine,
    pub(crate) cursor: Option<PaintQuad>,
    pub(crate) selection: Option<PaintQuad>,
    pub(crate) text_bounds: Bounds<Pixels>,
    pub(crate) active: bool,
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
        let dialog = app.editor.modal.workspace_creation();
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
        let runs = if let Some(marked_range) = editor
            .and_then(|editor| editor.marked_range.as_ref())
            .filter(|_| !content.is_empty())
        {
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
                self.input.read(cx).editor.workspace_input_focus[self.field.index()].clone();
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
        if let Some(selection) = state.selection.take() {
            window.paint_quad(selection);
        }
        if state
            .line
            .paint(state.text_bounds.origin, window.line_height(), window, cx)
            .is_err()
        {
            let field_index = self.field.index();
            self.input.update(cx, |app, _| {
                app.editor.workspace_input_layouts[field_index] = None;
                app.editor.workspace_input_bounds[field_index] = None;
            });
            return;
        }
        if let Some(cursor) = state.cursor.take() {
            window.paint_quad(cursor);
        }
        let line = state.line.clone();
        let field_index = self.field.index();
        self.input.update(cx, |app, _| {
            app.editor.workspace_input_layouts[field_index] = Some(line);
            app.editor.workspace_input_bounds[field_index] = Some(state.text_bounds);
        });
    }
}

pub(crate) struct TerminalInputElement {
    pub(crate) input: Entity<HhApp>,
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
        if app.editor.modal.workspace_creation().is_none() {
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
pub(crate) struct SidebarResizeCaptureElement {
    pub(crate) input: Entity<HhApp>,
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
pub(crate) struct TerminalPointerElement {
    pub(crate) input: Entity<HhApp>,
    pub(crate) pane_id: Uuid,
    pub(crate) row: u16,
    pub(crate) columns: u16,
    pub(crate) cell_width: f32,
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
