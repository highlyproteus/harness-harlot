//! Appearance settings, color pickers, and workstation banner art.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, ClipboardItem, Context, Image, ImageFormat, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, StatefulInteractiveElement, Styled, StyledImage,
    div, img, px, rgb, rgba,
};
use hh_protocol::{AppearanceColor, AssistantAccess, ClientRequest, validate_workspace_dir};
use std::sync::{Arc, LazyLock};

use crate::elements::{HsvFieldElement, HsvFieldKind};
use crate::helpers::{
    banner_fit_size, hsv_to_rgb, parse_hex_color, render_terminal_profile_icon, rgb_to_hsv,
};
use crate::view_models::{ColorPickerState, ColorTarget, Modal, SettingsSection, TooltipView};
use crate::voice::{VOICE_PRIVACY_URL, VoiceSettingsField};
use crate::{
    APPEARANCE_PRESETS, BUNDLED_BANNER_PIXEL_HEIGHT, BUNDLED_BANNER_PIXEL_WIDTH, HhApp,
    PANE_HEADER_HEIGHT, THEME,
};

/// Section title with its one-line explanation.
fn settings_heading(title: &'static str, description: &'static str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .font_family(".SystemUIFont")
                .text_lg()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(THEME.foreground))
                .child(title),
        )
        .child(
            div()
                .font_family(".SystemUIFont")
                .text_sm()
                .text_color(rgb(THEME.muted))
                .child(description),
        )
        .into_any_element()
}

/// Label above a card inside one section panel.
fn settings_section_title(title: &'static str) -> AnyElement {
    div()
        .font_family(".SystemUIFont")
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(THEME.muted))
        .child(title)
        .into_any_element()
}

fn settings_card(children: Vec<AnyElement>) -> AnyElement {
    div()
        .p(px(14.0))
        .rounded(px(9.0))
        .bg(rgb(THEME.surface))
        .border_1()
        .border_color(rgb(THEME.border))
        .flex()
        .flex_col()
        .gap(px(12.0))
        .children(children)
        .into_any_element()
}

fn settings_row(title: &'static str, detail: Option<String>, trailing: AnyElement) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(12.0))
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .child(title),
                )
                .when_some(detail, |element, detail| {
                    element.child(
                        div()
                            .truncate()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child(detail),
                    )
                }),
        )
        .child(trailing)
        .into_any_element()
}

fn radio_glyph(selected: bool) -> AnyElement {
    div()
        .font_family("SF Mono")
        .text_sm()
        .text_color(rgb(if selected { THEME.accent } else { THEME.dim }))
        .child(if selected { "●" } else { "○" })
        .into_any_element()
}

/// Budget the settings preview fits inside, borders excluded.
pub(crate) const SETTINGS_BANNER_PREVIEW_MAX_WIDTH: f32 = 420.0;

pub(crate) const SETTINGS_BANNER_PREVIEW_MAX_HEIGHT: f32 = 220.0;

pub(crate) fn color_picker_hosted(target: ColorTarget, modal: &Modal) -> bool {
    match target {
        ColorTarget::Pane(id) => matches!(modal, Modal::TabMenu(menu) if menu.pane_id == id),
        ColorTarget::Workspace(id) => {
            matches!(modal, Modal::WorkspaceMenu(menu) if menu.workspace_id == id)
        }
        ColorTarget::Tab(id) => matches!(modal, Modal::GroupMenu(menu) if menu.tab_id == id),
        ColorTarget::DefaultTerminal | ColorTarget::DefaultWorkspace => true,
    }
}

/// A banner ready to render: decoded-image handle plus its pixel dimensions,
/// which drive rail-header height and preview sizing.
#[derive(Clone, Debug)]
pub(crate) struct BannerArtwork {
    pub(crate) image: Arc<Image>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl BannerArtwork {
    // Banner dimensions are capped at 8,192 px, so both integer values are
    // exactly representable as f32.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn aspect_ratio(&self) -> f32 {
        self.width as f32 / self.height.max(1) as f32
    }
}

/// Keep the banner available to the native renderer even while a Dev bundle is
/// rebuilt in place. The same user-owned artwork remains packaged as a bundle
/// resource; this stable in-process source prevents an asynchronous file-load
/// miss from leaving the rail header blank after a relaunch.
pub(crate) fn workstation_banner_artwork() -> BannerArtwork {
    static BANNER: LazyLock<BannerArtwork> = LazyLock::new(|| BannerArtwork {
        image: Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../assets/harnessharlot-banner.png").to_vec(),
        )),
        width: BUNDLED_BANNER_PIXEL_WIDTH,
        height: BUNDLED_BANNER_PIXEL_HEIGHT,
    });
    BANNER.clone()
}

