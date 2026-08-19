//! Stable local terminal profiles and bounded exact detection.

use serde::{Deserialize, Serialize};

/// A stable local terminal profile. The protocol carries only identity, never
/// artwork; the desktop resolves bundled icons from its local asset registry.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalProfile {
    #[default]
    Terminal,
    Hermes,
    Codex,
    Claude,
    Droid,
    KiloCode,
    Cursor,
    OpenCode,
    Aider,
    GitHubCopilot,
    Gemini,
    Tmux,
}

impl TerminalProfile {
    pub const ALL: [Self; 12] = [
        Self::Terminal,
        Self::Hermes,
        Self::Codex,
        Self::Claude,
        Self::Droid,
        Self::KiloCode,
        Self::Cursor,
        Self::OpenCode,
        Self::Aider,
        Self::GitHubCopilot,
        Self::Gemini,
        Self::Tmux,
    ];

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Hermes => "Hermes Agent",
            Self::Codex => "Codex CLI",
            Self::Claude => "Claude Code",
            Self::Droid => "Droid",
            Self::KiloCode => "Kilo Code",
            Self::Cursor => "Cursor",
            Self::OpenCode => "OpenCode",
            Self::Aider => "Aider",
            Self::GitHubCopilot => "GitHub Copilot CLI",
            Self::Gemini => "Gemini CLI",
            Self::Tmux => "tmux",
        }
    }

    /// Neutral fallback used only when no official bundled product asset is
    /// available. Full product labels remain visible beside this glyph.
    pub const fn fallback_glyph(self) -> &'static str {
        ">_"
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalIdentitySource {
    UserRename,
    UserProfile,
    TerminalTitle,
    Command,
    #[default]
    Fallback,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalIdentity {
    pub profile: TerminalProfile,
    pub source: TerminalIdentitySource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalProfileDefinition {
    pub profile: TerminalProfile,
    pub commands: &'static [&'static str],
    pub terminal_titles: &'static [&'static str],
}

/// Local, compile-time registry used for explicit profiles and bounded exact
/// detection. It performs no network access and contains no third-party art.
pub const TERMINAL_PROFILE_REGISTRY: [TerminalProfileDefinition; 11] = [
    TerminalProfileDefinition {
        profile: TerminalProfile::Hermes,
        commands: &["hermes", "hermes-agent"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::Codex,
        commands: &["codex"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::Claude,
        commands: &["claude"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::Droid,
        commands: &["droid"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::KiloCode,
        commands: &["kilo", "kilocode"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::Cursor,
        commands: &["cursor-agent"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::OpenCode,
        commands: &["opencode"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::Aider,
        commands: &["aider"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::GitHubCopilot,
        commands: &["copilot"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::Gemini,
        commands: &["gemini"],
        terminal_titles: &[],
    },
    TerminalProfileDefinition {
        profile: TerminalProfile::Tmux,
        commands: &["tmux"],
        terminal_titles: &[],
    },
];

pub fn terminal_profile_for_command(command: &str) -> Option<TerminalProfile> {
    let command = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let command = command
        .get(..command.len().saturating_sub(4))
        .filter(|_| {
            command
                .get(command.len().saturating_sub(4)..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
        })
        .unwrap_or(command);
    TERMINAL_PROFILE_REGISTRY.iter().find_map(|definition| {
        definition
            .commands
            .iter()
            .any(|known| command.eq_ignore_ascii_case(known))
            .then_some(definition.profile)
    })
}

/// Recognizes stable executable-location signatures for launchers that replace
/// their product name with a generic interpreter process.
///
/// This intentionally accepts only the official Hermes Agent installation
/// namespace and a Python runtime leaf. It does not inspect arguments,
/// environment variables, working directories, terminal content, or files.
pub fn terminal_profile_for_executable(executable: &std::path::Path) -> Option<TerminalProfile> {
    let components = executable
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let leaf = components.last()?;
    let python_runtime = matches!(leaf.as_str(), "python" | "python3")
        || leaf.strip_prefix("python3.").is_some_and(|version| {
            !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
        });
    if !python_runtime {
        return None;
    }
    components
        .windows(2)
        .any(|window| window == [".hermes", "hermes-agent"])
        .then_some(TerminalProfile::Hermes)
}

pub fn terminal_profile_for_title(title: &str) -> Option<TerminalProfile> {
    if title.chars().count() > 80 || title.chars().any(char::is_control) {
        return None;
    }
    let normalized = title.trim().to_ascii_lowercase();
    TERMINAL_PROFILE_REGISTRY.iter().find_map(|definition| {
        definition
            .terminal_titles
            .contains(&normalized.as_str())
            .then_some(definition.profile)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_profile_registry_maps_known_commands_titles_and_unknown_fallbacks() {
        assert_eq!(
            terminal_profile_for_command("/opt/homebrew/bin/hermes"),
            Some(TerminalProfile::Hermes)
        );
        assert_eq!(
            terminal_profile_for_command("CODEX.EXE"),
            Some(TerminalProfile::Codex)
        );
        assert_eq!(
            terminal_profile_for_command("/Users/example/.local/bin/droid"),
            Some(TerminalProfile::Droid)
        );
        assert_eq!(
            terminal_profile_for_command("/usr/local/bin/kilocode"),
            Some(TerminalProfile::KiloCode)
        );
        assert_eq!(
            terminal_profile_for_command("cursor-agent"),
            Some(TerminalProfile::Cursor)
        );
        assert_eq!(
            terminal_profile_for_command("opencode"),
            Some(TerminalProfile::OpenCode)
        );
        assert_eq!(
            terminal_profile_for_command("aider"),
            Some(TerminalProfile::Aider)
        );
        assert_eq!(
            terminal_profile_for_command("copilot"),
            Some(TerminalProfile::GitHubCopilot)
        );
        assert_eq!(
            terminal_profile_for_command("gemini"),
            Some(TerminalProfile::Gemini)
        );
        assert_eq!(
            terminal_profile_for_command("tmux"),
            Some(TerminalProfile::Tmux)
        );
        assert_eq!(terminal_profile_for_command("vim"), None);
        assert_eq!(terminal_profile_for_command("chatgpt"), None);
        assert_eq!(terminal_profile_for_command("agent"), None);
        assert_eq!(terminal_profile_for_title("Claude Code"), None);
        assert_eq!(terminal_profile_for_title("fix claude code docs"), None);
    }

    #[test]
    fn hermes_interpreter_detection_is_limited_to_the_official_install_namespace() {
        for executable in [
            "/Users/example/.hermes/hermes-agent/venv/bin/python",
            "/Users/example/.hermes/hermes-agent/venv/bin/python3",
            "/Users/example/.hermes/hermes-agent/.hermes-runtime/python/build/bin/python3.11",
        ] {
            assert_eq!(
                terminal_profile_for_executable(std::path::Path::new(executable)),
                Some(TerminalProfile::Hermes),
                "executable: {executable}"
            );
        }
        for executable in [
            "/usr/bin/python3",
            "/tmp/hermes-agent/venv/bin/python",
            "/Users/example/.hermes/other-agent/venv/bin/python",
            "/Users/example/.hermes/hermes-agent/venv/bin/node",
        ] {
            assert_eq!(
                terminal_profile_for_executable(std::path::Path::new(executable)),
                None,
                "executable: {executable}"
            );
        }
    }

    #[test]
    fn every_profile_has_a_full_accessible_product_name_and_neutral_fallback() {
        for profile in TerminalProfile::ALL {
            assert!(!profile.display_name().is_empty());
            assert_eq!(profile.fallback_glyph(), ">_");
        }
    }
}
