//! macOS privacy permissions for programs running in Harness Harlot terminals.
//!
//! macOS grants Screen Recording and Accessibility to the app a process is
//! attributed to (its *responsible process*). Terminals outlive the window, so
//! the desktop starts the session service under `hh session-host`: a
//! windowless copy of the app binary spawned as its own responsible process.
//! Everything the service launches, including the private tmux server and
//! every shell, uses Harness Harlot's grants for as long as the host runs, and
//! the host stays alive while any of those processes do.
//!
//! Terminals started before this existed, or by a host from a different app
//! build, stay attributed elsewhere until they are restarted, which the
//! Permissions settings offer.

use std::ffi::OsStr;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use hh_session_client::SessionClient;
use rustix::process::{Pid, Signal, kill_process};
use serde::{Deserialize, Serialize};

use crate::appearance::settings_heading;
use crate::view_models::{Modal, SettingsSection};
use crate::{HhApp, THEME};

pub(crate) const SESSION_HOST_COMMAND: &str = "session-host";
pub(crate) const STATUS_COMMAND: &str = "privacy-status";

/// How often a host whose service has exited checks for remaining terminals.
const HOST_LINGER_POLL: Duration = Duration::from_secs(10);
/// Status refresh cadence while the Permissions panel is visible, so a grant
/// made in System Settings next to the window shows up promptly.
pub(crate) const VISIBLE_REFRESH: Duration = Duration::from_secs(3);
/// Background cadence that keeps the sidebar notice current.
pub(crate) const BACKGROUND_REFRESH: Duration = Duration::from_mins(1);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

const SCREEN_RECORDING_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const ACCESSIBILITY_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Grants as seen by a fresh process of this app build.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrivacyGrants {
    pub(crate) screen_recording: bool,
    pub(crate) accessibility: bool,
}

/// Whether programs in the running terminals receive this app's grants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalAttribution {
    /// The service and tmux server run under a session host of this build.
    Current,
    /// Some terminals run without this build's permissions until restarted.
    Stale,
    /// No session service is reachable.
    NoService,
    /// `HH_DISABLE_BUNDLED_SERVICE`: the service is managed outside the app.
    Unmanaged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PrivacyStatus {
    pub(crate) grants: PrivacyGrants,
    pub(crate) terminals: TerminalAttribution,
}

impl PrivacyStatus {
    /// Terminals are missing a permission the user already granted.
    pub(crate) fn needs_terminal_restart(self) -> bool {
        self.terminals == TerminalAttribution::Stale
            && (self.grants.screen_recording || self.grants.accessibility)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Permission {
    ScreenRecording,
    Accessibility,
}

impl Permission {
    const fn settings_url(self) -> &'static str {
        match self {
            Self::ScreenRecording => SCREEN_RECORDING_SETTINGS,
            Self::Accessibility => ACCESSIBILITY_SETTINGS,
        }
    }

    /// The `tccutil` service name.
    const fn tcc_service(self) -> &'static str {
        match self {
            Self::ScreenRecording => "ScreenCapture",
            Self::Accessibility => "Accessibility",
        }
    }
}

/// Clears this app's entry for `permission`. macOS keys each entry to the
/// exact build that was granted; unnotarized builds differ on every update,
/// so an entry left by an earlier build keeps its switch shown in System
/// Settings while blocking this build's prompt. Only called while this build
/// lacks the permission, so no working grant is lost.
fn reset_outdated_grant(permission: Permission) {
    let Some(bundle) = hh_macos_privacy::main_bundle_identifier() else {
        return;
    };
    let _ = Command::new("/usr/bin/tccutil")
        .args(["reset", permission.tcc_service(), &bundle])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RestartPhase {
    #[default]
    Idle,
    Confirming,
    Running,
}

#[derive(Debug, Default)]
pub(crate) struct PrivacyUi {
    status: Option<PrivacyStatus>,
    refreshing: bool,
    last_refresh: Option<Instant>,
    /// macOS shows each permission's prompt once; later clicks open Settings.
    prompted_screen_recording: bool,
    prompted_accessibility: bool,
    restart: RestartPhase,
    error: Option<String>,
}

impl PrivacyUi {
    pub(crate) fn needs_terminal_restart(&self) -> bool {
        self.status
            .is_some_and(PrivacyStatus::needs_terminal_restart)
    }
}

/// `hh privacy-status`: prints this process's grants as JSON.
pub(crate) fn print_status() -> Result<()> {
    let grants = PrivacyGrants {
        screen_recording: hh_macos_privacy::screen_recording_allowed(),
        accessibility: hh_macos_privacy::accessibility_allowed(),
    };
    println!("{}", serde_json::to_string(&grants)?);
    Ok(())
}

/// `hh session-host`: runs the session service and then stays alive while
/// any process it is responsible for (the tmux server and its shells) runs.
pub(crate) fn run_session_host(service: &Path) -> Result<()> {
    Command::new(service)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("run session service {}", service.display()))?;
    let host = std::process::id();
    while !hh_macos_privacy::processes_responsible_to(host)
        .context("list processes started by the session host")?
        .is_empty()
    {
        thread::sleep(HOST_LINGER_POLL);
    }
    Ok(())
}