impl HhApp {
    /// Drops a picker whose menu is gone so it can never swallow terminal input.
    pub(crate) fn active_color_picker_mut(&mut self) -> Option<&mut ColorPickerState> {
        if self
            .editor
            .color_picker
            .as_ref()
            .is_some_and(|picker| !color_picker_hosted(picker.target, &self.editor.modal))
        {
            self.editor.color_picker = None;
        }
        self.editor.color_picker.as_mut()
    }

    pub(crate) fn appearance_choices(&self) -> Vec<AppearanceColor> {
        let mut colors = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.appearance.recent_colors.clone())
            .unwrap_or_default();
        for preset in APPEARANCE_PRESETS {
            if !colors.contains(&preset) {
                colors.push(preset);
            }
        }
        colors.truncate(12);
        colors
    }

    pub(crate) fn color_for_target(&self, target: ColorTarget) -> AppearanceColor {
        match target {
            ColorTarget::DefaultTerminal => self
                .session
                .snapshot
                .as_ref()
                .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                    snapshot.appearance.default_terminal_accent
                }),
            ColorTarget::DefaultWorkspace => self
                .session
                .snapshot
                .as_ref()
                .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                    snapshot.appearance.default_workspace_color
                }),
            ColorTarget::Pane(pane_id) => self.terminal_accent(pane_id),
            ColorTarget::Workspace(workspace_id) => self.workspace_color(workspace_id),
            ColorTarget::Tab(tab_id) => self
                .session
                .snapshot
                .as_ref()
                .and_then(|snapshot| {
                    snapshot
                        .workspaces
                        .iter()
                        .flat_map(|workspace| workspace.tabs.iter())
                        .find(|tab| tab.id == tab_id)
                        .and_then(|tab| tab.color)
                        .or(Some(snapshot.appearance.default_workspace_color))
                })
                .unwrap_or(AppearanceColor::DARK_GRAY),
        }
    }

    pub(crate) fn apply_color(
        &mut self,
        target: ColorTarget,
        color: Option<AppearanceColor>,
        cx: &mut Context<Self>,
    ) {
        let request = match (target, color) {
            (ColorTarget::DefaultTerminal, Some(color)) => {
                ClientRequest::SetDefaultTerminalAccent { color }
            }
            (ColorTarget::DefaultWorkspace, Some(color)) => {
                ClientRequest::SetDefaultWorkspaceColor { color }
            }
            (ColorTarget::Pane(pane_id), color) => ClientRequest::SetPaneColor { pane_id, color },
            (ColorTarget::Workspace(workspace_id), color) => ClientRequest::SetWorkspaceColor {
                workspace_id,
                color,
            },
            (ColorTarget::Tab(tab_id), color) => ClientRequest::SetTabColor { tab_id, color },
            (ColorTarget::DefaultTerminal | ColorTarget::DefaultWorkspace, None) => return,
        };
        self.dispatch(request);
        if matches!(
            self.editor.modal,
            Modal::TabMenu(_) | Modal::WorkspaceMenu(_) | Modal::GroupMenu(_)
        ) {
            self.editor.modal = Modal::None;
        }
        self.editor.color_picker = None;
        cx.notify();
    }

    pub(crate) fn open_color_picker(&mut self, target: ColorTarget, cx: &mut Context<Self>) {
        let current = self.color_for_target(target).as_rgb();
        let (hue, saturation, value) = rgb_to_hsv(current);
        self.editor.color_picker = Some(ColorPickerState {
            target,
            hex: format!("{current:06X}"),
            hue,
            saturation,
            value,
            replace_on_type: true,
            invalid: false,
        });
        if !matches!(
            target,
            ColorTarget::Pane(_) | ColorTarget::Workspace(_) | ColorTarget::Tab(_)
        ) {
            self.editor.modal = Modal::None;
        }
        cx.notify();
    }

    pub(crate) fn toggle_color_picker(&mut self, target: ColorTarget, cx: &mut Context<Self>) {
        if self
            .editor
            .color_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target)
        {
            self.editor.color_picker = None;
        } else {
            self.open_color_picker(target, cx);
        }
        if let Modal::TabMenu(menu) = &mut self.editor.modal {
            menu.identity_picker_open = false;
        }
        if let Modal::GroupMenu(menu) = &mut self.editor.modal {
            menu.icon_picker_open = false;
        }
        if let Modal::WorkspaceMenu(menu) = &mut self.editor.modal {
            menu.icon_picker_open = false;
        }
        cx.notify();
    }

    pub(super) fn sync_picker_hsv(picker: &mut ColorPickerState) {
        if let Some(color) = parse_hex_color(&format!("#{}", picker.hex)) {
            (picker.hue, picker.saturation, picker.value) = rgb_to_hsv(color.as_rgb());
        }
    }

    pub(crate) fn submit_color_picker(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.editor.color_picker.as_ref() else {
            return;
        };
        let target = picker.target;
        let color = parse_hex_color(&picker.hex);
        if let Some(color) = color {
            self.apply_color(target, Some(color), cx);
        } else if let Some(picker) = self.editor.color_picker.as_mut() {
            picker.invalid = true;
            cx.notify();
        }
    }

    pub(crate) fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.editor.modal = Modal::AppearanceSettings;
        self.editor.settings_section = section;
        self.editor.color_picker = None;
        self.editor.history_editor = None;
        self.editor.history_clear_confirmation = None;
        match crate::voice::VoiceSettingsEditor::load() {
            Ok(editor) => self.assistant.settings_editor = editor,
            Err(error) => self.report(&error),
        }
        self.refresh_history_status();
        if section == SettingsSection::Assistant && !self.assistant.coding_agents.loaded {
            self.refresh_coding_agents(cx);
        }
        cx.notify();
    }

    pub(crate) fn select_settings_section(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        self.editor.settings_section = section;
        self.assistant.settings_editor.active_field = None;
        if section == SettingsSection::Assistant && !self.assistant.coding_agents.loaded {
            self.refresh_coding_agents(cx);
        }
        cx.notify();
    }

    pub(crate) fn choose_workstation_banner(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            self.report(&anyhow::anyhow!(
                "application state is unavailable; cannot save a custom banner"
            ));
            cx.notify();
            return;
        };
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose workstation banner".into()),
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
                    let error = anyhow::anyhow!("workstation banner picker failed: {error}");
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
                .background_spawn(async move { store.import_workstation_banner(&path) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(stored) => {
                    this.sidebar.workstation_banner = Some(BannerArtwork {
                        image: Arc::new(Image::from_bytes(ImageFormat::Png, stored.png)),
                        width: stored.width,
                        height: stored.height,
                    });
                    cx.notify();
                }
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn prompt_local_directory(
        &mut self,
        prompt: &'static str,
        on_pick: impl Fn(&mut Self, String, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match selection.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("directory picker failed: {error}");
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
            let dir = path.to_string_lossy().into_owned();
            if let Err(message) = validate_workspace_dir(&dir) {
                let error = anyhow::Error::from(message);
                let _ = this.update(cx, |this, cx| {
                    this.report(&error);
                    cx.notify();
                });
                return;
            }
            let _ = this.update(cx, move |this, cx| on_pick(this, dir, cx));
        })
        .detach();
    }

    pub(crate) fn reset_workstation_banner(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            self.report(&anyhow::anyhow!(
                "application state is unavailable; cannot reset the custom banner"
            ));
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { store.reset_workstation_banner() })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.sidebar.workstation_banner = None;
                    cx.notify();
                }
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn toggle_workstation_banner_visibility(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            self.report(&anyhow::anyhow!(
                "application state is unavailable; cannot save the banner visibility"
            ));
            cx.notify();
            return;
        };
        let hidden = !self.sidebar.workstation_banner_hidden;
        self.sidebar.workstation_banner_hidden = hidden;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { store.save_workstation_banner_hidden(hidden) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.sidebar.workstation_banner_hidden = !hidden;
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn render_color_choices(
        &self,
        target: ColorTarget,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children({
                let applied = self.color_for_target(target);
                self.appearance_choices()
                    .into_iter()
                    .enumerate()
                    .map(move |(index, color)| {
                        let rgb_value = color.as_rgb();
                        let selected = applied == color;
                        div()
                            .id((id_prefix, index))
                            .w(px(20.0))
                            .h(px(20.0))
                            .rounded(px(if selected { 7.0 } else { 5.0 }))
                            .cursor_pointer()
                            .bg(rgb(rgb_value))
                            .when(selected, |element| {
                                element.border_2().border_color(rgb(THEME.foreground))
                            })
                            .when(!selected, |element| {
                                element.border_1().border_color(rgb(THEME.border_strong))
                            })
                            .hover(|element| element.border_color(rgb(THEME.foreground)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| TooltipView {
                                    text: format!("#{rgb_value:06X}"),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_color(target, Some(color), cx)
                            }))
                    })
            })
            .into_any_element()
    }

    fn render_color_picker_body(
        &self,
        picker: &ColorPickerState,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(
                div()
                    .h(px(120.0))
                    .w_full()
                    .rounded(px(4.0))
                    .overflow_hidden()
                    .child(HsvFieldElement {
                        input: cx.entity(),
                        kind: HsvFieldKind::SquareSv,
                    }),
            )
            .child(
                div()
                    .h(px(14.0))
                    .w_full()
                    .rounded(px(4.0))
                    .overflow_hidden()
                    .child(HsvFieldElement {
                        input: cx.entity(),
                        kind: HsvFieldKind::HueStrip,
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .w(px(22.0))
                            .h(px(22.0))
                            .rounded(px(4.0))
                            .bg(rgb(hsv_to_rgb(picker.hue, picker.saturation, picker.value))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(32.0))
                            .px(px(8.0))
                            .rounded(px(5.0))
                            .bg(rgb(THEME.terminal))
                            .border_1()
                            .border_color(if picker.invalid {
                                rgb(THEME.danger)
                            } else {
                                rgb(THEME.border_strong)
                            })
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .child("#")
                            .child(
                                div()
                                    .when(picker.replace_on_type, |element| {
                                        element.bg(rgb(THEME.selection))
                                    })
                                    .child(picker.hex.clone()),
                            )
                            .when(picker.invalid, |element| {
                                element.child(
                                    div()
                                        .ml(px(4.0))
                                        .font_family(".SystemUIFont")
                                        .text_xs()
                                        .text_color(rgb(THEME.danger))
                                        .child("Six hex digits"),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child("Recent and Harbor Night colors"),
            )
            .child(self.render_color_choices(picker.target, id_prefix, cx))
            .into_any_element()
    }

    /// Color pickers stay inside their owning context menu so selection does
    /// not interrupt terminal work with a second modal layer.
    pub(crate) fn render_inline_color_picker(
        &self,
        picker: &ColorPickerState,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = picker.target;
        div()
            .mx(px(5.0))
            .mb(px(5.0))
            .p(px(8.0))
            .rounded(px(5.0))
            .bg(rgb(THEME.surface))
            .border_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(self.render_color_picker_body(picker, id_prefix, cx))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(7.0))
                    .child(
                        div()
                            .id("inline-workstation-color-default")
                            .px(px(7.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.elevated)))
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.apply_color(target, None, cx)
                                }),
                            )
                            .child("Use default"),
                    )
                    .child(
                        div()
                            .id("cancel-inline-workstation-color")
                            .px(px(7.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .hover(|element| element.bg(rgb(THEME.elevated)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor.color_picker = None;
                                cx.notify();
                            }))
                            .child("Cancel"),
                    )
                    .child(
                        div()
                            .id("apply-inline-workstation-color")
                            .px(px(7.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.accent_soft))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_color_picker(cx)))
                            .child("Apply"),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_workstation_banner_setting(&self, cx: &mut Context<Self>) -> AnyElement {
        let custom = self.sidebar.workstation_banner.is_some();
        let hidden = self.sidebar.workstation_banner_hidden;
        let banner = self
            .sidebar
            .workstation_banner
            .clone()
            .unwrap_or_else(workstation_banner_artwork);
        let (preview_width, preview_height) = banner_fit_size(
            SETTINGS_BANNER_PREVIEW_MAX_WIDTH - 2.0,
            SETTINGS_BANNER_PREVIEW_MAX_HEIGHT - 2.0,
            banner.aspect_ratio(),
        );
        div()
            .pt(px(4.0))
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
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
                            .child("Workstation banner"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .px(px(7.0))
                                    .py(px(3.0))
                                    .rounded(px(4.0))
                                    .bg(rgb(if custom {
                                        THEME.accent_soft
                                    } else {
                                        THEME.surface
                                    }))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.foreground))
                                    .child(if custom { "Custom" } else { "Default" }),
                            )
                            .child(
                                div()
                                    .id("workstation-banner-visible")
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(if hidden {
                                        THEME.surface
                                    } else {
                                        THEME.accent_soft
                                    }))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.foreground))
                                    .hover(|element| element.bg(rgb(THEME.elevated)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_workstation_banner_visibility(cx);
                                    }))
                                    .child(if hidden { "Hidden" } else { "Shown" }),
                            ),
                    ),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child(
                        "Shown at the top of the workstation sidebar. Any aspect ratio is shown whole; the rail header matches the image and is capped at 260 px tall.",
                    ),
            )
            .child(
                div()
                    .w(px(preview_width + 2.0))
                    .h(px(preview_height + 2.0))
                    .flex_none()
                    .overflow_hidden()
                    .rounded(px(6.0))
                    .bg(rgb(THEME.terminal))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        img(banner.image)
                            .id("settings-workstation-banner-preview")
                            .w(px(preview_width))
                            .h(px(preview_height))
                            .object_fit(gpui::ObjectFit::Contain),
                    ),
            )
            .when(hidden, |element| {
                element.child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "Hidden from the workstation sidebar. The image stays saved for when you show it again.",
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .id("choose-workstation-banner")
                            .px(px(9.0))
                            .py(px(5.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.accent_soft))
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose_workstation_banner(cx);
                            }))
                            .child(if custom {
                                "Replace image…"
                            } else {
                                "Choose image…"
                            }),
                    )
                    .when(custom, |element| {
                        element.child(
                            div()
                                .id("reset-workstation-banner")
                                .px(px(9.0))
                                .py(px(5.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.surface))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .hover(|element| element.bg(rgb(THEME.elevated)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.reset_workstation_banner(cx);
                                }))
                                .child("Use default"),
                        )
                    }),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(
                        "PNG, JPEG, WebP, or GIF · 12 MiB maximum · copied to private local storage",
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_appearance_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let section = self.editor.settings_section;
        let nav = SettingsSection::ALL
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                let active = candidate == section;
                div()
                    .id(("settings-section", index))
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .when(active, |element| {
                        element
                            .bg(rgb(THEME.accent_soft))
                            .text_color(rgb(THEME.foreground))
                    })
                    .when(!active, |element| {
                        element
                            .text_color(rgb(THEME.muted))
                            .hover(|element| element.bg(rgb(THEME.elevated)))
                    })
                    .child(candidate.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_settings_section(candidate, cx);
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let panel = match section {
            SettingsSection::Appearance => self.render_appearance_panel(cx),
            SettingsSection::Assistant => self.render_assistant_panel(cx),
            SettingsSection::Voice => self.render_voice_panel(cx),
            SettingsSection::History => vec![
                settings_heading("History", "Local terminal history archive."),
                self.render_history_settings(cx),
            ],
            SettingsSection::Updates => vec![
                settings_heading("Updates", "Signed automatic updates."),
                self.render_update_settings(cx),
            ],
        };
        div()
            .id("settings-workspace-surface")
            .size_full()
            .min_h(px(0.0))
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(PANE_HEADER_HEIGHT))
                    .flex_none()
                    .px(px(10.0))
                    .bg(rgb(THEME.surface))
                    .border_b_1()
                    .border_color(rgb(THEME.border))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(22.0))
                            .text_center()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("⚙"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .id("close-appearance")
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(THEME.muted))
                            .hover(|element| {
                                element
                                    .bg(rgb(THEME.elevated))
                                    .text_color(rgb(THEME.foreground))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor.modal = Modal::None;
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .min_h(px(0.0))
                    .flex_1()
                    .flex()
                    .child(
                        div()
                            .w(px(188.0))
                            .flex_none()
                            .bg(rgb(THEME.sidebar))
                            .border_r_1()
                            .border_color(rgb(THEME.border))
                            .py(px(12.0))
                            .px(px(8.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .children(nav),
                    )
                    .child(
                        div()
                            .id("settings-workspace-content")
                            .min_h(px(0.0))
                            .flex_1()
                            .overflow_y_scroll()
                            .px(px(32.0))
                            .py(px(24.0))
                            .child(
                                div()
                                    .max_w(px(640.0))
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .gap(px(18.0))
                                    .children(panel),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_appearance_panel(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let appearance = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.appearance.clone())
            .unwrap_or_default();
        vec![
            settings_heading(
                "Appearance",
                "Global defaults stay independent. Terminal accents never recolor workstations, and workstation colors never recolor terminals.",
            ),
            settings_card(vec![
                self.render_appearance_row(
                    "Default terminal accent",
                    "Focus rail, active tab, cursor, and terminal focus treatment",
                    ColorTarget::DefaultTerminal,
                    appearance.default_terminal_accent,
                    cx,
                ),
                self.render_appearance_row(
                    "Default workstation color",
                    "Selected workstation and workstation marker in the left rail",
                    ColorTarget::DefaultWorkspace,
                    appearance.default_workspace_color,
                    cx,
                ),
                self.render_workstation_banner_setting(cx),
            ]),
            div()
                .font_family("SF Mono")
                .text_xs()
                .text_color(rgb(THEME.dim))
                .child("Saved locally with session layout · no network or telemetry")
                .into_any_element(),
        ]
    }

    fn render_terminal_agents_setting(&self, cx: &mut Context<Self>) -> AnyElement {
        let command = std::env::var_os(hh_protocol::CLI_ENV)
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("hh"));
        let config = crate::cli::mcp::server_config(&command);
        let config_text =
            serde_json::to_string_pretty(&config).expect("static MCP configuration is valid");
        let clipboard_text = config_text.clone();
        let status = self.editor.agent_skill_status.clone().unwrap_or_else(|| {
            "Installs the bundled Harness Harlot skill for Claude Code, Codex, and pi.".to_owned()
        });
        settings_card(vec![
            settings_row(
                "MCP server",
                Some(format!("{} mcp", command.display())),
                div()
                    .id("copy-hh-mcp-config")
                    .cursor_pointer()
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .text_xs()
                    .text_color(rgb(THEME.accent))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text.clone()));
                        cx.stop_propagation();
                    }))
                    .child("Copy JSON")
                    .into_any_element(),
            ),
            div()
                .font_family("SF Mono")
                .text_xs()
                .text_color(rgb(THEME.muted))
                .bg(rgb(THEME.terminal))
                .border_1()
                .border_color(rgb(THEME.border))
                .rounded(px(6.0))
                .p(px(10.0))
                .child(config_text)
                .into_any_element(),
            settings_row(
                "Agent skill",
                Some(status),
                div()
                    .id("install-hh-agent-skill")
                    .cursor_pointer()
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .text_xs()
                    .text_color(rgb(THEME.accent))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.editor.agent_skill_status =
                            Some(match crate::cli::skill::install_default() {
                                Ok(paths) => {
                                    format!("Installed in {} agent skill directories.", paths.len())
                                }
                                Err(error) => format!("Skill installation failed: {error:#}"),
                            });
                        cx.notify();
                    }))
                    .child("Install")
                    .into_any_element(),
            ),
        ])
    }

    fn render_assistant_panel(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let assistant = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.assistant.clone())
            .unwrap_or_default();
        let state = &self.assistant.coding_agents;
        let mut agent_rows: Vec<AnyElement> = Vec::new();
        if state.loading {
            agent_rows.push(
                div()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.dim))
                    .child("Scanning your login PATH…")
                    .into_any_element(),
            );
        } else if let Some(error) = state.error.as_ref() {
            agent_rows.push(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.danger))
                    .child(error.clone())
                    .into_any_element(),
            );
            agent_rows.push(
                div()
                    .id("coding-agents-retry")
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.accent))
                    .child("Retry")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_coding_agents(cx)))
                    .into_any_element(),
            );
        } else if state.loaded && state.agents.is_empty() {
            agent_rows.push(settings_row(
                "No coding agent CLIs were found on your login PATH",
                Some(
                    "Install omp, Claude Code, Codex, Gemini CLI, Aider, or another supported agent and click Rescan"
                        .to_owned(),
                ),
                div().into_any_element(),
            ));
        } else {
            agent_rows.push(
                div()
                    .id("coding-agent-auto")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(radio_glyph(assistant.preferred_agent.is_none()))
                    .child(settings_row(
                        "Let the assistant choose",
                        Some("Picks from the installed agents below".to_owned()),
                        div().into_any_element(),
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_preferred_agent(None);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            agent_rows.extend(state.agents.iter().enumerate().map(|(index, agent)| {
                let profile = agent.profile;
                let selected = assistant.preferred_agent == Some(profile);
                div()
                    .id(("coding-agent", index))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(radio_glyph(selected))
                    .child(render_terminal_profile_icon(profile, THEME.muted, 18.0))
                    .child(settings_row(
                        profile.display_name(),
                        Some(agent.path.clone()),
                        div().into_any_element(),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_preferred_agent(Some(profile));
                        cx.notify();
                    }))
                    .into_any_element()
            }));
        }
        agent_rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .id("coding-agents-rescan")
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.accent))
                        .child("Rescan")
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_coding_agents(cx))),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child(
                            "Applies to assistants started after this change; restart a running assistant from its header.",
                        ),
                )
                .into_any_element(),
        );
        vec![
            settings_heading(
                "Assistant",
                "The assistant orchestrates coding agents in your terminals. It launches whichever installed agent fits the task unless you prefer one.",
            ),
            settings_section_title("Coding agents"),
            settings_card(agent_rows),
            settings_section_title("Terminal agents"),
            self.render_terminal_agents_setting(cx),
            settings_section_title("Permissions"),
            settings_card(vec![settings_row(
                "Guarded actions",
                Some("Sending terminal input, closing panes, and launching commands".to_owned()),
                self.settings_segmented(
                    "assistant-access",
                    &[
                        (AssistantAccess::Full, "Full"),
                        (AssistantAccess::Confirm, "Confirm"),
                    ],
                    assistant.access,
                    |this, access, _| this.set_assistant_access(access),
                    cx,
                ),
            )]),
            settings_section_title("Model"),
            settings_card(vec![
                settings_row(
                    "Default model",
                    Some(
                        assistant
                            .model
                            .clone()
                            .unwrap_or_else(|| "pi default".to_owned()),
                    ),
                    div().into_any_element(),
                ),
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Change it from the model menu at the bottom of any Assistant.")
                    .into_any_element(),
            ]),
        ]
    }

    fn render_voice_panel(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let editor = &self.assistant.settings_editor;
        let api_key_display = if editor.api_key_input.is_empty() {
            String::new()
        } else {
            let mut tail = editor
                .api_key_input
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>();
            tail.reverse();
            format!("•••• {}", tail.into_iter().collect::<String>())
        };
        let model = match editor.settings.model.as_str() {
            "gpt-realtime-2.1" => "gpt-realtime-2.1",
            "gpt-realtime-2.1-mini" => "gpt-realtime-2.1-mini",
            _ => "unsupported",
        };
        let model_detail = (model == "unsupported")
            .then(|| format!("Unsupported saved value: {}", editor.settings.model));
        let voice = match editor.settings.voice.as_str() {
            "marin" => "marin",
            "cedar" => "cedar",
            "alloy" => "alloy",
            _ => "unsupported",
        };
        let voice_detail = (voice == "unsupported")
            .then(|| format!("Unsupported saved value: {}", editor.settings.voice));
        let full_duplex = editor.settings.full_duplex;
        vec![
            settings_heading(
                "Voice",
                "Optional. Uses the OpenAI Realtime API to talk with the assistant; the microphone stays off until you start voice.",
            ),
            settings_section_title("Connection"),
            settings_card(vec![
                self.settings_text_input(
                    "voice-api-key",
                    "OpenAI API key",
                    api_key_display,
                    "Paste API key",
                    VoiceSettingsField::ApiKey,
                    cx,
                ),
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Used for this process only; it is not saved. Set HH_OPENAI_API_KEY for future launches.")
                    .into_any_element(),
            ]),
            settings_section_title("Speech"),
            settings_card(vec![
                settings_row(
                    "Realtime model",
                    model_detail,
                    self.settings_segmented(
                        "voice-model",
                        &[
                            ("gpt-realtime-2.1", "Standard"),
                            ("gpt-realtime-2.1-mini", "Mini"),
                        ],
                        model,
                        |this, value, cx| this.set_voice_model(value, cx),
                        cx,
                    ),
                ),
                settings_row(
                    "Voice",
                    voice_detail,
                    self.settings_segmented(
                        "voice-voice",
                        &[("marin", "Marin"), ("cedar", "Cedar"), ("alloy", "Alloy")],
                        voice,
                        |this, value, cx| this.set_voice(value, cx),
                        cx,
                    ),
                ),
                settings_row(
                    "Full duplex",
                    Some("Requires headphones".to_owned()),
                    self.settings_segmented(
                        "voice-full-duplex",
                        &[(true, "On"), (false, "Off")],
                        full_duplex,
                        |this, value, cx| {
                            if value != this.assistant.settings_editor.settings.full_duplex {
                                this.toggle_full_duplex(cx);
                            }
                        },
                        cx,
                    ),
                ),
                self.settings_text_input(
                    "voice-idle-timeout",
                    "Idle timeout (seconds)",
                    editor.idle_timeout_input.clone(),
                    "0 disables; minimum 60",
                    VoiceSettingsField::IdleTimeout,
                    cx,
                ),
            ]),
            div()
                .font_family(".SystemUIFont")
                .text_xs()
                .text_color(rgb(THEME.dim))
                .child("Changes apply to the next assistant session.")
                .into_any_element(),
            div()
                .id("voice-privacy-disclosure")
                .cursor_pointer()
                .font_family(".SystemUIFont")
                .text_xs()
                .text_color(rgb(THEME.accent))
                .child("Voice privacy and data handling")
                .on_click(cx.listener(|_, _, _, cx| cx.open_url(VOICE_PRIVACY_URL)))
                .into_any_element(),
        ]
    }

    /// Pill group; the selected option carries the accent fill.
    fn settings_segmented<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[(T, &'static str)],
        current: T,
        on_select: fn(&mut Self, T, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut group = div()
            .rounded(px(6.0))
            .bg(rgb(THEME.elevated))
            .p(px(2.0))
            .flex()
            .gap(px(2.0));
        for (index, (value, label)) in options.iter().enumerate() {
            let value = *value;
            let selected = value == current;
            group = group.child(
                div()
                    .id((id, index))
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .when(selected, |element| {
                        element
                            .bg(rgb(THEME.accent_soft))
                            .text_color(rgb(THEME.foreground))
                    })
                    .when(!selected, |element| element.text_color(rgb(THEME.muted)))
                    .child(*label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        on_select(this, value, cx);
                        cx.notify();
                    })),
            );
        }
        group.into_any_element()
    }

    /// Append/backspace/paste field; keyboard editing lives in
    /// `handle_voice_settings_key`.
    fn settings_text_input(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        placeholder: &'static str,
        field: VoiceSettingsField,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.assistant.settings_editor.active_field == Some(field);
        let empty = value.is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child(label),
            )
            .child(
                div()
                    .id(id)
                    .h(px(36.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .bg(rgb(THEME.terminal))
                    .border_1()
                    .border_color(rgb(if active {
                        THEME.accent
                    } else {
                        THEME.border_strong
                    }))
                    .cursor_text()
                    .flex()
                    .items_center()
                    .font_family("SF Mono")
                    .text_sm()
                    .text_color(rgb(if empty { THEME.dim } else { THEME.foreground }))
                    .child(if empty { placeholder.to_owned() } else { value })
                    .when(active, |element| {
                        element.child(div().text_color(rgb(THEME.accent)).child("▮"))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.assistant.settings_editor.active_field = Some(field);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    pub(crate) fn render_appearance_row(
        &self,
        label: &'static str,
        description: &'static str,
        target: ColorTarget,
        color: AppearanceColor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rgb_value = color.as_rgb();
        div()
            .p(px(12.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.surface))
            .border_1()
            .border_color(rgb(THEME.border))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .w(px(28.0))
                    .h(px(28.0))
                    .rounded(px(7.0))
                    .bg(rgb(rgb_value))
                    .border_1()
                    .border_color(rgb(THEME.border_strong)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(THEME.foreground))
                            .child(label),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .child(description),
                    ),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(format!("#{rgb_value:06X}")),
            )
            .child(
                div()
                    .id(match target {
                        ColorTarget::DefaultTerminal => "pick-default-terminal",
                        ColorTarget::DefaultWorkspace => "pick-default-workspace",
                        ColorTarget::Pane(_) => "pick-pane",
                        ColorTarget::Workspace(_) => "pick-workspace",
                        ColorTarget::Tab(_) => "pick-tab",
                    })
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.border_color(rgb(rgb_value)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_color_picker(target, cx)))
                    .child("Pick color…"),
            )
            .into_any_element()
    }

    pub(crate) fn render_color_picker(
        &self,
        picker: &ColorPickerState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = picker.target;
        let (title, can_reset) = match target {
            ColorTarget::DefaultTerminal => ("Pick default terminal accent", false),
            ColorTarget::DefaultWorkspace => ("Pick default workstation color", false),
            ColorTarget::Pane(_) => ("Pick terminal color", true),
            ColorTarget::Workspace(_) => ("Pick workstation color", true),
            ColorTarget::Tab(_) => ("Pick group or project color", true),
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0faa))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(340.0))
                    .p(px(16.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(11.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(title),
                    )
                    .child(self.render_color_picker_body(picker, "modal-color", cx))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .when(can_reset, |element| {
                                element.child(
                                    div()
                                        .id("picker-use-default")
                                        .px(px(11.0))
                                        .py(px(7.0))
                                        .rounded(px(5.0))
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(rgb(THEME.foreground))
                                        .hover(|element| element.bg(rgb(THEME.surface)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.apply_color(target, None, cx)
                                        }))
                                        .child("Use default"),
                                )
                            })
                            .child(
                                div()
                                    .id("cancel-color-picker")
                                    .px(px(11.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.surface)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.editor.color_picker = None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("apply-color-picker")
                                    .px(px(11.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.accent_soft))
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .hover(|element| element.bg(rgb(THEME.selection)))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.submit_color_picker(cx)),
                                    )
                                    .child("Apply"),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::color_picker_hosted;
    use crate::view_models::{ColorTarget, Modal, WorkspaceMenu};
    use gpui::{point, px};
    use uuid::Uuid;

    #[test]
    fn color_picker_hosted_only_by_its_matching_menu() {
        let workspace_id = Uuid::new_v4();
        let other_workspace_id = Uuid::new_v4();
        let workspace_menu = Modal::WorkspaceMenu(WorkspaceMenu {
            workspace_id,
            position: point(px(0.0), px(0.0)),
            icon_picker_open: false,
            customize_open: false,
        });

        assert!(color_picker_hosted(
            ColorTarget::Workspace(workspace_id),
            &workspace_menu
        ));
        assert!(!color_picker_hosted(
            ColorTarget::Workspace(other_workspace_id),
            &workspace_menu
        ));
        assert!(!color_picker_hosted(
            ColorTarget::Pane(Uuid::new_v4()),
            &Modal::None
        ));
        assert!(color_picker_hosted(
            ColorTarget::DefaultTerminal,
            &Modal::None
        ));
        assert!(color_picker_hosted(
            ColorTarget::DefaultWorkspace,
            &Modal::None
        ));
    }
}
