//! Pane and tab identity: profiles, custom icons, and detection.
use crate::agent_icons::import_custom_icon;
use crate::helpers::{IDENTITY_MARK_SIZE, element_key, render_terminal_profile_mark};
use crate::view_models::{Modal, TooltipView};
use crate::{HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, PathPromptOptions, div, img, px, rgb, svg,
};
use gpui::{AppContext, ParentElement, StatefulInteractiveElement, Styled, StyledImage};
use hh_protocol::{ClientRequest, Pane, TerminalProfile};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
pub(crate) enum CustomIconTarget {
    Pane(Uuid),
    Tab(Uuid),
    Workspace(Uuid),
}

pub(crate) fn detected_project_icon(project_dir: &Path) -> Option<PathBuf> {
    const ROOTS: [&str; 4] = ["", "public", "assets", "static"];
    const FILES: [&str; 8] = [
        "icon.png",
        "logo.png",
        "favicon.png",
        "apple-touch-icon.png",
        "icon.jpg",
        "logo.jpg",
        "icon.webp",
        "logo.webp",
    ];
    for root in ROOTS {
        for file in FILES {
            let candidate = project_dir.join(root).join(file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

impl HhApp {
    pub(crate) fn set_pane_profile(
        &mut self,
        pane_id: Uuid,
        profile: Option<TerminalProfile>,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(ClientRequest::SetPaneProfile { pane_id, profile });
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn set_custom_icon(
        &mut self,
        target: CustomIconTarget,
        icon: Option<String>,
        cx: &mut Context<Self>,
    ) {
        match target {
            CustomIconTarget::Pane(pane_id) => {
                self.dispatch(ClientRequest::SetPaneCustomIcon { pane_id, icon });
            }
            CustomIconTarget::Tab(tab_id) => {
                self.dispatch(ClientRequest::SetTabCustomIcon { tab_id, icon });
            }
            CustomIconTarget::Workspace(workspace_id) => {
                self.dispatch(ClientRequest::SetWorkspaceCustomIcon { workspace_id, icon });
            }
        }
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn set_pane_custom_icon(
        &mut self,
        pane_id: Uuid,
        icon: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.set_custom_icon(CustomIconTarget::Pane(pane_id), icon, cx);
    }

    pub(crate) fn set_tab_custom_icon(
        &mut self,
        tab_id: Uuid,
        icon: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.set_custom_icon(CustomIconTarget::Tab(tab_id), icon, cx);
    }

    pub(crate) fn import_custom_icon_for(
        &mut self,
        target: CustomIconTarget,
        cx: &mut Context<Self>,
    ) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose custom icon image".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match selection.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("custom icon picker failed: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let result = cx
                .background_spawn(async move { import_custom_icon(&path) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(icon) => {
                    let icon_id = icon.id.clone();
                    if !this.custom_icons.iter().any(|saved| saved.id == icon_id) {
                        this.custom_icons.push(icon);
                    }
                    this.set_custom_icon(target, Some(icon_id), cx);
                }
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn import_pane_custom_icon(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.import_custom_icon_for(CustomIconTarget::Pane(pane_id), cx);
    }

    pub(crate) fn import_tab_custom_icon(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        self.import_custom_icon_for(CustomIconTarget::Tab(tab_id), cx);
    }

    pub(crate) fn set_workspace_custom_icon(
        &mut self,
        workspace_id: Uuid,
        icon: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.set_custom_icon(CustomIconTarget::Workspace(workspace_id), icon, cx);
    }

    pub(crate) fn import_workspace_custom_icon(
        &mut self,
        workspace_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.import_custom_icon_for(CustomIconTarget::Workspace(workspace_id), cx);
    }

    pub(crate) fn detect_and_set_project_icon(
        &mut self,
        tab_id: Uuid,
        project_dir: String,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    detected_project_icon(Path::new(&project_dir))
                        .map(|path| import_custom_icon(&path))
                        .transpose()
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Some(icon)) => {
                    let icon_id = icon.id.clone();
                    if !this.custom_icons.iter().any(|saved| saved.id == icon_id) {
                        this.custom_icons.push(icon);
                    }
                    this.dispatch(ClientRequest::SetTabCustomIcon {
                        tab_id,
                        icon: Some(icon_id),
                    });
                    cx.notify();
                }
                Ok(None) => {}
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn reset_pane_identity(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch(ClientRequest::ResetPaneIdentity { pane_id });
        self.editor.modal = Modal::None;
        cx.notify();
    }

    pub(crate) fn custom_icon_path(&self, icon: &str) -> Option<PathBuf> {
        self.custom_icons
            .iter()
            .find(|saved| saved.id == icon)
            .map(|saved| saved.path.clone())
    }

    pub(crate) fn render_pane_identity_mark(
        &self,
        pane: &Pane,
        fallback_color: u32,
        frame_color: u32,
    ) -> AnyElement {
        if let Some(path) = pane
            .custom_icon
            .as_deref()
            .and_then(|icon| self.custom_icon_path(icon))
        {
            return img(path)
                .w(px(IDENTITY_MARK_SIZE))
                .h(px(IDENTITY_MARK_SIZE))
                .object_fit(gpui::ObjectFit::Contain)
                .rounded(px(3.0))
                .into_any_element();
        }
        if pane.kind.is_browser() {
            #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
            if let Some(favicon) = self
                .browser
                .browser_views
                .get(&pane.id)
                .and_then(|view| view.shared.borrow().favicon.clone())
            {
                return img(favicon)
                    .w(px(IDENTITY_MARK_SIZE))
                    .h(px(IDENTITY_MARK_SIZE))
                    .object_fit(gpui::ObjectFit::Contain)
                    .rounded(px(3.0))
                    .into_any_element();
            }
            return svg()
                .path("agent-icons/browser-globe.svg")
                .w(px(IDENTITY_MARK_SIZE))
                .h(px(IDENTITY_MARK_SIZE))
                .text_color(rgb(fallback_color))
                .into_any_element();
        }
        if pane.kind.is_assistant() {
            let active = self.voice.sessions.get(&pane.id).is_some_and(|session| {
                !crate::voice::assistant_session_is_idle(session)
                    && !matches!(session.engine_state, hh_voice::EngineState::Error(_))
            });
            return div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(rgb(if active { THEME.ansi[2] } else { THEME.dim }))
                .into_any_element();
        }

        render_terminal_profile_mark(pane.identity.profile, fallback_color, frame_color)
    }

    pub(crate) fn render_profile_choices(
        &self,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane = self.pane_metadata(pane_id);
        let is_assistant = pane.as_ref().is_some_and(|pane| pane.kind.is_assistant());
        let selected = pane.as_ref().and_then(|pane| pane.profile_override);
        let selected_custom = pane.and_then(|pane| pane.custom_icon);
        let choices = std::iter::once(None).chain(
            TerminalProfile::ALL
                .into_iter()
                .map(Some)
                .filter(move |_| !is_assistant),
        );
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(choices.enumerate().map(|(index, profile)| {
                let active = selected_custom.is_none() && selected == profile;
                let label = profile.map_or_else(
                    || "Automatic terminal icon".to_owned(),
                    |profile| profile.display_name().to_owned(),
                );
                div()
                    .id(("identity-profile", index))
                    .w(px(30.0))
                    .h(px(28.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(if active {
                        rgb(THEME.foreground)
                    } else {
                        rgb(THEME.muted)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if is_assistant {
                            this.set_pane_custom_icon(pane_id, None, cx);
                        } else {
                            this.set_pane_profile(pane_id, profile, cx);
                        }
                    }))
                    .tooltip(move |_, cx| {
                        cx.new(|_| TooltipView {
                            text: label.clone(),
                        })
                        .into()
                    })
                    .children(profile.map(|profile| {
                        render_terminal_profile_mark(
                            profile,
                            if active {
                                THEME.foreground
                            } else {
                                THEME.muted
                            },
                            if active { THEME.accent } else { THEME.muted },
                        )
                    }))
                    .when(profile.is_none(), |element| element.child("A"))
            }))
            .children(self.custom_icons.iter().enumerate().map(|(index, icon)| {
                let active = selected_custom.as_deref() == Some(icon.id.as_str());
                let icon_id = icon.id.clone();
                let path = icon.path.clone();
                div()
                    .id(("custom-identity-profile", index))
                    .w(px(30.0))
                    .h(px(28.0))
                    .p(px(3.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_pane_custom_icon(pane_id, Some(icon_id.clone()), cx)
                    }))
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Saved custom image".to_owned(),
                        })
                        .into()
                    })
                    .child(
                        img(path)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(3.0)),
                    )
            }))
            .child(
                div()
                    .id(("upload-custom-identity", element_key(pane_id)))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.import_pane_custom_icon(pane_id, cx)
                        }),
                    )
                    .child("Upload image…"),
            )
            .into_any_element()
    }

    pub(crate) fn render_group_icon_choices(
        &self,
        tab_id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| tab.custom_icon.clone())
        });
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(
                div()
                    .id(("automatic-group-icon", element_key(tab_id)))
                    .w(px(30.0))
                    .h(px(28.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected.is_none() {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.set_tab_custom_icon(tab_id, None, cx)
                        }),
                    )
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Automatic project or group icon".to_owned(),
                        })
                        .into()
                    })
                    .child("A"),
            )
            .children(self.custom_icons.iter().enumerate().map(|(index, icon)| {
                let active = selected.as_deref() == Some(icon.id.as_str());
                let icon_id = icon.id.clone();
                let path = icon.path.clone();
                div()
                    .id(("group-custom-icon", index))
                    .w(px(30.0))
                    .h(px(28.0))
                    .p(px(3.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_tab_custom_icon(tab_id, Some(icon_id.clone()), cx)
                    }))
                    .child(
                        img(path)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(3.0)),
                    )
            }))
            .child(
                div()
                    .id(("upload-group-icon", element_key(tab_id)))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.import_tab_custom_icon(tab_id, cx)),
                    )
                    .child("Upload image…"),
            )
            .into_any_element()
    }

    pub(crate) fn render_workspace_icon_choices(
        &self,
        workspace_id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .and_then(|workspace| workspace.custom_icon.clone())
        });
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(
                div()
                    .id(("automatic-workspace-icon", element_key(workspace_id)))
                    .w(px(30.0))
                    .h(px(28.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected.is_none() {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_workspace_custom_icon(workspace_id, None, cx)
                    }))
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Default marker".to_owned(),
                        })
                        .into()
                    })
                    .child("A"),
            )
            .children(self.custom_icons.iter().enumerate().map(|(index, icon)| {
                let active = selected.as_deref() == Some(icon.id.as_str());
                let icon_id = icon.id.clone();
                let path = icon.path.clone();
                div()
                    .id(("workspace-custom-icon", index))
                    .w(px(30.0))
                    .h(px(28.0))
                    .p(px(3.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_workspace_custom_icon(workspace_id, Some(icon_id.clone()), cx)
                    }))
                    .child(
                        img(path)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(3.0)),
                    )
            }))
            .child(
                div()
                    .id(("upload-workspace-icon", element_key(workspace_id)))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.import_workspace_custom_icon(workspace_id, cx)
                    }))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .child("Upload image…"),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Uuid, detected_project_icon};

    #[test]
    fn project_icon_detection_uses_documented_root_and_name_precedence() {
        let directory = std::env::temp_dir().join(format!("hh-project-icon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(directory.join("public")).unwrap();
        std::fs::write(directory.join("public/icon.png"), b"public icon").unwrap();
        std::fs::write(directory.join("logo.png"), b"root logo").unwrap();
        assert_eq!(
            detected_project_icon(&directory),
            Some(directory.join("logo.png"))
        );
        std::fs::write(directory.join("icon.png"), b"root icon").unwrap();
        assert_eq!(
            detected_project_icon(&directory),
            Some(directory.join("icon.png"))
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
