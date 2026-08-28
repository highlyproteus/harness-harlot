//! The scrollable workstation list and one workstation's card.
use crate::elements::SidebarPaneRowContext;
use crate::helpers::{
    HeaderDropZone, SidebarSection, WorkstationTabEntry, abbreviate_home, click_suppression_active,
    element_key, header_drop_zone, partition_workstation_entries, readable_text_color,
    terminal_tab_count_label, workspace_tab_entries, workspace_terminal_tabs,
};
use crate::view_models::{
    TabDrag, TabDropPreview, TooltipView, WorkspaceDrag, WorkspaceDropPreview,
};
use crate::{HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    Point, div, img, px, rgb, rgba,
};
use gpui::{AppContext, ParentElement, StatefulInteractiveElement, Styled, StyledImage};
use hh_protocol::{
    AppearanceColor, Pane, Workspace, WorkspaceConnection, WorkspaceConnectionStatus,
};
use std::time::Instant;
use uuid::Uuid;

struct TabRowEntry {
    tab_id: Uuid,
    group_label: Option<String>,
    project_dir: Option<String>,
    tab_color: Option<AppearanceColor>,
    custom_icon: Option<String>,
    parent_tab: Option<Uuid>,
    panes: Vec<Pane>,
    section: Option<(SidebarSection, bool)>,
}

struct WorkspaceGroupRow {
    tab_id: Uuid,
    label: String,
    project_dir: Option<String>,
    tab_color: Option<AppearanceColor>,
    custom_icon: Option<String>,
    panes: Vec<Pane>,
    group_indent: f32,
    pane_indent: f32,
    tab_active: bool,
    tab_focus_target: Option<Uuid>,
    is_project: bool,
}

#[allow(clippy::struct_excessive_bools)]
struct WorkspaceSectionCtx {
    workspace_id: Uuid,
    is_assistant: bool,
    index: usize,
    pinned: bool,
    active: bool,
    offline: bool,
    connected: bool,
    expanded: bool,
    terminal_count: usize,
    card_color: u32,
    active_text: u32,
    workspace_title: String,
    workspace_dir: Option<String>,
    custom_icon: Option<String>,
    drop_above: bool,
    drop_below: bool,
}

