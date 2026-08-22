//! Workspace top-tab strip rendering and drag/drop interactions.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Point, StatefulInteractiveElement, Styled, StyledImage, div,
    img, px, rgb,
};
use hh_protocol::{AppearanceColor, Workspace};
use std::time::Instant;

use crate::helpers::{
    IDENTITY_MARK_SIZE, WorkspaceTabScope, click_suppression_active, collect_terminal_tabs,
    composite_rgb, element_key, tab_identity_presentation, workspace_strip_active_tab,
    workspace_tab_focus_target, workspace_tab_set, workspace_tab_standalone_pane,
};
use crate::view_models::{
    CreateMenu, CreateMenuTarget, Modal, TabDrag, TabDropPreview, TooltipView,
};
use crate::{HhApp, TAB_COLOR_ALPHA, THEME, WORKSPACE_TAB_STRIP_HEIGHT};

impl HhApp {
    #[allow(clippy::too_many_lines)]
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
                // The top bar mirrors the sidebar's drag contract: strip tabs
                // reorder against each other and drag into terminal views to
                // split, sharing the same TabDrag payload and drop preview.
                let strip_drag = TabDrag {
                    workspace_id,
                    tab_id,
                    pane_id,
                    from_group: false,
                    title: label.clone(),
                    position: Point::default(),
                };
                let drop_before = self.sidebar.tab_drop_preview.is_some_and(|preview| {
                    preview.target_tab_id == tab_id && !preview.into_group && !preview.after
                });
                let drop_after = self.sidebar.tab_drop_preview.is_some_and(|preview| {
                    preview.target_tab_id == tab_id && !preview.into_group && preview.after
                });
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
                    .when(drop_before, |element| {
                        element.border_l_2().border_color(rgb(THEME.accent))
                    })
                    .when(drop_after, |element| {
                        element.border_r_2().border_color(rgb(THEME.accent))
                    })
                    .hover(|element| element.bg(rgb(THEME.elevated)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if click_suppression_active(
                            &mut this.sidebar.suppress_tab_click_until,
                            Instant::now(),
                        ) {
                            cx.notify();
                            return;
                        }
                        this.select_workspace_tab_root(workspace_id, tab_id, cx);
                    }))
                    .on_drag(strip_drag, |info: &TabDrag, position, _, cx| {
                        cx.new(|_| TabDrag {
                            position,
                            ..info.clone()
                        })
                    })
                    .on_drag_move::<TabDrag>(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                            let drag = event.drag(cx);
                            if drag.workspace_id != workspace_id
                                || (drag.tab_id == tab_id && !drag.from_group)
                            {
                                if this.sidebar.tab_drop_preview.take().is_some() {
                                    cx.notify();
                                }
                                return;
                            }
                            if event.bounds.contains(&event.event.position) {
                                this.sidebar.tab_drop_preview = Some(TabDropPreview {
                                    target_tab_id: tab_id,
                                    after: event.event.position.x > event.bounds.center().x,
                                    into_group: false,
                                });
                                cx.stop_propagation();
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(move |this, info: &TabDrag, _, cx| {
                        if info.workspace_id == workspace_id {
                            let after = this.sidebar.tab_drop_preview.is_some_and(|preview| {
                                preview.target_tab_id == tab_id && preview.after
                            });
                            if let Some(source_pane) = info.pane_id.filter(|_| info.from_group) {
                                this.move_sidebar_pane_to_new_tab(
                                    source_pane,
                                    tab_id,
                                    after,
                                    None,
                                    cx,
                                );
                            } else if info.tab_id != tab_id {
                                this.reorder_workspace_tab(info.tab_id, tab_id, after, cx);
                            }
                        }
                        cx.stop_propagation();
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
            .h(px(WORKSPACE_TAB_STRIP_HEIGHT))
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
}
