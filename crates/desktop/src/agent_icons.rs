use std::borrow::Cow;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use gpui::{AssetSource, SharedString};
use hh_protocol::{TerminalProfile, state_directory};
use uuid::Uuid;

use crate::ui_state::CanonicalPng;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentIconFormat {
    Svg,
    ColorSvg,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomIcon {
    pub id: String,
    pub path: PathBuf,
}

const MAX_CUSTOM_ICON_BYTES: u64 = 5 * 1024 * 1024;
const MAX_CUSTOM_ICON_DIMENSION: u32 = 2_048;
const MAX_CUSTOM_ICON_PIXELS: u64 = 4_194_304;

pub fn load_custom_icons() -> Vec<CustomIcon> {
    let Some(directory) = custom_icon_directory() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut icons = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry.file_type().ok().filter(std::fs::FileType::is_file)?;
            let id = entry.file_name().into_string().ok()?;
            is_valid_custom_icon_id(&id).then(|| CustomIcon {
                id,
                path: entry.path(),
            })
        })
        .collect::<Vec<_>>();
    icons.sort_by(|left, right| left.id.cmp(&right.id));
    icons
}

pub fn import_custom_icon(source: &Path) -> Result<CustomIcon> {
    let extension = supported_custom_icon_extension(source)
        .context("choose a PNG, JPEG, WebP, or GIF image")?;
    let bytes = hh_protocol::read_private_file(source, MAX_CUSTOM_ICON_BYTES)
        .with_context(|| format!("read custom icon from {}", source.display()))?;
    if !matches_custom_icon_format(extension, &bytes) {
        bail!("custom icon contents do not match its file extension");
    }
    let canonical = canonical_custom_icon(&bytes)?;

    let directory =
        custom_icon_directory().context("application state directory is unavailable")?;
    ensure_private_icon_directory(&directory)?;
    let id = format!("{}.png", Uuid::new_v4());
    let path = directory.join(&id);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create custom icon {}", path.display()))?;
    file.write_all(&canonical.png)
        .with_context(|| format!("save custom icon to {}", path.display()))?;
    file.sync_all().context("sync custom icon")?;

    Ok(CustomIcon { id, path })
}

fn canonical_custom_icon(bytes: &[u8]) -> Result<CanonicalPng> {
    crate::ui_state::decode_canonical_png(
        bytes,
        &crate::ui_state::CanonicalPngLimits {
            what: "custom icon",
            max_dimension: MAX_CUSTOM_ICON_DIMENSION,
            max_pixels: MAX_CUSTOM_ICON_PIXELS,
            max_alloc: MAX_CUSTOM_ICON_PIXELS * 4,
        },
    )
}

fn ensure_private_icon_directory(directory: &Path) -> Result<()> {
    hh_protocol::ensure_private_directory(directory)
        .with_context(|| format!("prepare custom icon directory {}", directory.display()))
}

fn custom_icon_directory() -> Option<PathBuf> {
    state_directory().map(|directory| directory.join("icons"))
}

fn supported_custom_icon_extension(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "webp" => Some("webp"),
        "gif" => Some("gif"),
        _ => None,
    }
}

fn is_valid_custom_icon_id(id: &str) -> bool {
    let path = Path::new(id);
    path.file_name().and_then(|name| name.to_str()) == Some(id)
        && supported_custom_icon_extension(path).is_some()
        && id
            .split_once('.')
            .is_some_and(|(stem, _)| Uuid::parse_str(stem).is_ok())
}

fn matches_custom_icon_format(extension: &str, bytes: &[u8]) -> bool {
    match extension {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" => bytes.starts_with(b"\xff\xd8\xff"),
        "webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP",
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        _ => false,
    }
}

/// Single-color mark drawn as a mask in the surrounding text color.
const fn svg(path: &'static str, sha256: &'static str) -> AgentIconAsset {
    AgentIconAsset {
        path,
        format: AgentIconFormat::Svg,
        sha256,
    }
}

