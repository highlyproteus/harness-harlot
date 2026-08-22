//! Appearance settings, color pickers, and workstation banner art.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, Image, ImageFormat, InteractiveElement, IntoElement, PathPromptOptions,
    div, img, px, rgb, rgba,
};
use gpui::{AppContext, ParentElement, StatefulInteractiveElement, Styled, StyledImage};
use hh_protocol::{AppearanceColor, ClientRequest, validate_workspace_dir};
use std::sync::{Arc, LazyLock};

use crate::elements::{HsvFieldElement, HsvFieldKind};
use crate::helpers::{banner_fit_size, hsv_to_rgb, parse_hex_color, rgb_to_hsv};
use crate::view_models::{ColorPickerState, ColorTarget, Modal, TooltipView};
use crate::{
    APPEARANCE_PRESETS, BUNDLED_BANNER_PIXEL_HEIGHT, BUNDLED_BANNER_PIXEL_WIDTH, HhApp,
    PANE_HEADER_HEIGHT, THEME,
};

/// Budget the settings preview fits inside, borders excluded.
pub(crate) const SETTINGS_BANNER_PREVIEW_MAX_WIDTH: f32 = 420.0;

pub(crate) const SETTINGS_BANNER_PREVIEW_MAX_HEIGHT: f32 = 220.0;

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

    pub(crate) fn open_appearance_settings(&mut self, cx: &mut Context<Self>) {
        self.editor.modal = Modal::AppearanceSettings;
        self.editor.color_picker = None;
        self.editor.history_editor = None;
        self.editor.history_clear_confirmation = None;
        self.refresh_history_status();
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
        let appearance = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.appearance.clone())
            .unwrap_or_default();
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
                    .id("settings-workspace-content")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .px(px(24.0))
                    .py(px(20.0))
                    .child(
                        div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Appearance"),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("Global defaults stay independent. Terminal accents never recolor workstations, and workstation colors never recolor terminals."),
                    )
                    .child(self.render_appearance_row(
                        "Default terminal accent",
                        "Focus rail, active tab, cursor, and terminal focus treatment",
                        ColorTarget::DefaultTerminal,
                        appearance.default_terminal_accent,
                        cx,
                    ))
                    .child(self.render_appearance_row(
                        "Default workstation color",
                        "Selected workstation and workstation marker in the left rail",
                        ColorTarget::DefaultWorkspace,
                        appearance.default_workspace_color,
                        cx,
                    ))
                    .child(self.render_workstation_banner_setting(cx))
                    .child(
                        div()
                            .pt(px(2.0))
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Saved locally with session layout · no network or telemetry"),
                    )
                    .child(self.render_update_settings(cx))
                    .child(self.render_history_settings(cx)),
                    ),
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
