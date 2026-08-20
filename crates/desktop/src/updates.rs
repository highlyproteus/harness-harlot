use std::time::Duration;

use crate::{AvailableUpdateBanner, HhApp, THEME, development_build};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use hh_updater::fetch::{fetch_available_update, runtime_architecture, runtime_platform};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use hh_updater::{CurrentRelease, TRUSTED_UPDATE_KEYS, current_build, explicit_install_supported};

pub(crate) const fn automatic_update_check_interval() -> Duration {
    Duration::from_hours(1)
}

pub(crate) fn automatic_update_checks_enabled() -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        update_checks_enabled_for(
            development_build(),
            !TRUSTED_UPDATE_KEYS.is_empty(),
            current_build(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

const fn update_checks_enabled_for(
    development_build: bool,
    trusted_keys_available: bool,
    packaged_build: u64,
) -> bool {
    !development_build && trusted_keys_available && packaged_build > 0
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum UpdateCheckStatus {
    #[default]
    Idle,
    Checking,
    Current,
    Available(String),
    Failed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UpdateCheckState {
    status: UpdateCheckStatus,
}

impl UpdateCheckState {
    pub(crate) fn begin(&mut self) -> bool {
        if self.checking() {
            return false;
        }
        self.status = UpdateCheckStatus::Checking;
        true
    }

    pub(crate) fn finish_current(&mut self) {
        self.status = UpdateCheckStatus::Current;
    }

    pub(crate) fn finish_available(&mut self, version: String) {
        self.status = UpdateCheckStatus::Available(version);
    }

    pub(crate) fn finish_failed(&mut self) {
        self.status = UpdateCheckStatus::Failed;
    }

    pub(crate) fn checking(&self) -> bool {
        self.status == UpdateCheckStatus::Checking
    }

    pub(crate) fn status_text(&self) -> String {
        match &self.status {
            UpdateCheckStatus::Idle => "Checks automatically every hour".to_owned(),
            UpdateCheckStatus::Checking => "Checking for updates…".to_owned(),
            UpdateCheckStatus::Current => "Harness Harlot is up to date".to_owned(),
            UpdateCheckStatus::Available(version) => format!("Version {version} is available"),
            UpdateCheckStatus::Failed => "Unable to check for updates".to_owned(),
        }
    }
}

impl HhApp {
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if !automatic_update_checks_enabled() || !self.editor.update_check.begin() {
            return;
        }
        cx.notify();

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let platform = runtime_platform()?;
                    let architecture = runtime_architecture()?;
                    fetch_available_update(&CurrentRelease {
                        version: env!("CARGO_PKG_VERSION"),
                        build: current_build(),
                        platform,
                        architecture,
                        protocol_version: hh_protocol::PROTOCOL_VERSION,
                    })
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Some(update)) => {
                        let installing =
                            this.editor
                                .update_available
                                .as_ref()
                                .is_some_and(|current| {
                                    current.version == update.version && current.installing
                                });
                        this.editor
                            .update_check
                            .finish_available(update.version.clone());
                        this.editor.update_available = Some(AvailableUpdateBanner {
                            version: update.version,
                            requires_service_restart: update.requires_service_restart,
                            install_supported: explicit_install_supported(
                                update.artifact.platform.as_str(),
                            ),
                            installing,
                        });
                    }
                    Ok(None) => {
                        this.editor.update_check.finish_current();
                        this.editor.update_available = None;
                    }
                    Err(error) => {
                        eprintln!("Harness Harlot update check failed: {error:#}");
                        this.editor.update_check.finish_failed();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn render_update_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let checks_enabled = automatic_update_checks_enabled();
        let checking = self.editor.update_check.checking();
        let status = if checks_enabled {
            self.editor.update_check.status_text()
        } else {
            "Update checks are available in release builds".to_owned()
        };

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
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(THEME.foreground))
                    .child("Updates"),
            )
            .child(
                div()
                    .p(px(10.0))
                    .rounded(px(7.0))
                    .bg(rgb(THEME.surface))
                    .border_1()
                    .border_color(rgb(THEME.border))
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
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(THEME.foreground))
                                    .child("Software updates"),
                            )
                            .child(
                                div()
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.muted))
                                    .child(status),
                            ),
                    )
                    .child(
                        div()
                            .id("check-for-updates")
                            .px(px(10.0))
                            .py(px(6.0))
                            .rounded(px(5.0))
                            .bg(rgb(THEME.elevated))
                            .border_1()
                            .border_color(rgb(THEME.border_strong))
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(if checks_enabled && !checking {
                                THEME.foreground
                            } else {
                                THEME.dim
                            }))
                            .when(checks_enabled && !checking, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(rgb(THEME.accent_soft)))
                            })
                            .when(checks_enabled, |button| {
                                button.on_click(cx.listener(|this, _, _, cx| {
                                    this.check_for_updates(cx);
                                }))
                            })
                            .child(if checking {
                                "Checking…"
                            } else {
                                "Check for Updates"
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_update_checks_repeat_hourly() {
        assert_eq!(automatic_update_check_interval(), Duration::from_hours(1));
    }

    #[test]
    fn release_without_packaged_build_metadata_does_not_check_for_updates() {
        assert!(!update_checks_enabled_for(false, true, 0));
        assert!(update_checks_enabled_for(false, true, 1));
    }

    #[test]
    fn manual_update_check_reports_progress_and_blocks_duplicate_requests() {
        let mut state = UpdateCheckState::default();

        assert!(state.begin());
        assert!(!state.begin());
        assert_eq!(state.status_text(), "Checking for updates…");
    }

    #[test]
    fn completed_manual_check_reports_that_the_installed_release_is_current() {
        let mut state = UpdateCheckState::default();
        assert!(state.begin());

        state.finish_current();

        assert_eq!(state.status_text(), "Harness Harlot is up to date");
        assert!(!state.checking());
    }

    #[test]
    fn completed_manual_check_reports_the_available_version() {
        let mut state = UpdateCheckState::default();
        assert!(state.begin());

        state.finish_available("0.1.5".to_owned());

        assert_eq!(state.status_text(), "Version 0.1.5 is available");
        assert!(!state.checking());
    }

    #[test]
    fn failed_manual_check_reports_a_retryable_status() {
        let mut state = UpdateCheckState::default();
        assert!(state.begin());

        state.finish_failed();

        assert_eq!(state.status_text(), "Unable to check for updates");
        assert!(!state.checking());
    }
}