/// Starts `hh session-host` as its own responsible process.
pub(crate) fn start_session_host() -> Result<()> {
    let desktop = std::env::current_exe().context("resolve Harness Harlot executable")?;
    let pid =
        hh_macos_privacy::spawn_disclaimed(&desktop, &[OsStr::new(SESSION_HOST_COMMAND)], None)
            .context("start session host")?;
    thread::Builder::new()
        .name("hh-session-host-reaper".to_owned())
        .spawn(move || {
            let _ = hh_macos_privacy::wait_for_exit(pid);
        })
        .context("start session host reaper")?;
    Ok(())
}

/// Probes grants in a fresh disclaimed process (the host's exact identity,
/// without this process's cached answers) and checks terminal attribution.
fn current_status() -> Result<PrivacyStatus> {
    let desktop = desktop_executable()?;
    let output = hh_macos_privacy::disclaimed_output(&desktop, &[OsStr::new(STATUS_COMMAND)])
        .context("probe privacy permissions")?;
    let grants = serde_json::from_slice(&output).context("decode privacy permissions")?;
    Ok(PrivacyStatus {
        grants,
        terminals: terminal_attribution(),
    })
}

fn desktop_executable() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("resolve Harness Harlot executable")?;
    std::fs::canonicalize(&executable).with_context(|| format!("resolve {}", executable.display()))
}

fn terminal_attribution() -> TerminalAttribution {
    if std::env::var_os("HH_DISABLE_BUNDLED_SERVICE").is_some() {
        return TerminalAttribution::Unmanaged;
    }
    let Some(service) = service_pid() else {
        return TerminalAttribution::NoService;
    };
    // Grants follow the exact build, so the host must run this build's code,
    // not merely the same path, which a rebuilt or updated bundle reuses.
    let own_code = hh_macos_privacy::code_hash(std::process::id());
    let hosted = |pid| {
        own_code.is_some()
            && hh_macos_privacy::responsible_pid(pid)
                .filter(|host| *host != pid)
                .and_then(hh_macos_privacy::code_hash)
                == own_code
    };
    if hosted(service) && tmux_server_pid().is_none_or(hosted) {
        TerminalAttribution::Current
    } else {
        TerminalAttribution::Stale
    }
}

fn service_pid() -> Option<u32> {
    let client = SessionClient::connect().ok()?;
    hh_macos_privacy::peer_pid(client.as_fd()).ok()
}

fn tmux_server_pid() -> Option<u32> {
    let state_directory = hh_protocol::state_directory()?;
    let socket_name = hh_protocol::managed_tmux_socket_name(&state_directory);
    let stream = UnixStream::connect(hh_protocol::tmux_socket_path(&socket_name)).ok()?;
    hh_macos_privacy::peer_pid(stream.as_fd()).ok()
}

/// Stops the session service (which saves the layout), then the private tmux
/// server and every program in it, and starts both again under a session host
/// of this build. Local panes reopen as fresh shells in their last folders.
fn restart_terminals() -> Result<()> {
    if let Some(service) = service_pid() {
        terminate(service).context("stop the session service")?;
        wait_until(|| SessionClient::connect().is_err())
            .context("the session service did not stop")?;
    }
    if let Some(tmux) = tmux_server_pid() {
        terminate(tmux).context("stop the terminal tmux server")?;
        wait_until(|| tmux_server_pid().is_none())
            .context("the terminal tmux server did not stop")?;
    }
    crate::ensure_bundled_session_service();
    Ok(())
}

fn terminate(pid: u32) -> Result<()> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .context("invalid process id")?;
    kill_process(pid, Signal::TERM)?;
    Ok(())
}

