//! The workstation sidebar: banner, toolbar, list, and resize handle.
use crate::appearance::workstation_banner_artwork;
use crate::elements::SidebarPaneRowContext;
use crate::helpers::{
    SidebarSection, banner_fit_size, click_suppression_active, composite_rgb, element_key,
    find_pane, identity_detail, identity_label, readable_text_color, render_bell_icon,
    render_robot_icon, render_sidebar_toggle_icon, rgba_with_alpha, sidebar_width_for_visibility,
    workstation_banner_header_height,
};
use crate::view_models::{
    CreateMenu, CreateMenuTarget, Modal, SidebarMode, TabDrag, TabDropPreview, TooltipView,
    UpdateRestartConfirmation,
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
use hh_protocol::Pane;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Instant;
use uuid::Uuid;

mod bots;
mod notifications;
mod workstation_list;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateInstallPlan {
    Install,
    ConfirmServiceRestart { live_terminals: Option<u32> },
}

pub(crate) fn update_install_plan(
    requires_service_restart: bool,
    active_terminal_count: Option<u32>,
) -> UpdateInstallPlan {
    if !requires_service_restart || active_terminal_count == Some(0) {
        UpdateInstallPlan::Install
    } else {
        UpdateInstallPlan::ConfirmServiceRestart {
            live_terminals: active_terminal_count,
        }
    }
}

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
            .child(match self.sidebar.sidebar_mode {
                SidebarMode::Workstations => self.render_workstation_list(cx),
                SidebarMode::Notifications => self.render_sidebar_notifications(cx),
                SidebarMode::Bots => self.render_sidebar_bots(cx),
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
        let active_terminal_count = self.session.snapshot.as_ref().map(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.active_terminal_count)
                .sum::<u32>()
        });
        match update_install_plan(update.requires_service_restart, active_terminal_count) {
            UpdateInstallPlan::Install => self.spawn_update_installer(false, cx),
            UpdateInstallPlan::ConfirmServiceRestart { live_terminals } => {
                self.editor.modal = Modal::UpdateRestart(UpdateRestartConfirmation {
                    version: update.version.clone(),
                    live_terminals,
                });
                cx.notify();
            }
        }
    }

    pub(crate) fn confirm_update_restart(&mut self, cx: &mut Context<Self>) {
        let Modal::UpdateRestart(_) = std::mem::take(&mut self.editor.modal) else {
            return;
        };
        self.spawn_update_installer(true, cx);
    }

    fn spawn_update_installer(&mut self, restart_service: bool, cx: &mut Context<Self>) {
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
        let mut command = Command::new(tool);
        command.args([
            "install",
            "--current-version",
            env!("CARGO_PKG_VERSION"),
            "--current-build",
            &current_build,
            "--wait-pid",
            &process_id,
            "--wait-start-time",
            &process_start_time,
        ]);
        if restart_service {
            command.arg("--restart-service");
        }
        match command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                let stdout = child.stdout.take().expect("piped installer stdout");
                let stderr = child.stderr.take().expect("piped installer stderr");
                if let Some(update) = self.editor.update_available.as_mut() {
                    update.installing = true;
                }
                cx.notify();
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            let ready = BufReader::new(stdout)
                                .lines()
                                .map_while(Result::ok)
                                .any(|line| line == hh_updater::DOWNLOAD_COMPLETE_LINE);
                            if ready {
                                return Ok(());
                            }
                            let mut detail = String::new();
                            let _ = BufReader::new(stderr).read_to_string(&mut detail);
                            let _ = child.wait();
                            Err(detail
                                .lines()
                                .rev()
                                .find(|line| !line.trim().is_empty())
                                .unwrap_or("update installer exited before downloading")
                                .to_owned())
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| match result {
                        Ok(()) => cx.quit(),
                        Err(message) => {
                            if let Some(update) = this.editor.update_available.as_mut() {
                                update.installing = false;
                            }
                            this.session.connection_error =
                                Some(format!("update installer failed: {message}"));
                            cx.notify();
                        }
                    });
                })
                .detach();
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
        let settings_open = matches!(
            self.editor.modal,
            crate::view_models::Modal::AppearanceSettings
        );
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
            .child(self.render_sidebar_mode_button(
                SidebarMode::Notifications,
                "Notifications",
                render_bell_icon,
                self.needs_you_count(),
                cx,
            ))
            .child(self.render_sidebar_mode_button(
                SidebarMode::Bots,
                "Bots",
                render_robot_icon,
                self.bots_needing_you(),
                cx,
            ))
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
                    .when(settings_open, |element| {
                        element
                            .bg(rgb(THEME.accent_soft))
                            .border_1()
                            .border_color(rgb(THEME.accent))
                            .text_color(rgb(THEME.foreground))
                    })
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
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx)))
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

    /// A toolbar toggle for one sidebar mode, with a red count badge.
    fn render_sidebar_mode_button(
        &self,
        mode: SidebarMode,
        label: &'static str,
        icon: fn(u32) -> AnyElement,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.sidebar.sidebar_mode == mode;
        let count_label = if count > 99 {
            "99+".to_owned()
        } else {
            count.to_string()
        };
        div()
            .id(label)
            .relative()
            .flex_none()
            .w(px(26.0))
            .h(px(26.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .when(active, |element| {
                element
                    .bg(rgb(THEME.accent_soft))
                    .border_1()
                    .border_color(rgb(THEME.accent))
            })
            .hover(|element| element.bg(rgb(THEME.elevated)))
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: label.to_owned(),
                })
                .into()
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_sidebar_mode(mode, cx)))
            .flex()
            .items_center()
            .justify_center()
            .child(icon(if active {
                THEME.foreground
            } else {
                THEME.muted
            }))
            .when(count > 0, |element| {
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
                        .child(count_label),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
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
            activity,
        } = row;
        let pane_id = pane.id;
        let bot_row = activity.as_ref().is_some_and(|activity| activity.bot);
        // Notifications rows jump to their pane but never drag-reorder.
        let drag_tab_id = tab_id.filter(|_| activity.is_none());
        let selected = self.layout.focused_pane == Some(pane_id);
        let input = cx.entity();
        let drag_title = identity_label(pane).to_owned();
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
        let user_color = pane.color.or(tab_color);
        let pane_accent = user_color
            .unwrap_or_else(|| self.terminal_accent(pane_id))
            .as_rgb();
        let row_background = user_color.map_or(
            composite_rgb(pane_accent, THEME.sidebar, TAB_COLOR_ALPHA),
            |color| color.as_rgb(),
        );
        let row_text = readable_text_color(row_background);
        div()
            .id(("workspace-tab", element_key(pane_id)))
            .ml(px(indent))
            .mr(px(4.0))
            .px(px(7.0))
            .when(activity.is_none(), |element| element.h(px(27.0)))
            .when(activity.is_some(), |element| element.py(px(5.0)))
            .rounded(px(4.0))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(user_color.map_or(
                rgba(rgba_with_alpha(pane_accent, TAB_COLOR_ALPHA)),
                |color| rgb(color.as_rgb()),
            ))
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
                let text = input
                    .read(cx)
                    .session
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .flat_map(|workspace| &workspace.tabs)
                            .find_map(|tab| find_pane(&tab.layout, pane_id))
                    })
                    .map(identity_detail)
                    .unwrap_or_default();
                cx.new(|_| TooltipView { text }).into()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if click_suppression_active(
                    &mut this.sidebar.suppress_tab_click_until,
                    Instant::now(),
                ) {
                    cx.notify();
                    return;
                }
                if let Some(tab_id) = tab_id.filter(|_| bot_row) {
                    this.open_bot(tab_id, cx);
                } else if let Some(tab_id) = tab_id {
                    this.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
                } else {
                    this.select_workspace_tab(workspace_id, pane_id, cx);
                }
                cx.stop_propagation();
            }))
            .when_some(drag_tab_id, |element, tab_id| {
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
                                        let next = Some(TabDropPreview {
                                            target_tab_id: tab_id,
                                            after: event.event.position.y > event.bounds.center().y,
                                            into_group: false,
                                        });
                                        cx.stop_propagation();
                                        if this.sidebar.tab_drop_preview != next {
                                            this.sidebar.tab_drop_preview = next;
                                            cx.notify();
                                        }
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
                    match tab_id.filter(|_| bot_row) {
                        Some(tab_id) => this.open_bot_menu(tab_id, event.position, cx),
                        None => this.open_tab_menu(pane_id, event.position, cx),
                    }
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
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .font_weight(if selected {
                                gpui::FontWeight::MEDIUM
                            } else {
                                gpui::FontWeight::NORMAL
                            })
                            .text_color(rgb(row_text))
                            .child(identity_label(pane).to_owned()),
                    )
                    .when_some(activity.as_ref(), |element, activity| {
                        element.child(
                            div()
                                .truncate()
                                .font_family(".SystemUIFont")
                                .text_size(px(10.0))
                                .text_color(rgb(row_text))
                                .opacity(0.75)
                                .child(activity.location.clone()),
                        )
                    }),
            )
            .when_some(activity, |element, activity| {
                element.child(
                    div()
                        .flex_none()
                        .px(px(5.0))
                        .py(px(1.0))
                        .rounded(px(4.0))
                        .bg(rgb(activity.badge_color))
                        .font_family(".SystemUIFont")
                        .text_size(px(9.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(readable_text_color(activity.badge_color)))
                        .child(activity.badge),
                )
            })
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
            .map(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .filter(|workspace| !workspace.is_bots())
                    .collect::<Vec<_>>()
            })
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
