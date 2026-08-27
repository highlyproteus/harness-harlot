use super::*;

impl HhApp {
    pub(crate) fn render_assistant_workspace(
        &self,
        pane: &Pane,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane_id = pane.id;
        // Reconciliation normally owns every assistant session; keep the
        // ephemeral fallback for the initial frame before a snapshot arrives.
        let empty = AssistantSession::new();
        let session = self.voice.sessions.get(&pane_id).unwrap_or(&empty);
        let show_idle = assistant_workspace_shows_idle(session);
        div()
            .id(("assistant-workspace", element_key(pane_id)))
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_assistant_header(pane_id, session, cx))
            .when(show_idle, |element| {
                element.child(self.render_assistant_idle(pane_id, session, cx))
            })
            .when(!show_idle, |element| {
                element.child(self.render_assistant_live(pane_id, session, cx))
            })
            .into_any_element()
    }

    fn render_assistant_header(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (state_label, state_color) = match &session.engine_state {
            EngineState::Connecting => ("Connecting", THEME.accent_soft),
            EngineState::Listening => ("Listening", THEME.ansi[2]),
            EngineState::Thinking => ("Thinking", THEME.accent),
            EngineState::Speaking => ("Speaking", THEME.accent),
            EngineState::ToolRunning => ("Running tool", THEME.dim),
            EngineState::Suspended => ("Suspended", THEME.dim),
            EngineState::Error(_) => ("Error", THEME.danger),
        };
        let model = self.voice.settings_editor.settings.model.clone();
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
            .child(
                div()
                    .w(px(7.0))
                    .h(px(7.0))
                    .rounded_full()
                    .bg(rgb(state_color)),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child(state_label),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .font_family("SF Mono")
                    .text_size(px(9.5))
                    .text_color(rgb(THEME.dim))
                    .child(model),
            )
            .child(
                div()
                    .id(("assistant-settings-button", element_key(pane_id)))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child("Settings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_appearance_settings(cx);
                        cx.stop_propagation();
                    })),
            )
            .into_any_element()
    }

    fn render_previous_threads(&self, pane_id: Uuid, cx: &mut Context<Self>) -> Option<AnyElement> {
        let workspace_id = self.workspace_id_for_pane(pane_id)?;
        let summaries = self
            .voice
            .thread_index
            .iter()
            .filter(|summary| summary.workspace_id == Some(workspace_id))
            .filter(|summary| self.workspace_id_for_pane(summary.thread_id).is_none())
            .take(10)
            .cloned()
            .collect::<Vec<_>>();
        if summaries.is_empty() {
            return None;
        }
        let rows = summaries.into_iter().map(|summary| {
            let thread_id = summary.thread_id;
            let title = summary
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "Untitled thread".to_owned());
            let turn_label = format!(
                "{} turn{}",
                summary.turns,
                if summary.turns == 1 { "" } else { "s" }
            );
            let activity = format_thread_activity(summary.last_at_ms);
            div()
                .id(("assistant-previous-thread", element_key(thread_id)))
                .w_full()
                .min_w(px(0.0))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .bg(rgb(THEME.surface))
                .hover(|element| element.bg(rgb(THEME.elevated)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.0))
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
                                .child(title),
                        )
                        .child(
                            div()
                                .id(("assistant-delete-thread", element_key(thread_id)))
                                .cursor_pointer()
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.danger))
                                .child("Delete")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_saved_thread(thread_id, cx);
                                    cx.stop_propagation();
                                })),
                        ),
                )
                .child(
                    div()
                        .font_family("SF Mono")
                        .text_size(px(9.0))
                        .text_color(rgb(THEME.dim))
                        .child(format!("{turn_label} · {activity}")),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.reopen_thread(workspace_id, thread_id, cx);
                    cx.stop_propagation();
                }))
                .into_any_element()
        });
        Some(
            div()
                .w_full()
                .max_w(px(440.0))
                .pt(px(10.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.muted))
                                .child("Previous threads"),
                        )
                        .child(
                            div()
                                .id(("assistant-clear-threads", element_key(pane_id)))
                                .cursor_pointer()
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.danger))
                                .child("Clear all")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.clear_saved_threads(cx);
                                    cx.stop_propagation();
                                })),
                        ),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_assistant_idle(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = "Start voice assistant";
        let button_id = "assistant-start";
        let error_line = match &session.engine_state {
            EngineState::Error(message) => Some(message.clone()),
            _ => self.voice.persistence_error.clone(),
        };
        let previous_threads = self.render_previous_threads(pane_id, cx);
        div()
            .id(("assistant-idle", element_key(pane_id)))
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .px(px(24.0))
            .py(px(24.0))
            .overflow_y_scroll()
            .bg(rgb(THEME.terminal))
            .when_some(error_line, |element, message| {
                element.child(
                    div()
                        .font_family("SF Mono")
                        .text_xs()
                        .text_color(rgb(THEME.danger))
                        .child(message),
                )
            })
            .child(
                div()
                    .id((button_id, element_key(pane_id)))
                    .px(px(16.0))
                    .py(px(9.0))
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.accent))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.window))
                    .hover(|element| element.opacity(0.9))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.start_voice_assistant(pane_id, cx);
                        cx.stop_propagation();
                    })),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Not connected — no microphone or API usage while idle."),
            )
            .when_some(previous_threads, |element, history| element.child(history))
            .into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn render_assistant_live(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active_bars = ((session.mic_level * 5.0).ceil() as usize).min(5);
        let mic_bars = format!("{}{}", "▮".repeat(active_bars), "▯".repeat(5 - active_bars));
        let transcript_len = session.transcript.len();
        let mut transcript = session
            .transcript
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let user = entry.role == VoiceTranscriptRole::User;
                let selected = session.selected_transcript == Some(index);
                let bubble_background = if user { THEME.elevated } else { THEME.surface };
                let live_assistant = index + 1 == transcript_len
                    && entry.role == VoiceTranscriptRole::Assistant
                    && !entry.final_;
                let text = if live_assistant && !session.speaker_muted {
                    let mut prefix: String = entry
                        .text
                        .chars()
                        .take(session.assistant_reveal_chars)
                        .collect();
                    prefix.push('▮');
                    prefix
                } else {
                    entry.text.clone()
                };
                let message_text = entry.text.clone();
                div()
                    .id(("voice-transcript", index))
                    .w_full()
                    .flex()
                    .when(user, |element| element.justify_end())
                    .child(
                        div()
                            .id(("voice-transcript-bubble", index))
                            .min_w(px(0.0))
                            .max_w(relative(0.85))
                            .px(px(10.0))
                            .py(px(7.0))
                            .rounded(px(7.0))
                            .cursor_text()
                            .bg(rgb(bubble_background))
                            .border_1()
                            .border_color(rgb(if selected {
                                THEME.accent
                            } else {
                                bubble_background
                            }))
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(if user { THEME.muted } else { THEME.foreground }))
                            .flex()
                            .flex_col()
                            .when_some(entry.image.clone(), |element, path| {
                                element.child(
                                    img(path)
                                        .max_w(px(220.0))
                                        .max_h(px(160.0))
                                        .object_fit(gpui::ObjectFit::Contain)
                                        .rounded(px(5.0)),
                                )
                            })
                            .children(text.split('\n').map(|line| {
                                let line = if line.is_empty() { " " } else { line };
                                div().child(line.to_owned())
                            }))
                            .child(
                                div()
                                    .w_full()
                                    .pt(px(3.0))
                                    .flex()
                                    .when(user, |element| element.justify_end())
                                    .font_family("SF Mono")
                                    .text_size(px(9.0))
                                    .text_color(rgb(THEME.dim))
                                    .child(entry.timestamp.clone())
                                    .child(
                                        div()
                                            .id(("voice-transcript-copy", index))
                                            .ml(px(6.0))
                                            .cursor_pointer()
                                            .text_color(rgb(THEME.dim))
                                            .hover(|element| {
                                                element.text_color(rgb(THEME.foreground))
                                            })
                                            .tooltip(|_, cx| {
                                                cx.new(|_| TooltipView {
                                                    text: "Copy message".to_owned(),
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    message_text.clone(),
                                                ));
                                                cx.stop_propagation();
                                            }))
                                            .child("⧉"),
                                    ),
                            )
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Click to select; Command-C copies this message"
                                        .to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(session) = this.voice.sessions.get_mut(&pane_id) {
                                    session.selected_transcript = Some(index);
                                }
                                this.focus_pane_with_snapshot(pane_id, cx);
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let has_live_user_transcript = session
            .transcript
            .last()
            .is_some_and(|entry| entry.role == VoiceTranscriptRole::User && !entry.final_);
        if session.user_speaking && !has_live_user_transcript {
            let dot_phase = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 400
                % 3;
            let dots = ["·", "· ·", "· · ·"][dot_phase as usize];
            transcript.push(
                div()
                    .id(("voice-listening", element_key(pane_id)))
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .min_w(px(0.0))
                            .max_w(relative(0.85))
                            .px(px(10.0))
                            .py(px(7.0))
                            .rounded(px(7.0))
                            .bg(rgb(THEME.elevated))
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.dim))
                            .child(format!("{mic_bars} {dots}")),
                    )
                    .into_any_element(),
            );
        }
        let ledger = session
            .ledger
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                div()
                    .id(("voice-ledger", index))
                    .w_full()
                    .min_w(px(0.0))
                    .truncate()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(format!("{} — {}", entry.name, entry.summary))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let activity = assistant_activity_row(pane_id, session);
        let approvals = session
            .approvals
            .iter()
            .map(|approval| {
                let approval_id = approval.id;
                div()
                    .id(("voice-approval", approval_id))
                    .mx(px(10.0))
                    .mb(px(8.0))
                    .p(px(10.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(rgb(THEME.danger))
                    .bg(rgb(THEME.elevated))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child(approval.description.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id(("voice-approve", approval_id))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.accent))
                                    .text_color(rgb(THEME.window))
                                    .child("Approve")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_assistant_command(
                                            pane_id,
                                            VoiceCommand::Approve { approval_id },
                                        );
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("voice-deny", approval_id))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(rgb(THEME.border_strong))
                                    .text_color(rgb(THEME.muted))
                                    .child("Deny")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_assistant_command(
                                            pane_id,
                                            VoiceCommand::Deny { approval_id },
                                        );
                                        cx.stop_propagation();
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .id(("assistant-live", element_key(pane_id)))
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .bg(rgb(THEME.terminal))
            .child(
                div()
                    .id("voice-transcript-scroll")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&session.transcript_scroll)
                    .px(px(10.0))
                    .py(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(transcript)
                    .children(ledger)
                    .when_some(activity, |element, activity| element.child(activity)),
            )
            .children(approvals)
            .child(self.render_assistant_composer_row(pane_id, session, cx))
            .into_any_element()
    }
    #[allow(clippy::too_many_lines)]
    fn render_assistant_composer_row(
        &self,
        pane_id: Uuid,
        session: &AssistantSession,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let engine_present = session
            .engine
            .as_ref()
            .is_some_and(|engine| !engine.is_finished());
        let voice_active =
            engine_present && !matches!(&session.engine_state, EngineState::Suspended);
        let attachment_info = self
            .editor
            .assistant_composer
            .as_ref()
            .filter(|composer| composer.pane_id == pane_id)
            .and_then(|composer| composer.attachment.as_ref())
            .map(|attachment| (attachment.filename.clone(), attachment.path.clone()));
        div()
            .flex_none()
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .when_some(attachment_info, |element, (filename, path)| {
                element.child(
                    div()
                        .px(px(10.0))
                        .pt(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .px(px(8.0))
                                .py(px(3.0))
                                .rounded(px(5.0))
                                .bg(rgb(THEME.elevated))
                                .border_1()
                                .border_color(rgb(THEME.border_strong))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .max_w(relative(0.7))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    img(path)
                                        .h(px(48.0))
                                        .max_w(px(160.0))
                                        .object_fit(gpui::ObjectFit::Contain)
                                        .rounded(px(4.0)),
                                )
                                .child(div().min_w(px(0.0)).truncate().child(filename)),
                        )
                        .child(
                            div()
                                .id(("composer-attachment-remove", element_key(pane_id)))
                                .cursor_pointer()
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .hover(|element| element.text_color(rgb(THEME.danger)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(composer) = this.editor.assistant_composer.as_mut()
                                    {
                                        composer.attachment = None;
                                    }
                                    cx.notify();
                                    cx.stop_propagation();
                                }))
                                .child("×"),
                        ),
                )
            })
            .child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .flex()
                    .items_end()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id(("assistant-composer-field", element_key(pane_id)))
                            .min_h(px(26.0))
                            .min_w(px(0.0))
                            .flex_1()
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(rgb(THEME.border_strong))
                            .bg(rgb(THEME.surface))
                            .cursor_pointer()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .font_family("SF Mono")
                            .text_sm()
                            .when(
                                self.editor
                                    .assistant_composer
                                    .as_ref()
                                    .is_some_and(|composer| composer.pane_id == pane_id),
                                |element| {
                                    let composer = self
                                        .editor
                                        .assistant_composer
                                        .as_ref()
                                        .filter(|composer| composer.pane_id == pane_id);
                                    let text =
                                        composer.map_or("", |composer| composer.text.as_str());
                                    let selected_all =
                                        composer.is_some_and(AssistantComposer::all_selected);
                                    let lines = text.split('\n').collect::<Vec<_>>();
                                    let start = lines.len().saturating_sub(5);
                                    let visible = &lines[start..];
                                    let mut rendered =
                                        Vec::with_capacity(visible.len() + usize::from(start > 0));
                                    if start > 0 {
                                        rendered.push(
                                            div()
                                                .text_color(rgb(THEME.dim))
                                                .child("…")
                                                .into_any_element(),
                                        );
                                    }
                                    for (index, line) in visible.iter().enumerate() {
                                        let line = if line.is_empty() { " " } else { *line };
                                        let line = line.to_owned();
                                        if index + 1 == visible.len() && !selected_all {
                                            rendered.push(
                                                div()
                                                    .flex()
                                                    .child(line)
                                                    .child(
                                                        div()
                                                            .text_color(rgb(THEME.accent))
                                                            .child("▮"),
                                                    )
                                                    .into_any_element(),
                                            );
                                        } else {
                                            rendered.push(
                                                div()
                                                    .when(selected_all, |element| {
                                                        element.bg(rgb(THEME.accent_soft))
                                                    })
                                                    .child(line)
                                                    .into_any_element(),
                                            );
                                        }
                                    }
                                    element.text_color(rgb(THEME.foreground)).children(rendered)
                                },
                            )
                            .when(
                                self.editor
                                    .assistant_composer
                                    .as_ref()
                                    .is_none_or(|composer| composer.pane_id != pane_id),
                                |element| {
                                    element
                                        .text_color(rgb(THEME.dim))
                                        .child("Type to the assistant…")
                                },
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.focus_pane_with_snapshot(pane_id, cx);
                                activate_assistant_composer(
                                    &mut this.editor.assistant_composer,
                                    pane_id,
                                );
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("assistant-composer-attach", element_key(pane_id)))
                            .h(px(26.0))
                            .w(px(26.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(rgb(THEME.border_strong))
                            .bg(rgb(THEME.surface))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Attach image".to_owned(),
                                })
                                .into()
                            })
                            .child("+")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.attach_assistant_image(pane_id, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("assistant-composer-send", element_key(pane_id)))
                            .size(px(26.0))
                            .rounded(px(5.0))
                            .bg(rgb(THEME.accent))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.window))
                            .child("↩")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit_assistant_composer(cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id(("assistant-voice-toggle", element_key(pane_id)))
                            .size(px(26.0))
                            .rounded_full()
                            .border_2()
                            .border_color(rgb(if voice_active {
                                THEME.accent
                            } else {
                                THEME.border_strong
                            }))
                            .bg(rgb(THEME.surface))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .tooltip(move |_, cx| {
                                cx.new(|_| TooltipView {
                                    text: if voice_active {
                                        "Voice on — click to pause".to_owned()
                                    } else {
                                        "Start voice assistant".to_owned()
                                    },
                                })
                                .into()
                            })
                            .child(
                                div()
                                    .text_color(rgb(if voice_active {
                                        THEME.accent
                                    } else {
                                        THEME.dim
                                    }))
                                    .child("●"),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if voice_active {
                                    this.send_assistant_command(pane_id, VoiceCommand::Suspend);
                                } else {
                                    this.start_voice_assistant(pane_id, cx);
                                }
                                cx.stop_propagation();
                            })),
                    ),
            )
            .into_any_element()
    }
}