fn wait_until(mut done: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    while !done() {
        anyhow::ensure!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

impl HhApp {
    pub(crate) fn permissions_panel_visible(&self) -> bool {
        matches!(self.editor.modal, Modal::AppearanceSettings)
            && self.editor.settings_section == SettingsSection::Permissions
    }

    /// Toolbar pill shown while terminals lack a permission the user granted.
    pub(crate) fn render_privacy_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.editor.privacy.needs_terminal_restart() || self.permissions_panel_visible() {
            return None;
        }
        Some(
            div()
                .id("privacy-notice")
                .h(px(26.0))
                .px(px(8.0))
                .rounded(px(5.0))
                .bg(rgb(THEME.surface))
                .border_1()
                .border_color(rgb(THEME.warning))
                .font_family(".SystemUIFont")
                .text_xs()
                .text_color(rgb(THEME.foreground))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|button| button.bg(rgb(THEME.elevated)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_settings(SettingsSection::Permissions, cx);
                }))
                .child("Permissions")
                .into_any_element(),
        )
    }

    /// Refreshes when due: every [`VISIBLE_REFRESH`] while the Permissions
    /// panel shows, otherwise every [`BACKGROUND_REFRESH`].
    pub(crate) fn refresh_privacy_status_if_due(&mut self, cx: &mut Context<Self>) {
        let interval = if self.permissions_panel_visible() {
            VISIBLE_REFRESH
        } else {
            BACKGROUND_REFRESH
        };
        if self
            .editor
            .privacy
            .last_refresh
            .is_none_or(|last| last.elapsed() >= interval)
        {
            self.refresh_privacy_status(cx);
        }
    }

    pub(crate) fn refresh_privacy_status(&mut self, cx: &mut Context<Self>) {
        let privacy = &mut self.editor.privacy;
        if privacy.refreshing || privacy.restart == RestartPhase::Running {
            return;
        }
        privacy.refreshing = true;
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { current_status() }).await;
            let _ = this.update(cx, |this, cx| {
                let privacy = &mut this.editor.privacy;
                privacy.refreshing = false;
                privacy.last_refresh = Some(Instant::now());
                match result {
                    Ok(status) => {
                        privacy.status = Some(status);
                        privacy.error = None;
                    }
                    Err(error) => {
                        eprintln!("Harness Harlot privacy status failed: {error:#}");
                        privacy.error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn request_permission(&mut self, permission: Permission, cx: &mut Context<Self>) {
        let privacy = &mut self.editor.privacy;
        let prompted = match permission {
            Permission::ScreenRecording => &mut privacy.prompted_screen_recording,
            Permission::Accessibility => &mut privacy.prompted_accessibility,
        };
        if *prompted {
            cx.open_url(permission.settings_url());
        } else {
            *prompted = true;
            reset_outdated_grant(permission);
            match permission {
                Permission::ScreenRecording => {
                    hh_macos_privacy::request_screen_recording();
                }
                Permission::Accessibility => {
                    hh_macos_privacy::request_accessibility();
                }
            }
        }
        self.refresh_privacy_status(cx);
    }

    fn restart_terminals_for_privacy(&mut self, cx: &mut Context<Self>) {
        let privacy = &mut self.editor.privacy;
        if privacy.restart == RestartPhase::Running {
            return;
        }
        privacy.restart = RestartPhase::Running;
        privacy.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { restart_terminals() }).await;
            let _ = this.update(cx, |this, cx| {
                this.editor.privacy.restart = RestartPhase::Idle;
                if let Err(error) = result {
                    this.editor.privacy.error =
                        Some(format!("Could not restart terminals: {error:#}"));
                }
                this.refresh_privacy_status(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn render_permissions_panel(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let privacy = &self.editor.privacy;
        let grants = privacy.status.map(|status| status.grants);
        let mut panel = vec![
            settings_heading(
                "Permissions",
                "Let programs in your terminals, such as coding agents, see the screen and control the computer.",
            ),
            permission_row(
                "screen-recording",
                "Screen Recording",
                "Screenshots and screen sharing",
                grants.map(|grants| grants.screen_recording),
                Permission::ScreenRecording,
                cx,
            ),
            permission_row(
                "accessibility",
                "Accessibility",
                "Clicking, typing, and reading other apps' controls",
                grants.map(|grants| grants.accessibility),
                Permission::Accessibility,
                cx,
            ),
            self.render_terminal_attribution(cx),
        ];
        if cfg!(feature = "community-macos") || crate::development_build() {
            panel.push(note(
                "This build isn't notarized by Apple, so macOS may ask for these permissions again after each update.",
            ));
        }
        if let Some(error) = &privacy.error {
            panel.push(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.danger))
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        panel
    }

    fn render_terminal_attribution(&self, cx: &mut Context<Self>) -> AnyElement {
        let privacy = &self.editor.privacy;
        let terminals = privacy.status.map(|status| status.terminals);
        let (ok, detail) = match terminals {
            None => (None, "Checking…"),
            Some(TerminalAttribution::Current) => (
                Some(true),
                "Terminal programs use these permissions. Restart a program that was already running after you change them.",
            ),
            Some(TerminalAttribution::Stale) => (
                Some(false),
                "Terminals started before this version, or by an app that has since quit, can't use these permissions until they restart.",
            ),
            Some(TerminalAttribution::NoService) => (None, "The terminal service isn't running."),
            Some(TerminalAttribution::Unmanaged) => {
                (None, "The terminal service is managed outside the app.")
            }
        };
        let stale = terminals == Some(TerminalAttribution::Stale);
        let action = if privacy.restart == RestartPhase::Running {
            Some(
                button("restart-terminals", "Restarting…", ButtonTone::Disabled).into_any_element(),
            )
        } else if stale && privacy.restart == RestartPhase::Idle {
            Some(
                button(
                    "restart-terminals",
                    "Restart Terminals…",
                    ButtonTone::Normal,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.editor.privacy.restart = RestartPhase::Confirming;
                    cx.notify();
                }))
                .into_any_element(),
            )
        } else {
            None
        };
        let card = card("Terminals", detail, ok, action);
        if !(stale && privacy.restart == RestartPhase::Confirming) {
            return card;
        }
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(card)
            .child(
                div()
                    .p(px(10.0))
                    .rounded(px(7.0))
                    .bg(rgb(THEME.surface))
                    .border_1()
                    .border_color(rgb(THEME.danger))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .child("Every running terminal program closes. Your tabs and splits stay, and reopen as fresh shells in the same folders. SSH tabs stay offline until you reconnect."),
                    )
                    .child(
                        button("cancel-restart-terminals", "Cancel", ButtonTone::Normal)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor.privacy.restart = RestartPhase::Idle;
                                cx.notify();
                            })),
                    )
                    .child(
                        button("confirm-restart-terminals", "Restart Now", ButtonTone::Danger)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.restart_terminals_for_privacy(cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

fn permission_row(
    id: &'static str,
    title: &'static str,
    purpose: &'static str,
    allowed: Option<bool>,
    permission: Permission,
    cx: &mut Context<HhApp>,
) -> AnyElement {
    let detail = match allowed {
        None => format!("{purpose} · checking…"),
        Some(true) => format!("{purpose} · allowed"),
        Some(false) => format!("{purpose} · not allowed"),
    };
    let action = (allowed == Some(false)).then(|| {
        button(id, "Allow…", ButtonTone::Normal)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.request_permission(permission, cx);
            }))
            .into_any_element()
    });
    card(title, detail, allowed, action)
}

fn card(
    title: &'static str,
    detail: impl Into<gpui::SharedString>,
    ok: Option<bool>,
    action: Option<AnyElement>,
) -> AnyElement {
    let indicator = match ok {
        Some(true) => THEME.ansi[2],
        Some(false) => THEME.warning,
        None => THEME.dim,
    };
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
                .flex_none()
                .w(px(8.0))
                .h(px(8.0))
                .rounded(px(4.0))
                .bg(rgb(indicator)),
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
                        .child(title),
                )
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.muted))
                        .child(detail.into()),
                ),
        )
        .children(action)
        .into_any_element()
}

