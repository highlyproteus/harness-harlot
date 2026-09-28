//! The scrollable workstation tree and one workstation's card.
use crate::elements::SidebarPaneRowContext;
use crate::helpers::{
    HeaderDropZone, WorkstationTabEntry, abbreviate_home, click_suppression_active, element_key,
    header_drop_zone, identity_detail, partition_workstation_entries, readable_text_color,
    render_terminal_profile_icon, split_control_id, terminal_tab_count_label,
    visible_workstation_tree, workspace_tab_entries, workspace_terminal_tabs,
};
use crate::notifications::{SeenScope, activity_badge};
use crate::tab_chrome::{PaneIndicator, workstation_rollup_indicator};
use crate::view_models::{
    TabDrag, TabDropPreview, TooltipView, WorkspaceDrag, WorkspaceDropPreview,
};
use crate::{HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    Point, div, img, px, relative, rgb, rgba,
};
use gpui::{AppContext, ParentElement, StatefulInteractiveElement, Styled, StyledImage};
use hh_protocol::{
    AppearanceColor, Pane, PaneLayout, SplitAxis, TerminalProfile, Workspace, WorkspaceConnection,
    WorkspaceConnectionStatus, effective_working_dir,
};
use std::time::Instant;
use uuid::Uuid;

/// Horizontal step, in pixels, between a workstation and the ones nested in it.
const NESTING_INDENT: f32 = 13.0;
/// Left margin of a top-level workstation card.
const CARD_MARGIN: f32 = 7.0;
/// Side of the circle around a bot card's agent icon that carries its ring.
const BOT_ICON_RING_SIZE: f32 = 20.0;

struct TabRowEntry<'a> {
    tab_id: Uuid,
    label: Option<&'a str>,
    title: Option<&'a str>,
    tab_color: Option<AppearanceColor>,
    panes: Vec<&'a Pane>,
    /// `Some(is_first)` for tabs in the Pinned section.
    pinned_section: Option<bool>,
}

#[derive(Clone, Copy)]
struct WorkspaceTabRow<'a> {
    tab_id: Uuid,
    label: &'a str,
    tab_color: Option<AppearanceColor>,
    tab_indent: f32,
    tab_active: bool,
    tab_focus_target: Option<Uuid>,
}

fn tab_row_entries(
    entries: Vec<WorkstationTabEntry<'_>>,
    pinned: bool,
) -> impl Iterator<Item = TabRowEntry<'_>> {
    entries
        .into_iter()
        .enumerate()
        .map(move |(entry_index, entry)| TabRowEntry {
            tab_id: entry.tab_id,
            label: entry.label,
            title: entry.title,
            tab_color: entry.color,
            panes: entry.panes,
            pinned_section: pinned.then_some(entry_index == 0),
        })
}

#[allow(clippy::struct_excessive_bools)]
struct WorkspaceSectionCtx {
    workspace_id: Uuid,
    /// Sidebar position shown before a top-level workstation's title
    /// (the ⌘ number); `None` for nested workstations and bots.
    number: Option<usize>,
    parent: Option<Uuid>,
    /// Left offset of this card from its nesting depth.
    card_indent: f32,
    pinned: bool,
    home: bool,
    active: bool,
    offline: bool,
    connected: bool,
    expanded: bool,
    terminal_count: usize,
    /// Status dot a collapsed card shows for itself and its nested
    /// workstations.
    rollup: PaneIndicator,
    card_color: u32,
    active_text: u32,
    workspace_title: String,
    workspace_dir: Option<String>,
    custom_icon: Option<String>,
    drop_above: bool,
    drop_below: bool,
    /// A tab dragged from another workstation on the same machine hovers
    /// this card and would move here on drop.
    tab_drop_into: bool,
    /// A bot card: its agent replaces the workstation number.
    bot: Option<TerminalProfile>,
    /// A bot card whose thread needs the user or finished unseen: the agent
    /// icon wears the orange ring.
    bot_ring: bool,
}

