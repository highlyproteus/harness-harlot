//! Settings → Bots: default agent, per-agent integration, and the MCP/skill
//! setup shared with every terminal agent.
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use hh_protocol::TerminalProfile;

use crate::appearance::{
    radio_glyph, settings_card, settings_heading, settings_row, settings_section_title,
};
use crate::helpers::render_terminal_profile_icon;
use crate::{HhApp, THEME};

/// How a bot running this agent reaches Harness Harlot's tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BotIntegration {
    /// omp loads the bundled Harness Harlot plugin at launch.
    Plugin,
    /// The launch command attaches the MCP server.
    McpAtLaunch,
    /// The agent needs this one-time command to register the MCP server.
    SetupCommand(String),
    /// The agent has no MCP command; the server JSON goes in its settings.
    ManualConfig,
}

/// POSIX single-quoting, skipped for plain paths so commands stay readable.
fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./:@%+=".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

pub(crate) fn bot_integration(profile: TerminalProfile, hh: &Path) -> BotIntegration {
    let hh = shell_word(&hh.to_string_lossy());
    match profile {
        TerminalProfile::Omp => BotIntegration::Plugin,
        TerminalProfile::Claude | TerminalProfile::Codex => BotIntegration::McpAtLaunch,
        TerminalProfile::Hermes => BotIntegration::SetupCommand(format!(
            "hermes mcp add harness-harlot --command {hh} --args mcp"
        )),
        TerminalProfile::Gemini => BotIntegration::SetupCommand(format!(
            "gemini mcp add --scope user harness-harlot {hh} mcp"
        )),
        TerminalProfile::Droid => {
            BotIntegration::SetupCommand(format!("droid mcp add harness-harlot {hh} mcp"))
        }
        TerminalProfile::Terminal
        | TerminalProfile::KiloCode
        | TerminalProfile::Cursor
        | TerminalProfile::OpenCode
        | TerminalProfile::Aider
        | TerminalProfile::GitHubCopilot
        | TerminalProfile::Tmux => BotIntegration::ManualConfig,
    }
}

/// The `hh` executable agents launch as the MCP server.
fn hh_command() -> PathBuf {
    std::env::var_os(hh_protocol::CLI_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("hh"))
}

fn copy_button(
    id: impl Into<gpui::ElementId>,
    text: String,
    cx: &mut Context<HhApp>,
) -> AnyElement {
    div()
        .id(id)
        .flex_none()
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
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            cx.stop_propagation();
        }))
        .child("Copy")
        .into_any_element()
}

fn code_block(text: String) -> AnyElement {
    div()
        .min_w(px(0.0))
        .flex_1()
        .font_family("SF Mono")
        .text_xs()
        .text_color(rgb(THEME.muted))
        .bg(rgb(THEME.terminal))
        .border_1()
        .border_color(rgb(THEME.border))
        .rounded(px(6.0))
        .p(px(8.0))
        .child(text)
        .into_any_element()
}

fn note(text: impl Into<gpui::SharedString>) -> AnyElement {
    div()
        .font_family(".SystemUIFont")
        .text_xs()
        .text_color(rgb(THEME.dim))
        .child(text.into())
        .into_any_element()
}

impl HhApp {
    pub(crate) fn render_bots_settings_panel(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        vec![
            settings_heading(
                "Bots",
                "A bot is a coding agent's own interface that you talk to. It opens worker tabs in your workstations, watches them, and tells you when one needs you.",
            ),
            settings_section_title("Default agent"),
            settings_card(self.render_default_agent_rows(cx)),
            settings_section_title("Agent integration"),
            settings_card(self.render_agent_integration_rows(cx)),
            settings_section_title("Terminal agents"),
            self.render_terminal_agents_setting(cx),
        ]
    }

