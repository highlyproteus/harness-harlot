//! Custom gpui elements for terminals, input, and resize capture.

use gpui::{
    App, BorderStyle, Bounds, CursorStyle, DispatchPhase, Element, ElementId, ElementInputHandler,
    Entity, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, ScrollWheelEvent,
    ShapedLine, StrikethroughStyle, Style, TextRun, UnderlineStyle, Window, fill,
    linear_color_stop, linear_gradient, point, px, quad, relative, rgb, rgba, size,
    transparent_black,
};
use hh_protocol::{
    AppearanceColor, TerminalAttributes, TerminalColor, TerminalCursor, TerminalRun,
    TerminalScreen, TerminalSelection,
};
use std::rc::Rc;

use crate::helpers::{hsv_to_rgb, selection_span, terminal_point_at, terminal_run_display_text};
use crate::typography::TerminalCellMetrics;
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

#[derive(Clone, Copy)]
pub(crate) enum HsvFieldKind {
    SquareSv,
    HueStrip,
}

pub(crate) struct HsvFieldElement {
    pub(crate) input: Entity<HhApp>,
    pub(crate) kind: HsvFieldKind,
}

impl IntoElement for HsvFieldElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HsvFieldElement {
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
        cx: &mut App,
    ) {
        window.set_cursor_style(
            match self.kind {
                HsvFieldKind::SquareSv => CursorStyle::Crosshair,
                HsvFieldKind::HueStrip => CursorStyle::ResizeLeftRight,
            },
            hitbox,
        );
        let Some(picker) = self.input.read(cx).editor.color_picker.as_ref() else {
            return;
        };
        let hue = picker.hue;
        let saturation = picker.saturation;
        let value = picker.value;

        match self.kind {
            HsvFieldKind::SquareSv => {
                window.paint_quad(fill(
                    bounds,
                    linear_gradient(
                        90.0,
                        linear_color_stop(rgb(0xffffff), 0.0),
                        linear_color_stop(rgb(hsv_to_rgb(hue, 1.0, 1.0)), 1.0),
                    ),
                ));
                window.paint_quad(fill(
                    bounds,
                    linear_gradient(
                        180.0,
                        linear_color_stop(transparent_black(), 0.0),
                        linear_color_stop(rgb(0x000000), 1.0),
                    ),
                ));
                let center_x = bounds.left() + bounds.size.width * saturation;
                let center_y = bounds.top() + bounds.size.height * (1.0 - value);
                window.paint_quad(quad(
                    Bounds::new(
                        point(center_x - px(5.0), center_y - px(5.0)),
                        size(px(10.0), px(10.0)),
                    ),
                    px(5.0),
                    transparent_black(),
                    px(2.0),
                    rgb(0xffffff),
                    BorderStyle::default(),
                ));
            }
            HsvFieldKind::HueStrip => {
                let anchors = [
                    0xff0000, 0xffff00, 0x00ff00, 0x00ffff, 0x0000ff, 0xff00ff, 0xff0000,
                ];
                let segment_width = bounds.size.width / 6.0;
                for index in 0_u8..6 {
                    let anchor = usize::from(index);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(
                                bounds.left() + segment_width * f32::from(index),
                                bounds.top(),
                            ),
                            size(segment_width, bounds.size.height),
                        ),
                        linear_gradient(
                            90.0,
                            linear_color_stop(rgb(anchors[anchor]), 0.0),
                            linear_color_stop(rgb(anchors[anchor + 1]), 1.0),
                        ),
                    ));
                }
                let thumb_x = bounds.left() + bounds.size.width * (hue / 360.0) - px(2.5);
                window.paint_quad(quad(
                    Bounds::new(
                        point(thumb_x, bounds.top()),
                        size(px(5.0), bounds.size.height),
                    ),
                    px(2.0),
                    transparent_black(),
                    px(2.0),
                    rgb(0xffffff),
                    BorderStyle::default(),
                ));
            }
        }

        let input = self.input.clone();
        let kind = self.kind;
        let pointer_hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if event.button == MouseButton::Left
                && phase == DispatchPhase::Capture
                && pointer_hitbox.is_hovered(window)
            {
                cx.stop_propagation();
                input.update(cx, |this, cx| {
                    let Some(picker) = this.editor.color_picker.as_mut() else {
                        return;
                    };
                    let width = f32::from(bounds.size.width).max(f32::EPSILON);
                    let height = f32::from(bounds.size.height).max(f32::EPSILON);
                    match kind {
                        HsvFieldKind::SquareSv => {
                            picker.saturation = (f32::from(event.position.x - bounds.left())
                                / width)
                                .clamp(0.0, 1.0);
                            picker.value = (1.0
                                - f32::from(event.position.y - bounds.top()) / height)
                                .clamp(0.0, 1.0);
                        }
                        HsvFieldKind::HueStrip => {
                            picker.hue = (f32::from(event.position.x - bounds.left()) / width
                                * 360.0)
                                .clamp(0.0, 360.0);
                        }
                    }
                    picker.hex = format!(
                        "{:06X}",
                        hsv_to_rgb(picker.hue, picker.saturation, picker.value)
                    );
                    picker.invalid = false;
                    picker.replace_on_type = false;
                    cx.notify();
                });
            }
        });

        let input = self.input.clone();
        let kind = self.kind;
        let pointer_hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if event.pressed_button == Some(MouseButton::Left)
                && phase == DispatchPhase::Capture
                && pointer_hitbox.is_hovered(window)
            {
                cx.stop_propagation();
                input.update(cx, |this, cx| {
                    let Some(picker) = this.editor.color_picker.as_mut() else {
                        return;
                    };
                    let width = f32::from(bounds.size.width).max(f32::EPSILON);
                    let height = f32::from(bounds.size.height).max(f32::EPSILON);
                    match kind {
                        HsvFieldKind::SquareSv => {
                            picker.saturation = (f32::from(event.position.x - bounds.left())
                                / width)
                                .clamp(0.0, 1.0);
                            picker.value = (1.0
                                - f32::from(event.position.y - bounds.top()) / height)
                                .clamp(0.0, 1.0);
                        }
                        HsvFieldKind::HueStrip => {
                            picker.hue = (f32::from(event.position.x - bounds.left()) / width
                                * 360.0)
                                .clamp(0.0, 360.0);
                        }
                    }
                    picker.hex = format!(
                        "{:06X}",
                        hsv_to_rgb(picker.hue, picker.saturation, picker.value)
                    );
                    picker.invalid = false;
                    picker.replace_on_type = false;
                    cx.notify();
                });
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

/// One shaped text run inside a cached terminal row.
///
/// Content depends only on (screen revision, columns, font metrics); selection,
/// cursor, and focus are painted as quads every frame and never cached.
pub(crate) struct CachedRun {
    pub(crate) x: f32,
    pub(crate) width: f32,
    pub(crate) background: Option<u32>,
    pub(crate) shaped: ShapedLine,
}

/// Shaped lines for one pane's current screen revision. Stored on `HhApp`
/// behind a `RefCell` because element prepaint only observes `&HhApp`.
pub(crate) struct PaneShapeCache {
    pub(crate) revision: u64,
    pub(crate) columns: u16,
    pub(crate) font_size: f32,
    pub(crate) cell_width: f32,
    pub(crate) rows: Rc<Vec<Vec<CachedRun>>>,
}

pub(crate) struct TerminalGridElement {
    pub(crate) input: Entity<HhApp>,
    pub(crate) pane_id: Uuid,
    pub(crate) metrics: TerminalCellMetrics,
    pub(crate) focused: bool,
    pub(crate) pane_accent: u32,
}

pub(crate) struct TerminalGridPrepaintState {
    rows: Rc<Vec<Vec<CachedRun>>>,
    selection: Option<TerminalSelection>,
    cursor: Option<TerminalCursor>,
    columns: u16,
}

/// Shapes every run of one screen with the same attribute mapping as
/// `render_terminal_run` (bold/dim/italic/underline/strikethrough, theme
/// colors, tab expansion), so a cached frame is pixel-identical to the
/// div-based path at revision-change time.
fn build_pane_shape_cache(
    app: &HhApp,
    screen: &TerminalScreen,
    metrics: TerminalCellMetrics,
    window: &mut Window,
) -> PaneShapeCache {
    let rows = screen
        .lines
        .iter()
        .map(|line| {
            let mut start_column = 0_u16;
            line.runs
                .iter()
                .filter_map(|run| {
                    let columns = run.columns;
                    let cached = cache_terminal_run(app, run, metrics, start_column, window);
                    start_column = start_column.saturating_add(columns);
                    cached
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    PaneShapeCache {
        revision: screen.revision,
        columns: screen.columns,
        font_size: metrics.font_size,
        cell_width: metrics.cell_width,
        rows: Rc::new(rows),
    }
}

fn cache_terminal_run(
    app: &HhApp,
    style: &TerminalRun,
    metrics: TerminalCellMetrics,
    start_column: u16,
    window: &mut Window,
) -> Option<CachedRun> {
    let bold = style.attributes.contains(TerminalAttributes::BOLD);
    let dim = style.attributes.contains(TerminalAttributes::DIM);
    let italic = style.attributes.contains(TerminalAttributes::ITALIC);
    let underline = style.attributes.contains(TerminalAttributes::UNDERLINE);
    let strikethrough = style.attributes.contains(TerminalAttributes::STRIKETHROUGH);
    let foreground = THEME.terminal_color(style.foreground, bold, dim);
    let background = (style.background != TerminalColor::DefaultBackground)
        .then(|| THEME.terminal_color(style.background, false, false));
    let span = metrics.span(start_column, style.columns);
    let text = if style.text.contains('\t') {
        terminal_run_display_text(style, start_column)
    } else {
        style.text.clone()
    };
    if text.is_empty() {
        return None;
    }
    let run = TextRun {
        len: text.len(),
        font: app.terminal_font.font(bold, italic),
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
    };
    let shaped = window.text_system().shape_line(
        text.into(),
        px(metrics.font_size),
        std::slice::from_ref(&run),
        None,
    );
    Some(CachedRun {
        x: span.x,
        width: span.width,
        background,
        shaped,
    })
}

impl IntoElement for TerminalGridElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalGridElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<TerminalGridPrepaintState>;

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
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let app = self.input.read(cx);
        let screen = app.session.screens.get(&self.pane_id)?;
        let mut cache = app.terminal_shape_cache.borrow_mut();
        let stale = match cache.get(&self.pane_id) {
            Some(entry) => {
                entry.revision != screen.revision
                    || entry.columns != screen.columns
                    || entry.font_size.to_bits() != self.metrics.font_size.to_bits()
                    || entry.cell_width.to_bits() != self.metrics.cell_width.to_bits()
            }
            None => true,
        };
        if stale {
            cache.insert(
                self.pane_id,
                build_pane_shape_cache(app, screen, self.metrics, window),
            );
        }
        let rows = cache.get(&self.pane_id)?.rows.clone();
        Some(TerminalGridPrepaintState {
            rows,
            selection: screen.selection,
            cursor: screen.cursor,
            columns: screen.columns,
        })
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
        let Some(state) = state.as_mut() else {
            return;
        };
        let metrics = self.metrics;
        let glyph_top = (metrics.baseline - metrics.ascent).max(0.0);
        let glyph_height = metrics.ascent + metrics.descent;
        for (row, row_runs) in state.rows.iter().enumerate() {
            let row_number = u16::try_from(row).unwrap_or(u16::MAX);
            let row_top = bounds.origin.y + px(f32::from(row_number) * metrics.line_height);
            if let Some((start, width)) = state
                .selection
                .and_then(|selection| selection_span(selection, row, state.columns))
            {
                let span = metrics.span(start, width);
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.origin.x + px(span.x), row_top),
                        size(px(span.width), px(span.height)),
                    ),
                    rgb(THEME.selection),
                ));
            }
            for run in row_runs {
                if let Some(background) = run.background {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.origin.x + px(run.x), row_top),
                            size(px(run.width), px(metrics.line_height)),
                        ),
                        rgb(background),
                    ));
                }
                let _ = run.shaped.paint(
                    point(bounds.origin.x + px(run.x), row_top + px(glyph_top)),
                    px(glyph_height),
                    window,
                    cx,
                );
            }
        }
        if let Some(cursor) = state.cursor {
            let span = metrics.span(cursor.column, 1);
            let cursor_bounds = Bounds::new(
                point(
                    bounds.origin.x + px(span.x),
                    bounds.origin.y + px(f32::from(cursor.row) * metrics.line_height),
                ),
                size(px(span.width), px(span.height)),
            );
            let accent = if self.focused {
                self.pane_accent
            } else {
                THEME.muted
            };
            let background = if self.focused {
                rgba((self.pane_accent << 8) | 0x30)
            } else {
                gpui::transparent_black().into()
            };
            window.paint_quad(quad(
                cursor_bounds,
                px(1.0),
                background,
                px(1.0),
                rgb(accent),
                gpui::BorderStyle::default(),
            ));
        }
    }
}