impl HhApp {
    /// The Workstations view: its header, then the scrollable workstation
    /// tree or the empty-state hint.
    pub(crate) fn render_workstation_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let workspaces = self
            .session
            .snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.workspaces.as_slice());
        let rows = visible_workstation_tree(workspaces, &self.sidebar.expanded_workspaces);
        let has_workspaces = !rows.is_empty();
        let mut top_level_index = 0;
        let sections = rows
            .into_iter()
            .map(|(workspace, depth)| {
                let number = (depth == 1).then(|| {
                    top_level_index += 1;
                    top_level_index - 1
                });
                self.render_workspace_section(number, depth, workspace, cx)
            })
            .collect::<Vec<_>>();
        div()
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .child(Self::render_sidebar_view_header(
                "Workstations",
                "new-workstation",
                "New workstation",
                Self::new_workspace,
                cx,
            ))
            .child(
                div()
                    .id("sidebar-workstation-list")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .children(sections)
                    .when(!has_workspaces, |element| {
                        element.child(
                            div()
                                .px(px(12.0))
                                .py(px(6.0))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child("No workstations yet. Use ＋ above to add one."),
                        )
                    }),
            )
            .into_any_element()
    }

    /// One workstation (or bot) card at nesting `depth` (1 for top level),
    /// with its tab and terminal rows when expanded.
    pub(crate) fn render_workspace_section(
        &self,
        number: Option<usize>,
        depth: usize,
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
        let workspaces = self
            .session
            .snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.workspaces.as_slice());
        let workspace_title = workspace.title.clone();
        let workspace_dir = if workspace.is_bot() {
            workspace.working_dir.as_deref().map(abbreviate_home)
        } else {
            Some(
                effective_working_dir(workspaces, workspace_id)
                    .map_or_else(|| "~".to_owned(), abbreviate_home),
            )
        };
        let (pinned_entries, other_entries) =
            partition_workstation_entries(workspace_tab_entries(workspace));
        let tab_entries = tab_row_entries(pinned_entries, true)
            .chain(tab_row_entries(other_entries, false))
            .collect::<Vec<_>>();
        let terminal_count = workspace_terminal_tabs(workspace).len();
        let expanded = self.sidebar.expanded_workspaces.contains(&workspace_id);
        let rollup = if expanded || workspace.is_bot() {
            PaneIndicator::None
        } else {
            workstation_rollup_indicator(workspaces, workspace_id, |pane| self.pane_indicator(pane))
        };
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
            parent: workspace.parent_workstation,
            pinned,
            title: workspace_title.clone(),
            position: Point::default(),
        };
        #[allow(clippy::cast_precision_loss)]
        let card_indent = depth.saturating_sub(1) as f32 * NESTING_INDENT;
        let ctx = WorkspaceSectionCtx {
            workspace_id,
            number,
            parent: workspace.parent_workstation,
            card_indent,
            pinned,
            home: workspace.home,
            active,
            offline,
            connected,
            expanded,
            terminal_count,
            rollup,
            card_color,
            active_text,
            workspace_title,
            workspace_dir,
            custom_icon: workspace.custom_icon.clone(),
            drop_above,
            drop_below,
            tab_drop_into: self.sidebar.tab_drop_workspace == Some(workspace_id),
            bot: workspace.bot.as_ref().map(|bot| bot.agent),
            bot_ring: crate::notifications::bot_wants_you(workspace, &self.session.pane_states),
        };
        let saved_threads = ctx
            .bot
            .filter(|_| expanded)
            .and_then(|agent| self.render_saved_thread_rows(workspace_id, agent, cx));
        div()
            .child(
                div()
                    .id(("workspace-section", element_key(workspace.id)))
                    .ml(px(CARD_MARGIN + card_indent))
                    .mr(px(CARD_MARGIN))
                    .mb(px(3.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(self.render_workspace_card_header(&ctx, drag, cx))
                    .when(expanded, |element| {
                        if terminal_count == 0 && ctx.bot.is_some() {
                            element
                        } else if terminal_count == 0 {
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
                    })
                    .children(saved_threads),
            )
            .into_any_element()
    }

    fn render_workspace_tab_rows(
        &self,
        ctx: &WorkspaceSectionCtx,
        tab_entries: Vec<TabRowEntry<'_>>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let workspace_id = ctx.workspace_id;
        let pinned_collapsed = self
            .sidebar
            .collapsed_pinned_sections
            .contains(&workspace_id);
        tab_entries
            .into_iter()
            .flat_map(
                |TabRowEntry {
                     tab_id,
                     label,
                     title,
                     tab_color,
                     panes,
                     pinned_section,
                 }| {
                    let mut rows = Vec::new();
                    if let Some(is_first) = pinned_section {
                        if is_first {
                            rows.push(self.render_pinned_section_row(ctx, pinned_collapsed, cx));
                        }
                        if pinned_collapsed {
                            return rows;
                        }
                    }
                    let tab_indent = 20.0;
                    let tab_active = self
                        .layout
                        .focused_pane
                        .is_some_and(|focused| panes.iter().any(|pane| pane.id == focused));
                    let tab_focus_target = self
                        .layout
                        .focused_pane
                        .filter(|focused| panes.iter().any(|pane| pane.id == *focused))
                        .or_else(|| panes.first().map(|pane| pane.id));
                    match label {
                        None => {
                            if let Some(pane) = panes.into_iter().next() {
                                rows.push(self.render_workspace_terminal_row(
                                    pane,
                                    SidebarPaneRowContext {
                                        workspace_id,
                                        tab_id: Some(tab_id),
                                        tab_color,
                                        from_pane_map: false,
                                        indent: tab_indent,
                                        // Bot thread rows keep their thread names.
                                        title:
                                            title.filter(|_| ctx.bot.is_none()).map(str::to_owned),
                                        activity: None,
                                    },
                                    cx,
                                ));
                            }
                        }
                        Some(label) => rows.push(self.render_workspace_tab_window(
                            ctx,
                            WorkspaceTabRow {
                                tab_id,
                                label,
                                tab_color,
                                tab_indent,
                                tab_active,
                                tab_focus_target,
                            },
                            cx,
                        )),
                    }
                    rows
                },
            )
            .collect()
    }

    /// A multi-pane or named tab: the ring of its pane chips laid out like
    /// the tab itself. Click focuses it, dragging reorders it, and dropping
    /// a chip from another tab onto it moves that pane in.
    fn render_workspace_tab_window(
        &self,
        ctx: &WorkspaceSectionCtx,
        row: WorkspaceTabRow<'_>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = ctx.workspace_id;
        let WorkspaceTabRow {
            tab_id,
            label,
            tab_color,
            tab_indent,
            tab_active,
            tab_focus_target,
        } = row;
        let drop_preview = self.sidebar.tab_drop_preview;
        let drop_into =
            drop_preview.is_some_and(|preview| preview.target_tab_id == tab_id && preview.into_tab);
        let drop_above = drop_preview.is_some_and(|preview| {
            preview.target_tab_id == tab_id && !preview.into_tab && !preview.after
        });
        let drop_below = drop_preview.is_some_and(|preview| {
            preview.target_tab_id == tab_id && !preview.into_tab && preview.after
        });
        let drag = TabDrag {
            workspace_id,
            tab_id,
            pane_id: None,
            from_pane_map: false,
            title: label.to_owned(),
            position: Point::default(),
        };
        let content = self.tab_layout(workspace_id, tab_id).map(|layout| {
            self.render_pane_map(
                workspace_id,
                tab_id,
                layout,
                tab_indent + ctx.card_indent,
                cx,
            )
        });
        div()
            .id(("workspace-tab-window", element_key(tab_id)))
            .ml(px(tab_indent))
            .mr(px(4.0))
            .p(px(3.0))
            .my(px(2.0))
            .rounded(px(6.0))
            .border_1()
            .when(drop_above, |element| element.border_t(px(2.0)))
            .when(drop_below, |element| element.border_b(px(2.0)))
            .border_color(rgb(if drop_into || drop_above || drop_below {
                THEME.accent
            } else {
                THEME.border_strong
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
                if let Some(pane_id) = tab_focus_target {
                    this.select_sidebar_pane(workspace_id, tab_id, pane_id, SeenScope::Tab, cx);
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
                        let next = Some(TabDropPreview {
                            target_tab_id: tab_id,
                            after: zone == HeaderDropZone::After,
                            into_tab: zone == HeaderDropZone::Into
                                && drag.pane_id.is_some()
                                && drag.tab_id != tab_id,
                        });
                        cx.stop_propagation();
                        if this.sidebar.tab_drop_preview != next {
                            this.sidebar.tab_drop_preview = next;
                            cx.notify();
                        }
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
                    let into_tab = preview.is_some_and(|preview| preview.into_tab);
                    let after = preview.is_some_and(|preview| preview.after);
                    if into_tab {
                        if let Some(source_pane) = info.pane_id {
                            this.move_sidebar_pane_into_tab(source_pane, tab_id, cx);
                        }
                    } else if let Some(source_pane) = info.pane_id.filter(|_| info.from_pane_map) {
                        this.move_sidebar_pane_to_new_tab(source_pane, tab_id, after, cx);
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
                    this.open_tab_row_menu(tab_id, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .children(content)
            .into_any_element()
    }

    fn tab_layout(&self, workspace_id: Uuid, tab_id: Uuid) -> Option<&PaneLayout> {
        self.session
            .snapshot
            .as_ref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)?
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| &tab.layout)
    }

    /// A window's terminals drawn as a small map of its real layout: the map
    /// has the window's proportions (tall enough for every row to stay
    /// readable), and each split keeps its actual ratio, so side-by-side
    /// terminals are tall narrow chips and a full-width bottom row spans the
    /// whole map.
    fn render_pane_map(
        &self,
        workspace_id: Uuid,
        tab_id: Uuid,
        layout: &PaneLayout,
        indent: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const ROW_MIN: f32 = 24.0;
        let (window_width, window_height) = self.layout.workspace_pixels;
        let map_width = (self.sidebar.sidebar_pixels - indent - 28.0).max(80.0);
        let shaped = if window_width > 0.0 && window_height > 0.0 {
            map_width * window_height / window_width * 0.6
        } else {
            0.0
        };
        let rows = f32::from(pane_map_rows(layout));
        let height = shaped.max(rows * ROW_MIN).clamp(ROW_MIN, 180.0);
        div()
            .w_full()
            .h(px(height))
            .flex()
            .child(self.render_pane_map_node(workspace_id, tab_id, layout, cx))
            .into_any_element()
    }

    fn render_pane_map_node(
        &self,
        workspace_id: Uuid,
        tab_id: Uuid,
        layout: &PaneLayout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Each cell pads itself, so relative sizes still add up to the map.
        let cell = || div().min_w(px(0.0)).min_h(px(0.0)).p(px(1.5)).flex();
        match layout {
            PaneLayout::Leaf { pane } => cell()
                .size_full()
                .child(self.render_tab_pane_chip(workspace_id, tab_id, pane, cx))
                .into_any_element(),
            PaneLayout::Stack { panes, .. } => cell()
                .size_full()
                .children(
                    panes
                        .iter()
                        .map(|pane| {
                            cell().flex_1().h_full().child(self.render_tab_pane_chip(
                                workspace_id,
                                tab_id,
                                pane,
                                cx,
                            ))
                        })
                        .collect::<Vec<_>>(),
                )
                .into_any_element(),
            PaneLayout::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let ratio = self
                    .layout
                    .split_ratios
                    .get(&split_control_id(first, second))
                    .copied()
                    .unwrap_or(*ratio)
                    .clamp(0.05, 0.95);
                let side_by_side = *axis == SplitAxis::Horizontal;
                div()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .flex()
                    .when(!side_by_side, |element| element.flex_col())
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex()
                            .when(side_by_side, |element| element.w(relative(ratio)).h_full())
                            .when(!side_by_side, |element| element.h(relative(ratio)).w_full())
                            .child(self.render_pane_map_node(workspace_id, tab_id, first, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex()
                            .child(self.render_pane_map_node(workspace_id, tab_id, second, cx)),
                    )
                    .into_any_element()
            }
        }
    }

    /// One terminal of a window, tmux-style: a compact chip with its icon,
    /// name, and status. Click focuses it, dragging moves it out to its own
    /// tab, and right-click opens its tab menu.
    fn render_tab_pane_chip(
        &self,
        workspace_id: Uuid,
        tab_id: Uuid,
        pane: &Pane,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane_id = pane.id;
        let title = self.pane_label(pane);
        let exited = self.pane_exited(pane_id);
        let indicator = self.pane_indicator(pane);
        let indicator_tooltip = self.pane_indicator_tooltip(pane);
        let awaits_input = self.pane_awaits_input(pane);
        let focused = self.layout.focused_pane == Some(pane_id);
        let border = if focused {
            THEME.accent
        } else if indicator == PaneIndicator::NeedsYou {
            indicator.color()
        } else {
            THEME.border
        };
        // In a bot, a chip is a thread: × deletes it.
        let (close_tooltip, close_thread) = match self.bot_for_pane(pane_id) {
            Some(bot_id) => (
                "Delete thread…".to_owned(),
                Some((bot_id, self.live_thread_id(bot_id, pane_id), title.clone())),
            ),
            None => (format!("Close {title}…"), None),
        };
        let tooltip = match activity_badge(pane, exited) {
            Some(badge) => format!("{} — {badge}", identity_detail(pane)),
            None => identity_detail(pane),
        };
        let drag = TabDrag {
            workspace_id,
            tab_id,
            pane_id: Some(pane_id),
            from_pane_map: true,
            title: title.clone(),
            position: Point::default(),
        };
        let chip = div()
            .id(("tab-pane-chip", element_key(pane_id)))
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .px(px(6.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(border))
            .when(focused, |element| element.bg(rgb(THEME.accent_soft)))
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .hover(|element| element.bg(rgb(THEME.elevated)))
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: tooltip.clone(),
                })
                .into()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if click_suppression_active(
                    &mut this.sidebar.suppress_tab_click_until,
                    Instant::now(),
                ) {
                    cx.notify();
                    return;
                }
                this.select_sidebar_pane(workspace_id, tab_id, pane_id, SeenScope::Pane, cx);
                cx.stop_propagation();
            }))
            .on_drag(drag, |info: &TabDrag, position, _, cx| {
                cx.new(|_| TabDrag {
                    position,
                    ..info.clone()
                })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_tab_menu(pane_id, event.position, SeenScope::Pane, cx);
                    cx.stop_propagation();
                }),
            )
            .child(render_terminal_profile_icon(
                pane.identity.profile,
                THEME.muted,
                13.0,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(if exited { THEME.dim } else { THEME.foreground }))
                    .child(title),
            )
            .child(self.render_pane_indicator_with_tooltip(
                indicator,
                ("tab-pane-chip-status", element_key(pane_id)),
                indicator_tooltip,
            ))
            .child(self.render_close_button(
                ("close-tab-pane-chip", element_key(pane_id)),
                THEME.foreground,
                close_tooltip,
                move |this, cx| match close_thread.clone() {
                    Some((bot_id, thread_id, title)) => {
                        this.begin_bot_thread_delete(bot_id, thread_id, title, cx);
                    }
                    None => this.begin_close(pane_id, cx),
                },
                cx,
            ));
        self.with_needs_input_border(chip, awaits_input, 4.0)
            .into_any_element()
    }

    fn render_pinned_section_row(
        &self,
        ctx: &WorkspaceSectionCtx,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = ctx.workspace_id;
        div()
            .id(("sidebar-pinned-section", element_key(workspace_id)))
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
                this.toggle_pinned_section(workspace_id, cx);
                cx.stop_propagation();
            }))
            .child(div().flex_none().child(if collapsed { "›" } else { "⌄" }))
            .child("Pinned")
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
        let tab_drop_into = ctx.tab_drop_into;
        let parent = ctx.parent;
        let bot = ctx.bot.is_some();
        div()
            .id(("workspace", element_key(workspace_id)))
            .h(px(if workspace_dir.is_some() { 42.0 } else { 31.0 }))
            .px(px(8.0))
            .rounded(px(6.0))
            .border_t(if drop_above { px(2.0) } else { px(0.0) })
            .border_b(if drop_below { px(2.0) } else { px(0.0) })
            .when(tab_drop_into, |element| element.border_1())
            .border_color(rgb(if drop_above || drop_below || tab_drop_into {
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
                    if bot {
                        this.open_bot(workspace_id, cx);
                    } else {
                        this.select_workspace(workspace_id, SeenScope::Tab, cx);
                    }
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
                        && drag.parent == parent
                        && event.bounds.contains(&event.event.position)
                    {
                        let next_dragging = Some(drag.workspace_id);
                        let next_preview = Some(WorkspaceDropPreview {
                            target_workspace_id: workspace_id,
                            after: event.event.position.y > event.bounds.center().y,
                        });
                        cx.stop_propagation();
                        if this.sidebar.dragging_workspace != next_dragging
                            || this.sidebar.workspace_drop_preview != next_preview
                        {
                            this.sidebar.dragging_workspace = next_dragging;
                            this.sidebar.workspace_drop_preview = next_preview;
                            cx.notify();
                        }
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
            .when(!bot, |element| {
                element
                    .on_drag_move::<TabDrag>(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                            let drag = event.drag(cx);
                            let accepts = !drag.from_pane_map
                                && event.bounds.contains(&event.event.position)
                                && this.accepts_tab_move(drag.workspace_id, workspace_id);
                            let next = if accepts {
                                Some(workspace_id)
                            } else if this.sidebar.tab_drop_workspace == Some(workspace_id) {
                                None
                            } else {
                                return;
                            };
                            if accepts {
                                cx.stop_propagation();
                            }
                            if this.sidebar.tab_drop_workspace != next {
                                this.sidebar.tab_drop_workspace = next;
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(move |this, info: &TabDrag, _, cx| {
                        if !info.from_pane_map
                            && this.accepts_tab_move(info.workspace_id, workspace_id)
                        {
                            this.move_tab_to_workstation(info.tab_id, workspace_id, cx);
                        }
                        this.sidebar.tab_drop_workspace = None;
                        cx.notify();
                        cx.stop_propagation();
                    }))
            })
            .on_drop(cx.listener(move |this, info: &WorkspaceDrag, _, cx| {
                if info.workspace_id != workspace_id
                    && info.pinned == pinned
                    && info.parent == parent
                {
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
                    this.open_card_menu(workspace_id, bot, event.position, cx);
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
                                "Collapse workstation".to_owned()
                            } else {
                                "Expand workstation".to_owned()
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
            .when(ctx.rollup != PaneIndicator::None, |element| {
                element.child(self.render_pane_indicator_with_tooltip(
                    ctx.rollup,
                    ("workstation-rollup-status", element_key(ctx.workspace_id)),
                    ctx.rollup.tooltip(),
                ))
            })
            .when(!bot, |element| {
                element.child(self.render_workspace_tab_count(ctx))
            })
            .when(bot, |element| {
                element.child(self.render_new_thread_button(workspace_id, cx))
            })
            .child(self.render_workspace_menu_button(ctx, cx))
            .children(self.render_remote_card_controls(ctx, cx))
            .into_any_element()
    }

    /// The connected SSH card's info button and green dot, or the offline
    /// card's reconnect and delete buttons.
    fn render_remote_card_controls(
        &self,
        ctx: &WorkspaceSectionCtx,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let workspace_id = ctx.workspace_id;
        let active_text = ctx.active_text;
        if ctx.connected {
            vec![
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
                        this.open_workspace_connection_info(workspace_id, event.position(), cx);
                        cx.stop_propagation();
                    }))
                    .child("ⓘ")
                    .into_any_element(),
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
                    })
                    .into_any_element(),
            ]
        } else if ctx.offline {
            vec![
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
                    .child("↻")
                    .into_any_element(),
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
                    .child("⌫")
                    .into_any_element(),
            ]
        } else {
            Vec::new()
        }
    }

    fn render_workspace_card_title(&self, ctx: &WorkspaceSectionCtx) -> AnyElement {
        let title = match ctx.number.filter(|_| ctx.bot.is_none()) {
            Some(index) => format!("{}  {}", index + 1, ctx.workspace_title),
            None => ctx.workspace_title.clone(),
        };
        let text_color = if ctx.active || ctx.connected || ctx.offline {
            ctx.active_text
        } else {
            THEME.foreground
        };
        let icon_path = ctx
            .custom_icon
            .as_deref()
            .and_then(|icon| self.custom_icon_path(icon));
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
                    .when_some(ctx.bot, |element, agent| {
                        let icon = div()
                            .flex_none()
                            .w(px(BOT_ICON_RING_SIZE))
                            .h(px(BOT_ICON_RING_SIZE))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(render_terminal_profile_icon(agent, text_color, 14.0));
                        element.child(self.with_needs_input_border(
                            icon,
                            ctx.bot_ring,
                            BOT_ICON_RING_SIZE / 2.0,
                        ))
                    })
                    .when(ctx.home, |element| {
                        element.child(render_this_machine_mark(ctx.workspace_id, text_color))
                    })
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
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(text_color))
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
        let bot = ctx.bot.is_some();
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
                    this.open_card_menu(workspace_id, bot, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child("⋮")
            .into_any_element()
    }
}

/// The home workstation's small monitor glyph, marking the card that stands
/// for this machine.
fn render_this_machine_mark(workspace_id: Uuid, color: u32) -> AnyElement {
    div()
        .id(("this-machine-mark", element_key(workspace_id)))
        .flex_none()
        .w(px(13.0))
        .h(px(11.0))
        .flex()
        .flex_col()
        .items_center()
        .tooltip(|_, cx| {
            cx.new(|_| TooltipView {
                text: hh_protocol::this_machine_title().to_owned(),
            })
            .into()
        })
        .child(
            div()
                .w(px(13.0))
                .h(px(8.0))
                .rounded(px(1.5))
                .border_1()
                .border_color(rgb(color)),
        )
        .child(div().w(px(1.5)).h(px(1.5)).bg(rgb(color)))
        .child(div().w(px(6.0)).h(px(1.0)).rounded(px(0.5)).bg(rgb(color)))
        .into_any_element()
}

/// How many terminal rows a window stacks vertically, so the sidebar map is
/// tall enough for each row's chip to stay readable.
fn pane_map_rows(layout: &PaneLayout) -> u16 {
    match layout {
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => 1,
        PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            first,
            second,
            ..
        } => pane_map_rows(first).max(pane_map_rows(second)),
        PaneLayout::Split {
            axis: SplitAxis::Vertical,
            first,
            second,
            ..
        } => pane_map_rows(first).saturating_add(pane_map_rows(second)),
    }
}

#[cfg(test)]
mod tests {
    use super::pane_map_rows;
    use hh_protocol::{PaneLayout, SessionSnapshot, SplitAxis};

    fn leaf() -> PaneLayout {
        SessionSnapshot::seeded()
            .workspaces
            .remove(0)
            .tabs
            .remove(0)
            .layout
    }

    fn split(axis: SplitAxis, first: PaneLayout, second: PaneLayout) -> PaneLayout {
        PaneLayout::Split {
            axis,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn map_rows_follow_the_window_layout() {
        let columns = split(SplitAxis::Horizontal, leaf(), leaf());
        assert_eq!(pane_map_rows(&columns), 1, "side by side is one tall row");

        let two_over_one = split(SplitAxis::Vertical, columns.clone(), leaf());
        assert_eq!(pane_map_rows(&two_over_one), 2, "1|2 over a full-width 3");

        let three = split(SplitAxis::Horizontal, leaf(), columns.clone());
        let three_over_two = split(SplitAxis::Vertical, three, columns);
        assert_eq!(pane_map_rows(&three_over_two), 2, "three columns over two");

        let stacked = split(
            SplitAxis::Vertical,
            leaf(),
            split(SplitAxis::Vertical, leaf(), leaf()),
        );
        assert_eq!(pane_map_rows(&stacked), 3);
    }
}
