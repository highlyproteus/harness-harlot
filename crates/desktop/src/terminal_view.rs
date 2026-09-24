//! Terminal pane rendering: headers, search, and drops.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, CursorStyle, ExternalPaths, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, Point, div, px, relative, rgb, rgba,
};
use gpui::{AppContext, ParentElement, StatefulInteractiveElement, Styled};
use hh_protocol::{
    ClientRequest, DropPlacement, Pane, PaneLayout, PaneStatus, SplitAxis, WorkspaceConnection,
};

use crate::browser::browser_command_available;
use crate::commands::AppCommand;
use crate::elements::{TerminalGridElement, TerminalPointerElement};
use crate::helpers::{
    IDENTITY_MARK_SIZE, effective_split_ratio, element_key, find_pane, identity_detail,
    identity_label, split_child_dimensions, split_control_id, split_element_key,
    split_placement_at, split_target_for_drag, split_target_for_drag_ids,
    terminal_tab_secondary_label, workspace_layout_for_focused_pane, workspace_tab_standalone_pane,
    zoom_projection,
};
use crate::view_models::{
    DragDestination, Modal, PaneControlIcon, PaneDrag, ResizeDrag, SearchEditor, SplitControlId,
    TabDrag, TooltipView, WorkspaceDrag,
};
use crate::{HhApp, PANE_HEADER_HEIGHT, TERMINAL_BOTTOM_GUARD, THEME, pane_status_color};
use uuid::Uuid;