    fn render_default_agent_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let state = &self.coding_agents;
        let default_agent = self.bot_settings().default_agent;
        let mut rows = Vec::new();
        if state.loading {
            rows.push(note("Scanning your login PATH…"));
        } else if let Some(error) = state.error.as_ref() {
            rows.push(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.danger))
                    .child(error.clone())
                    .into_any_element(),
            );
        } else if state.loaded && state.agents.is_empty() {
            rows.push(settings_row(
                "No coding agent CLIs were found on your login PATH",
                Some("Install omp, Claude Code, Codex, Hermes, or another supported agent and click Rescan".to_owned()),
                div().into_any_element(),
            ));
        } else {
            rows.push(
                div()
                    .id("bot-default-agent-auto")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(radio_glyph(default_agent.is_none()))
                    .child(settings_row(
                        "Automatic",
                        Some("omp when installed, else the first installed agent".to_owned()),
                        div().into_any_element(),
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_default_bot_agent(None);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            rows.extend(state.agents.iter().enumerate().map(|(index, agent)| {
                let profile = agent.profile;
                div()
                    .id(("bot-default-agent", index))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(radio_glyph(default_agent == Some(profile)))
                    .child(render_terminal_profile_icon(profile, THEME.muted, 18.0))
                    .child(settings_row(
                        profile.display_name(),
                        Some(agent.path.clone()),
                        div().into_any_element(),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_default_bot_agent(Some(profile));
                        cx.notify();
                    }))
                    .into_any_element()
            }));
        }
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .id("bot-agents-rescan")
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.accent))
                        .child("Rescan")
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_coding_agents(cx))),
                )
                .child(note(
                    "New bots start with this agent. Change a bot's agent from its right-click menu.",
                ))
                .into_any_element(),
        );
        rows
    }

    fn render_agent_integration_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.coding_agents.agents.is_empty() {
            return vec![note(
                "Installed agents appear here once found on your login PATH.",
            )];
        }
        let hh = hh_command();
        let instructions =
            note("Every agent reads its bot instructions from AGENTS.md in the bot's home folder.");
        std::iter::once(instructions)
            .chain(self.coding_agents.agents.iter().enumerate()
            .map(|(index, agent)| {
                let profile = agent.profile;
                let row = div().flex().flex_col().gap(px(6.0)).child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(render_terminal_profile_icon(profile, THEME.muted, 18.0))
                        .child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.foreground))
                                .child(profile.display_name()),
                        ),
                );
                match bot_integration(profile, &hh) {
                    BotIntegration::Plugin => {
                        row.child(note("The Harness Harlot plugin loads automatically."))
                    }
                    BotIntegration::McpAtLaunch => row.child(note(
                        "The Harness Harlot MCP server is attached at launch. It may ask once to trust the bot folder.",
                    )),
                    BotIntegration::SetupCommand(command) => row
                        .child(note("Run this once so bots get the Harness Harlot tools:"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(code_block(command.clone()))
                                .child(copy_button(("copy-bot-setup", index), command, cx)),
                        ),
                    BotIntegration::ManualConfig => row.child(note(
                        "Add the MCP server JSON below to this agent's MCP settings once.",
                    )),
                }
                .into_any_element()
            }))
            .collect()
    }

    fn render_terminal_agents_setting(&self, cx: &mut Context<Self>) -> AnyElement {
        let command = hh_command();
        let config = crate::cli::mcp::server_config(&command);
        let config_text =
            serde_json::to_string_pretty(&config).expect("static MCP configuration is valid");
        let status = self.editor.agent_skill_status.clone().unwrap_or_else(|| {
            "Installs the bundled Harness Harlot skill for Claude Code, Codex, and pi.".to_owned()
        });
        settings_card(vec![
            settings_row(
                "MCP server",
                Some(format!("{} mcp", command.display())),
                copy_button("copy-hh-mcp-config", config_text.clone(), cx),
            ),
            code_block(config_text),
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
}

#[cfg(test)]
mod tests {
    use super::{BotIntegration, bot_integration};
    use hh_protocol::TerminalProfile;
    use std::path::Path;

    #[test]
    fn integration_names_the_exact_one_time_command_and_quotes_the_hh_path() {
        let hh = Path::new("/Applications/Harness Harlot.app/Contents/MacOS/hh");
        assert_eq!(
            bot_integration(TerminalProfile::Hermes, hh),
            BotIntegration::SetupCommand(
                "hermes mcp add harness-harlot --command '/Applications/Harness Harlot.app/Contents/MacOS/hh' --args mcp"
                    .to_owned()
            )
        );
        assert_eq!(
            bot_integration(TerminalProfile::Gemini, Path::new("/usr/local/bin/hh")),
            BotIntegration::SetupCommand(
                "gemini mcp add --scope user harness-harlot /usr/local/bin/hh mcp".to_owned()
            )
        );
        assert_eq!(
            bot_integration(TerminalProfile::Omp, hh),
            BotIntegration::Plugin
        );
        assert_eq!(
            bot_integration(TerminalProfile::Claude, hh),
            BotIntegration::McpAtLaunch
        );
        assert_eq!(
            bot_integration(TerminalProfile::Codex, hh),
            BotIntegration::McpAtLaunch
        );
    }
}
