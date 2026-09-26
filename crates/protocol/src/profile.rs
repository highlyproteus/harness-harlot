//! Stable local terminal profiles and bounded exact detection.

use std::ffi::OsStr;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A stable local terminal profile. The protocol carries only identity, never
/// artwork; the desktop resolves bundled icons from its local asset registry.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalProfile {
    #[default]
    Terminal,
    Hermes,
    Omp,
    Pi,
    Codex,
    Claude,
    Droid,
    KiloCode,
    Cursor,
    OpenCode,
    Aider,
    GitHubCopilot,
    Gemini,
    Amp,
    QwenCode,
    GrokBuild,
    KimiCode,
    Antigravity,
    KiroCli,
    MistralVibe,
    Crush,
    Goose,
    Cline,
    Auggie,
    ContinueCli,
    Tmux,
}

impl TerminalProfile {
    pub const ALL: [Self; 26] = [
        Self::Terminal,
        Self::Hermes,
        Self::Omp,
        Self::Pi,
        Self::Codex,
        Self::Claude,
        Self::Droid,
        Self::KiloCode,
        Self::Cursor,
        Self::OpenCode,
        Self::Aider,
        Self::GitHubCopilot,
        Self::Gemini,
        Self::Amp,
        Self::QwenCode,
        Self::GrokBuild,
        Self::KimiCode,
        Self::Antigravity,
        Self::KiroCli,
        Self::MistralVibe,
        Self::Crush,
        Self::Goose,
        Self::Cline,
        Self::Auggie,
        Self::ContinueCli,
        Self::Tmux,
    ];

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Hermes => "Hermes Agent",
            Self::Omp => "omp",
            Self::Pi => "Pi",
            Self::Codex => "Codex CLI",
            Self::Claude => "Claude Code",
            Self::Droid => "Droid",
            Self::KiloCode => "Kilo Code",
            Self::Cursor => "Cursor",
            Self::OpenCode => "OpenCode",
            Self::Aider => "Aider",
            Self::GitHubCopilot => "GitHub Copilot CLI",
            Self::Gemini => "Gemini CLI",
            Self::Amp => "Amp",
            Self::QwenCode => "Qwen Code",
            Self::GrokBuild => "Grok Build",
            Self::KimiCode => "Kimi Code CLI",
            Self::Antigravity => "Antigravity CLI",
            Self::KiroCli => "Kiro CLI",
            Self::MistralVibe => "Mistral Vibe",
            Self::Crush => "Crush",
            Self::Goose => "goose",
            Self::Cline => "Cline CLI",
            Self::Auggie => "Auggie CLI",
            Self::ContinueCli => "Continue CLI",
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
    /// Exact executable basenames: the commands users type, plus names a
    /// launcher gives its own process (such as a process title).
    pub commands: &'static [&'static str],
    /// Adjacent path components (compared case-insensitively) that identify
    /// the product's official install location, for launches whose process
    /// is a generic interpreter or a renamed binary.
    pub install_locations: &'static [&'static [&'static str]],
    pub terminal_titles: &'static [&'static str],
}

const fn profile(
    profile: TerminalProfile,
    commands: &'static [&'static str],
    install_locations: &'static [&'static [&'static str]],
) -> TerminalProfileDefinition {
    TerminalProfileDefinition {
        profile,
        commands,
        install_locations,
        terminal_titles: &[],
    }
}

