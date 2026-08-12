use std::borrow::Cow;

use gpui::{AssetSource, SharedString};
use rust_mux_protocol::TerminalProfile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentIconFormat {
    Svg,
    Png,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentIconAsset {
    pub path: &'static str,
    pub format: AgentIconFormat,
    pub sha256: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentIconDefinition {
    pub profile: TerminalProfile,
    pub accessible_name: &'static str,
    pub asset: Option<AgentIconAsset>,
    pub notice_key: &'static str,
}

const fn svg(path: &'static str, sha256: &'static str) -> AgentIconAsset {
    AgentIconAsset {
        path,
        format: AgentIconFormat::Svg,
        sha256,
    }
}

const fn png(path: &'static str, sha256: &'static str) -> AgentIconAsset {
    AgentIconAsset {
        path,
        format: AgentIconFormat::Png,
        sha256,
    }
}

/// Desktop-only icon registry. Assets are compiled into the executable and
/// are never fetched or resolved from the user's environment at runtime.
pub const AGENT_ICON_REGISTRY: [AgentIconDefinition; 10] = [
    AgentIconDefinition {
        profile: TerminalProfile::Terminal,
        accessible_name: "Terminal",
        asset: None,
        notice_key: "built-in-terminal",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Hermes,
        accessible_name: "Hermes Agent",
        asset: Some(png(
            "agent-icons/hermes-agent.png",
            "0cad9cd8f57639ffd60fe1ff2e6cb722bca4fc1bf8e9137068dba4b2f3abc989",
        )),
        notice_key: "hermes-agent",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Codex,
        accessible_name: "Codex CLI",
        asset: Some(png(
            "agent-icons/codex-cli.png",
            "69fb4384e161be8a20dcb94a9ac34aea4fbfaeb67514110a71e7b0732eccb0fc",
        )),
        notice_key: "codex-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Claude,
        accessible_name: "Claude Code",
        asset: Some(svg(
            "agent-icons/claude-code.svg",
            "7651073e8c8e830f99876fa335b3c988cd5ad821378a8994ed6db9a5c2c36345",
        )),
        notice_key: "claude-code",
    },
    AgentIconDefinition {
        profile: TerminalProfile::KiloCode,
        accessible_name: "Kilo Code",
        asset: Some(svg(
            "agent-icons/kilo-code.svg",
            "4f6cdc4a3ed773568f8053e7c112cb4692dcb6d804416b375e27c5ab350d0aa2",
        )),
        notice_key: "kilo-code",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Cursor,
        accessible_name: "Cursor",
        asset: Some(svg(
            "agent-icons/cursor.svg",
            "cd0e3e5d8991a4cdd4577f8896cd063105207665165c73e25a1ff918dd367eb7",
        )),
        notice_key: "cursor",
    },
    AgentIconDefinition {
        profile: TerminalProfile::OpenCode,
        accessible_name: "OpenCode",
        asset: Some(svg(
            "agent-icons/opencode.svg",
            "e29bbe33380ad1c1ada9134b52f229d30e9776d60481512c9d81f2bb6f37def9",
        )),
        notice_key: "opencode",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Aider,
        accessible_name: "Aider",
        asset: Some(png(
            "agent-icons/aider.png",
            "6efbd1fc700f455630b59d233aa37bfc764cffb0bcb255a42e73837f12497a2b",
        )),
        notice_key: "aider",
    },
    AgentIconDefinition {
        profile: TerminalProfile::GitHubCopilot,
        accessible_name: "GitHub Copilot CLI",
        asset: None,
        notice_key: "github-copilot-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Gemini,
        accessible_name: "Gemini CLI",
        asset: Some(png(
            "agent-icons/gemini-cli.png",
            "351e9f5b1bf863d738cd7be4ed040a625a1419450ae7fc490143e4042b7c2438",
        )),
        notice_key: "gemini-cli",
    },
];

pub fn agent_icon_definition(profile: TerminalProfile) -> &'static AgentIconDefinition {
    AGENT_ICON_REGISTRY
        .iter()
        .find(|definition| definition.profile == profile)
        .expect("every terminal profile must have an icon registry entry")
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AgentIconAssets;

const EMBEDDED_ASSETS: [(&str, &[u8]); 8] = [
    (
        "agent-icons/hermes-agent.png",
        include_bytes!("../assets/agent-icons/hermes-agent.png"),
    ),
    (
        "agent-icons/codex-cli.png",
        include_bytes!("../assets/agent-icons/codex-cli.png"),
    ),
    (
        "agent-icons/claude-code.svg",
        include_bytes!("../assets/agent-icons/claude-code.svg"),
    ),
    (
        "agent-icons/kilo-code.svg",
        include_bytes!("../assets/agent-icons/kilo-code.svg"),
    ),
    (
        "agent-icons/cursor.svg",
        include_bytes!("../assets/agent-icons/cursor.svg"),
    ),
    (
        "agent-icons/opencode.svg",
        include_bytes!("../assets/agent-icons/opencode.svg"),
    ),
    (
        "agent-icons/aider.png",
        include_bytes!("../assets/agent-icons/aider.png"),
    ),
    (
        "agent-icons/gemini-cli.png",
        include_bytes!("../assets/agent-icons/gemini-cli.png"),
    ),
];

impl AssetSource for AgentIconAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(EMBEDDED_ASSETS
            .iter()
            .find_map(|(asset_path, bytes)| (*asset_path == path).then_some(Cow::Borrowed(*bytes))))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let prefix = path.trim_end_matches('/');
        Ok(EMBEDDED_ASSETS
            .iter()
            .filter_map(|(asset_path, _)| {
                asset_path
                    .strip_prefix(prefix)
                    .and_then(|suffix| suffix.strip_prefix('/'))
                    .filter(|suffix| !suffix.contains('/'))
                    .map(SharedString::from)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    #[test]
    fn registry_covers_every_profile_with_the_same_accessible_name() {
        assert_eq!(AGENT_ICON_REGISTRY.len(), TerminalProfile::ALL.len());
        for profile in TerminalProfile::ALL {
            let definition = agent_icon_definition(profile);
            assert_eq!(definition.accessible_name, profile.display_name());
            assert!(!definition.notice_key.is_empty());
        }
    }

    #[test]
    fn registered_assets_are_embedded_local_files() {
        let source = AgentIconAssets;
        for definition in AGENT_ICON_REGISTRY {
            let Some(asset) = definition.asset else {
                continue;
            };
            assert!(!asset.path.contains("://"));
            let bytes = source
                .load(asset.path)
                .expect("embedded asset lookup")
                .expect("registered asset bytes");
            assert!(!bytes.is_empty());
            let digest = Sha256::digest(bytes.as_ref());
            let actual = digest.iter().fold(String::new(), |mut output, byte| {
                write!(output, "{byte:02x}").expect("write SHA-256 hex");
                output
            });
            assert_eq!(actual, asset.sha256, "asset changed: {}", asset.path);
        }
    }

    #[test]
    fn copilot_and_terminal_use_the_documented_neutral_fallback() {
        assert_eq!(agent_icon_definition(TerminalProfile::Terminal).asset, None);
        assert_eq!(
            agent_icon_definition(TerminalProfile::GitHubCopilot).asset,
            None
        );
    }
}
