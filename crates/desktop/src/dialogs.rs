//! Modal dialogs: renames, creation, directories, and confirmations.
use crate::elements::WorkspaceTextInputElement;
use crate::view_models::{
    CloseConfirmation, CloseConfirmationKind, DialogAction, DialogSpec, DialogTone, DirEditor,
    DirEditorTarget, Modal, TabCloseConfirmation, TmuxSelectionChange, TmuxSessionPicker,
    UpdateRestartConfirmation, WorkspaceCreationDialog, WorkspaceCreationField,
    WorkspaceCreationKind, WorkspaceCreationStep, WorkspaceDeleteConfirmation,
    WorkspaceDisconnectConfirmation,
};
use crate::{HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, div, px,
    rgb, rgba,
};
use gpui::{ParentElement, StatefulInteractiveElement, Styled};
use hh_protocol::{TmuxScanScope, this_machine_title};
use std::fmt::Write as _;

impl HhApp {
    pub(crate) fn confirm_dialog(
        &self,
        body: AnyElement,
        spec: DialogSpec,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let DialogSpec {
            title,
            confirm_label,
            confirm_tone,
            confirm_id,
            action,
        } = spec;
        let confirm_background = match confirm_tone {
            DialogTone::Accent => THEME.accent,
            DialogTone::Danger => THEME.danger,
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(440.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(title),
                    )
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-dialog")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.surface)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.editor.modal = Modal::None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id(confirm_id)
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(confirm_background))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(move |this, _, _, cx| match action {
                                        DialogAction::RenamePane => this.submit_rename(cx),
                                        DialogAction::RenameWorkspace => {
                                            this.submit_workspace_rename(cx);
                                        }
                                        DialogAction::RenameTab => {
                                            this.submit_tab_rename(cx);
                                        }
                                        DialogAction::DeleteWorkspace => {
                                            this.confirm_workspace_delete(cx);
                                        }
                                        DialogAction::DisconnectWorkspace => {
                                            this.confirm_workspace_disconnect(cx);
                                        }
                                        DialogAction::ClosePane => this.confirm_close(cx),
                                        DialogAction::ConfirmDirEditor => {
                                            this.submit_dir_editor(cx);
                                        }
                                        DialogAction::CloseTab => this.confirm_tab_close(cx),
                                        DialogAction::DeleteBotThread => {
                                            this.confirm_bot_thread_delete(cx);
                                        }
                                        DialogAction::InstallUpdate => {
                                            this.confirm_update_restart(cx)
                                        }
                                    }))
                                    .child(confirm_label),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// One parameterized rename dialog covering terminal, tab, and
    /// workstation renames. `input_id` opts into the focused inline editor
    /// chrome; workstation renames render the plain value instead.
    pub(crate) fn render_rename_dialog(
        &self,
        input_id: Option<(&'static str, bool)>,
        value: String,
        title: &'static str,
        confirm_id: &'static str,
        action: DialogAction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut field = div();
        if let Some((_, replace_on_type)) = input_id
            && replace_on_type
        {
            field = field.bg(rgb(THEME.selection));
        }
        let base = div()
            .h(px(36.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.terminal))
            .border_1()
            .border_color(rgb(THEME.accent))
            .flex()
            .items_center()
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .child(field.child(value))
            .child("│");
        let body = match input_id {
            Some((input_id, _)) => base
                .id(input_id)
                .track_focus(&self.focus_handle)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.focus_handle.focus(window);
                        cx.stop_propagation();
                    }),
                )
                .font_family(".SystemUIFont")
                .into_any_element(),
            None => base.into_any_element(),
        };
        self.confirm_dialog(
            body,
            DialogSpec {
                title: title.to_owned(),
                confirm_label: "Rename",
                confirm_tone: DialogTone::Accent,
                confirm_id,
                action,
            },
            cx,
        )
    }

    pub(crate) fn render_workspace_creation_dialog(
        &self,
        dialog: &WorkspaceCreationDialog,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match dialog.step {
            WorkspaceCreationStep::Details => self.render_workspace_creation_details(dialog, cx),
            WorkspaceCreationStep::ConfirmSsh => {
                self.render_workspace_creation_confirm_ssh(dialog, cx)
            }
        };

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(520.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_workspace_kind_card(
        id: &'static str,
        label: &'static str,
        card_kind: WorkspaceCreationKind,
        initial_field: WorkspaceCreationField,
        selected_kind: WorkspaceCreationKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .px(px(12.0))
            .py(px(7.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if selected_kind == card_kind {
                THEME.accent
            } else {
                THEME.border_strong
            }))
            .bg(rgb(if selected_kind == card_kind {
                THEME.accent_soft
            } else {
                THEME.surface
            }))
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(dialog) = this.editor.modal.workspace_creation_mut() {
                    dialog.kind = card_kind;
                    dialog.field = initial_field;
                    dialog.error = None;
                }
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    fn render_workspace_creation_input(
        &self,
        id: &'static str,
        active_field: WorkspaceCreationField,
        field: WorkspaceCreationField,
        placeholder: &'static str,
        font_family: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self.editor.workspace_input_focus[field.index()].clone();
        div()
            .id(id)
            .track_focus(&focus)
            .h(px(36.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.terminal))
            .border_1()
            .border_color(rgb(if active_field == field {
                THEME.accent
            } else {
                THEME.border_strong
            }))
            .overflow_hidden()
            .cursor_pointer()
            .flex()
            .items_center()
            .font_family(font_family)
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_workspace_creation_field(
                        field,
                        Some(event.position),
                        event.modifiers.shift,
                        event.click_count,
                        window,
                    );
                    cx.notify();
                }),
            )
            .child(WorkspaceTextInputElement {
                input: cx.entity(),
                field,
                placeholder,
            })
            .into_any_element()
    }

    fn render_workspace_creation_details(
        &self,
        dialog: &WorkspaceCreationDialog,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = dialog.kind;
        let field = dialog.field;
        let error = dialog.error.clone();
        let bot = kind == WorkspaceCreationKind::Bot;
        let heading = match &dialog.parent {
            _ if bot => "New bot".to_owned(),
            Some(parent) => format!("New workstation in {}", parent.title),
            None => "New Workstation".to_owned(),
        };
        let choose_location = !bot && dialog.parent.is_none();
        let asks_local_root = dialog.asks_local_root();
        let name_label = if bot {
            "Bot name (optional)"
        } else {
            "Workstation name (optional)"
        };
        let name_placeholder = if bot { "Bot name" } else { "Workstation name" };
        let submit_label = match kind {
            WorkspaceCreationKind::Local => "Create workstation",
            WorkspaceCreationKind::SystemSsh => "Review connection",
            WorkspaceCreationKind::Bot => "Create bot",
        };

        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(THEME.foreground))
                    .child(heading),
            )
            .when(choose_location, |element| {
                element.child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(Self::render_workspace_kind_card(
                        "new-workspace-local",
                        this_machine_title(),
                        WorkspaceCreationKind::Local,
                        WorkspaceCreationField::Name,
                        kind,
                        cx,
                    ))
                    .child(Self::render_workspace_kind_card(
                        "new-workspace-ssh",
                        "Remote (SSH)",
                        WorkspaceCreationKind::SystemSsh,
                        WorkspaceCreationField::Destination,
                        kind,
                        cx,
                    )),
                )
            })
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(name_label),
            )
            .child(self.render_workspace_creation_input(
                "workspace-name-input",
                field,
                WorkspaceCreationField::Name,
                name_placeholder,
                ".SystemUIFont",
                cx,
            ))
            .when(asks_local_root, |element| {
                element
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Root folder (optional)"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div().flex_1().min_w(px(0.0)).child(
                                    self.render_workspace_creation_input(
                                        "workspace-root-folder-input",
                                        field,
                                        WorkspaceCreationField::WorkingDir,
                                        "New terminals open here; empty inherits",
                                        "SF Mono",
                                        cx,
                                    ),
                                ),
                            )
                            .child(
                                div()
                                    .id("choose-workspace-root-folder")
                                    .flex_none()
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(rgb(THEME.border_strong))
                                    .bg(rgb(THEME.surface))
                                    .font_family(".SystemUIFont")
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_workspace_creation_folder(cx);
                                    }))
                                    .child("Choose folder…"),
                            ),
                    )
            })
            .when(bot, |element| {
                element
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Agent"),
                    )
                    .child(self.render_bot_agent_picker(dialog.agent, cx))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Project folder (optional)"),
                    )
                    .child(self.render_workspace_creation_input(
                        "workspace-working-dir-input",
                        field,
                        WorkspaceCreationField::WorkingDir,
                        "Workers open here by default",
                        "SF Mono",
                        cx,
                    ))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Instructions (optional)"),
                    )
                    .child(self.render_workspace_creation_input(
                        "workspace-instructions-input",
                        field,
                        WorkspaceCreationField::Instructions,
                        "Instructions (optional)",
                        ".SystemUIFont",
                        cx,
                    ))
            })
            .when(kind == WorkspaceCreationKind::SystemSsh, |element| {
                element
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("SSH destination or exact ssh command"),
                    )
                    .child(self.render_workspace_creation_input(
                        "workspace-ssh-input",
                        field,
                        WorkspaceCreationField::Destination,
                        "ssh user@host-or-alias",
                        "SF Mono",
                        cx,
                    ))
            })
            .when(kind == WorkspaceCreationKind::SystemSsh, |element| {
                element.child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "The remote workstation connects immediately after confirmation and saves only its name, destination, pin/order, and offline/connected intent locally. System OpenSSH keeps authority over config, agent, keys, proxies, and known_hosts. Harness Harlot stores no credentials or SSH config contents.",
                        ),
                )
            })
            .when_some(error, |element, message| {
                element.child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.danger))
                        .child(message),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("cancel-workspace-create")
                            .px(px(12.0))
                            .py(px(7.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor.modal = Modal::None;
                                cx.notify();
                            }))
                            .child("Cancel"),
                    )
                    .child(
                        div()
                            .id("submit-workspace-create")
                            .px(px(12.0))
                            .py(px(7.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.accent))
                            .text_sm()
                            .text_color(rgb(0xffffff))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit_workspace_creation(cx);
                            }))
                            .child(submit_label),
                    ),
            )
            .into_any_element()
    }

    fn render_workspace_creation_confirm_ssh(
        &self,
        dialog: &WorkspaceCreationDialog,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let destination = dialog.destination.text.clone();
        let error = dialog.error.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(THEME.foreground))
                    .child(format!("Connect and save {destination}?")),
            )

            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child(
                        "This starts the installed OpenSSH client now and saves safe workstation metadata locally for later reconnect. Harness Harlot adds no SSH options, stores no credentials, and does not change your config, agent, forwarding, or host-key policy.",
                    ),
            )
            .when_some(error, |element, message| {
                element.child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.danger))
                        .child(message),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("back-workspace-create")
                            .px(px(12.0))
                            .py(px(7.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(dialog) = this.editor.modal.workspace_creation_mut() {
                                    dialog.step = WorkspaceCreationStep::Details;
                                    dialog.error = None;
                                }
                                cx.notify();
                            }))
                            .child("Back"),
                    )
                    .child(
                        div()
                            .id("confirm-workspace-create")
                            .px(px(12.0))
                            .py(px(7.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.accent))
                            .text_sm()
                            .text_color(rgb(0xffffff))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit_workspace_creation(cx)
                            }))
                            .child("Connect and save"),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_dir_editor_dialog(
        &self,
        editor: &DirEditor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, confirm_label) = match editor.target {
            DirEditorTarget::WorkspaceRoot(_) => ("Set root folder", "Set"),
        };
        let body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .h(px(36.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .bg(rgb(THEME.terminal))
                    .border_1()
                    .border_color(rgb(THEME.accent))
                    .flex()
                    .items_center()
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child(editor.value.clone())
                    .child("│"),
            )
            .when(!editor.suggestions.is_empty(), |element| {
                element.child(
                    div()
                        .font_family("SF Mono")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child(editor.suggestions.join("  ")),
                )
            })
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: title.to_owned(),
                confirm_label,
                confirm_tone: DialogTone::Accent,
                confirm_id: "confirm-dir-editor",
                action: DialogAction::ConfirmDirEditor,
            },
            cx,
        )
    }

    pub(crate) fn render_update_restart_dialog(
        &self,
        confirmation: &UpdateRestartConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let terminals = confirmation.live_terminals.map_or_else(
            || "Local terminals managed by HH's private tmux server will resume after the service restart. Fallback terminals may reopen as fresh shells in their last directories. SSH tabs stay offline until reconnected.".to_owned(),
            |count| format!("{count} running terminal{} will resume when managed by HH's private tmux server. Fallback terminals may reopen as fresh shells in their last directories. SSH tabs stay offline until reconnected.", if count == 1 { "" } else { "s" }),
        );
        let body = div()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.muted))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child("This update changes the terminal service protocol, so the service restarts while installing.")
            .child(terminals)
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Update to {}", confirmation.version),
                confirm_label: "Update and restart",
                confirm_tone: DialogTone::Accent,
                confirm_id: "confirm-update-restart",
                action: DialogAction::InstallUpdate,
            },
            cx,
        )
    }

    pub(crate) fn render_workspace_delete_dialog(
        &self,
        confirmation: &WorkspaceDeleteConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if confirmation.bot {
            return self.confirm_dialog(
                div()
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child(
                        "This permanently removes the bot and ends its agent and its open threads.",
                    )
                    .into_any_element(),
                DialogSpec {
                    title: format!("Delete bot {}?", confirmation.title),
                    confirm_label: "Delete bot",
                    confirm_tone: DialogTone::Danger,
                    confirm_id: "confirm-workspace-delete",
                    action: DialogAction::DeleteWorkspace,
                },
                cx,
            );
        }
        let mut message = if confirmation.active_terminal_count == 0 {
            "This removes the saved workstation metadata from this machine. No active terminal process will be ended.".to_owned()
        } else {
            format!(
                "This permanently removes the workstation and ends {} active terminal process{}. Disconnecting is the non-destructive choice for a saved SSH workstation.",
                confirmation.active_terminal_count,
                if confirmation.active_terminal_count == 1 {
                    ""
                } else {
                    "es"
                }
            )
        };
        if confirmation.nested_count > 0 {
            let _ = write!(
                message,
                " Its {} nested workstation{} will also be removed.",
                confirmation.nested_count,
                if confirmation.nested_count == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        let body = div()
            .text_sm()
            .text_color(rgb(THEME.muted))
            .child(message)
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Delete workstation {}?", confirmation.title),
                confirm_label: "Delete workstation",
                confirm_tone: DialogTone::Danger,
                confirm_id: "confirm-workspace-delete",
                action: DialogAction::DeleteWorkspace,
            },
            cx,
        )
    }

    pub(crate) fn render_tab_close_dialog(
        &self,
        confirmation: &TabCloseConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .text_sm()
            .text_color(rgb(THEME.muted))
            .child(format!(
                "This permanently removes the tab and ends {} terminal process{}.",
                confirmation.terminal_count,
                if confirmation.terminal_count == 1 {
                    ""
                } else {
                    "es"
                },
            ))
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Delete tab {}?", confirmation.title),
                confirm_label: "Delete tab",
                confirm_tone: DialogTone::Danger,
                confirm_id: "confirm-tab-close",
                action: DialogAction::CloseTab,
            },
            cx,
        )
    }

    pub(crate) fn render_tmux_session_picker(
        &self,
        picker: &TmuxSessionPicker,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scope = match &picker.scope {
            TmuxScanScope::Local => "this Mac".to_owned(),
            TmuxScanScope::SystemSsh { destination } => format!("SSH workstation {destination}"),
        };
        let selected_count = picker.selected_session_ids.len();
        let can_open = selected_count > 0;
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(460.0))
                    .max_h(px(520.0))
                    .p(px(18.0))
                    .rounded(px(9.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Open tmux sessions"),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child(format!(
                                "Scanned metadata only on {scope}. One tab is opened per selected session and attaches like `tmux attach`."
                            )),
                    )
                    .when(picker.no_server, |element| {
                        element.child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .child("No tmux server is running for this scope."),
                        )
                    })
                    .when_some(picker.status.clone(), |element, status| {
                        element.child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .child(status),
                        )
                    })
                    .when_some(picker.error.clone(), |element, error| {
                        element.child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.danger))
                                .child(error),
                        )
                    })
                    .when(!picker.sessions.is_empty(), |element| {
                        element.child(
                            div()
                                .flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    div()
                                        .font_family(".SystemUIFont")
                                        .text_xs()
                                        .text_color(rgb(THEME.muted))
                                        .child(format!("{selected_count} session(s) selected")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(10.0))
                                        .child(
                                            div()
                                                .id("select-all-tmux-sessions")
                                                .cursor_pointer()
                                                .font_family(".SystemUIFont")
                                                .text_xs()
                                                .text_color(rgb(THEME.accent))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.mutate_tmux_selection(
                                                        TmuxSelectionChange::All,
                                                        cx,
                                                    );
                                                }))
                                                .child("Select All"),
                                        )
                                        .child(
                                            div()
                                                .id("clear-all-tmux-sessions")
                                                .cursor_pointer()
                                                .font_family(".SystemUIFont")
                                                .text_xs()
                                                .text_color(rgb(THEME.accent))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.mutate_tmux_selection(
                                                        TmuxSelectionChange::None,
                                                        cx,
                                                    );
                                                }))
                                                .child("Clear All"),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("tmux-session-list")
                            .min_h(px(0.0))
                            .flex_1()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .children(picker.sessions.iter().enumerate().map(|(index, session)| {
                                let session_id = session.id.clone();
                                let is_open = picker.is_open(&session.id);
                                let is_selected =
                                    picker.selected_session_ids.contains(&session.id);
                                div()
                                    .id(("tmux-session", index))
                                    .px(px(10.0))
                                    .py(px(8.0))
                                    .rounded(px(6.0))
                                    .when(!is_open, |element| element.cursor_pointer())
                                    .border_1()
                                    .border_color(rgb(if is_selected {
                                        THEME.accent
                                    } else {
                                        THEME.border_strong
                                    }))
                                    .bg(rgb(if is_selected {
                                        THEME.accent_soft
                                    } else {
                                        THEME.surface
                                    }))
                                    .when(!is_open, |element| {
                                        element.on_click(cx.listener(move |this, _, _, cx| {
                                            this.mutate_tmux_selection(
                                                TmuxSelectionChange::Session(session_id.clone()),
                                                cx,
                                            );
                                        }))
                                    })
                                    .child(
                                        div()
                                            .font_family(".SystemUIFont")
                                            .text_sm()
                                            .text_color(rgb(if is_open {
                                                THEME.muted
                                            } else {
                                                THEME.foreground
                                            }))
                                            .child(format!(
                                                "{}{}",
                                                if is_selected { "✓ " } else { "" },
                                                session.name
                                            )),
                                    )
                                    .child(
                                        div()
                                            .mt(px(2.0))
                                            .font_family(".SystemUIFont")
                                            .text_xs()
                                            .text_color(rgb(THEME.muted))
                                            .child(if is_open {
                                                "Already open in a tab".to_owned()
                                            } else {
                                                format!(
                                                    "{} window(s) · {}",
                                                    session.windows,
                                                    if session.attached_clients == 0 {
                                                        "detached".to_owned()
                                                    } else {
                                                        format!(
                                                            "{} attached",
                                                            session.attached_clients
                                                        )
                                                    }
                                                )
                                            }),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-tmux-session-picker")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.editor.modal = Modal::None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("open-selected-tmux-sessions")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(if can_open {
                                        THEME.accent
                                    } else {
                                        THEME.border_strong
                                    }))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_selected_tmux_sessions(cx);
                                    }))
                                    .child(format!("Open {selected_count} selected session(s)")),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_workspace_disconnect_dialog(
        &self,
        confirmation: &WorkspaceDisconnectConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child(
                        "This closes the active system OpenSSH terminals of the workstation and its nested workstations. They stay saved and available for reconnect.",
                    ),
            )
            .child(
                div()
                    .p(px(8.0))
                    .rounded(px(5.0))
                    .bg(rgb(THEME.terminal))
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child(confirmation.destination.clone()),
            )
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Disconnect {}?", confirmation.title),
                confirm_label: "Disconnect",
                confirm_tone: DialogTone::Accent,
                confirm_id: "confirm-workspace-disconnect",
                action: DialogAction::DisconnectWorkspace,
            },
            cx,
        )
    }

    pub(crate) fn render_close_dialog(
        &self,
        confirmation: &CloseConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = match (confirmation.kind, confirmation.leaves_workspace_empty) {
            (CloseConfirmationKind::Browser, true) => {
                "This will close the last browser and leave the saved workstation empty. You can open a new terminal or browser from its empty state."
            }
            (CloseConfirmationKind::Browser, false) => {
                "This permanently closes this browser tab. Other tabs stay open."
            }
            (CloseConfirmationKind::Terminal, true) => {
                "This will terminate the last terminal and leave the saved workstation empty. You can open a new terminal from its empty state."
            }
            (CloseConfirmationKind::Terminal, false) => {
                "This will terminate this terminal and its running shell process. Other terminal tabs stay open."
            }
        };
        let body = div()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.muted))
            .child(message)
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Close {}?", confirmation.title),
                confirm_label: match confirmation.kind {
                    CloseConfirmationKind::Browser => "Close Browser",
                    CloseConfirmationKind::Terminal => "Close Terminal",
                },
                confirm_tone: DialogTone::Danger,
                confirm_id: "confirm-close",
                action: DialogAction::ClosePane,
            },
            cx,
        )
    }
}
