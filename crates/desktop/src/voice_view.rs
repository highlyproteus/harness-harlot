use super::*;
use gpui::ClipboardItem;

impl HhApp {
    pub(crate) fn render_assistant_workspace(
        &self,
        pane: &Pane,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane_id = pane.id;
        let fallback = AssistantPaneState::default();
        let state = self.assistant.panes.get(&pane_id).unwrap_or(&fallback);
        div()
            .id(("assistant-workspace", element_key(pane_id)))
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_assistant_header(pane_id, state, cx))
            .child(self.render_assistant_thread(pane_id, state, cx))
            .child(self.render_assistant_composer_row(pane_id, state, cx))
            .into_any_element()
    }

    fn render_assistant_header(
        &self,
        pane_id: Uuid,
        pane: &AssistantPaneState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = pane.view.as_ref().map(|view| &view.status);
        let (label, color) = match status {
            Some(AssistantStatus::Starting) => ("Starting", THEME.accent_soft),
            Some(AssistantStatus::Idle) => ("Idle", THEME.dim),
            Some(AssistantStatus::Streaming) => ("Working", THEME.ansi[2]),
            Some(AssistantStatus::Compacting) => ("Compacting", THEME.accent),
            Some(AssistantStatus::Exited { .. }) => ("Exited", THEME.danger),
            Some(AssistantStatus::Unavailable { .. }) => ("Unavailable", THEME.danger),
            None => ("Loading", THEME.dim),
        };
        let can_abort = matches!(
            status,
            Some(AssistantStatus::Streaming | AssistantStatus::Compacting)
        );
        let can_restart = matches!(
            status,
            Some(AssistantStatus::Exited { .. } | AssistantStatus::Unavailable { .. })
        );
        div()
            .h(px(PANE_HEADER_HEIGHT))
            .flex_none()
            .px(px(10.0))
            .border_b_1()
            .border_color(rgb(THEME.border))
            .bg(rgb(THEME.surface))
            .flex()
            .items_center()
            .gap(px(7.0))
            .child(div().size(px(7.0)).rounded_full().bg(rgb(color)))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child(label),
            )
            .child(div().flex_1())
            .when(can_abort, |element| {
                element.child(
                    div()
                        .id(("assistant-abort", element_key(pane_id)))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.danger))
                        .child("Stop")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dispatch(ClientRequest::AssistantAbort { pane_id });
                            cx.stop_propagation();
                        })),
                )
            })
            .when(can_restart, |element| {
                element.child(
                    div()
                        .id(("assistant-restart", element_key(pane_id)))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.accent))
                        .child("Restart")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dispatch(ClientRequest::AssistantRestart { pane_id });
                            cx.stop_propagation();
                        })),
                )
            })
            .child(
                div()
                    .id(("assistant-settings-button", element_key(pane_id)))
                    .w(px(22.0))
                    .h(px(22.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .child("⚙")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_settings(crate::view_models::SettingsSection::Assistant, cx);
                        cx.stop_propagation();
                    })),
            )
            .into_any_element()
    }

    fn render_assistant_thread(
        &self,
        pane_id: Uuid,
        pane: &AssistantPaneState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = Vec::new();
        if let Some(view) = pane.view.as_ref() {
            if view.truncated_entries > 0 {
                rows.push(
                    div()
                        .w_full()
                        .font_family("SF Mono")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child(format!("{} older entries omitted", view.truncated_entries))
                        .into_any_element(),
                );
            }
            rows.extend(view.entries.iter().enumerate().map(|(index, entry)| {
                self.render_assistant_entry(pane_id, index, entry, pane, cx)
            }));
            if let Some(approval) = view.pending_approval.as_ref() {
                rows.push(self.render_assistant_approval(pane_id, approval, cx));
            }
            if let AssistantStatus::Exited { message } | AssistantStatus::Unavailable { message } =
                &view.status
            {
                rows.push(
                    div()
                        .w_full()
                        .p(px(9.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(rgb(THEME.danger))
                        .font_family("SF Mono")
                        .text_xs()
                        .text_color(rgb(THEME.danger))
                        .child(message.clone())
                        .into_any_element(),
                );
            }
        }
        if let Some(message) = pane.local_notice.as_ref() {
            rows.push(
                div()
                    .w_full()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.danger))
                    .child(message.clone())
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .w_full()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("What should the assistant do in this workstation?"),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("It opens terminals, runs coding agents, and reports back."),
                    )
                    .into_any_element(),
            );
        }
        div()
            .id(("assistant-thread", element_key(pane_id)))
            .min_h(px(0.0))
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&pane.transcript_scroll)
            .px(px(16.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .bg(rgb(THEME.terminal))
            .child(
                div()
                    .w_full()
                    .max_w(px(760.0))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_assistant_entry(
        &self,
        pane_id: Uuid,
        index: usize,
        entry: &AssistantEntry,
        pane: &AssistantPaneState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match entry {
            AssistantEntry::User {
                text,
                image_count,
                timestamp_ms,
            } => self.render_assistant_message(
                pane_id,
                index,
                true,
                text,
                true,
                *timestamp_ms,
                (*image_count > 0).then(|| format!("{image_count} image attached")),
                None,
                pane.selected_entry == Some(index),
                cx,
            ),
            AssistantEntry::Assistant {
                text,
                final_,
                timestamp_ms,
            } => self.render_assistant_message(
                pane_id,
                index,
                false,
                text,
                *final_,
                *timestamp_ms,
                None,
                pane.voice.as_ref().and_then(|voice| {
                    pane.view.as_ref().and_then(|view| {
                        voice
                            .spoken_transcripts
                            .get(&absolute_entry(view, index))
                            .cloned()
                    })
                }),
                pane.selected_entry == Some(index),
                cx,
            ),
            AssistantEntry::ToolCall {
                tool_call_id: _,
                tool_name,
                summary,
                output,
                done,
                is_error,
                target_pane,
            } => {
                let expanded = pane.selected_entry == Some(index);
                let target = *target_pane;
                div()
                    .id(("assistant-tool", index))
                    .w_full()
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(if *is_error {
                        THEME.danger
                    } else {
                        THEME.border
                    }))
                    .bg(rgb(THEME.surface))
                    .cursor_pointer()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(
                                div()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(rgb(if *is_error {
                                        THEME.danger
                                    } else {
                                        THEME.accent
                                    }))
                                    .child(if *done { "✓" } else { "…" }),
                            )
                            .child(
                                div()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(rgb(THEME.foreground))
                                    .child(tool_name.clone()),
                            )
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex_1()
                                    .truncate()
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.muted))
                                    .child(summary.clone()),
                            )
                            .when_some(target, |element, target_pane| {
                                element.child(
                                    div()
                                        .id(("assistant-tool-target", index))
                                        .font_family("SF Mono")
                                        .text_xs()
                                        .text_color(rgb(THEME.accent))
                                        .child("Open pane")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.focus_pane_with_snapshot(target_pane, cx);
                                            cx.stop_propagation();
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(rgb(THEME.dim))
                                    .child(if expanded { "⌄" } else { "›" }),
                            ),
                    )
                    .when(expanded && !output.is_empty(), |element| {
                        element.child(
                            div()
                                .id(("assistant-tool-output", index))
                                .w_full()
                                .mt(px(6.0))
                                .p(px(8.0))
                                .rounded(px(6.0))
                                .bg(rgb(THEME.terminal))
                                .max_h(px(240.0))
                                .overflow_y_scroll()
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(if *is_error { THEME.danger } else { THEME.muted }))
                                .children(output.lines().map(|line| div().child(line.to_owned()))),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(pane) = this.assistant.panes.get_mut(&pane_id) {
                            pane.selected_entry = if pane.selected_entry == Some(index) {
                                None
                            } else {
                                Some(index)
                            };
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .into_any_element()
            }
            AssistantEntry::Notice { message, level } => {
                let color = match level {
                    hh_protocol::AssistantNoticeLevel::Info => THEME.dim,
                    hh_protocol::AssistantNoticeLevel::Warning => THEME.accent,
                    hh_protocol::AssistantNoticeLevel::Error => THEME.danger,
                };
                div()
                    .w_full()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(color))
                    .child(message.clone())
                    .into_any_element()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_assistant_message(
        &self,
        pane_id: Uuid,
        index: usize,
        user: bool,
        text: &str,
        final_: bool,
        timestamp_ms: u64,
        attachment: Option<String>,
        spoken: Option<String>,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = text.to_owned();
        div()
            .id(("assistant-entry", index))
            .w_full()
            .flex()
            .when(user, |element| element.justify_end())
            .child(
                div()
                    .id(("assistant-message", index))
                    .min_w(px(0.0))
                    .cursor_text()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .line_height(relative(1.5))
                    .text_color(rgb(THEME.foreground))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .when(user, |element| {
                        element
                            .max_w(relative(0.8))
                            .px(px(12.0))
                            .py(px(8.0))
                            .rounded(px(12.0))
                            .bg(rgb(THEME.elevated))
                    })
                    .when(!user, |element| element.w_full())
                    .when(!user && selected, |element| {
                        element
                            .border_l_2()
                            .border_color(rgb(THEME.accent))
                            .pl(px(8.0))
                    })
                    .when(user && selected, |element| {
                        element.border_1().border_color(rgb(THEME.accent))
                    })
                    .when_some(attachment, |element, label| {
                        element.child(
                            div()
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(THEME.accent))
                                .child(label),
                        )
                    })
                    .children(text.lines().map(|line| {
                        div().child(if line.is_empty() {
                            " ".to_owned()
                        } else {
                            line.to_owned()
                        })
                    }))
                    .when(!final_, |element| {
                        element.child(div().text_color(rgb(THEME.accent)).child("▮"))
                    })
                    .when_some(spoken, |element, transcript| {
                        element.child(
                            div()
                                .italic()
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child(transcript),
                        )
                    })
                    .child(
                        div()
                            .w_full()
                            .pt(px(2.0))
                            .flex()
                            .when(user, |element| element.justify_end())
                            .font_family("SF Mono")
                            .text_size(px(10.0))
                            .text_color(rgb(THEME.dim))
                            .child(format_clock(timestamp_ms))
                            .child(
                                div()
                                    .id(("assistant-message-copy", index))
                                    .ml(px(6.0))
                                    .cursor_pointer()
                                    .child("⧉")
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            message.clone(),
                                        ));
                                        cx.stop_propagation();
                                    })),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(pane) = this.assistant.panes.get_mut(&pane_id) {
                            pane.selected_entry = Some(index);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    })),
            )
            .into_any_element()
    }

    fn render_assistant_approval(
        &self,
        pane_id: Uuid,
        approval: &hh_protocol::AssistantApproval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let allow_id = approval.request_id.clone();
        let deny_id = approval.request_id.clone();
        div()
            .id(("assistant-approval", element_key(pane_id)))
            .w_full()
            .p(px(10.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(rgb(THEME.accent))
            .bg(rgb(THEME.surface))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child(approval.title.clone()),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child(approval.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id(("assistant-approval-allow", element_key(pane_id)))
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.accent))
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.window))
                            .child("Allow (Enter)")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dispatch(ClientRequest::AssistantApprovalResponse {
                                    pane_id,
                                    request_id: allow_id.clone(),
                                    allow: true,
                                });
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("assistant-approval-deny", element_key(pane_id)))
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(THEME.danger))
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.danger))
                            .child("Deny (Esc)")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dispatch(ClientRequest::AssistantApprovalResponse {
                                    pane_id,
                                    request_id: deny_id.clone(),
                                    allow: false,
                                });
                                cx.stop_propagation();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_assistant_composer_row(
        &self,
        pane_id: Uuid,
        pane: &AssistantPaneState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let voice_active = pane.voice.as_ref().is_some_and(|voice| {
            !voice.engine.is_finished() && !matches!(voice.engine_state, EngineState::Suspended)
        });
        let attachment = self
            .editor
            .assistant_composer
            .as_ref()
            .filter(|composer| composer.pane_id == pane_id)
            .and_then(|composer| composer.attachment.as_ref())
            .map(|attachment| (attachment.filename.clone(), attachment.path.clone()));
        let composer_active = self
            .editor
            .assistant_composer
            .as_ref()
            .filter(|composer| composer.pane_id == pane_id);
        let text = composer_active.map_or(String::new(), |composer| composer.text.clone());
        let interim = pane
            .voice
            .as_ref()
            .map_or("", |voice| voice.interim_transcript.as_str());
        let model_label = pane
            .view
            .as_ref()
            .and_then(|view| view.model.as_deref())
            .map_or_else(
                || "Select model".to_owned(),
                |model| model.rsplit('/').next().unwrap_or(model).to_owned(),
            );
        let access = self
            .session
            .snapshot
            .as_ref()
            .map_or(AssistantAccess::Full, |snapshot| snapshot.assistant.access);
        let has_attachment = attachment.is_some();
        let can_send = !pane.prompt_in_flight && (!text.trim().is_empty() || has_attachment);
        let active = composer_active.is_some();
        div()
            .flex_none()
            .px(px(16.0))
            .pt(px(6.0))
            .pb(px(14.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .id(("assistant-composer-box", element_key(pane_id)))
                    .w_full()
                    .max_w(px(760.0))
                    .mx_auto()
                    .rounded(px(12.0))
                    .bg(rgb(THEME.surface))
                    .border_1()
                    .border_color(rgb(if active { THEME.accent } else { THEME.border }))
                    .flex()
                    .flex_col()
                    .when_some(attachment, |element, (filename, path)| {
                        element.child(
                            div()
                                .px(px(12.0))
                                .pt(px(8.0))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    img(path)
                                        .h(px(42.0))
                                        .max_w(px(140.0))
                                        .object_fit(gpui::ObjectFit::Contain)
                                        .rounded(px(4.0)),
                                )
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .font_family(".SystemUIFont")
                                        .text_xs()
                                        .text_color(rgb(THEME.foreground))
                                        .child(filename),
                                )
                                .child(
                                    div()
                                        .id(("assistant-attachment-remove", element_key(pane_id)))
                                        .cursor_pointer()
                                        .text_color(rgb(THEME.danger))
                                        .child("×")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(composer) =
                                                this.editor.assistant_composer.as_mut()
                                            {
                                                composer.attachment = None;
                                            }
                                            cx.notify();
                                            cx.stop_propagation();
                                        })),
                                ),
                        )
                    })
                    .child(
                        self.render_assistant_composer_field(pane_id, &text, interim, active, cx),
                    )
                    .child(
                        div()
                            .px(px(10.0))
                            .pb(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id(("assistant-model", element_key(pane_id)))
                                    .px(px(8.0))
                                    .py(px(3.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.elevated))
                                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.foreground))
                                    .child(format!("{model_label} ⌄"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_assistant_models(pane_id, cx);
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("assistant-access", element_key(pane_id)))
                                    .px(px(8.0))
                                    .py(px(3.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.elevated))
                                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.muted))
                                    .child(if access == AssistantAccess::Full {
                                        "Full access"
                                    } else {
                                        "Confirm actions"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_assistant_access(
                                            if access == AssistantAccess::Full {
                                                AssistantAccess::Confirm
                                            } else {
                                                AssistantAccess::Full
                                            },
                                        );
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id(("assistant-composer-attach", element_key(pane_id)))
                                    .size(px(26.0))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .font_family("SF Mono")
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.elevated)))
                                    .child("+")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.attach_assistant_image(pane_id, cx);
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("assistant-voice-toggle", element_key(pane_id)))
                                    .size(px(26.0))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(rgb(if voice_active {
                                        THEME.accent
                                    } else {
                                        THEME.muted
                                    }))
                                    .hover(|element| element.bg(rgb(THEME.elevated)))
                                    .child("●")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if voice_active {
                                            this.send_assistant_command(
                                                pane_id,
                                                VoiceCommand::Suspend,
                                            );
                                        } else {
                                            this.start_voice_assistant(pane_id, cx);
                                        }
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("assistant-composer-send", element_key(pane_id)))
                                    .size(px(26.0))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .font_family("SF Mono")
                                    .bg(rgb(if can_send {
                                        THEME.accent
                                    } else {
                                        THEME.elevated
                                    }))
                                    .text_color(rgb(if can_send {
                                        THEME.window
                                    } else {
                                        THEME.dim
                                    }))
                                    .child("↑")
                                    .when(can_send, |element| {
                                        element.cursor_pointer().on_click(cx.listener(
                                            |this, _, _, cx| {
                                                this.submit_assistant_composer(cx);
                                                cx.stop_propagation();
                                            },
                                        ))
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Growing multi-line composer text area; keystrokes stay with the
    /// pane-level key handler.
    fn render_assistant_composer_field(
        &self,
        pane_id: Uuid,
        text: &str,
        interim: &str,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let placeholder = !active && interim.is_empty() && text.is_empty();
        let source = if interim.is_empty() { text } else { interim };
        let lines = source.split('\n').collect::<Vec<_>>();
        let last = lines.len().saturating_sub(1);
        let caret = active || !interim.is_empty();
        div()
            .id(("assistant-composer-field", element_key(pane_id)))
            .w_full()
            .px(px(12.0))
            .py(px(8.0))
            .min_h(px(24.0))
            .max_h(px(168.0))
            .overflow_y_scroll()
            .cursor_text()
            .font_family(".SystemUIFont")
            .text_sm()
            .line_height(relative(1.45))
            .text_color(rgb(if placeholder {
                THEME.dim
            } else {
                THEME.foreground
            }))
            .when(placeholder, |element| {
                element.child("Message the assistant…")
            })
            .when(!placeholder, |element| {
                element.children(lines.iter().enumerate().map(|(index, line)| {
                    let mut rendered = (*line).to_owned();
                    if caret && index == last {
                        rendered.push('▮');
                    } else if rendered.is_empty() {
                        rendered.push(' ');
                    }
                    div().whitespace_normal().child(rendered)
                }))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.focus_pane_with_snapshot(pane_id, cx);
                activate_assistant_composer(&mut this.editor.assistant_composer, pane_id);
                cx.notify();
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    pub(crate) fn render_assistant_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let picker = &self.assistant.models_picker;
        let rows = picker
            .models
            .iter()
            .enumerate()
            .map(|(index, model)| {
                let selected = picker.selected == index;
                let provider = model.provider.clone();
                let id = model.id.clone();
                let name = model.name.clone();
                div()
                    .id(("assistant-model-option", index))
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .bg(rgb(if selected {
                        THEME.accent_soft
                    } else {
                        THEME.surface
                    }))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child(name),
                    )
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child(format!("{provider}/{id}")),
                    )
                    .when(selected, |element| {
                        element.child(
                            div()
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(THEME.accent))
                                .child("✓"),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.assistant.models_picker.selected = index;
                        this.select_assistant_model(cx);
                        cx.stop_propagation();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(gpui::rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(460.0))
                    .max_h(px(540.0))
                    .p(px(18.0))
                    .rounded(px(12.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Choose a model"),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .child(
                                "Applies to this assistant and becomes the default for new ones.",
                            ),
                    )
                    .when(picker.loading, |element| {
                        element.child(
                            div()
                                .font_family("SF Mono")
                                .text_sm()
                                .text_color(rgb(THEME.dim))
                                .child("Loading models…"),
                        )
                    })
                    .when_some(picker.error.clone(), |element, error| {
                        element.child(
                            div()
                                .font_family("SF Mono")
                                .text_sm()
                                .text_color(rgb(THEME.danger))
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .id("assistant-model-list")
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .children(rows),
                    )
                    .child(
                        div()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("↑/↓ select · Enter apply · Esc close"),
                    ),
            )
            .into_any_element()
    }
}
