//! The Notifications sidebar: live panes that need the user or are running,
//! then the service's stored notifications (Recent), newest first.
use crate::elements::{ActivityRow, SidebarPaneRowContext};
use crate::notifications::{ActivitySection, activity_entries};
use crate::{HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{AnyElement, Context, InteractiveElement, IntoElement, div, px, rgb};
use gpui::{ParentElement, StatefulInteractiveElement, Styled};
use hh_protocol::{NotificationKind, SessionNotification};

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

fn header_action(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&mut HhApp) + 'static,
    cx: &mut Context<HhApp>,
) -> AnyElement {
    div()
        .id(id)
        .px(px(6.0))
        .cursor_pointer()
        .font_family(".SystemUIFont")
        .text_xs()
        .text_color(rgb(THEME.muted))
        .hover(|element| element.text_color(rgb(THEME.foreground)))
        .on_click(cx.listener(move |this, _, _, cx| {
            on_click(this);
            cx.notify();
        }))
        .child(label)
        .into_any_element()
}

/// A stored notification's headline: its text, else what happened.
fn notification_title(notification: &SessionNotification) -> String {
    notification
        .message
        .clone()
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            match notification.kind {
                NotificationKind::Completed => "Done",
                NotificationKind::Attention => "Needs you",
                NotificationKind::Message => "Message",
            }
            .to_owned()
        })
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
                    let bot = entry.workspace.is_bot();
                    self.render_workspace_terminal_row(
                        entry.pane,
                        SidebarPaneRowContext {
                            workspace_id: entry.workspace.id,
                            tab_id: Some(entry.tab.id),
                            tab_color: entry.tab.color,
                            from_pane_map: false,
                            indent: 4.0,
                            activity: Some(ActivityRow {
                                indicator: self.pane_indicator(entry.pane),
                                unread: entry.pane.unseen,
                                location: if bot {
                                    format!("Bot · {}", entry.workspace.title)
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
        if !self.session.notifications.is_empty() {
            rows.push(self.render_recent_header(cx));
            rows.extend(self.render_recent_rows(cx));
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
            .into_any_element()
    }

    fn render_recent_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let any_unread = self.unread_badge().is_some();
        div()
            .pl(px(12.0))
            .pr(px(6.0))
            .pt(px(8.0))
            .pb(px(3.0))
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(THEME.dim))
                    .child("Recent"),
            )
            .when(any_unread, |element| {
                element.child(header_action(
                    "notifications-mark-all-read",
                    "Mark all read",
                    Self::mark_all_notifications_read,
                    cx,
                ))
            })
            .child(header_action(
                "notifications-clear",
                "Clear",
                Self::clear_notifications,
                cx,
            ))
            .into_any_element()
    }

    /// Stored notifications, newest first: unread ones carry a blue dot and
    /// a bold title. Clicking one opens its pane.
    fn render_recent_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut notifications = self.session.notifications.iter().collect::<Vec<_>>();
        notifications.sort_by_key(|notification| std::cmp::Reverse(notification.id));
        let now_ms = crate::bots::now_ms();
        notifications
            .into_iter()
            .map(|notification| {
                let id = notification.id;
                let unread = !notification.read;
                let marker = match notification.kind {
                    NotificationKind::Attention => THEME.warning,
                    NotificationKind::Completed | NotificationKind::Message => THEME.accent,
                };
                div()
                    .id(("sidebar-notification", id))
                    .mx(px(4.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|element| element.bg(rgb(THEME.elevated)))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_notification(id, cx);
                        cx.stop_propagation();
                    }))
                    .child(crate::tab_chrome::render_unread_dot(unread))
                    .child(
                        div()
                            .flex_none()
                            .w(px(3.0))
                            .h(px(22.0))
                            .rounded(px(1.5))
                            .bg(rgb(marker)),
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
                                    .font_weight(if unread {
                                        gpui::FontWeight::SEMIBOLD
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(rgb(if unread {
                                        THEME.foreground
                                    } else {
                                        THEME.muted
                                    }))
                                    .child(notification_title(notification)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(".SystemUIFont")
                                    .text_size(px(10.0))
                                    .text_color(rgb(THEME.dim))
                                    .child(format!(
                                        "{} · {} · {}",
                                        notification.pane_title,
                                        notification.workspace_title,
                                        crate::bots::relative_time(now_ms, notification.at_ms),
                                    )),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }
}
