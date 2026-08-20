//! The workstation sidebar: banner, toolbar, list, and resize handle.
use crate::appearance::workstation_banner_artwork;
use crate::elements::SidebarPaneRowContext;
use crate::helpers::{
    SidebarSection, banner_fit_size, click_suppression_active, composite_rgb, element_key,
    readable_text_color, render_bell_icon, render_sidebar_toggle_icon,
    render_terminal_profile_icon, rgba_with_alpha, sidebar_width_for_visibility,
    tab_identity_presentation, workstation_banner_header_height,
};
use crate::view_models::{
    CreateMenu, CreateMenuTarget, Modal, TabDrag, TabDropPreview, TooltipView,
};
use crate::{
    HhApp, MACOS_TRAFFIC_LIGHT_SAFE_INSET, SIDEBAR_RESIZE_HIT_WIDTH, SIDEBAR_RESIZE_VISUAL_WIDTH,
    TAB_COLOR_ALPHA, THEME, TITLEBAR_HEIGHT,
};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, Point, div, img, px, rgb, rgba,
};
use gpui::{AppContext, ParentElement, StatefulInteractiveElement, Styled, StyledImage};
use hh_protocol::{NotificationKind, Pane};
use std::process::Command;
use std::time::Instant;
use uuid::Uuid;

mod workstation_list;

impl HhApp {
    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar.sidebar_visible = !self.sidebar.sidebar_visible;
        if self.sidebar.sidebar_resize.finish() {
            self.persist_sidebar_width(cx);
        }
        let window_width = self.layout.workspace_pixels.0 + self.sidebar.sidebar_pixels;
        self.sidebar.sidebar_pixels = sidebar_width_for_visibility(
            self.sidebar.preferred_sidebar_width,
            window_width,
            self.sidebar.sidebar_visible,
        );
        self.layout.workspace_pixels.0 = (window_width - self.sidebar.sidebar_pixels).max(1.0);
        self.layout.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    pub(crate) fn toggle_sidebar_activity(&mut self, cx: &mut Context<Self>) {
        self.sidebar.sidebar_activity = !self.sidebar.sidebar_activity;
        if self.sidebar.sidebar_activity {
            self.sidebar.sidebar_visible = true;
        }
        self.refresh_notifications();
        cx.notify();
    }

    pub(crate) fn toggle_workspace_expanded(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        if !self.sidebar.expanded_workspaces.remove(&workspace_id) {
            self.sidebar.expanded_workspaces.insert(workspace_id);
        }
        cx.notify();
    }

    pub(crate) fn toggle_group_collapsed(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        if !self.sidebar.collapsed_groups.remove(&tab_id) {
            self.sidebar.collapsed_groups.insert(tab_id);
        }
        cx.notify();
    }

    pub(crate) fn toggle_sidebar_section(
        &mut self,
        workspace_id: Uuid,
        section: SidebarSection,
        cx: &mut Context<Self>,
    ) {
        let collapsed = match section {
            SidebarSection::Pinned => &mut self.sidebar.collapsed_pinned_sections,
            SidebarSection::Projects => &mut self.sidebar.collapsed_project_sections,
        };
        if !collapsed.remove(&workspace_id) {
            collapsed.insert(workspace_id);
        }
        cx.notify();
    }