impl HhApp {
    /// The scrollable workstation list, or the empty-state hint.
    pub(crate) fn render_workstation_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut workspaces = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.workspaces.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        workspaces.sort_by_key(|workspace| (!workspace.pinned, workspace.order));
        let has_workspaces = !workspaces.is_empty();
        div()
            .id("sidebar-workstation-list")
            .min_h(px(0.0))
            .flex_1()
            .overflow_y_scroll()
            .children(
                workspaces
                    .into_iter()
                    .enumerate()
                    .map(|(index, workspace)| self.render_workspace_section(index, workspace, cx)),
            )
            .when(!has_workspaces, |element| {
                element.child(
                    div()
                        .px(px(14.0))
                        .pb(px(6.0))
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child("No workstations yet — use Add workstation above"),
                )
            })
            .into_any_element()
    }

    /// One workstation card with its tabs, groups, and terminal rows.
    pub(crate) fn render_workspace_section(
        &self,
        index: usize,
        workspace: &Workspace,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pinned = workspace.pinned;
        let active = Some(workspace.id) == self.sidebar.active_workspace;
        let workspace_id = workspace.id;
        let offline = matches!(
            &workspace.connection,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            }
        );
        let connected = matches!(
            &workspace.connection,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Connected,
                ..
            }
        );
        let workspace_title = workspace.title.clone();
        let workspace_dir = workspace.working_dir.as_deref().map(abbreviate_home);
        let (pinned_entries, project_entries, floating_entries) =
            partition_workstation_entries(workspace_tab_entries(workspace));
        let flatten_entries = |entries: Vec<WorkstationTabEntry<'_>>,
                               section: Option<SidebarSection>| {
            entries
                .into_iter()
                .enumerate()
                .flat_map(move |(entry_index, entry)| {
                    let parent_id = entry.tab_id;
                    let mut flattened = vec![TabRowEntry {
                        tab_id: entry.tab_id,
                        group_label: entry.group_label,
                        project_dir: entry.project_dir,
                        tab_color: entry.color,
                        custom_icon: entry.custom_icon,
                        parent_tab: None,
                        panes: entry.panes.into_iter().cloned().collect::<Vec<_>>(),
                        section: section.map(|section| (section, entry_index == 0)),
                    }];
                    flattened.extend(entry.children.into_iter().map(|child| TabRowEntry {
                        tab_id: child.tab_id,
                        group_label: child.group_label,
                        project_dir: child.project_dir,
                        tab_color: child.color,
                        custom_icon: child.custom_icon,
                        parent_tab: Some(parent_id),
                        panes: child.panes.into_iter().cloned().collect::<Vec<_>>(),
                        section: section.map(|section| (section, false)),
                    }));
                    flattened
                })
                .collect::<Vec<_>>()
        };
        let mut tab_entries = flatten_entries(pinned_entries, Some(SidebarSection::Pinned));
        tab_entries.extend(flatten_entries(
            project_entries,
            Some(SidebarSection::Projects),
        ));
        tab_entries.extend(flatten_entries(floating_entries, None));
        let terminal_count = workspace_terminal_tabs(workspace).len();
        let expanded = self.sidebar.expanded_workspaces.contains(&workspace_id);
        let workspace_color = self.workspace_color(workspace_id).as_rgb();
        let card_color = workspace_color;
        let active_text = readable_text_color(card_color);
        let drop_preview = self.sidebar.workspace_drop_preview;
        let drop_above = drop_preview
            .is_some_and(|preview| preview.target_workspace_id == workspace_id && !preview.after);
        let drop_below = drop_preview
            .is_some_and(|preview| preview.target_workspace_id == workspace_id && preview.after);
        let drag = WorkspaceDrag {
            workspace_id,
            pinned,
            title: workspace_title.clone(),
            position: Point::default(),
        };
        let ctx = WorkspaceSectionCtx {
            workspace_id,
            is_assistant: workspace.is_assistant(),
            index,
            pinned,
            active,
            offline,
            connected,
            expanded,
            terminal_count,
            card_color,
            active_text,
            workspace_title,
            workspace_dir,
            custom_icon: workspace.custom_icon.clone(),
            drop_above,
            drop_below,
        };
        div()
            .child(
                div()
                    .id(("workspace-section", element_key(workspace.id)))
                    .mx(px(7.0))
                    .mb(px(3.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(self.render_workspace_card_header(&ctx, drag, cx))
                    .when(expanded, |element| {
                        if terminal_count == 0 {
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
                            element.children(self.render_workspace_tab_rows(&ctx, tab_entries, cx))
                        }
                    }),
            )
            .into_any_element()
    }

    fn render_workspace_tab_rows(
        &self,
        ctx: &WorkspaceSectionCtx,
        tab_entries: Vec<TabRowEntry>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let workspace_id = ctx.workspace_id;
        tab_entries
            .into_iter()
            .flat_map(
                |TabRowEntry {
                     tab_id,
                     group_label,
                     project_dir,
                     tab_color,
                     custom_icon,
                     parent_tab,
                     panes,
                     section,
                 }| {
                    let is_project = project_dir.is_some();
                    let mut rows = Vec::new();
                    if let Some((section, is_section_header)) = section {
                        let collapsed = match section {
                            SidebarSection::Pinned => self
                                .sidebar
                                .collapsed_pinned_sections
                                .contains(&workspace_id),
                            SidebarSection::Projects => self
                                .sidebar
                                .collapsed_project_sections
                                .contains(&workspace_id),
                        };
                        if is_section_header {
                            rows.push(self.render_sidebar_section_row(ctx, section, collapsed, cx));
                        }
                        if collapsed {
                            return rows;
                        }
                    }
                    if parent_tab
                        .is_some_and(|parent_id| self.sidebar.collapsed_groups.contains(&parent_id))
                    {
                        return rows;
                    }
                    let group_indent = if parent_tab.is_some() { 48.0 } else { 20.0 };
                    let pane_indent = if parent_tab.is_some() { 62.0 } else { 34.0 };
                    let tab_active = self
                        .layout
                        .focused_pane
                        .is_some_and(|focused| panes.iter().any(|pane| pane.id == focused));
                    let tab_focus_target = self
                        .layout
                        .focused_pane
                        .filter(|focused| panes.iter().any(|pane| pane.id == *focused))
                        .or_else(|| panes.first().map(|pane| pane.id));
                    match group_label {
                        None => {
                            if let Some(pane) = panes.into_iter().next() {
                                rows.push(self.render_workspace_terminal_row(
                                    &pane,
                                    SidebarPaneRowContext {
                                        workspace_id,
                                        tab_id: Some(tab_id),
                                        tab_color,
                                        from_group: false,
                                        indent: group_indent,
                                    },
                                    cx,
                                ));
                            }
                        }
                        Some(label) => rows.extend(self.render_workspace_group_rows(
                            ctx,
                            WorkspaceGroupRow {
                                tab_id,
                                label,
                                project_dir,
                                tab_color,
                                custom_icon,
                                panes,
                                group_indent,
                                pane_indent,
                                tab_active,
                                tab_focus_target,
                                is_project,
                            },
                            cx,
                        )),
                    }
                    rows
                },
            )
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    fn render_workspace_group_rows(
        &self,
        ctx: &WorkspaceSectionCtx,
        row: WorkspaceGroupRow,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let workspace_id = ctx.workspace_id;
        let WorkspaceGroupRow {
            tab_id,
            label,
            project_dir,
            tab_color,
            custom_icon,
            panes,
            group_indent,
            pane_indent,
            tab_active,
            tab_focus_target,
            is_project,
        } = row;
        let mut rows = Vec::new();
        let collapsed = self.sidebar.collapsed_groups.contains(&tab_id);
        let count_label = terminal_tab_count_label(panes.len());
        let drop_preview = self.sidebar.tab_drop_preview;
        let drop_into = drop_preview
            .is_some_and(|preview| preview.target_tab_id == tab_id && preview.into_group);
        let drop_above = drop_preview.is_some_and(|preview| {
            preview.target_tab_id == tab_id && !preview.into_group && !preview.after
        });
        let drop_below = drop_preview.is_some_and(|preview| {
            preview.target_tab_id == tab_id && !preview.into_group && preview.after
        });
        let drag = TabDrag {
            workspace_id,
            tab_id,
            pane_id: None,
            from_group: false,
            title: label.clone(),
            position: Point::default(),
        };
        let custom_icon_path = custom_icon
            .as_deref()
            .and_then(|icon| self.custom_icon_path(icon));
        let group_text = tab_color.map_or(THEME.foreground, |color| {
            readable_text_color(color.as_rgb())
        });
        let group_detail_text =
            tab_color.map_or(THEME.dim, |color| readable_text_color(color.as_rgb()));
        rows.push(
            div()
                .id(("workspace-group", element_key(tab_id)))
                .ml(px(group_indent))
                .mr(px(4.0))
                .px(px(7.0))
                .h(px(27.0))
                .rounded(px(4.0))
                .border_t(if drop_above { px(2.0) } else { px(0.0) })
                .border_b(if drop_below { px(2.0) } else { px(0.0) })
                .when(drop_into, |element| element.border_1())
                .border_color(rgb(if drop_into || drop_above || drop_below {
                    THEME.accent
                } else {
                    THEME.border
                }))
                .cursor_pointer()
                .flex()
                .items_center()
                .gap(px(6.0))
                .when_some(tab_color, |element, color| element.bg(rgb(color.as_rgb())))
                .when(tab_active && tab_color.is_none(), |element| {
                    element.bg(rgb(THEME.accent_soft))
                })
                .when(tab_color.is_none(), |element| {
                    element.hover(|element| element.bg(rgb(THEME.elevated)))
                })
                .when(tab_color.is_some(), |element| {
                    element.hover(|element| {
                        element.border_1().border_color(rgb(readable_text_color(
                            tab_color.map_or(THEME.foreground, |color| color.as_rgb()),
                        )))
                    })
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if click_suppression_active(
                        &mut this.sidebar.suppress_tab_click_until,
                        Instant::now(),
                    ) {
                        cx.notify();
                        return;
                    }
                    this.sidebar.collapsed_groups.remove(&tab_id);
                    if let Some(pane_id) = tab_focus_target {
                        this.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
                    }
                    cx.stop_propagation();
                }))
                .on_drag(drag, |info: &TabDrag, position, _, cx| {
                    cx.new(|_| TabDrag {
                        position,
                        ..info.clone()
                    })
                })
                .on_drag_move::<TabDrag>(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                        let drag = event.drag(cx);
                        let previews_this_tab = this
                            .sidebar
                            .tab_drop_preview
                            .is_some_and(|preview| preview.target_tab_id == tab_id);
                        if drag.workspace_id != workspace_id
                            || (drag.tab_id == tab_id && drag.pane_id.is_none())
                        {
                            if previews_this_tab {
                                this.sidebar.tab_drop_preview = None;
                                cx.notify();
                            }
                            return;
                        }
                        if event.bounds.contains(&event.event.position) {
                            let zone = header_drop_zone(
                                f32::from(event.event.position.y),
                                f32::from(event.bounds.origin.y),
                                f32::from(event.bounds.origin.y + event.bounds.size.height),
                            );
                            this.sidebar.tab_drop_preview = Some(TabDropPreview {
                                target_tab_id: tab_id,
                                after: zone == HeaderDropZone::After,
                                into_group: zone == HeaderDropZone::Into
                                    && (is_project
                                        || (drag.pane_id.is_some() && drag.tab_id != tab_id)),
                            });
                            cx.stop_propagation();
                            cx.notify();
                        } else if previews_this_tab {
                            this.sidebar.tab_drop_preview = None;
                            cx.notify();
                        }
                    },
                ))
                .on_drop(cx.listener(move |this, info: &TabDrag, _, cx| {
                    if info.workspace_id == workspace_id {
                        let preview = this
                            .sidebar
                            .tab_drop_preview
                            .filter(|preview| preview.target_tab_id == tab_id);
                        let into_group = preview.is_some_and(|preview| preview.into_group);
                        let after = preview.is_some_and(|preview| preview.after);
                        if into_group && is_project {
                            if let Some(source_pane) = info.pane_id.filter(|_| info.from_group) {
                                this.move_sidebar_pane_to_new_tab(
                                    source_pane,
                                    tab_id,
                                    false,
                                    Some(tab_id),
                                    cx,
                                );
                            } else if info.tab_id != tab_id {
                                this.move_tab_to_project(info.tab_id, tab_id, cx);
                            }
                        } else if into_group {
                            if let Some(source_pane) = info.pane_id {
                                this.move_sidebar_pane_to_group(source_pane, tab_id, cx);
                            }
                        } else if let Some(source_pane) = info.pane_id.filter(|_| info.from_group) {
                            this.move_sidebar_pane_to_new_tab(source_pane, tab_id, after, None, cx);
                        } else if info.tab_id != tab_id {
                            this.reorder_workspace_tab(info.tab_id, tab_id, after, cx);
                        }
                    }
                    this.sidebar.tab_drop_preview = None;
                    cx.notify();
                    cx.stop_propagation();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.open_group_menu(tab_id, event.position, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .id(("toggle-workspace-group", element_key(tab_id)))
                        .flex_none()
                        .w(px(12.0))
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(group_detail_text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_group_collapsed(tab_id, cx);
                            cx.stop_propagation();
                        }))
                        .child(if collapsed { "▸" } else { "▾" }),
                )
                .when_some(custom_icon_path.clone(), |element, path| {
                    element.child(
                        img(path)
                            .flex_none()
                            .w(px(11.0))
                            .h(px(11.0))
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(2.0)),
                    )
                })
                .when(
                    custom_icon_path.is_none() && project_dir.is_some(),
                    |element| {
                        element.child(
                            div()
                                .relative()
                                .flex_none()
                                .w(px(11.0))
                                .h(px(8.0))
                                .child(
                                    div()
                                        .absolute()
                                        .left(px(0.0))
                                        .top(px(2.0))
                                        .w(px(11.0))
                                        .h(px(6.0))
                                        .rounded(px(1.5))
                                        .border_1()
                                        .border_color(rgb(THEME.muted)),
                                )
                                .child(
                                    div()
                                        .absolute()
                                        .left(px(0.0))
                                        .top(px(0.0))
                                        .w(px(5.0))
                                        .h(px(3.0))
                                        .rounded(px(1.0))
                                        .bg(rgb(THEME.muted)),
                                ),
                        )
                    },
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .truncate()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(group_text))
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(group_detail_text))
                        .child(count_label),
                )
                .child(self.render_workspace_group_menu_button(tab_id, cx))
                .into_any_element(),
        );
        if !collapsed {
            rows.extend(panes.into_iter().map(|pane| {
                self.render_workspace_terminal_row(
                    &pane,
                    SidebarPaneRowContext {
                        workspace_id,
                        tab_id: Some(tab_id),
                        tab_color,
                        from_group: true,
                        indent: pane_indent,
                    },
                    cx,
                )
            }));
        }
        rows
    }

    fn render_workspace_group_menu_button(
        &self,
        tab_id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(("group-row-menu", element_key(tab_id)))
            .flex_none()
            .w(px(16.0))
            .h(px(18.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .justify_center()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.dim))
            .hover(|element| element.text_color(rgb(THEME.foreground)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_group_menu(tab_id, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child("…")
            .into_any_element()
    }

    fn render_sidebar_section_row(
        &self,
        ctx: &WorkspaceSectionCtx,
        section: SidebarSection,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = ctx.workspace_id;
        div()
            .id((section.element_id(), element_key(workspace_id)))
            .ml(px(20.0))
            .py(px(3.0))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(4.0))
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(THEME.dim))
            .hover(|element| element.text_color(rgb(THEME.muted)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_sidebar_section(workspace_id, section, cx);
                cx.stop_propagation();
            }))
            .child(div().flex_none().child(if collapsed { "›" } else { "⌄" }))
            .child(section.label())
            .into_any_element()
    }

    fn render_workspace_card_header(
        &self,
        ctx: &WorkspaceSectionCtx,
        drag: WorkspaceDrag,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = ctx.workspace_id;
        let pinned = ctx.pinned;
        let active = ctx.active;
        let offline = ctx.offline;
        let connected = ctx.connected;
        let expanded = ctx.expanded;
        let terminal_count = ctx.terminal_count;
        let card_color = ctx.card_color;
        let active_text = ctx.active_text;
        let workspace_dir = ctx.workspace_dir.clone();
        let drop_above = ctx.drop_above;
        let drop_below = ctx.drop_below;
        div()
            .id(("workspace", element_key(workspace_id)))
            .h(px(if workspace_dir.is_some() { 42.0 } else { 31.0 }))
            .px(px(8.0))
            .rounded(px(6.0))
            .border_t(if drop_above { px(2.0) } else { px(0.0) })
            .border_b(if drop_below { px(2.0) } else { px(0.0) })
            .border_color(rgb(if drop_above || drop_below {
                THEME.accent
            } else {
                THEME.border
            }))
            .when(!offline || terminal_count == 0, |element| {
                element.cursor_pointer()
            })
            .when(offline, |element| element.bg(rgb(card_color)))
            .when(active || connected, |element| element.bg(rgb(card_color)))
            .hover(|element| {
                if active || connected || offline {
                    element
                } else {
                    element.bg(rgb(THEME.surface))
                }
            })
            .when(!offline || terminal_count == 0, |element| {
                element.on_click(cx.listener(move |this, _, _, cx| {
                    if click_suppression_active(
                        &mut this.sidebar.suppress_workspace_click_until,
                        Instant::now(),
                    ) {
                        cx.notify();
                        return;
                    }
                    this.select_workspace(workspace_id, cx);
                }))
            })
            .on_drag(drag, |info: &WorkspaceDrag, position, _, cx| {
                cx.new(|_| WorkspaceDrag {
                    position,
                    ..info.clone()
                })
            })
            .on_drag_move::<WorkspaceDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<WorkspaceDrag>, _, cx| {
                    let drag = event.drag(cx);
                    if drag.workspace_id != workspace_id
                        && drag.pinned == pinned
                        && event.bounds.contains(&event.event.position)
                    {
                        this.sidebar.dragging_workspace = Some(drag.workspace_id);
                        this.sidebar.workspace_drop_preview = Some(WorkspaceDropPreview {
                            target_workspace_id: workspace_id,
                            after: event.event.position.y > event.bounds.center().y,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    } else if this
                        .sidebar
                        .workspace_drop_preview
                        .is_some_and(|preview| preview.target_workspace_id == workspace_id)
                    {
                        this.sidebar.dragging_workspace = None;
                        this.sidebar.workspace_drop_preview = None;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(move |this, info: &WorkspaceDrag, _, cx| {
                if info.workspace_id != workspace_id && info.pinned == pinned {
                    let after = this.sidebar.workspace_drop_preview.is_some_and(|preview| {
                        preview.target_workspace_id == workspace_id && preview.after
                    });
                    this.reorder_workspace(info.workspace_id, workspace_id, after, cx);
                }
                this.sidebar.dragging_workspace = None;
                this.sidebar.workspace_drop_preview = None;
                cx.notify();
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_workspace_menu(workspace_id, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .flex()
            .items_center()
            .gap(px(5.0))
            .child(
                div()
                    .id(("toggle-workspace-tabs", element_key(workspace_id)))
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
                                "Collapse workstation terminals".to_owned()
                            } else {
                                "Expand workstation terminals".to_owned()
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
            .child(self.render_workspace_card_title(ctx))
            .child(self.render_workspace_tab_count(ctx))
            .child(self.render_workspace_menu_button(ctx, cx))
            .when(connected, |element| {
                element
                    .child(
                        div()
                            .id(("workspace-connection-info", element_key(workspace_id)))
                            .flex_none()
                            .w(px(16.0))
                            .h(px(16.0))
                            .rounded_full()
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(active_text))
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|element| element.bg(rgba(0xffffff20)))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Connection details".to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                this.open_workspace_connection_info(
                                    workspace_id,
                                    event.position(),
                                    cx,
                                );
                                cx.stop_propagation();
                            }))
                            .child("ⓘ"),
                    )
                    .child(
                        div()
                            .id(("workspace-connected-indicator", element_key(workspace_id)))
                            .flex_none()
                            .w(px(8.0))
                            .h(px(8.0))
                            .rounded_full()
                            .bg(rgb(THEME.ansi[2]))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Connected".to_owned(),
                                })
                                .into()
                            }),
                    )
            })
            .when(offline, |element| {
                element
                    .child(
                        div()
                            .id(("reconnect-workspace", element_key(workspace_id)))
                            .flex_none()
                            .w(px(18.0))
                            .h(px(18.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.ansi[2]))
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|element| element.bg(rgba(0xffffff20)))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Reconnect with system OpenSSH".to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.reconnect_workspace(workspace_id, cx);
                                cx.stop_propagation();
                            }))
                            .child("↻"),
                    )
                    .child(
                        div()
                            .id(("delete-offline-workspace", element_key(workspace_id)))
                            .flex_none()
                            .w(px(18.0))
                            .h(px(18.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.danger))
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|element| element.bg(rgba(0xffffff20)))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Delete saved workstation…".to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.begin_workspace_delete(workspace_id, cx);
                                cx.stop_propagation();
                            }))
                            .child("⌫"),
                    )
            })
            .into_any_element()
    }

    fn render_workspace_card_title(&self, ctx: &WorkspaceSectionCtx) -> AnyElement {
        let title = if ctx.is_assistant {
            ctx.workspace_title.clone()
        } else {
            format!("{}  {}", ctx.index + 1, ctx.workspace_title)
        };
        let icon_path = ctx
            .custom_icon
            .as_deref()
            .and_then(|icon| self.custom_icon_path(icon));
        let has_icon = icon_path.is_some();
        div()
            .min_w(px(0.0))
            .overflow_hidden()
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .min_w(px(0.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .when_some(icon_path, |element, path| {
                        element.child(
                            img(path)
                                .flex_none()
                                .w(px(14.0))
                                .h(px(14.0))
                                .object_fit(gpui::ObjectFit::Contain)
                                .rounded(px(3.0)),
                        )
                    })
                    .when(ctx.is_assistant && !has_icon, |element| {
                        element.child(
                            div()
                                .flex_none()
                                .w(px(8.0))
                                .h(px(8.0))
                                .rounded_full()
                                .bg(rgb(THEME.accent)),
                        )
                    })
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if ctx.active || ctx.connected || ctx.offline {
                                rgb(ctx.active_text)
                            } else {
                                rgb(THEME.foreground)
                            })
                            .child(title),
                    ),
            )
            .when_some(ctx.workspace_dir.clone(), |element, directory| {
                element.child(
                    div()
                        .min_w(px(0.0))
                        .whitespace_nowrap()
                        .font_family("SF Mono")
                        .text_size(px(9.0))
                        .text_color(rgb(THEME.dim))
                        .child(directory),
                )
            })
            .into_any_element()
    }

    fn render_workspace_tab_count(&self, ctx: &WorkspaceSectionCtx) -> AnyElement {
        div()
            .id(("workspace-tab-count", element_key(ctx.workspace_id)))
            .flex_none()
            .min_w(px(18.0))
            .h(px(17.0))
            .px(px(5.0))
            .rounded_full()
            .bg(rgba(if ctx.active || ctx.connected || ctx.offline {
                0xffffff20
            } else {
                0xffffff0c
            }))
            .font_family("SF Mono")
            .text_size(px(9.5))
            .text_color(if ctx.active || ctx.connected || ctx.offline {
                rgb(ctx.active_text)
            } else {
                rgb(THEME.muted)
            })
            .flex()
            .items_center()
            .justify_center()
            .tooltip({
                let terminal_count = ctx.terminal_count;
                move |_, cx| {
                    cx.new(|_| TooltipView {
                        text: terminal_tab_count_label(terminal_count),
                    })
                    .into()
                }
            })
            .child(ctx.terminal_count.to_string())
            .into_any_element()
    }

    fn render_workspace_menu_button(
        &self,
        ctx: &WorkspaceSectionCtx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = ctx.workspace_id;
        div()
            .id(("workspace-row-menu", element_key(workspace_id)))
            .flex_none()
            .w(px(16.0))
            .h(px(18.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .justify_center()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.dim))
            .hover(|element| element.text_color(rgb(THEME.foreground)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_workspace_menu(workspace_id, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child("…")
            .into_any_element()
    }
}