fn note(text: &'static str) -> AnyElement {
    div()
        .font_family(".SystemUIFont")
        .text_xs()
        .text_color(rgb(THEME.dim))
        .child(text)
        .into_any_element()
}

#[derive(Clone, Copy)]
enum ButtonTone {
    Normal,
    Danger,
    Disabled,
}

fn button(id: &'static str, label: &'static str, tone: ButtonTone) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex_none()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(5.0))
        .border_1()
        .font_family(".SystemUIFont")
        .text_xs()
        .child(label);
    match tone {
        ButtonTone::Normal => base
            .bg(rgb(THEME.elevated))
            .border_color(rgb(THEME.border_strong))
            .text_color(rgb(THEME.foreground))
            .cursor_pointer()
            .hover(|button| button.bg(rgb(THEME.accent_soft))),
        ButtonTone::Danger => base
            .bg(rgb(THEME.danger))
            .border_color(rgb(THEME.danger))
            .text_color(rgb(0xffffff))
            .cursor_pointer(),
        ButtonTone::Disabled => base
            .bg(rgb(THEME.elevated))
            .border_color(rgb(THEME.border))
            .text_color(rgb(THEME.dim)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        screen_recording: bool,
        accessibility: bool,
        terminals: TerminalAttribution,
    ) -> PrivacyStatus {
        PrivacyStatus {
            grants: PrivacyGrants {
                screen_recording,
                accessibility,
            },
            terminals,
        }
    }

    #[test]
    fn terminal_restart_is_suggested_only_for_granted_permissions_terminals_lack() {
        assert!(status(true, false, TerminalAttribution::Stale).needs_terminal_restart());
        assert!(status(false, true, TerminalAttribution::Stale).needs_terminal_restart());
        // Nothing granted: restarting would not give terminals anything.
        assert!(!status(false, false, TerminalAttribution::Stale).needs_terminal_restart());
        for terminals in [
            TerminalAttribution::Current,
            TerminalAttribution::NoService,
            TerminalAttribution::Unmanaged,
        ] {
            assert!(!status(true, true, terminals).needs_terminal_restart());
        }
    }
}