/// Local, compile-time registry used for explicit profiles and bounded exact
/// detection. It performs no network access and contains no third-party art.
/// Order is the bot agent preference order.
pub const TERMINAL_PROFILE_REGISTRY: [TerminalProfileDefinition; 25] = [
    profile(
        TerminalProfile::Hermes,
        &["hermes", "hermes-agent"],
        &[&[".hermes", "hermes-agent"], &["cellar", "hermes-agent"]],
    ),
    profile(
        TerminalProfile::Omp,
        &["omp"],
        &[&["node_modules", "@oh-my-pi", "pi-coding-agent"]],
    ),
    profile(
        TerminalProfile::Pi,
        &["pi"],
        &[
            &["node_modules", "@earendil-works", "pi-coding-agent"],
            &["node_modules", "@mariozechner", "pi-coding-agent"],
        ],
    ),
    profile(
        TerminalProfile::Codex,
        &["codex"],
        &[&["node_modules", "@openai", "codex"]],
    ),
    profile(
        TerminalProfile::Claude,
        &["claude"],
        &[
            &["node_modules", "@anthropic-ai", "claude-code"],
            &["share", "claude", "versions"],
        ],
    ),
    profile(TerminalProfile::Droid, &["droid"], &[]),
    profile(
        TerminalProfile::KiloCode,
        &["kilo", "kilocode", ".kilo"],
        &[&["node_modules", "@kilocode", "cli"]],
    ),
    // `agent` is also Cursor's command, but Grok Build ships one too; the
    // bundled runtime's location identifies Cursor instead.
    profile(
        TerminalProfile::Cursor,
        &["cursor-agent"],
        &[&["cursor-agent", "versions"]],
    ),
    profile(
        TerminalProfile::OpenCode,
        &["opencode"],
        &[&["node_modules", "opencode-ai"]],
    ),
    profile(
        TerminalProfile::Aider,
        &["aider"],
        &[
            &["uv", "tools", "aider-chat"],
            &["pipx", "venvs", "aider-chat"],
            &["cellar", "aider"],
        ],
    ),
    profile(
        TerminalProfile::GitHubCopilot,
        &["copilot"],
        &[&["node_modules", "@github", "copilot"]],
    ),
    profile(
        TerminalProfile::Gemini,
        &["gemini"],
        &[&["node_modules", "@google", "gemini-cli"]],
    ),
    profile(
        TerminalProfile::Amp,
        &["amp"],
        &[
            &[".amp", "bin"],
            &["node_modules", "@ampcode", "cli"],
            &["node_modules", "@sourcegraph", "amp"],
        ],
    ),
    profile(
        TerminalProfile::QwenCode,
        &["qwen"],
        &[
            &["node_modules", "@qwen-code", "qwen-code"],
            &["lib", "qwen-code"],
        ],
    ),
    profile(
        TerminalProfile::GrokBuild,
        &["grok"],
        &[&[".grok", "downloads"]],
    ),
    profile(
        TerminalProfile::KimiCode,
        // The Python CLI replaces its command line with the title `Kimi Code`.
        &["kimi", "kimi-code", "kimi-cli", "kimi code"],
        &[
            &[".kimi-code", "bin"],
            &["node_modules", "@moonshot-ai", "kimi-code"],
            &["uv", "tools", "kimi-cli"],
        ],
    ),
    profile(TerminalProfile::Antigravity, &["agy"], &[]),
    profile(
        TerminalProfile::KiroCli,
        &["kiro-cli", "kiro-cli-chat"],
        &[&["kiro cli.app", "contents", "macos"]],
    ),
    profile(
        TerminalProfile::MistralVibe,
        &["vibe", "vibe-rs"],
        &[
            &["uv", "tools", "mistral-vibe"],
            &["cellar", "mistral-vibe"],
        ],
    ),
    profile(
        TerminalProfile::Crush,
        &["crush"],
        &[&["node_modules", "@charmland", "crush"]],
    ),
    profile(TerminalProfile::Goose, &["goose"], &[]),
    profile(
        TerminalProfile::Cline,
        &["cline", ".cline"],
        &[&["node_modules", "cline"]],
    ),
    profile(
        TerminalProfile::Auggie,
        &["auggie"],
        &[&["node_modules", "@augmentcode", "auggie"]],
    ),
    profile(
        TerminalProfile::ContinueCli,
        &["cn"],
        &[&["node_modules", "@continuedev", "cli"]],
    ),
    profile(TerminalProfile::Tmux, &["tmux"], &[]),
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

/// Recognizes a product by its official install location, for launchers that
/// run under a generic interpreter or a renamed binary: the path must contain
/// one of the registry's exact adjacent-component signatures.
pub fn terminal_profile_for_executable(executable: &Path) -> Option<TerminalProfile> {
    let components = executable
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    TERMINAL_PROFILE_REGISTRY.iter().find_map(|definition| {
        definition
            .install_locations
            .iter()
            .any(|signature| {
                components
                    .windows(signature.len())
                    .any(|window| window.iter().zip(*signature).all(|(a, b)| a == b))
            })
            .then_some(definition.profile)
    })
}

/// Recognizes a product from a process's command line when its process name
/// does not: the invoked command (`argv[0]`, which keeps a symlink's name and
/// any process title), or, when that is a script interpreter, the script it
/// runs (its file name without extension, or its install location). Only
/// those two arguments are examined.
pub fn terminal_profile_for_arguments<S: AsRef<OsStr>>(arguments: &[S]) -> Option<TerminalProfile> {
    let invoked = arguments.first()?.as_ref().to_str()?;
    if let Some(profile) = terminal_profile_for_command(invoked) {
        return Some(profile);
    }
    if !is_script_interpreter(invoked.rsplit('/').next().unwrap_or(invoked)) {
        return None;
    }
    let script = arguments[1..]
        .iter()
        .filter_map(|argument| argument.as_ref().to_str())
        .find(|argument| !argument.starts_with('-'))?;
    let file_name = script.rsplit('/').next().unwrap_or(script);
    let stem = [".js", ".mjs", ".cjs", ".ts", ".py"]
        .iter()
        .find_map(|extension| file_name.strip_suffix(extension))
        .unwrap_or(file_name);
    terminal_profile_for_command(stem)
        .or_else(|| terminal_profile_for_executable(Path::new(script)))
}

fn is_script_interpreter(name: &str) -> bool {
    // macOS's framework Python runs as `Python`.
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "node" | "nodejs" | "bun" | "deno" | "python" | "python3"
    ) || name.strip_prefix("python3.").is_some_and(|version| {
        !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
    })
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
            terminal_profile_for_command("/opt/homebrew/bin/omp"),
            Some(TerminalProfile::Omp)
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
    fn install_locations_identify_generic_and_renamed_executables() {
        for (executable, expected) in [
            (
                "/Users/example/.hermes/hermes-agent/venv/bin/python3",
                TerminalProfile::Hermes,
            ),
            (
                "/Users/example/.hermes/hermes-agent/.hermes-runtime/python/build/bin/python3.11",
                TerminalProfile::Hermes,
            ),
            // Cursor's launcher execs its bundled runtime.
            (
                "/Users/example/.local/share/cursor-agent/versions/2025.09.17-25b418f/node",
                TerminalProfile::Cursor,
            ),
            // Symlinked native installs run under the target's file name.
            (
                "/Users/example/.grok/downloads/grok-macos-aarch64",
                TerminalProfile::GrokBuild,
            ),
            (
                "/Users/example/.local/share/claude/versions/2.1.280",
                TerminalProfile::Claude,
            ),
            (
                "/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli-chat",
                TerminalProfile::KiroCli,
            ),
        ] {
            assert_eq!(
                terminal_profile_for_executable(Path::new(executable)),
                Some(expected),
                "executable: {executable}"
            );
        }
        for executable in [
            "/usr/bin/python3",
            "/tmp/hermes-agent/venv/bin/python",
            "/Users/example/.hermes/other-agent/venv/bin/python",
            "/opt/homebrew/bin/node",
            // A partial component is not a signature.
            "/Users/example/.grok-old/downloads/grok",
        ] {
            assert_eq!(
                terminal_profile_for_executable(Path::new(executable)),
                None,
                "executable: {executable}"
            );
        }
    }

    #[test]
    fn command_lines_identify_symlinked_titled_and_interpreted_launches() {
        let detect = |arguments: &[&str]| terminal_profile_for_arguments(arguments);
        // argv[0] keeps the symlink name the user typed, and process titles.
        assert_eq!(
            detect(&["/Users/example/.local/bin/claude", "--resume"]),
            Some(TerminalProfile::Claude)
        );
        assert_eq!(detect(&["pi"]), Some(TerminalProfile::Pi));
        assert_eq!(detect(&["Kimi Code"]), Some(TerminalProfile::KimiCode));
        // An interpreter runs a script named after the command...
        assert_eq!(
            detect(&[
                "node",
                "--no-warnings",
                "/opt/homebrew/bin/gemini",
                "-p",
                "hi"
            ]),
            Some(TerminalProfile::Gemini)
        );
        assert_eq!(
            detect(&[
                "/Users/example/.local/share/uv/tools/kimi-cli/bin/python",
                "/Users/example/.local/bin/kimi"
            ]),
            Some(TerminalProfile::KimiCode)
        );
        // ...or a generic entry point inside the product's package, where the
        // full npm scope tells omp and Pi apart.
        assert_eq!(
            detect(&[
                "node",
                "/usr/lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js"
            ]),
            Some(TerminalProfile::Pi)
        );
        assert_eq!(
            detect(&[
                "bun",
                "/Users/example/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js"
            ]),
            Some(TerminalProfile::Omp)
        );
        // Unrelated scripts, non-interpreters, and ambiguous names stay generic.
        assert_eq!(detect(&["node", "/srv/app/server.js"]), None);
        assert_eq!(detect(&["vim", "/opt/homebrew/bin/gemini"]), None);
        assert_eq!(detect(&["/Users/example/.grok/bin/agent"]), None);
        assert_eq!(detect(&["node", "--inspect"]), None);
        assert_eq!(terminal_profile_for_arguments::<&str>(&[]), None);
    }

    #[test]
    fn every_profile_has_a_full_accessible_product_name_and_neutral_fallback() {
        for profile in TerminalProfile::ALL {
            assert!(!profile.display_name().is_empty());
            assert_eq!(profile.fallback_glyph(), ">_");
        }
    }
}
