//! History settings mutation UI and editor dialog.
use crate::helpers::{
    format_bytes, format_history_date, history_label, history_scope_key, history_warning_text,
};
use crate::view_models::{HistoryEditField, HistoryEditor};
use crate::{HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{AnyElement, Context, InteractiveElement, IntoElement, div, px, rgb};
use gpui::{ParentElement, StatefulInteractiveElement, Styled};
use hh_protocol::{
    ClientRequest, HistoryArchiveStatus, HistoryCleanupPolicy, HistoryClearScope, HistoryRetention,
    HistorySettings, ServiceResponse,
};
use uuid::Uuid;

impl HhApp {
    pub(crate) fn apply_history_settings(
        &mut self,
        settings: HistorySettings,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::SetHistorySettings { settings },
            Box::new(|this, _cx, result| match result {
                Ok(ServiceResponse::HistoryStatus { status }) => {
                    this.session.history_status = Some(status);
                    this.editor.history_editor = None;
                    this.session.connection_error = None;
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        cx.notify();
    }

    pub(crate) fn mutate_history_settings(
        &mut self,
        update: impl FnOnce(&mut HistorySettings),
        cx: &mut Context<Self>,
    ) {
        let Some(mut settings) = self
            .session
            .history_status
            .as_ref()
            .map(|status| status.settings.clone())
        else {
            self.refresh_history_status();
            cx.notify();
            return;
        };
        update(&mut settings);
        self.apply_history_settings(settings, cx);
    }

    pub(crate) fn clear_history(&mut self, scope: HistoryClearScope, cx: &mut Context<Self>) {
        if self.editor.history_clear_confirmation != Some(scope) {
            self.editor.history_clear_confirmation = Some(scope);
            cx.notify();
            return;
        }
        self.dispatch_with(
            ClientRequest::ClearHistory { scope },
            Box::new(|this, _cx, result| match result {
                Ok(ServiceResponse::HistoryStatus { status }) => {
                    this.session.history_status = Some(status);
                    this.editor.history_clear_confirmation = None;
                    this.editor.archived_views.clear();
                    this.session.connection_error = None;
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        cx.notify();
    }

    pub(crate) fn begin_history_edit(&mut self, field: HistoryEditField, cx: &mut Context<Self>) {
        let text = match (field, self.session.history_status.as_ref()) {
            (
                HistoryEditField::RetentionDays,
                Some(HistoryArchiveStatus {
                    settings:
                        HistorySettings {
                            retention: HistoryRetention::Days { days },
                            ..
                        },
                    ..
                }),
            ) => days.to_string(),
            (HistoryEditField::RetentionDays, _) => "30".to_owned(),
            (HistoryEditField::QuotaGib, Some(status)) => {
                (status.settings.quota_bytes / 1024 / 1024 / 1024).to_string()
            }
            (HistoryEditField::QuotaGib, None) => "5".to_owned(),
        };
        self.editor.history_editor = Some(HistoryEditor {
            field,
            text,
            replace_on_type: true,
            invalid: false,
        });
        cx.notify();
    }

    pub(crate) fn submit_history_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.history_editor.as_ref() else {
            return;
        };
        let field = editor.field;
        let Ok(value) = editor.text.parse::<u64>() else {
            if let Some(editor) = self.editor.history_editor.as_mut() {
                editor.invalid = true;
            }
            cx.notify();
            return;
        };
        match field {
            HistoryEditField::RetentionDays if (1..=3_650).contains(&value) => {
                self.mutate_history_settings(
                    |settings| {
                        settings.retention = HistoryRetention::Days {
                            days: u32::try_from(value).unwrap_or(3_650),
                        };
                    },
                    cx,
                );
            }
            HistoryEditField::QuotaGib if (1..=4_096).contains(&value) => {
                self.mutate_history_settings(
                    |settings| {
                        settings.quota_bytes = value * 1024 * 1024 * 1024;
                    },
                    cx,
                );
            }
            _ => {
                if let Some(editor) = self.editor.history_editor.as_mut() {
                    editor.invalid = true;
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn render_history_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self
            .session
            .history_status
            .clone()
            .unwrap_or(HistoryArchiveStatus {
                settings: HistorySettings::default(),
                live_scrollback_lines: 2_000,
                archived_bytes: 0,
                retained_sessions: 0,
                oldest_started_ms: None,
                dropped_bytes: 0,
                warning: None,
            });
        let settings = status.settings.clone();
        let oldest = status
            .oldest_started_ms
            .map_or_else(|| "none yet".to_owned(), format_history_date);
        let warning = history_warning_text(status.warning, status.dropped_bytes);

        div()
            .pt(px(4.0))
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(self.render_history_status_header(&settings, cx))
            .child(self.render_history_summary_row(&status, &oldest))
            .children(self.render_history_warning_row(warning))
            .child(self.render_history_retention_row(&settings, cx))
            .child(self.render_history_quota_row(&settings, cx))
            .child(self.render_history_cleanup_row(&settings, cx))
            .child(self.render_history_actions_row(
                self.sidebar.active_workspace,
                self.layout.focused_pane,
                cx,
            ))
            .into_any_element()
    }

    fn render_history_status_header(
        &self,
        settings: &HistorySettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .pt(px(6.0))
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(THEME.foreground))
                    .child("History Storage"),
            )
            .child(
                div()
                    .id("history-enabled")
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(if settings.enabled {
                        THEME.accent_soft
                    } else {
                        THEME.surface
                    }))
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mutate_history_settings(
                            |settings| settings.enabled = !settings.enabled,
                            cx,
                        );
                    }))
                    .child(if settings.enabled {
                        "On · local only"
                    } else {
                        "Off"
                    }),
            )
            .into_any_element()
    }

    fn render_history_summary_row(
        &self,
        status: &HistoryArchiveStatus,
        oldest: &str,
    ) -> AnyElement {
        div()
            .font_family("SF Mono")
            .text_xs()
            .text_color(rgb(THEME.muted))
            .child(format!(
                "Live memory: {} lines · Local archive: {} · {} sessions · oldest {}",
                status.live_scrollback_lines,
                format_bytes(status.archived_bytes),
                status.retained_sessions,
                oldest
            ))
            .into_any_element()
    }

    fn render_history_warning_row(&self, warning: Option<String>) -> Option<AnyElement> {
        warning.map(|warning| {
            div()
                .px(px(8.0))
                .py(px(5.0))
                .rounded(px(5.0))
                .bg(rgb(THEME.surface))
                .font_family(".SystemUIFont")
                .text_xs()
                .text_color(rgb(THEME.danger))
                .child(warning)
                .into_any_element()
        })
    }

    fn render_history_retention_row(
        &self,
        settings: &HistorySettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(history_label("Retention"))
            .children(
                [
                    ("Forever", HistoryRetention::Indefinite),
                    ("7d", HistoryRetention::Days { days: 7 }),
                    ("30d", HistoryRetention::Days { days: 30 }),
                    ("90d", HistoryRetention::Days { days: 90 }),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (label, retention))| {
                    let selected = settings.retention == retention;
                    div()
                        .id(("history-retention", index))
                        .px(px(7.0))
                        .py(px(3.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .bg(rgb(if selected {
                            THEME.accent_soft
                        } else {
                            THEME.surface
                        }))
                        .text_xs()
                        .text_color(rgb(THEME.foreground))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.mutate_history_settings(
                                |settings| settings.retention = retention,
                                cx,
                            );
                        }))
                        .child(label)
                }),
            )
            .child(self.render_history_custom_field(
                HistoryEditField::RetentionDays,
                "Custom days",
                cx,
            ))
            .into_any_element()
    }

    fn render_history_quota_row(
        &self,
        settings: &HistorySettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(history_label("Quota"))
            .children(
                [
                    ("1 GiB", 1_u64),
                    ("5 GiB", 5),
                    ("10 GiB", 10),
                    ("50 GiB", 50),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (label, gib))| {
                    let bytes = gib * 1024 * 1024 * 1024;
                    let selected = settings.quota_bytes == bytes;
                    div()
                        .id(("history-quota", index))
                        .px(px(7.0))
                        .py(px(3.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .bg(rgb(if selected {
                            THEME.accent_soft
                        } else {
                            THEME.surface
                        }))
                        .text_xs()
                        .text_color(rgb(THEME.foreground))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.mutate_history_settings(
                                |settings| settings.quota_bytes = bytes,
                                cx,
                            );
                        }))
                        .child(label)
                }),
            )
            .child(self.render_history_custom_field(HistoryEditField::QuotaGib, "Custom GiB", cx))
            .into_any_element()
    }

    fn render_history_cleanup_row(
        &self,
        settings: &HistorySettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .child(history_label("At capacity"))
            .child(
                div()
                    .id("history-pause-policy")
                    .px(px(7.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .bg(rgb(
                        if settings.cleanup_policy == HistoryCleanupPolicy::PauseWhenFull {
                            THEME.accent_soft
                        } else {
                            THEME.surface
                        },
                    ))
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mutate_history_settings(
                            |settings| {
                                settings.cleanup_policy = HistoryCleanupPolicy::PauseWhenFull;
                            },
                            cx,
                        );
                    }))
                    .child("Pause + warn (safe)"),
            )
            .child(
                div()
                    .id("history-delete-oldest-policy")
                    .px(px(7.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .bg(rgb(
                        if settings.cleanup_policy == HistoryCleanupPolicy::DeleteOldest {
                            THEME.accent_soft
                        } else {
                            THEME.surface
                        },
                    ))
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mutate_history_settings(
                            |settings| {
                                settings.cleanup_policy = HistoryCleanupPolicy::DeleteOldest;
                            },
                            cx,
                        );
                    }))
                    .child("Auto-delete oldest (opt-in)"),
            )
            .into_any_element()
    }

    fn render_history_actions_row(
        &self,
        active_workspace: Option<Uuid>,
        focused_pane: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .child(history_label("Clear"))
            .when_some(focused_pane, |element, pane_id| {
                element.child(self.render_clear_history_button(
                    "Terminal",
                    HistoryClearScope::Terminal { pane_id },
                    cx,
                ))
            })
            .when_some(active_workspace, |element, workspace_id| {
                element.child(self.render_clear_history_button(
                    "Workstation",
                    HistoryClearScope::Workspace { workspace_id },
                    cx,
                ))
            })
            .child(self.render_clear_history_button("All history", HistoryClearScope::All, cx))
            .child(
                div()
                    .flex_1()
                    .text_right()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Future output only · older sessions cannot be recovered"),
            )
            .into_any_element()
    }

    pub(crate) fn render_history_custom_field(
        &self,
        field: HistoryEditField,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self
            .editor
            .history_editor
            .as_ref()
            .filter(|editor| editor.field == field);
        let label = editor.map_or_else(
            || placeholder.to_owned(),
            |editor| {
                if editor.invalid {
                    format!("{} · invalid", editor.text)
                } else {
                    format!("{} ↵", editor.text)
                }
            },
        );
        div()
            .id(match field {
                HistoryEditField::RetentionDays => "custom-retention",
                HistoryEditField::QuotaGib => "custom-quota",
            })
            .px(px(7.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if editor.is_some() {
                THEME.accent
            } else {
                THEME.border
            }))
            .font_family("SF Mono")
            .text_xs()
            .text_color(rgb(if editor.is_some_and(|editor| editor.invalid) {
                THEME.danger
            } else {
                THEME.muted
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.begin_history_edit(field, cx)))
            .child(label)
            .into_any_element()
    }

    pub(crate) fn render_clear_history_button(
        &self,
        label: &'static str,
        scope: HistoryClearScope,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let confirming = self.editor.history_clear_confirmation == Some(scope);
        div()
            .id(("clear-history", history_scope_key(scope)))
            .px(px(7.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if confirming {
                THEME.danger
            } else {
                THEME.border
            }))
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(if confirming {
                THEME.danger
            } else {
                THEME.muted
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.clear_history(scope, cx)))
            .child(if confirming {
                format!("Confirm {label}")
            } else {
                label.to_owned()
            })
            .into_any_element()
    }
}
