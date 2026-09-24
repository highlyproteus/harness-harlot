//! The Notifications sidebar: live pane activity plus service messages.
use crate::elements::{ActivityRow, SidebarPaneRowContext};
use crate::notifications::{ActivitySection, activity_badge, activity_entries};
use crate::{HhApp, THEME, pane_status_color};
use gpui::prelude::FluentBuilder;
use gpui::{AnyElement, Context, InteractiveElement, IntoElement, div, px, rgb};
use gpui::{ParentElement, StatefulInteractiveElement, Styled};

fn section_heading(title: &'static str) -> AnyElement {
    div()
        .px(px(12.0))
        .pt(px(8.0))
        .pb(px(3.0))
        .font_family(".SystemUIFont")
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(THEME.dim))
        .child(title)
        .into_any_element()
}

impl HhApp {
    pub(crate) fn render_sidebar_notifications(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut rows = Vec::new();
        if let Some(snapshot) = self.session.snapshot.as_ref() {
            let entries = activity_entries(snapshot, &self.session.pane_states);
            for section in ActivitySection::ALL {
                let mut section_entries = entries
                    .iter()
                    .filter(|entry| entry.section == section)
                    .peekable();
                if section_entries.peek().is_none() {
                    continue;
                }
                rows.push(section_heading(section.title()));
                rows.extend(section_entries.map(|entry| {
                    let bot = entry.workspace.is_bots();
                    self.render_workspace_terminal_row(
                        entry.pane,
                        SidebarPaneRowContext {
                            workspace_id: entry.workspace.id,
                            tab_id: Some(entry.tab.id),
                            tab_color: entry.tab.color,
                            from_group: false,
                            indent: 4.0,
                            activity: Some(ActivityRow {
                                badge: activity_badge(entry.pane.status, entry.exited),
                                badge_color: pane_status_color(entry.pane.status)
                                    .filter(|_| !entry.exited)
                                    .unwrap_or(THEME.dim),
                                location: if bot {
                                    "Bot".to_owned()
                                } else {
                                    entry.workspace.title.clone()
                                },
                                bot,
                            }),
                        },
                        cx,
                    )
                }));
            }
        }
        let empty = rows.is_empty();
        div()
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("sidebar-activity")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(rows)
                    .when(empty, |element| {
                        element.child(
                            div()
                                .px(px(12.0))
                                .py(px(10.0))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child("No terminal needs you, runs, or finished recently"),
                        )
                    }),
            )
            .when(!self.session.notifications.is_empty(), |element| {
                element.child(self.render_sidebar_messages(cx))
            })
            .into_any_element()
    }

    /// Service messages (e.g. tmux fallback errors), newest first.
    fn render_sidebar_messages(&self, cx: &mut Context<Self>) -> AnyElement {
        let messages = self.session.notifications.iter().rev().map(|notification| {
            div()
                .id(("sidebar-message", notification.id))
                .px(px(12.0))
                .py(px(4.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.foreground))
                        .child(
                            notification
                                .message
                                .clone()
                                .unwrap_or_else(|| notification.pane_title.clone()),
                        ),
                )
                .child(
                    div()
                        .truncate()
                        .font_family(".SystemUIFont")
                        .text_size(px(10.0))
                        .text_color(rgb(THEME.dim))
                        .child(format!(
                            "{} · {}",
                            notification.workspace_title, notification.pane_title
                        )),
                )
        });
        div()
            .flex_none()
            .max_h(px(180.0))
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.dim))
                            .child("Messages"),
                    )
                    .child(
                        div()
                            .id("sidebar-messages-clear")
                            .px(px(6.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .hover(|element| element.text_color(rgb(THEME.foreground)))
                            .on_click(cx.listener(|this, _, _, _| this.clear_notifications()))
                            .child("Clear"),
                    ),
            )
            .child(
                div()
                    .id("sidebar-messages")
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .children(messages),
            )
            .into_any_element()
    }
}
