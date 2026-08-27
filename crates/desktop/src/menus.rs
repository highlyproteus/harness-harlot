//! Context menus, create menu, and the command palette.
use crate::browser::{BrowserUrlEditor, browser_command_available};
use crate::commands::{AppCommand, descriptor, palette_matches};
use crate::helpers::{element_key, find_pane};
use crate::view_models::{
    ColorTarget, CommandPaletteState, CreateMenu, CreateMenuTarget, GroupMenu, Modal, TabMenu,
    WorkspaceConnectionInfo, WorkspaceMenu,
};
use crate::{COMMAND_PALETTE_LIMIT, HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, Pixels, Point, div, px,
    relative, rgb, rgba,
};
use gpui::{ParentElement, StatefulInteractiveElement, Styled};
use hh_protocol::{ClientRequest, ServiceResponse, WorkspaceConnection, WorkspaceConnectionStatus};
use uuid::Uuid;

fn tmux_scan_available(connection: &WorkspaceConnection) -> bool {
    matches!(
        connection,
        WorkspaceConnection::Local | WorkspaceConnection::SystemSsh { .. }
    )
}

impl HhApp {
    pub(crate) fn open_tab_menu(
        &mut self,
        pane_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::ActivateTab { pane_id },
            Box::new(move |this, cx, result| {
                if let Err(error) = result {
                    this.report(&error);
                }
                this.focus_pane_with_snapshot(pane_id, cx);
            }),
        );
        self.editor.modal = Modal::TabMenu(TabMenu {
            pane_id,
            position,
            identity_picker_open: false,
        });
        cx.notify();
    }

    pub(crate) fn open_workspace_menu(
        &mut self,
        workspace_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor.modal = Modal::WorkspaceMenu(WorkspaceMenu {
            workspace_id,
            position,
        });
        self.layout.last_sizes.clear();
        cx.notify();
    }

    pub(crate) fn open_group_menu(
        &mut self,
        tab_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor.modal = Modal::GroupMenu(GroupMenu {
            tab_id,
            position,
            icon_picker_open: false,
        });
        cx.notify();
    }

    pub(crate) fn new_group_browser(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let Some(target_pane) = self.group_metadata(tab_id).map(|(_, pane_id)| pane_id) else {
            self.editor.modal = Modal::None;
            cx.notify();
            return;
        };
        self.dispatch_with(
            ClientRequest::CreateGroupBrowser {
                target_pane,
                url: None,
            },
            Box::new(|this, cx, result| match result {
                Ok(ServiceResponse::PaneCreated { pane_id }) => {
                    this.focus_pane_with_snapshot(pane_id, cx);
                    this.editor.browser_url_editor = Some(BrowserUrlEditor {
                        pane_id,
                        text: String::new(),
                        replace_on_type: true,
                        invalid: false,
                    });
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
        self.layout.last_sizes.clear();
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn toggle_tab_identity_picker(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        if let Modal::TabMenu(menu) = &mut self.editor.modal
            && menu.pane_id == pane_id
        {
            menu.identity_picker_open = !menu.identity_picker_open;
            self.editor.color_picker = None;
            cx.notify();
        }
    }

    pub(crate) fn toggle_group_icon_picker(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        if let Modal::GroupMenu(menu) = &mut self.editor.modal
            && menu.tab_id == tab_id
        {
            menu.icon_picker_open = !menu.icon_picker_open;
            self.editor.color_picker = None;
            cx.notify();
        }
    }

    pub(crate) fn render_tab_menu(&self, menu: TabMenu, cx: &mut Context<Self>) -> AnyElement {
        let pane_id = menu.pane_id;
        let inline_color_picker = self
            .editor
            .color_picker
            .as_ref()
            .filter(|picker| picker.target == ColorTarget::Pane(pane_id));
        let pane = self.pane_metadata(pane_id);
        let is_browser = pane.as_ref().is_some_and(|pane| pane.kind.is_browser());
        let pinnable_tab = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                .filter(|tab| tab.parent_tab.is_none())
                .map(|tab| (tab.id, tab.pinned))
        });
        let workspace_id = self.workspace_id_for_pane(pane_id);
        div()
            .id(("terminal-context-menu", element_key(pane_id)))
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(232.0))
            .h_auto()
            .max_h(relative(0.72))
            .overflow_y_scroll()
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .when_some(
                workspace_id.filter(|_| browser_command_available()),
                |element, workspace_id| {
                    element.child(self.create_menu_item(
                        ("new-browser-from-tab-menu", element_key(pane_id)),
                        "New Browser",
                        cx,
                        move |this, cx| this.new_browser_tab_in(workspace_id, cx),
                    ))
                },
            )
            .when_some(pinnable_tab, |element, (tab_id, pinned)| {
                element.child(
                    div()
                        .id(("toggle-tab-pin", element_key(tab_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_tab_pinned(tab_id, !pinned, cx)
                        }))
                        .child(if pinned { "Unpin" } else { "Pin to top" }),
                )
            })
            .child(self.create_menu_item(
                ("rename-menu", element_key(pane_id)),
                "Rename…",
                cx,
                move |this, cx| this.begin_rename(pane_id, cx),
            ))
            .child(
                div()
                    .mt(px(5.0))
                    .mx(px(8.0))
                    .pt(px(7.0))
                    .border_t_1()
                    .border_color(rgb(THEME.border))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Terminal identity"),
            )
            .child(
                div()
                    .id(("select-terminal-icon", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_tab_identity_picker(pane_id, cx)
                    }))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child("Select icon")
                    .child(div().flex_1())
                    .children(pane.as_ref().map(|pane| {
                        self.render_pane_identity_mark(pane, THEME.foreground, THEME.accent)
                    }))
                    .child(if menu.identity_picker_open {
                        "⌄"
                    } else {
                        "›"
                    }),
            )
            .when(menu.identity_picker_open, |element| {
                element.child(self.render_profile_choices(pane_id, cx))
            })
            .child(
                div()
                    .id(("reset-identity-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.reset_pane_identity(pane_id, cx)),
                    )
                    .child("Reset"),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .mx(px(8.0))
                    .pt(px(7.0))
                    .border_t_1()
                    .border_color(rgb(THEME.border))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Terminal color"),
            )
            .child(
                div()
                    .id(("pick-terminal-color", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_color_picker(ColorTarget::Pane(pane_id), cx)
                    }))
                    .flex()
                    .items_center()
                    .child(div().flex_1().child("Pick color…"))
                    .child(if inline_color_picker.is_some() {
                        "⌄"
                    } else {
                        "›"
                    }),
            )
            .when_some(inline_color_picker, |element, picker| {
                element.child(self.render_inline_color_picker(picker, "inline-terminal-color", cx))
            })
            .child(
                div()
                    .id(("close-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_close(pane_id, cx)))
                    .child(if is_browser {
                        "Close Browser…"
                    } else {
                        "Close Terminal…"
                    }),
            )
            .into_any_element()
    }

    pub(crate) fn create_menu_item(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let label: gpui::SharedString = label.into();
        div()
            .id(id)
            .mx(px(5.0))
            .px(px(9.0))
            .py(px(7.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .hover(|element| element.bg(rgb(THEME.accent_soft)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.editor.modal = Modal::None;
                handler(this, cx);
            }))
            .child(label)
            .into_any_element()
    }

    pub(crate) fn render_create_menu(
        &self,
        menu: CreateMenu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let left = match menu.target {
            CreateMenuTarget::Global => menu.position.x,
            CreateMenuTarget::TabStrip { .. } => (menu.position.x - px(232.0)).max(px(0.0)),
        };
        let items = match menu.target {
            CreateMenuTarget::Global => vec![
                self.create_menu_item(
                    "create-new-workstation",
                    "New Workstation",
                    cx,
                    |this, cx| {
                        this.new_workspace(cx);
                    },
                ),
                self.create_menu_item("create-new-tab", "New Tab", cx, |this, cx| {
                    if let Some(workspace_id) = this.sidebar.active_workspace {
                        this.new_workspace_tab(workspace_id, cx);
                    } else {
                        cx.notify();
                    }
                }),
                self.create_menu_item("create-new-browser", "New Browser", cx, |this, cx| {
                    this.new_browser_tab(cx);
                }),
            ],
            CreateMenuTarget::TabStrip {
                workspace_id,
                target_tab,
            } => vec![
                self.create_menu_item("strip-add-project", "Add Project", cx, move |this, cx| {
                    this.begin_project_creation(workspace_id, cx);
                }),
                self.create_menu_item("strip-add-terminal", "Add Terminal", cx, move |this, cx| {
                    this.add_terminal_to_context(workspace_id, target_tab, cx);
                }),
                self.create_menu_item("strip-add-browser", "Add Browser", cx, move |this, cx| {
                    this.add_browser_to_context(workspace_id, target_tab, cx);
                }),
                self.create_menu_item("strip-add-group", "Add Group", cx, move |this, cx| {
                    this.add_group_to_context(workspace_id, target_tab, cx);
                }),
            ],
        };
        div()
            .absolute()
            .left(left)
            .top(menu.position.y)
            .w(px(232.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .children(items)
            .into_any_element()
    }

    pub(crate) fn render_group_menu(&self, menu: GroupMenu, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = menu.tab_id;
        let metadata = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot.workspaces.iter().find_map(|workspace| {
                workspace.tabs.iter().find_map(|tab| {
                    (tab.id == tab_id).then_some((
                        workspace.id,
                        tab.project_dir.is_some(),
                        tab.parent_tab.is_some(),
                        tab.pinned,
                    ))
                })
            })
        });
        let workspace_id = metadata.map(|metadata| metadata.0);
        let is_project = metadata.is_some_and(|metadata| metadata.1);
        let has_parent = metadata.is_some_and(|metadata| metadata.2);
        let pinned = metadata.is_some_and(|metadata| metadata.3);
        let inline_color_picker = self
            .editor
            .color_picker
            .as_ref()
            .filter(|picker| picker.target == ColorTarget::Tab(tab_id));
        div()
            .id(("group-context-menu", element_key(tab_id)))
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(252.0))
            .h_auto()
            .max_h(relative(0.72))
            .overflow_y_scroll()
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .child(self.create_menu_item(
                ("new-group-terminal", element_key(tab_id)),
                "New terminal in group",
                cx,
                move |this, cx| this.new_group_terminal(tab_id, cx),
            ))
            .when(!has_parent, |element| {
                element.child(
                    div()
                        .id(("toggle-tab-pin", element_key(tab_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_tab_pinned(tab_id, !pinned, cx)
                        }))
                        .child(if pinned { "Unpin" } else { "Pin to top" }),
                )
            })
            .when(browser_command_available(), |element| {
                element.child(self.create_menu_item(
                    ("new-browser-in-group", element_key(tab_id)),
                    "New Browser",
                    cx,
                    move |this, cx| this.new_group_browser(tab_id, cx),
                ))
            })
            .when_some(
                workspace_id.filter(|_| is_project && !has_parent),
                |element, workspace_id| {
                    element.child(self.create_menu_item(
                        ("new-project-group", element_key(tab_id)),
                        "New Group",
                        cx,
                        move |this, cx| this.new_project_group(workspace_id, tab_id, cx),
                    ))
                },
            )
            .when(is_project, |element| {
                element.child(self.create_menu_item(
                    ("change-project-dir-menu", element_key(tab_id)),
                    "Change Directory…",
                    cx,
                    move |this, cx| this.begin_project_dir_edit(tab_id, cx),
                ))
            })
            .child(self.create_menu_item(
                ("rename-group-menu", element_key(tab_id)),
                "Rename group…",
                cx,
                move |this, cx| this.begin_group_rename(tab_id, cx),
            ))
            .child(
                div()
                    .id(("select-group-icon", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_group_icon_picker(tab_id, cx)
                        }),
                    )
                    .flex()
                    .items_center()
                    .child("Select icon…")
                    .child(div().flex_1())
                    .child(if menu.icon_picker_open { "⌄" } else { "›" }),
            )
            .when(menu.icon_picker_open, |element| {
                element.child(self.render_group_icon_choices(tab_id, cx))
            })
            .child(
                div()
                    .id(("pick-group-color", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_color_picker(ColorTarget::Tab(tab_id), cx)
                    }))
                    .flex()
                    .items_center()
                    .child(div().flex_1().child("Pick color…"))
                    .child(if inline_color_picker.is_some() {
                        "⌄"
                    } else {
                        "›"
                    }),
            )
            .when_some(inline_color_picker, |element, picker| {
                element.child(self.render_inline_color_picker(picker, "inline-group-color", cx))
            })
            .child(
                div()
                    .id(("delete-group-menu", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_tab_close(tab_id, cx)))
                    .child(if is_project {
                        "Delete project…"
                    } else {
                        "Delete group…"
                    }),
            )
            .into_any_element()
    }

    pub(crate) fn render_workspace_menu(
        &self,
        menu: WorkspaceMenu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = menu.workspace_id;
        let workspace = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        let pinned = workspace.is_some_and(|workspace| workspace.pinned);
        let connection = workspace.map(|workspace| workspace.connection.clone());
        let has_working_dir = workspace.is_some_and(|workspace| workspace.working_dir.is_some());
        let tmux_scan_available = connection.as_ref().is_some_and(tmux_scan_available);
        let inline_color_picker = self
            .editor
            .color_picker
            .as_ref()
            .filter(|picker| picker.target == ColorTarget::Workspace(workspace_id));
        div()
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(232.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .child(self.create_menu_item(
                ("new-workspace-tab-menu", element_key(workspace_id)),
                "New Tab",
                cx,
                move |this, cx| this.new_workspace_tab(workspace_id, cx),
            ))
            .when(browser_command_available(), |element| {
                element.child(self.create_menu_item(
                    ("new-browser-tab-menu", element_key(workspace_id)),
                    "New Browser Tab",
                    cx,
                    move |this, cx| this.new_browser_tab_in(workspace_id, cx),
                ))
            })
            .child(self.create_menu_item(
                ("new-workspace-group-menu", element_key(workspace_id)),
                "New Group",
                cx,
                move |this, cx| this.new_workspace_group(workspace_id, cx),
            ))
            .child(self.create_menu_item(
                ("new-project-menu", element_key(workspace_id)),
                "New Project…",
                cx,
                move |this, cx| this.begin_project_creation(workspace_id, cx),
            ))
            .child(self.create_menu_item(
                ("set-workdir-menu", element_key(workspace_id)),
                "Set Working Directory…",
                cx,
                move |this, cx| this.begin_workspace_dir_edit(workspace_id, cx),
            ))
            .when(has_working_dir, |element| {
                element.child(self.create_menu_item(
                    ("clear-workdir-menu", element_key(workspace_id)),
                    "Use Default Directory",
                    cx,
                    move |this, cx| {
                        this.dispatch(ClientRequest::SetWorkspaceWorkingDir {
                            workspace_id,
                            working_dir: None,
                        });
                        cx.notify();
                    },
                ))
            })
            .child(self.create_menu_item(
                ("rename-workspace-menu", element_key(workspace_id)),
                "Rename workstation…",
                cx,
                move |this, cx| this.begin_workspace_rename(workspace_id, cx),
            ))
            .child(
                div()
                    .id(("pin-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_workspace_pinned(workspace_id, !pinned, cx)
                    }))
                    .child(if pinned {
                        "Unpin workstation"
                    } else {
                        "Pin workstation"
                    }),
            )
            .when_some(connection, |element, connection| match connection {
                WorkspaceConnection::Local
                | WorkspaceConnection::SystemSsh {
                    status: WorkspaceConnectionStatus::Connected,
                    ..
                } => element,
                WorkspaceConnection::SystemSsh {
                    status: WorkspaceConnectionStatus::Offline,
                    ..
                } => element.child(
                    div()
                        .id(("reconnect-workspace-menu", element_key(workspace_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.ansi[2]))
                        .hover(|item| item.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reconnect_workspace(workspace_id, cx)
                        }))
                        .child("Reconnect"),
                ),
            })
            .when(tmux_scan_available, |element| {
                element.child(self.create_menu_item(
                    ("scan-tmux-sessions-menu", element_key(workspace_id)),
                    "Scan tmux sessions…",
                    cx,
                    move |this, cx| this.scan_tmux_sessions(workspace_id, cx),
                ))
            })
            .child(
                div()
                    .id(("workspace-pick-color", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_color_picker(ColorTarget::Workspace(workspace_id), cx)
                    }))
                    .flex()
                    .items_center()
                    .child(div().flex_1().child("Pick color…"))
                    .child(if inline_color_picker.is_some() {
                        "⌄"
                    } else {
                        "›"
                    }),
            )
            .when_some(inline_color_picker, |element, picker| {
                element.child(self.render_inline_color_picker(
                    picker,
                    "inline-workstation-color",
                    cx,
                ))
            })
            .child(
                div()
                    .id(("delete-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .mt(px(4.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_delete(workspace_id, cx)
                    }))
                    .child("Delete workstation…"),
            )
            .into_any_element()
    }

    pub(crate) fn render_workspace_connection_info(
        &self,
        info: &WorkspaceConnectionInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let connection = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == info.workspace_id)
                .and_then(|workspace| match &workspace.connection {
                    WorkspaceConnection::SystemSsh {
                        destination,
                        status: WorkspaceConnectionStatus::Connected,
                    } => Some((workspace.title.clone(), destination.clone())),
                    WorkspaceConnection::Local
                    | WorkspaceConnection::SystemSsh {
                        status: WorkspaceConnectionStatus::Offline,
                        ..
                    } => None,
                })
        });
        let Some((title, destination)) = connection else {
            return div().into_any_element();
        };
        let workspace_id = info.workspace_id;
        div()
            .absolute()
            .left(info.position.x)
            .top(info.position.y)
            .w(px(260.0))
            .p(px(12.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child(title),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child("Connected with system OpenSSH"),
            )
            .child(
                div()
                    .p(px(8.0))
                    .rounded(px(5.0))
                    .bg(rgb(THEME.terminal))
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child(destination),
            )
            .child(
                div()
                    .id(("disconnect-workspace-from-info", element_key(workspace_id)))
                    .px(px(9.0))
                    .py(px(6.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.surface))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.border_color(rgb(THEME.accent)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_disconnect(workspace_id, cx)
                    }))
                    .child("Disconnect…"),
            )
            .into_any_element()
    }

    pub(crate) fn render_command_palette(
        &self,
        palette: &CommandPaletteState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let matches = palette_matches(&palette.query, COMMAND_PALETTE_LIMIT)
            .into_iter()
            .filter(|item| item.command != AppCommand::NewBrowserTab || browser_command_available())
            .collect::<Vec<_>>();
        let query = if palette.query.is_empty() {
            "Type a command…".to_owned()
        } else {
            palette.query.clone()
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x00000070))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(110.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.editor.modal = Modal::None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("command-palette")
                    .w(px(620.0))
                    .h_auto()
                    .max_h(relative(0.75))
                    .overflow_y_scroll()
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.elevated))
                    .shadow_lg()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        div()
                            .h(px(48.0))
                            .px(px(15.0))
                            .border_b_1()
                            .border_color(rgb(THEME.border))
                            .flex()
                            .items_center()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(if palette.query.is_empty() {
                                rgb(THEME.dim)
                            } else {
                                rgb(THEME.foreground)
                            })
                            .child(query),
                    )
                    .children(matches.into_iter().enumerate().map(|(index, item)| {
                        let command = item.command;
                        let metadata = descriptor(command);
                        let selected = index == palette.selected;
                        div()
                            .id(("palette-command", index))
                            .h(px(44.0))
                            .px(px(13.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .when(selected, |element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.execute_command(command, cx);
                                cx.stop_propagation();
                            }))
                            .child(
                                div()
                                    .w(px(210.0))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.dim))
                                    .child(format!("{} · {}", metadata.category, metadata.id)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .font_family(".SystemUIFont")
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .child(metadata.title),
                            )
                            .child(
                                div()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(rgb(THEME.muted))
                                    .child(self.binding_label(command)),
                            )
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::tmux_scan_available;
    use hh_protocol::{WorkspaceConnection, WorkspaceConnectionStatus};

    #[test]
    fn saved_ssh_workstation_can_scan_tmux_while_offline() {
        assert!(tmux_scan_available(&WorkspaceConnection::SystemSsh {
            destination: "developer@build-node".to_owned(),
            status: WorkspaceConnectionStatus::Offline,
        }));
    }
}
