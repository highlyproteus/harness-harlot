//! Terminal pane rendering: headers, lines, search, and drops.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, Point, StrikethroughStyle, StyledText, TextRun, UnderlineStyle, div, img, px,
    relative, rgb, rgba,
};
use gpui::{AppContext, ParentElement, StatefulInteractiveElement, Styled, StyledImage};
use hh_protocol::{
    AppearanceColor, DropPlacement, HistoryPageFlags, Pane, PaneLayout, SplitAxis,
    TerminalAttributes, TerminalColor, TerminalLine, TerminalRun, Workspace, WorkspaceConnection,
};
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use std::collections::HashSet;

use crate::browser::browser_command_available;
use crate::commands::AppCommand;
use crate::elements::TerminalPointerElement;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use crate::helpers::visible_panes;
use crate::helpers::{
    IDENTITY_MARK_SIZE, WorkspaceTabScope, collect_terminal_tabs, composite_rgb,
    effective_split_ratio, element_key, find_pane, plain_history_line, selection_span,
    split_child_dimensions, split_control_id, split_element_key, split_placement_at,
    split_target_for_drag, split_target_for_drag_ids, tab_identity_presentation,
    terminal_run_display_text, terminal_tab_secondary_label, workspace_layout_for_focused_pane,
    workspace_strip_active_tab, workspace_tab_focus_target, workspace_tab_set,
    workspace_tab_standalone_pane, zoom_projection,
};
use crate::typography::TerminalCellMetrics;
use crate::view_models::{
    CreateMenu, CreateMenuTarget, DragDestination, Modal, PaneControlIcon, PaneDrag, ResizeDrag,
    SearchEditor, SplitControlId, TabDrag, TerminalLineRender, TooltipView, WorkspaceDrag,
};
use crate::{HhApp, PANE_HEADER_HEIGHT, TAB_COLOR_ALPHA, THEME};
use uuid::Uuid;