impl HhApp {
    pub(crate) fn render_pane_header(
        &self,
        panes: &[Pane],
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
        panes: &[Pane],
        active: Uuid,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        panes
            .iter()
            .map(|pane| {
                let pane_id = pane.id;
                let label = identity_label(pane);
                let input = cx.entity();
                let secondary_label = terminal_tab_secondary_label(pane).map(str::to_owned);
                let selected = pane_id == active;
                let status = pane.status;
                let status_color = pane_status_color(status);
                let pane_accent = pane
                    .color
                    .unwrap_or_else(|| self.terminal_accent(pane_id))
                    .as_rgb();
                let close_tooltip = format!("Close {label}…");
                let drag = PaneDrag {
                    pane_id,
                    title: label.to_owned(),
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
                                let text = input
                                    .read(cx)
                                    .pane_metadata(pane_id)
                                    .as_ref()
                                    .map(identity_detail)
                                    .unwrap_or_default();
                                cx.new(|_| TooltipView { text }).into()
                            })
                            .child(self.render_pane_identity_mark(
                                pane,
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
                            .child(label.to_owned()),
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
                    .when(status != PaneStatus::Idle, |element| {
                        element.child(
                            div()
                                .flex_none()
                                .w(px(7.0))
                                .h(px(7.0))
                                .rounded_full()
                                .bg(rgb(status_color.expect("non-idle status has a color"))),
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

    #[allow(clippy::too_many_lines)]
    pub(crate) fn render_terminal(
        &self,
        panes: &[Pane],
        active: Uuid,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.layout.focused_pane == Some(active);
        let terminal_accent = self.terminal_accent(active).as_rgb();
        let metrics = self.terminal_metrics(active);
        let screen = self.session.screens.get(&active);
        let exited = self
            .session
            .pane_states
            .get(&active)
            .is_some_and(|state| state.exited);
        // A bot tab is a single pane; nothing may split into it.
        let drop_target = self
            .layout
            .dragging_pane
            .filter(|_| !self.pane_is_bot(active))
            .and_then(|source| split_target_for_drag(source, panes, active));
        let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        let tab_pane_ids = pane_ids.clone();
        // Live screens use cached glyphs and one pointer surface per pane.
        let terminal_grid = screen.map(|screen| {
            div()
                .size_full()
                .child(TerminalGridElement {
                    input: cx.entity(),
                    pane_id: active,
                    metrics,
                    focused,
                    pane_accent: terminal_accent,
                })
                .child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(0.0))
                        .size_full()
                        .child(TerminalPointerElement {
                            input: cx.entity(),
                            pane_id: active,
                            rows: screen.rows,
                            columns: screen.columns,
                            cell_width: metrics.cell_width,
                            line_height: metrics.line_height,
                        }),
                )
                .into_any_element()
        });
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
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                this.focus_pane_with_snapshot(active, cx);
                this.focus_handle.focus(window);
                this.paste_paths_to_terminal(active, paths.paths().to_vec(), cx);
                cx.stop_propagation();
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
                    .pt(px(6.0))
                    .pb(px(6.0 + TERMINAL_BOTTOM_GUARD))
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
                    .when_some(terminal_grid, |element, grid| element.child(grid))
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
                    .when_some(
                        screen.filter(|screen| screen.display_offset > 0),
                        |element, screen| {
                            let jump = -i32::try_from(screen.display_offset).unwrap_or(i32::MAX);
                            element.child(
                                div()
                                    .id(("scroll-bottom", element_key(active)))
                                    .absolute()
                                    .right(px(8.0))
                                    .bottom(px(8.0))
                                    .px(px(9.0))
                                    .h(px(24.0))
                                    .rounded(px(6.0))
                                    .bg(rgb(THEME.elevated))
                                    .border_1()
                                    .border_color(rgb(THEME.border_strong))
                                    .shadow_lg()
                                    .occlude()
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.foreground))
                                    .child(format!("↓ {} lines", screen.display_offset))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.dispatch_control(ClientRequest::ScrollPane {
                                            pane_id: active,
                                            lines: jump,
                                        });
                                        cx.stop_propagation();
                                    })),
                            )
                        },
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
        &self,
        layout: &PaneLayout,
        width: f32,
        height: f32,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match layout {
            PaneLayout::Leaf { pane } => {
                if pane.kind.is_browser() {
                    self.render_browser_workspace(
                        pane,
                        std::slice::from_ref(pane),
                        width,
                        height,
                        show_pane_header,
                        cx,
                    )
                } else if pane.kind.is_gallery() {
                    self.render_gallery_pane(pane, std::slice::from_ref(pane), show_pane_header, cx)
                } else {
                    let active = pane.id;
                    self.render_terminal(std::slice::from_ref(pane), active, show_pane_header, cx)
                }
            }
            PaneLayout::Stack { panes, active } => {
                if let Some(pane) = panes
                    .iter()
                    .find(|pane| pane.id == *active)
                    .filter(|pane| pane.kind.is_browser())
                {
                    self.render_browser_workspace(
                        pane,
                        panes.as_slice(),
                        width,
                        height,
                        show_pane_header,
                        cx,
                    )
                } else if let Some(pane) = panes
                    .iter()
                    .find(|pane| pane.id == *active)
                    .filter(|pane| pane.kind.is_gallery())
                {
                    self.render_gallery_pane(pane, panes.as_slice(), show_pane_header, cx)
                } else {
                    self.render_terminal(panes.as_slice(), *active, show_pane_header, cx)
                }
            }
            PaneLayout::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let split_id = split_control_id(first, second);
                let ratio = effective_split_ratio(
                    *axis,
                    width,
                    height,
                    self.layout
                        .split_ratios
                        .get(&split_id)
                        .copied()
                        .unwrap_or(*ratio),
                );
                let vertical = *axis == SplitAxis::Vertical;
                let (first_width, first_height, second_width, second_height) =
                    split_child_dimensions(*axis, width, height, ratio);
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
                            .child(self.render_layout(first, first_width, first_height, true, cx)),
                    )
                    .child(self.render_divider(split_id, *axis, cx))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex_1()
                            .child(self.render_layout(
                                second,
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

    pub(crate) fn render_workspace(&self, cx: &mut Context<Self>) -> AnyElement {
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
        if matches!(self.editor.modal, Modal::AppearanceSettings) {
            // Settings replaces the whole main area: no tab strip, bot
            // header, or pane stays visible behind it.
            return div()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .h_full()
                .flex_1()
                .bg(rgb(THEME.terminal))
                .child(self.render_appearance_settings(cx))
                .into_any_element();
        }
        let Some(workspace) = self.active_workspace_in(snapshot) else {
            return div().size_full().bg(rgb(THEME.terminal)).into_any_element();
        };
        if workspace.is_bots() {
            return self.render_bot_view(workspace, cx);
        }
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
            workspace_layout_for_focused_pane(workspace, self.layout.focused_pane);
        let standalone_root = self.layout.focused_pane.is_some_and(|pane_id| {
            workspace
                .tabs
                .iter()
                .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                .is_some_and(|tab| workspace_tab_standalone_pane(tab).is_some())
        });
        let zoomed_layout = canonical_layout.and_then(|layout| {
            self.layout
                .zoomed_pane
                .and_then(|pane_id| zoom_projection(layout, pane_id))
        });
        let layout = zoomed_layout.as_ref().or(canonical_layout);
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