    pub(crate) fn persist_sidebar_width(&self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            return;
        };
        let width = self.sidebar.preferred_sidebar_width;
        cx.background_spawn(async move {
            Self::load_ui_state(
                Some(&store),
                "sidebar width was not persisted",
                |store| store.save_workspace_sidebar_width(width),
                (),
            );
        })
        .detach();
    }

    pub(crate) fn render_sidebar_activity(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut notifications = self.session.notifications.clone();
        notifications.sort_by_key(|notification| {
            (
                notification.read,
                !matches!(notification.kind, NotificationKind::Attention),
                std::cmp::Reverse(notification.at_ms),
            )
        });

        let rows = notifications.into_iter().map(|notification| {
            let notification_id = notification.id;
            let pane_id = notification.pane_id;
            let workspace_id = notification.workspace_id;
            let unread = !notification.read;
            let color = match notification.kind {
                NotificationKind::Completed => THEME.ansi[2],
                NotificationKind::Attention => THEME.danger,
                NotificationKind::Message => THEME.accent,
            };
            div()
                .id(("sidebar-activity-row", notification_id))
                .mx(px(8.0))
                .my(px(2.0))
                .px(px(7.0))
                .py(px(6.0))
                .rounded(px(5.0))
                .cursor_pointer()
                .hover(|element| element.bg(rgb(THEME.elevated)))
                .flex()
                .items_center()
                .gap(px(7.0))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_notification(notification_id, pane_id, workspace_id, cx);
                }))
                .child(render_terminal_profile_icon(
                    notification.profile,
                    THEME.muted,
                    18.0,
                ))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(rgb(if unread {
                                    THEME.foreground
                                } else {
                                    THEME.muted
                                }))
                                .child(notification.pane_title),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child(notification.workspace_title),
                        ),
                )
                .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(color)))
        });
        div()
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(34.0))
                    .px(px(8.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Activity"),
                    )
                    .child(
                        div()
                            .id("feed-mark-read")
                            .px(px(6.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.mark_all_notifications_read(cx)),
                            )
                            .child("Read all"),
                    )
                    .child(
                        div()
                            .id("feed-clear")
                            .px(px(6.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .on_click(cx.listener(|this, _, _, cx| this.clear_notifications(cx)))
                            .child("Clear"),
                    ),
            )
            .child(
                div()
                    .id("sidebar-activity")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let sidebar_content_width = self.sidebar.sidebar_pixels - SIDEBAR_RESIZE_HIT_WIDTH;
        div()
            .w(px(sidebar_content_width))
            .h_full()
            .flex_none()
            .bg(rgb(THEME.sidebar))
            // The resize target remains a generous 12 px, while the visible
            // rail separation is intentionally a restrained hairline.
            .border_r(px(0.5))
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .when(!self.sidebar.workstation_banner_hidden, |element| {
                element
                    .child(self.render_banner_header())
                    .child(div().h(px(1.0)).flex_none().bg(rgb(THEME.border)))
            })
            .child(self.render_sidebar_toolbar(cx))
            .child(div().h(px(1.0)).flex_none().bg(rgb(THEME.border)))
            .when(self.sidebar.sidebar_activity, |element| {
                element.child(self.render_sidebar_activity(cx))
            })
            .when(!self.sidebar.sidebar_activity, |element| {
                element.child(self.render_workstation_list(cx))
            })
            .into_any_element()
    }

    /// The workstation banner rail header, hidden per user preference.
    pub(crate) fn render_banner_header(&self) -> AnyElement {
        let banner = self
            .sidebar
            .workstation_banner
            .clone()
            .unwrap_or_else(workstation_banner_artwork);
        let sidebar_content_width = self.sidebar.sidebar_pixels - SIDEBAR_RESIZE_HIT_WIDTH;
        let banner_aspect_ratio = banner.aspect_ratio();
        let banner_header_height =
            workstation_banner_header_height(sidebar_content_width, banner_aspect_ratio);
        let (banner_width, banner_height) = banner_fit_size(
            sidebar_content_width,
            banner_header_height,
            banner_aspect_ratio,
        );
        div()
            .id("workstation-banner")
            .relative()
            .w_full()
            // The header follows the banner's own aspect ratio, clamped between
            // WORKSTATION_BANNER_MIN_HEIGHT and
            // WORKSTATION_BANNER_MAX_HEIGHT, so any uploaded shape shows whole.
            // The image gets explicit pixel dimensions: percentage sizing here
            // rendered a cropped image because gpui injects an aspect ratio
            // during img layout.
            .h(px(banner_header_height))
            .flex_none()
            .overflow_hidden()
            .bg(rgb(THEME.terminal))
            .flex()
            .items_center()
            .justify_center()
            .child(
                img(banner.image)
                    .id("workstation-banner-image")
                    .w(px(banner_width))
                    .h(px(banner_height))
                    .object_fit(gpui::ObjectFit::Contain),
            )
            .into_any_element()
    }

    fn begin_update_install(&mut self, cx: &mut Context<Self>) {
        let Some(update) = self.editor.update_available.as_ref() else {
            return;
        };
        if update.installing {
            return;
        }
        if !update.install_supported {
            self.session.connection_error = Some(
                "This unnotarized community build only notifies about updates; download and run install-community-macos.sh from the GitHub release"
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        let has_live_terminals = self.session.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.active_terminal_count > 0)
        });
        if update.requires_service_restart && has_live_terminals {
            self.session.connection_error = Some(
                "Close all terminals, then update — live sessions must end before the service restarts"
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        let Some(tool) = std::env::current_exe()
            .ok()
            .and_then(|executable| {
                executable
                    .parent()
                    .map(|parent| parent.join("hh-update-tool"))
            })
            .filter(|candidate| candidate.is_file())
        else {
            self.session.connection_error =
                Some("the bundled hh-update-tool is missing".to_owned());
            cx.notify();
            return;
        };
        let process_id = std::process::id();
        let pid = sysinfo::Pid::from_u32(process_id);
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]));
        let Some(process_start_time) = system
            .process(pid)
            .map(|process| process.start_time().to_string())
        else {
            self.session.connection_error =
                Some("could not identify the desktop process for update handoff".to_owned());
            cx.notify();
            return;
        };
        let current_build = hh_updater::current_build().to_string();
        let process_id = process_id.to_string();
        match Command::new(tool)
            .args([
                "install",
                "--current-version",
                env!("CARGO_PKG_VERSION"),
                "--current-build",
                &current_build,
                "--wait-pid",
                &process_id,
                "--wait-start-time",
                &process_start_time,
            ])
            .spawn()
        {
            Ok(_) => {
                if let Some(update) = self.editor.update_available.as_mut() {
                    update.installing = true;
                }
                cx.quit();
            }
            Err(error) => {
                self.session.connection_error =
                    Some(format!("could not start update installer: {error}"));
            }
        }
        cx.notify();
    }

    /// The 40px create / notifications / settings toolbar under the banner.
    pub(crate) fn render_sidebar_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let history_needs_attention = self
            .session
            .history_status
            .as_ref()
            .is_some_and(|status| status.warning.is_some());
        let unread_notifications = self.unread_notification_count();
        let unread_label = if unread_notifications > 99 {
            "99+".to_owned()
        } else {
            unread_notifications.to_string()
        };
        div()
            .h(px(40.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .id("new-workspace")
                    .flex_none()
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded(px(5.0))
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
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        this.editor.modal = Modal::CreateMenu(CreateMenu {
                            position: event.position(),
                            target: CreateMenuTarget::Global,
                        });
                        cx.notify();
                    }))
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Create… (⌘N)".to_owned(),
                        })
                        .into()
                    })
                    .child("＋"),
            )
            .child(
                div()
                    .id("notifications")
                    .relative()
                    .flex_none()
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .when(self.sidebar.sidebar_activity, |element| {
                        element.bg(rgb(THEME.elevated))
                    })
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Notifications".to_owned(),
                        })
                        .into()
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar_activity(cx)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(render_bell_icon(if self.sidebar.sidebar_activity {
                        THEME.foreground
                    } else {
                        THEME.muted
                    }))
                    .when(unread_notifications > 0, |element| {
                        element.child(
                            div()
                                .absolute()
                                .top(px(-3.0))
                                .right(px(-5.0))
                                .min_w(px(15.0))
                                .h(px(14.0))
                                .px(px(3.0))
                                .rounded_full()
                                .bg(rgb(THEME.danger))
                                .font_family(".SystemUIFont")
                                .text_size(px(9.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(0xffffff))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(unread_label),
                        )
                    }),
            )
            .when_some(self.editor.update_available.as_ref(), |toolbar, update| {
                let label = update.label();
                toolbar.child(
                    div()
                        .id("install-update")
                        .h(px(26.0))
                        .px(px(8.0))
                        .rounded(px(5.0))
                        .bg(rgb(THEME.surface))
                        .border_1()
                        .border_color(rgb(THEME.accent))
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.foreground))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(label)
                        .when(update.can_install(), |button| {
                            button
                                .cursor_pointer()
                                .hover(|button| button.bg(rgb(THEME.elevated)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.begin_update_install(cx);
                                }))
                        }),
                )
            })
            .child(
                div()
                    .id("appearance-settings")
                    .relative()
                    .flex_none()
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
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
                            text: "Settings".to_owned(),
                        })
                        .into()
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.open_appearance_settings(cx)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child("⚙")
                    .when(history_needs_attention, |element| {
                        element.child(
                            div()
                                .absolute()
                                .top(px(3.0))
                                .right(px(3.0))
                                .w(px(5.0))
                                .h(px(5.0))
                                .rounded_full()
                                .bg(rgb(THEME.danger)),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(crate) fn render_workspace_terminal_row(
        &self,
        pane: &Pane,
        row: SidebarPaneRowContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SidebarPaneRowContext {
            workspace_id,
            tab_id,
            tab_color,
            from_group,
            indent,
        } = row;
        let pane_id = pane.id;
        let selected = self.layout.focused_pane == Some(pane_id);
        let identity = tab_identity_presentation(pane);
        let identity_detail = identity.detail.clone();
        let drag_title = identity.label.clone();
        let drop_above = !from_group
            && tab_id.is_some_and(|tab_id| {
                self.sidebar.tab_drop_preview.is_some_and(|preview| {
                    preview.target_tab_id == tab_id && !preview.into_group && !preview.after
                })
            });
        let drop_below = !from_group
            && tab_id.is_some_and(|tab_id| {
                self.sidebar.tab_drop_preview.is_some_and(|preview| {
                    preview.target_tab_id == tab_id && !preview.into_group && preview.after
                })
            });
        let pane_accent = pane
            .color
            .or(tab_color)
            .unwrap_or_else(|| self.terminal_accent(pane_id))
            .as_rgb();
        let row_background = composite_rgb(pane_accent, THEME.sidebar, TAB_COLOR_ALPHA);
        let row_text = readable_text_color(row_background);
        div()
            .id(("workspace-tab", element_key(pane_id)))
            .ml(px(indent))
            .mr(px(4.0))
            .px(px(7.0))
            .h(px(27.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(rgba(rgba_with_alpha(pane_accent, TAB_COLOR_ALPHA)))
            .border_t(if drop_above { px(2.0) } else { px(0.0) })
            .border_b(if drop_below { px(2.0) } else { px(0.0) })
            .border_color(rgb(if drop_above || drop_below {
                THEME.accent
            } else {
                row_text
            }))
            .when(selected, |element| element.border_1())
            .hover(|element| element.border_1().border_color(rgb(row_text)))
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: identity_detail.clone(),
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
                if let Some(tab_id) = tab_id {
                    this.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
                } else {
                    this.select_workspace_tab(workspace_id, pane_id, cx);
                }
                cx.stop_propagation();
            }))
            .when_some(tab_id, |element, tab_id| {
                let drag = TabDrag {
                    workspace_id,
                    tab_id,
                    pane_id: Some(pane_id),
                    from_group,
                    title: drag_title,
                    position: Point::default(),
                };
                element
                    .on_drag(drag, |info: &TabDrag, position, _, cx| {
                        cx.new(|_| TabDrag {
                            position,
                            ..info.clone()
                        })
                    })
                    .when(!from_group, |element| {
                        element
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
                                            after: event.event.position.y > event.bounds.center().y,
                                            into_group: false,
                                        });
                                        cx.stop_propagation();
                                        cx.notify();
                                    }
                                },
                            ))
                            .on_drop(cx.listener(move |this, info: &TabDrag, _, cx| {
                                if info.workspace_id == workspace_id {
                                    let after =
                                        this.sidebar.tab_drop_preview.is_some_and(|preview| {
                                            preview.target_tab_id == tab_id && preview.after
                                        });
                                    if let Some(source_pane) =
                                        info.pane_id.filter(|_| info.from_group)
                                    {
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
                    })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_tab_menu(pane_id, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(18.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.render_pane_identity_mark(pane, row_text, row_text)),
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
                    .text_color(rgb(row_text))
                    .child(identity.label),
            )
            .into_any_element()
    }

    pub(crate) fn render_sidebar_resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("workspace-sidebar-resize-handle")
            .relative()
            // The hit target is intentionally wider than the 2 px visual
            // divider, but stays transparent so hover never reads as a fat
            // rail or steals visual space from the workstation list.
            .w(px(SIDEBAR_RESIZE_HIT_WIDTH))
            .h_full()
            .flex_none()
            .cursor(CursorStyle::ResizeLeftRight)
            .flex()
            .justify_center()
            .bg(rgba(0x00000000))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.layout.resizing = None;
                    this.sidebar
                        .sidebar_resize
                        .begin(this.sidebar.preferred_sidebar_width);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("workspace-sidebar-resize-visual")
                    .w(px(SIDEBAR_RESIZE_VISUAL_WIDTH))
                    .h_full()
                    .bg(rgb(THEME.border))
                    .hover(|element| element.bg(rgb(THEME.accent))),
            )
            .into_any_element()
    }

    pub(crate) fn render_global_navigation(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut workspaces = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.workspaces.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        workspaces.sort_by_key(|workspace| (!workspace.pinned, workspace.order));
        let sidebar_visible = self.sidebar.sidebar_visible;
        let navigation_hint = format!(
            "{} · {} · ⇧⌘P commands",
            THEME.name, self.terminal_font.family
        );
        let tab_scroll_to_start = self.sidebar.workstation_tab_scroll.clone();
        let tab_scroll_to_end = self.sidebar.workstation_tab_scroll.clone();
        let last_workspace_index = workspaces.len().saturating_sub(1);

        div()
            .id("global-workstation-navigation")
            .h(px(TITLEBAR_HEIGHT))
            .flex_none()
            // This is the actual macOS titlebar row. Keep controls clear of
            // the traffic lights while sharing their vertical alignment.
            .pl(px(MACOS_TRAFFIC_LIGHT_SAFE_INSET))
            .pr(px(10.0))
            .bg(rgb(THEME.window))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .id("toggle-workstation-sidebar")
                    .flex_none()
                    .w(px(24.0))
                    .h(px(24.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .focusable()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .in_focus(|style| style.bg(rgb(THEME.elevated)))
                    .tooltip(move |_, cx| {
                        cx.new(|_| TooltipView {
                            text: if sidebar_visible {
                                "Hide workstation sidebar (⌘B)".to_owned()
                            } else {
                                "Show workstation sidebar (⌘B)".to_owned()
                            },
                        })
                        .into()
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
                    .child(render_sidebar_toggle_icon(sidebar_visible)),
            )
            .child(
                div()
                    .w(px(1.0))
                    .h(px(18.0))
                    .flex_none()
                    .bg(rgb(THEME.border)),
            )
            .child(
                div()
                    .id("scroll-workstation-tabs-left")
                    .flex_none()
                    .w(px(20.0))
                    .h(px(24.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .focusable()
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
                            text: "Show first workstation tabs".to_owned(),
                        })
                        .into()
                    })
                    .on_click(move |_, _, cx| {
                        tab_scroll_to_start.scroll_to_item(0);
                        cx.refresh_windows();
                    })
                    .child("‹"),
            )
            .child(
                div()
                    .id("global-workstation-tabs")
                    .min_w(px(0.0))
                    .h_full()
                    .flex_1()
                    .overflow_x_scroll()
                    .track_scroll(&self.sidebar.workstation_tab_scroll)
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .children(
                        workspaces
                            .into_iter()
                            .enumerate()
                            .map(|(index, workspace)| {
                                let workspace_id = workspace.id;
                                let active = Some(workspace_id) == self.sidebar.active_workspace;
                                let title = workspace.title.clone();
                                let color = self.workspace_color(workspace_id).as_rgb();
                                let tooltip_title = title.clone();
                                let shortcut = (index < 9).then(|| format!(" (⌘{})", index + 1));
                                div()
                                    .id(("global-workstation-tab", element_key(workspace_id)))
                                    .flex_none()
                                    .max_w(px(220.0))
                                    .h(px(26.0))
                                    .px(px(9.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .focusable()
                                    .when(active, |element| {
                                        element
                                            .bg(rgb(THEME.elevated))
                                            .border_1()
                                            .border_color(rgb(color))
                                    })
                                    .when(!active, |element| {
                                        element
                                            .border_1()
                                            .border_color(rgb(THEME.border))
                                            .hover(|element| element.bg(rgb(THEME.elevated)))
                                    })
                                    .in_focus(|style| style.border_color(rgb(THEME.accent)))
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| TooltipView {
                                            text: format!(
                                                "Switch to {tooltip_title}{}",
                                                shortcut.as_deref().unwrap_or_default()
                                            ),
                                        })
                                        .into()
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_workspace(workspace_id, cx)
                                    }))
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.select_workspace(workspace_id, cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .w(px(7.0))
                                            .h(px(7.0))
                                            .flex_none()
                                            .rounded_full()
                                            .bg(rgb(color)),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .truncate()
                                            .whitespace_nowrap()
                                            .text_sm()
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
                                            .child(title),
                                    )
                            }),
                    ),
            )
            .child(
                div()
                    .id("scroll-workstation-tabs-right")
                    .flex_none()
                    .w(px(20.0))
                    .h(px(24.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .focusable()
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
                            text: "Show more workstation tabs".to_owned(),
                        })
                        .into()
                    })
                    .on_click(move |_, _, cx| {
                        tab_scroll_to_end.scroll_to_item(last_workspace_index);
                        cx.refresh_windows();
                    })
                    .child("›"),
            )
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: navigation_hint.clone(),
                })
                .into()
            })
            .into_any_element()
    }
}