impl HhApp {
    pub(crate) fn render_pane_header(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let merge_preview = self.layout.drag_hover.merges_into(active);
        let active_accent = self.terminal_accent(active).as_rgb();
        let terminal_controls = panes
            .iter()
            .find(|pane| pane.id == active)
            .is_some_and(|pane| pane.kind.is_terminal());
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
                        this.layout.dragging_pane = Some(event.drag(cx).pane_id);
                        this.layout.drag_hover.enter(DragDestination::Merge {
                            target_pane: active,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }
                },
            ))
            .on_drag_move::<WorkspaceDrag>(cx.listener(
                |this, event: &gpui::DragMoveEvent<WorkspaceDrag>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        this.sidebar.dragging_workspace = Some(event.drag(cx).workspace_id);
                        this.sidebar.workspace_drop_preview = None;
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
                    .children(self.render_pane_header_controls(panes, active, cx)),
            )
            .when(browser_command_available(), |element| {
                element.child(self.pane_control(
                    active,
                    "new-browser-tab",
                    PaneControlIcon::Web,
                    "New browser in this group",
                    cx,
                    |this, _pane_id, cx| this.new_browser_tab(cx),
                ))
            })
            .when(terminal_controls, |element| {
                element
                    .child(self.pane_control(
                        active,
                        "new-tab",
                        PaneControlIcon::Add,
                        "New terminal in this group",
                        cx,
                        HhApp::new_tab_at,
                    ))
                    .child(self.pane_control(
                        active,
                        "split-right",
                        PaneControlIcon::SplitRight,
                        "Split right (⌘D)",
                        cx,
                        |this, pane_id, cx| {
                            this.split_at(pane_id, SplitAxis::Horizontal, cx);
                        },
                    ))
                    .child(self.pane_control(
                        active,
                        "split-down",
                        PaneControlIcon::SplitDown,
                        "Split down (⇧⌘D)",
                        cx,
                        |this, pane_id, cx| {
                            this.split_at(pane_id, SplitAxis::Vertical, cx);
                        },
                    ))
            })
            .into_any_element()
    }

    fn render_pane_header_controls(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        panes
            .into_iter()
            .map(|pane| {
                let pane_id = pane.id;
                let identity = tab_identity_presentation(&pane);
                let identity_detail = identity.detail.clone();
                let secondary_label = terminal_tab_secondary_label(&pane).map(str::to_owned);
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
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.activate_tab(pane_id, cx);
                        cx.stop_propagation();
                    }))
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
                            .child(self.render_pane_identity_mark(
                                &pane,
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
                    .when_some(secondary_label, |element, label| {
                        element.child(
                            div()
                                .min_w(px(0.0))
                                .flex_shrink()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font_family("SF Mono")
                                .text_size(px(9.5))
                                .text_color(rgb(THEME.dim))
                                .child(label),
                        )
                    })
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
                    .into_any_element()
            })
            .collect()
    }

    pub(crate) fn pane_control(
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

    pub(crate) fn render_control_icon(&self, icon: PaneControlIcon) -> AnyElement {
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
            PaneControlIcon::Web => div()
                .relative()
                .w(px(13.0))
                .h(px(13.0))
                .rounded_full()
                .border_1()
                .border_color(rgb(THEME.muted))
                .child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(5.0))
                        .w_full()
                        .h(px(1.0))
                        .bg(rgb(THEME.muted)),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(3.5))
                        .top(px(0.0))
                        .w(px(4.0))
                        .h_full()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(THEME.muted)),
                )
                .into_any_element(),
        }
    }

    pub(crate) fn render_terminal(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.layout.focused_pane == Some(active);
        let terminal_accent = self.terminal_accent(active).as_rgb();
        let metrics = self.terminal_metrics(active);
        let screen = self.session.screens.get(&active);
        let archived = self.editor.archived_views.get(&active);
        let exited = self
            .session
            .pane_states
            .get(&active)
            .is_some_and(|state| state.exited);
        let drop_target = self
            .layout
            .dragging_pane
            .and_then(|source| split_target_for_drag(source, &panes, active));
        let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        let tab_pane_ids = pane_ids.clone();
        let rendered_lines = if let (Some(view), Some(screen)) = (archived, screen) {
            view.page
                .lines
                .iter()
                .skip(view.first_line)
                .take(usize::from(screen.rows))
                .map(|line| plain_history_line(line))
                .enumerate()
                .map(|(row, line)| {
                    self.render_terminal_line(
                        &line,
                        TerminalLineRender {
                            row,
                            cursor: None,
                            focused,
                            pane_id: active,
                            columns: screen.columns,
                            selection: None,
                        },
                        metrics,
                        cx,
                    )
                })
                .collect::<Vec<_>>()
        } else {
            screen
                .map(|screen| {
                    screen
                        .lines
                        .iter()
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
                                metrics,
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
                this.focus_pane_with_snapshot(active, cx);
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
                    this.layout.dragging_pane = Some(source);
                    if let Some(target_pane) = split_target_for_drag_ids(source, &pane_ids, active)
                        && let Some(placement) =
                            split_placement_at(event.event.position, event.bounds)
                    {
                        this.layout.drag_hover.enter(DragDestination::Split {
                            target_pane,
                            placement,
                        });
                    }
                    cx.notify();
                },
            ))
            .on_drag_move::<TabDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let Some(source) = event.drag(cx).pane_id else {
                        return;
                    };
                    this.layout.dragging_pane = Some(source);
                    if let Some(target_pane) =
                        split_target_for_drag_ids(source, &tab_pane_ids, active)
                        && let Some(placement) =
                            split_placement_at(event.event.position, event.bounds)
                    {
                        this.layout.drag_hover.enter(DragDestination::Split {
                            target_pane,
                            placement,
                        });
                    }
                    cx.notify();
                },
            ))
            .when(show_pane_header, |element| {
                element.child(self.render_pane_header(panes, active, cx))
            })
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
                    .text_size(px(metrics.font_size))
                    .line_height(px(metrics.line_height))
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
                            && self.editor.modal.search().is_none()
                            && self.editor.modal.pane_rename().is_none()
                            && !self.editor.ime_preedit.is_empty(),
                        |element| {
                            let cursor = screen.and_then(|screen| screen.cursor);
                            element.when_some(cursor, |element, cursor| {
                                let span = metrics.span(cursor.column, 1);
                                element.child(
                                    div()
                                        .absolute()
                                        .left(px(span.x))
                                        .top(px(f32::from(cursor.row) * metrics.line_height))
                                        .font(self.terminal_font.font(false, false))
                                        .text_size(px(metrics.font_size))
                                        .text_color(rgb(THEME.foreground))
                                        .border_b_1()
                                        .border_color(rgb(terminal_accent))
                                        .child(self.editor.ime_preedit.clone()),
                                )
                            })
                        },
                    )
                    .when_some(
                        self.editor.modal.search().filter(|_| focused),
                        |element, editor| element.child(self.render_search_bar(editor)),
                    )
                    .when(exited, |element| {
                        element.child(self.render_pane_reattach_notice(active, cx))
                    })
                    .when_some(drop_target, |element, target| {
                        element.child(self.render_drop_layer(target, cx))
                    }),
            )
            .into_any_element()
    }

    /// A pane whose process exited keeps its last frame but swallows every
    /// keystroke, which is indistinguishable from a hung terminal. Say so, and
    /// offer the one-click recovery.
    pub(crate) fn render_pane_reattach_notice(
        &self,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .absolute()
            .bottom(px(8.0))
            .left(px(8.0))
            .right(px(8.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .flex_1()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child("This terminal exited — input goes nowhere until it reattaches."),
            )
            .child(
                div()
                    .id(("reattach-pane", element_key(pane_id)))
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.accent))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(0xffffff))
                    .on_click(cx.listener(move |this, _, _, cx| this.reattach_pane(pane_id, cx)))
                    .child("Reattach"),
            )
            .into_any_element()
    }

    pub(crate) fn render_terminal_line(
        &self,
        line: &TerminalLine,
        render: TerminalLineRender,
        metrics: TerminalCellMetrics,
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
            .iter()
            .map(|style| {
                let columns = style.columns;
                let element = self.render_terminal_run(style, metrics, start_column, columns);
                start_column = start_column.saturating_add(columns);
                element
            })
            .collect::<Vec<_>>();
        let cursor_column = cursor
            .filter(|cursor| usize::from(cursor.row) == row)
            .map(|cursor| cursor.column);
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

    pub(crate) fn render_search_bar(&self, editor: &SearchEditor) -> AnyElement {
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
                    .child(
                        if editor.query.is_empty() && self.editor.ime_preedit.is_empty() {
                            "Type to search…".to_owned()
                        } else {
                            format!("{}{}", editor.query, self.editor.ime_preedit)
                        },
                    ),
            )
            .child(if editor.no_match {
                "No match"
            } else {
                "↵ next"
            })
            .into_any_element()
    }

    pub(crate) fn render_terminal_run(
        &self,
        style: &TerminalRun,
        metrics: TerminalCellMetrics,
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
        let span = metrics.span(start_column, columns);
        let glyph_top = (metrics.baseline - metrics.ascent).max(0.0);
        let glyph_height = metrics.ascent + metrics.descent;
        let text = if style.text.contains('\t') {
            terminal_run_display_text(style, start_column)
        } else {
            style.text.clone()
        };
        let text_len = text.len();
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
                    .child(StyledText::new(text).with_runs(vec![TextRun {
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

    pub(crate) fn render_drop_layer(
        &self,
        target_pane: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let preview = self.layout.drag_hover.split_for(target_pane);
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

    pub(crate) fn render_drop_zone(
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
            .on_drop(cx.listener(move |this, info: &TabDrag, _, cx| {
                if let Some(source_pane) = info.pane_id {
                    this.move_pane_to_split(source_pane, target_pane, placement, cx);
                    cx.stop_propagation();
                }
            }))
            .into_any_element()
    }

    pub(crate) fn render_layout(
        &mut self,
        layout: PaneLayout,
        width: f32,
        height: f32,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match layout {
            PaneLayout::Leaf { pane } => {
                if pane.kind.is_browser() {
                    self.render_browser_workspace(
                        &pane,
                        vec![pane.clone()],
                        width,
                        height,
                        show_pane_header,
                        cx,
                    )
                } else {
                    let active = pane.id;
                    self.render_terminal(vec![pane], active, show_pane_header, cx)
                }
            }
            PaneLayout::Stack { panes, active } => {
                if let Some(pane) = panes
                    .iter()
                    .find(|pane| pane.id == active)
                    .filter(|pane| pane.kind.is_browser())
                    .cloned()
                {
                    self.render_browser_workspace(&pane, panes, width, height, show_pane_header, cx)
                } else {
                    self.render_terminal(panes, active, show_pane_header, cx)
                }
            }
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
                    self.layout
                        .split_ratios
                        .get(&split_id)
                        .copied()
                        .unwrap_or(ratio),
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
                            .child(self.render_layout(*first, first_width, first_height, true, cx)),
                    )
                    .child(self.render_divider(split_id, axis, cx))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex_1()
                            .child(self.render_layout(
                                *second,
                                second_width,
                                second_height,
                                true,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
        }
    }

    pub(crate) fn render_divider(
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
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.layout.resizing = Some(ResizeDrag { split_id, axis });
                    this.focus_handle.focus(window);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    pub(crate) fn binding_label(&self, command: AppCommand) -> String {
        self.keymap
            .bindings
            .iter()
            .filter(|binding| binding.command == command)
            .map(|binding| binding.sequence.as_str())
            .collect::<Vec<_>>()
            .join("  ")
    }

    pub(crate) fn render_workspace_tab_strip(
        &self,
        workspace: &Workspace,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = workspace.id;
        let tab_set = workspace_tab_set(workspace, self.sidebar.workspace_tab_scope);
        let scope = tab_set.scope;
        let target_tab = match scope {
            WorkspaceTabScope::Workstation => None,
            WorkspaceTabScope::Project(project_id) => Some(project_id),
        };
        let active_tab = workspace_strip_active_tab(workspace, scope, self.layout.focused_pane);
        let tabs = tab_set
            .tabs
            .into_iter()
            .filter(|tab| !self.sidebar.dismissed_workspace_tabs.contains(&tab.id))
            .map(|tab| {
                let pane_id = workspace_tab_focus_target(tab, self.layout.focused_pane);
                let active = active_tab == Some(tab.id);
                let standalone_pane = workspace_tab_standalone_pane(tab);
                let is_standalone = standalone_pane.is_some();
                let label = standalone_pane.map_or_else(
                    || {
                        tab.custom_title
                            .clone()
                            .unwrap_or_else(|| tab.title.clone())
                    },
                    |pane| tab_identity_presentation(pane).label,
                );
                let icon = if let Some(pane) = standalone_pane {
                    let accent = pane
                        .color
                        .unwrap_or_else(|| self.terminal_accent(pane.id))
                        .as_rgb();
                    div()
                        .flex_none()
                        .w(px(IDENTITY_MARK_SIZE))
                        .h(px(IDENTITY_MARK_SIZE))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(self.render_pane_identity_mark(
                            pane,
                            if active {
                                THEME.foreground
                            } else {
                                THEME.muted
                            },
                            accent,
                        ))
                        .into_any_element()
                } else if let Some(path) = tab
                    .custom_icon
                    .as_deref()
                    .and_then(|icon| self.custom_icon_path(icon))
                {
                    img(path)
                        .flex_none()
                        .w(px(13.0))
                        .h(px(13.0))
                        .object_fit(gpui::ObjectFit::Contain)
                        .rounded(px(2.0))
                        .into_any_element()
                } else {
                    div()
                        .flex_none()
                        .w(px(7.0))
                        .h(px(7.0))
                        .rounded_full()
                        .bg(rgb(tab.color.map_or(THEME.dim, AppearanceColor::as_rgb)))
                        .into_any_element()
                };
                let pane_count = {
                    let mut panes = Vec::new();
                    collect_terminal_tabs(&tab.layout, &mut panes);
                    panes.len()
                };
                let tab_id = tab.id;
                let close_tooltip = if is_standalone {
                    format!("Close {label}…")
                } else {
                    format!("Remove {label} from top bar")
                };
                let background = tab.color.map_or(
                    if active {
                        THEME.elevated
                    } else {
                        THEME.surface
                    },
                    |color| composite_rgb(color.as_rgb(), THEME.surface, TAB_COLOR_ALPHA),
                );
                div()
                    .id(("workspace-strip-tab", element_key(tab_id)))
                    .group("workspace-strip-tab")
                    .h_full()
                    .flex_shrink()
                    .overflow_hidden()
                    .min_w(px(64.0))
                    .max_w(px(220.0))
                    .px(px(9.0))
                    .border_r_1()
                    .border_color(rgb(THEME.border))
                    .bg(rgb(background))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .when(active, |element| {
                        element.border_b_2().border_color(rgb(THEME.accent))
                    })
                    .hover(|element| element.bg(rgb(THEME.elevated)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_workspace_tab_root(workspace_id, tab_id, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            if !is_standalone {
                                return;
                            }
                            let Some(pane_id) = pane_id else {
                                return;
                            };
                            this.open_tab_menu(pane_id, event.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(icon)
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .whitespace_nowrap()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .font_weight(if active {
                                gpui::FontWeight::SEMIBOLD
                            } else {
                                gpui::FontWeight::NORMAL
                            })
                            .text_color(rgb(if active {
                                THEME.foreground
                            } else {
                                THEME.muted
                            }))
                            .child(label),
                    )
                    .when(pane_count > 1, |element| {
                        element.child(
                            div()
                                .flex_none()
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child(pane_count.to_string()),
                        )
                    })
                    .child(
                        div()
                            .id(("close-workspace-strip-tab", element_key(tab_id)))
                            .flex_none()
                            .w(px(16.0))
                            .h(px(16.0))
                            .rounded(px(3.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .hover(|element| {
                                element
                                    .bg(rgb(THEME.accent_soft))
                                    .text_color(rgb(THEME.foreground))
                            })
                            .tooltip(move |_, cx| {
                                cx.new(|_| TooltipView {
                                    text: close_tooltip.clone(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dismiss_workspace_tab(tab_id, cx);
                                cx.stop_propagation();
                            }))
                            .child("×"),
                    )
                    .into_any_element()
            });
        div()
            .id(("workspace-tab-strip", element_key(workspace_id)))
            .h(px(32.0))
            .flex_none()
            .border_b_1()
            .border_color(rgb(THEME.border_strong))
            .bg(rgb(THEME.surface))
            .flex()
            .items_center()
            .child(
                div()
                    .id(("workspace-strip-scroll", element_key(workspace_id)))
                    .min_w(px(0.0))
                    .h_full()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .children(tabs),
            )
            .child(
                div()
                    .id(("new-workspace-strip-tab", element_key(workspace_id)))
                    .flex_none()
                    .w(px(31.0))
                    .h_full()
                    .border_l_1()
                    .border_color(rgb(THEME.border))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Add project, terminal, browser, or group".to_owned(),
                        })
                        .into()
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.editor.modal = Modal::CreateMenu(CreateMenu {
                            position: event.position(),
                            target: CreateMenuTarget::TabStrip {
                                workspace_id,
                                target_tab,
                            },
                        });
                        cx.notify();
                    }))
                    .child("+"),
            )
            .into_any_element()
    }

    pub(crate) fn render_workspace(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = &self.session.snapshot else {
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
        let workspace_tab_strip = self.render_workspace_tab_strip(workspace, cx);
        // A workstation owns several top-level terminal tabs. The service
        // validates activation by pane ID, while the desktop owns the visible
        // tab selection through `focused_pane`. Rendering the first tab here
        // hid every later (including runtime-only tmux) tab even after a
        // successful sidebar click, so route the viewport to the tab that
        // contains the focused pane instead.
        let canonical_layout =
            workspace_layout_for_focused_pane(workspace, self.layout.focused_pane).cloned();
        let standalone_root = self.layout.focused_pane.is_some_and(|pane_id| {
            workspace
                .tabs
                .iter()
                .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                .is_some_and(|tab| workspace_tab_standalone_pane(tab).is_some())
        });
        let layout = canonical_layout.as_ref().map(|layout| {
            self.layout
                .zoomed_pane
                .and_then(|pane_id| zoom_projection(layout, pane_id))
                .unwrap_or_else(|| layout.clone())
        });
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        let mut visible_browsers: HashSet<Uuid> = layout
            .as_ref()
            .map(|layout| {
                visible_panes(layout)
                    .into_iter()
                    .filter(|pane_id| {
                        find_pane(layout, *pane_id).is_some_and(|pane| pane.kind.is_browser())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let workspace_content = if let Some(layout) = layout {
            self.render_layout(
                layout,
                self.layout.workspace_pixels.0,
                self.layout.workspace_pixels.1,
                !standalone_root,
                cx,
            )
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
                                    "Open a fresh remote terminal with this workstation's saved system OpenSSH destination."
                                } else {
                                    "This workstation is saved and ready when you want another local shell."
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
        let showing_appearance_settings = matches!(self.editor.modal, Modal::AppearanceSettings);
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        if showing_appearance_settings {
            visible_browsers.clear();
        }
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        self.sync_browser_view_presentation(&visible_browsers);
        let workspace_content = if showing_appearance_settings {
            self.render_appearance_settings(cx)
        } else {
            workspace_content
        };
        div()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .flex_1()
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .child(workspace_tab_strip)
            .child(div().min_h(px(0.0)).flex_1().child(workspace_content))
            .into_any_element()
    }
}