/// Artwork whose own colors (or backing tile) carry the mark, drawn as an
/// image; as a mask it would render as a solid shape.
const fn color_svg(path: &'static str, sha256: &'static str) -> AgentIconAsset {
    AgentIconAsset {
        path,
        format: AgentIconFormat::ColorSvg,
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
pub const AGENT_ICON_REGISTRY: [AgentIconDefinition; 26] = [
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
        profile: TerminalProfile::Omp,
        accessible_name: "omp",
        asset: Some(color_svg(
            "agent-icons/omp.svg",
            "9419975a0c24961341221c4cec18703db26a989fa037768f92cda74e3769fe05",
        )),
        notice_key: "omp",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Pi,
        accessible_name: "Pi",
        asset: Some(color_svg(
            "agent-icons/pi.svg",
            "abd66e7868b2d24f0f0895f9237ee8a6dcb22337583b0dc54aeb595acecb4d6b",
        )),
        notice_key: "pi",
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
        asset: Some(png(
            "agent-icons/claude-code.png",
            "c7b5642f810adfba78781592d9dec18d7eb376c7ebf403c4d882fb9d39f65408",
        )),
        notice_key: "claude-code",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Droid,
        accessible_name: "Droid",
        asset: Some(png(
            "agent-icons/droid.png",
            "4a4c7d641f83920af6844367cecb65fea9bd79620e64af6d2ee626ffbd0a6a44",
        )),
        notice_key: "factory-droid",
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
        asset: Some(color_svg(
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
    AgentIconDefinition {
        profile: TerminalProfile::Amp,
        accessible_name: "Amp",
        asset: Some(png(
            "agent-icons/amp.png",
            "fbadb205d29ba1217d954a02412b3c1b7ac1ba09614b5765b596b7db690da833",
        )),
        notice_key: "amp",
    },
    AgentIconDefinition {
        profile: TerminalProfile::QwenCode,
        accessible_name: "Qwen Code",
        asset: Some(color_svg(
            "agent-icons/qwen-code.svg",
            "f7e324607c3f657953977d2742fbb8a3a0a354a13736671d27bfb906e9d1f3f2",
        )),
        notice_key: "qwen-code",
    },
    AgentIconDefinition {
        profile: TerminalProfile::GrokBuild,
        accessible_name: "Grok Build",
        asset: Some(svg(
            "agent-icons/grok-build.svg",
            "b20648e2f111d7fbc91f58b22d1e76e9885b68a163cb5a1010f7f11bf5840491",
        )),
        notice_key: "grok-build",
    },
    AgentIconDefinition {
        profile: TerminalProfile::KimiCode,
        accessible_name: "Kimi Code CLI",
        asset: Some(png(
            "agent-icons/kimi-code-cli.png",
            "dbd00e2ad61ea8832ef0b024662a4a8a5d1b66f0599d5d42e1c9688b9d4cfdf6",
        )),
        notice_key: "kimi-code-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Antigravity,
        accessible_name: "Antigravity CLI",
        asset: Some(png(
            "agent-icons/antigravity-cli.png",
            "8f0b95d2d21dbf930b4d100e2fdc4505673e900a731aa56ea633a4b59c312799",
        )),
        notice_key: "antigravity-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::KiroCli,
        accessible_name: "Kiro CLI",
        asset: Some(png(
            "agent-icons/kiro-cli.png",
            "dcc333999ca9506662e97897d8b14ecd4a9f1eb15085a5b7e4f538915667e559",
        )),
        notice_key: "kiro-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::MistralVibe,
        accessible_name: "Mistral Vibe",
        asset: Some(svg(
            "agent-icons/mistral-vibe.svg",
            "7c0e3122d2be9fc603c4e37956033f13bc8391da7d163e3ffc91ad80a59f40f9",
        )),
        notice_key: "mistral-vibe",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Crush,
        accessible_name: "Crush",
        asset: Some(png(
            "agent-icons/crush.png",
            "2a92e5dafb44b8f9661421db739b4f97a58b10192638558f72effdb200e30c14",
        )),
        notice_key: "crush",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Goose,
        accessible_name: "goose",
        asset: Some(color_svg(
            "agent-icons/goose.svg",
            "b4a9ab0aa1da378732f41c0e00733e7a37c9b97bab368ea6fddc25821ddbefc2",
        )),
        notice_key: "goose",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Cline,
        accessible_name: "Cline CLI",
        asset: Some(color_svg(
            "agent-icons/cline.svg",
            "7ee1785df25a4b4d30609514608a29a2509f8ab33f53b63e2dabcfbff6dc7464",
        )),
        notice_key: "cline-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Auggie,
        accessible_name: "Auggie CLI",
        asset: Some(svg(
            "agent-icons/auggie.svg",
            "dfed39cd6e56aaa0a9d3c04e4eb6384707d677556146eae08f980cba7c1353f3",
        )),
        notice_key: "auggie-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::ContinueCli,
        accessible_name: "Continue CLI",
        asset: Some(color_svg(
            "agent-icons/continue-cli.svg",
            "1ed7c1c8c09f1910be3263dd4ded7b646ead52a464871d8ac6316af48af649b6",
        )),
        notice_key: "continue-cli",
    },
    AgentIconDefinition {
        profile: TerminalProfile::Tmux,
        accessible_name: "tmux",
        asset: Some(svg(
            "agent-icons/tmux.svg",
            "bdc956f2193c3cf4a49d304b76f43d6c6e69670224a45e0422b3e8066de8e358",
        )),
        notice_key: "tmux",
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

const EMBEDDED_ASSETS: [(&str, &[u8]); 27] = [
    (
        "agent-icons/hermes-agent.png",
        include_bytes!("../assets/agent-icons/hermes-agent.png"),
    ),
    (
        "agent-icons/omp.svg",
        include_bytes!("../assets/agent-icons/omp.svg"),
    ),
    (
        "agent-icons/pi.svg",
        include_bytes!("../assets/agent-icons/pi.svg"),
    ),
    (
        "agent-icons/codex-cli.png",
        include_bytes!("../assets/agent-icons/codex-cli.png"),
    ),
    (
        "agent-icons/claude-code.png",
        include_bytes!("../assets/agent-icons/claude-code.png"),
    ),
    (
        "agent-icons/droid.png",
        include_bytes!("../assets/agent-icons/droid.png"),
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
    (
        "agent-icons/amp.png",
        include_bytes!("../assets/agent-icons/amp.png"),
    ),
    (
        "agent-icons/qwen-code.svg",
        include_bytes!("../assets/agent-icons/qwen-code.svg"),
    ),
    (
        "agent-icons/grok-build.svg",
        include_bytes!("../assets/agent-icons/grok-build.svg"),
    ),
    (
        "agent-icons/kimi-code-cli.png",
        include_bytes!("../assets/agent-icons/kimi-code-cli.png"),
    ),
    (
        "agent-icons/antigravity-cli.png",
        include_bytes!("../assets/agent-icons/antigravity-cli.png"),
    ),
    (
        "agent-icons/kiro-cli.png",
        include_bytes!("../assets/agent-icons/kiro-cli.png"),
    ),
    (
        "agent-icons/mistral-vibe.svg",
        include_bytes!("../assets/agent-icons/mistral-vibe.svg"),
    ),
    (
        "agent-icons/crush.png",
        include_bytes!("../assets/agent-icons/crush.png"),
    ),
    (
        "agent-icons/goose.svg",
        include_bytes!("../assets/agent-icons/goose.svg"),
    ),
    (
        "agent-icons/cline.svg",
        include_bytes!("../assets/agent-icons/cline.svg"),
    ),
    (
        "agent-icons/auggie.svg",
        include_bytes!("../assets/agent-icons/auggie.svg"),
    ),
    (
        "agent-icons/continue-cli.svg",
        include_bytes!("../assets/agent-icons/continue-cli.svg"),
    ),
    (
        "agent-icons/tmux.svg",
        include_bytes!("../assets/agent-icons/tmux.svg"),
    ),
    (
        "agent-icons/browser-globe.svg",
        include_bytes!("../assets/agent-icons/browser-globe.svg"),
    ),
    (
        "agent-icons/hammer.svg",
        include_bytes!("../assets/agent-icons/hammer.svg"),
    ),
    (
        "agent-icons/tmux-LICENSE.txt",
        include_bytes!("../assets/agent-icons/tmux-LICENSE.txt"),
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
    use super::{
        AGENT_ICON_REGISTRY, AgentIconAssets, AgentIconFormat, MAX_CUSTOM_ICON_DIMENSION,
        TerminalProfile, agent_icon_definition, canonical_custom_icon, matches_custom_icon_format,
    };
    use gpui::AssetSource;
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    fn png_crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn append_png_chunk(output: &mut Vec<u8>, kind: [u8; 4], payload: &[u8]) {
        output.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        output.extend_from_slice(&kind);
        output.extend_from_slice(payload);
        let mut checksum_input = Vec::with_capacity(kind.len() + payload.len());
        checksum_input.extend_from_slice(&kind);
        checksum_input.extend_from_slice(payload);
        output.extend_from_slice(&png_crc32(&checksum_input).to_be_bytes());
    }

    fn oversized_png_header() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&(MAX_CUSTOM_ICON_DIMENSION + 1).to_be_bytes());
        header.extend_from_slice(&1_u32.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        append_png_chunk(&mut png, *b"IHDR", &header);
        append_png_chunk(&mut png, *b"IEND", &[]);
        png
    }

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

    #[test]
    fn droid_uses_the_unchanged_official_ico_png_resource() {
        let asset = agent_icon_definition(TerminalProfile::Droid)
            .asset
            .expect("Droid official asset");
        assert_eq!(asset.format, AgentIconFormat::Png);
        assert_eq!(asset.path, "agent-icons/droid.png");
        let bytes = AgentIconAssets
            .load(asset.path)
            .expect("embedded asset lookup")
            .expect("Droid asset bytes");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn tmux_uses_the_bundled_official_logomark_and_license() {
        let asset = agent_icon_definition(TerminalProfile::Tmux)
            .asset
            .expect("tmux official asset");
        assert_eq!(asset.format, AgentIconFormat::Svg);
        assert_eq!(asset.path, "agent-icons/tmux.svg");
        assert!(
            AgentIconAssets
                .load("agent-icons/tmux-LICENSE.txt")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn custom_icon_import_accepts_only_matching_bounded_raster_formats() {
        assert!(matches_custom_icon_format(
            "png",
            b"\x89PNG\r\n\x1a\npayload"
        ));
        assert!(matches_custom_icon_format("jpg", b"\xff\xd8\xffpayload"));
        assert!(matches_custom_icon_format(
            "webp",
            b"RIFF\x04\0\0\0WEBPpayload"
        ));
        assert!(matches_custom_icon_format("gif", b"GIF89apayload"));
        assert!(!matches_custom_icon_format("png", b"GIF89apayload"));
    }
    #[test]
    fn custom_icon_decoder_rejects_oversized_dimensions_before_allocation() {
        let error = canonical_custom_icon(&oversized_png_header()).unwrap_err();
        let message = format!("{error:#}").to_ascii_lowercase();
        assert!(message.contains("limit") || message.contains("dimension"));
    }
}
