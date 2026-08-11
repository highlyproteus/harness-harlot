use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 6;
pub const SOCKET_ENV: &str = "RUST_MUX_SOCKET";
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_SSH_HOST_LEN: usize = 253;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub revision: u64,
    #[serde(default)]
    pub appearance: AppearanceSettings,
    pub workspaces: Vec<Workspace>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct AppearanceColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl AppearanceColor {
    pub const HARBOR_BLUE: Self = Self::new(0x62, 0xad, 0xff);

    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub const fn as_rgb(self) -> u32 {
        ((self.red as u32) << 16) | ((self.green as u32) << 8) | self.blue as u32
    }
}

impl Default for AppearanceColor {
    fn default() -> Self {
        Self::HARBOR_BLUE
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppearanceSettings {
    #[serde(default)]
    pub default_terminal_accent: AppearanceColor,
    #[serde(default)]
    pub default_workspace_color: AppearanceColor,
    #[serde(default)]
    pub recent_colors: Vec<AppearanceColor>,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            default_terminal_accent: AppearanceColor::HARBOR_BLUE,
            default_workspace_color: AppearanceColor::HARBOR_BLUE,
            recent_colors: Vec::new(),
        }
    }
}

impl SessionSnapshot {
    pub fn seeded() -> Self {
        let pane = Pane {
            id: Uuid::new_v4(),
            title: "Terminal 1".to_owned(),
            shell: "shell".to_owned(),
            color: None,
        };
        let tab = Tab {
            id: Uuid::new_v4(),
            title: "Shell".to_owned(),
            layout: PaneLayout::Leaf { pane },
        };

        Self {
            revision: 0,
            appearance: AppearanceSettings::default(),
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                title: "Workspace 1".to_owned(),
                color: None,
                tabs: vec![tab],
            }],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub title: String,
    #[serde(default)]
    pub color: Option<AppearanceColor>,
    pub tabs: Vec<Tab>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: Uuid,
    pub title: String,
    pub layout: PaneLayout,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneLayout {
    Leaf {
        pane: Pane,
    },
    Stack {
        panes: Vec<Pane>,
        active: Uuid,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: Uuid,
    pub title: String,
    pub shell: String,
    #[serde(default)]
    pub color: Option<AppearanceColor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalScreen {
    pub pane_id: Uuid,
    pub revision: u64,
    pub columns: u16,
    pub rows: u16,
    pub lines: Vec<TerminalLine>,
    pub cursor: Option<TerminalCursor>,
    pub selection: Option<TerminalSelection>,
    pub display_offset: u32,
    pub history_size: u32,
    pub modes: TerminalModes,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalModes {
    bits: u8,
}

impl TerminalModes {
    pub const BRACKETED_PASTE: u8 = 1 << 0;
    pub const MOUSE_REPORTING: u8 = 1 << 1;
    pub const MOUSE_MOTION: u8 = 1 << 2;
    pub const SGR_MOUSE: u8 = 1 << 3;

    pub const fn new(bits: u8) -> Self {
        Self { bits }
    }

    pub const fn contains(self, mode: u8) -> bool {
        self.bits & mode != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalSelection {
    pub start: TerminalPoint,
    pub end: TerminalPoint,
    pub is_block: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalPoint {
    pub row: u16,
    pub column: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSelectionKind {
    Simple,
    Block,
    Semantic,
    Lines,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMouseAction {
    Press,
    Release,
    Move,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalLine {
    pub runs: Vec<TerminalRun>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalRun {
    pub text: String,
    /// Number of terminal grid cells occupied by this run. Older protocol-v4
    /// peers omit this field, so renderers must fall back to the text width
    /// when it is zero.
    #[serde(default)]
    pub columns: u16,
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub attributes: TerminalAttributes,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalAttributes {
    bits: u8,
}

impl TerminalAttributes {
    pub const BOLD: u8 = 1 << 0;
    pub const DIM: u8 = 1 << 1;
    pub const ITALIC: u8 = 1 << 2;
    pub const UNDERLINE: u8 = 1 << 3;
    pub const STRIKETHROUGH: u8 = 1 << 4;

    pub const fn new(bits: u8) -> Self {
        Self { bits }
    }

    pub const fn contains(self, attribute: u8) -> bool {
        self.bits & attribute != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalCursor {
    pub row: u16,
    pub column: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalColor {
    DefaultForeground,
    DefaultBackground,
    Ansi { index: u8 },
    Indexed { index: u8 },
    Rgb { red: u8, green: u8, blue: u8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropPlacement {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    Hello {
        protocol_version: u16,
    },
    GetSnapshot,
    GetState,
    CreatePane {
        target_pane: Uuid,
        axis: SplitAxis,
    },
    CreateTab {
        target_pane: Uuid,
    },
    ConnectSsh {
        target_pane: Uuid,
        host: String,
    },
    ActivateTab {
        pane_id: Uuid,
    },
    SwapPanes {
        source_pane: Uuid,
        target_pane: Uuid,
    },
    MovePaneToSplit {
        source_pane: Uuid,
        target_pane: Uuid,
        placement: DropPlacement,
    },
    MovePaneToTab {
        source_pane: Uuid,
        target_pane: Uuid,
    },
    RenamePane {
        pane_id: Uuid,
        title: String,
    },
    ClosePane {
        pane_id: Uuid,
    },
    SetDefaultTerminalAccent {
        color: AppearanceColor,
    },
    SetDefaultWorkspaceColor {
        color: AppearanceColor,
    },
    SetPaneColor {
        pane_id: Uuid,
        color: Option<AppearanceColor>,
    },
    SetWorkspaceColor {
        workspace_id: Uuid,
        color: Option<AppearanceColor>,
    },
    CreateWorkspace {
        title: Option<String>,
    },
    WriteInput {
        pane_id: Uuid,
        bytes: Vec<u8>,
    },
    BeginSelection {
        pane_id: Uuid,
        point: TerminalPoint,
        kind: TerminalSelectionKind,
    },
    UpdateSelection {
        pane_id: Uuid,
        point: TerminalPoint,
    },
    ClearSelection {
        pane_id: Uuid,
    },
    CopySelection {
        pane_id: Uuid,
    },
    ScrollPane {
        pane_id: Uuid,
        lines: i32,
    },
    SearchPane {
        pane_id: Uuid,
        query: String,
        forward: bool,
    },
    MouseInput {
        pane_id: Uuid,
        point: TerminalPoint,
        button: TerminalMouseButton,
        action: TerminalMouseAction,
        modifiers: TerminalModifiers,
    },
    ResizePane {
        pane_id: Uuid,
        columns: u16,
        rows: u16,
    },
}

/// Validates the single OpenSSH destination accepted from the desktop UI.
///
/// Rust Mux deliberately accepts only a conservative host or `Host` alias
/// subset. In particular, option prefixes, usernames, ports, commands, shell
/// syntax, whitespace, and control characters are not part of this value.
/// OpenSSH remains responsible for resolving all connection configuration.
///
/// # Errors
///
/// Returns a user-facing validation message when `host` is empty, too long,
/// starts like an option, or contains anything outside the accepted subset.
pub fn validate_ssh_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("SSH host or alias is required");
    }
    if host.len() > MAX_SSH_HOST_LEN {
        return Err("SSH host or alias is too long");
    }
    let mut bytes = host.bytes();
    let Some(first) = bytes.next() else {
        return Err("SSH host or alias is required");
    };
    if !first.is_ascii_alphanumeric() {
        return Err("SSH host or alias must start with a letter or number");
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
        return Err(
            "SSH host or alias may contain only letters, numbers, dots, underscores, and hyphens",
        );
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceResponse {
    Hello {
        protocol_version: u16,
    },
    Snapshot {
        snapshot: SessionSnapshot,
    },
    State {
        snapshot: SessionSnapshot,
        screens: Vec<TerminalScreen>,
    },
    PaneCreated {
        pane_id: Uuid,
    },
    WorkspaceCreated {
        workspace_id: Uuid,
        pane_id: Uuid,
    },
    Ack,
    SelectionText {
        text: Option<String>,
    },
    SearchResult {
        found: bool,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum WireError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("peer closed the connection")]
    Closed,
    #[error("frame is too large: {0} bytes")]
    FrameTooLarge(usize),
}

pub fn socket_path() -> PathBuf {
    std::env::var_os(SOCKET_ENV).map_or_else(
        || std::env::temp_dir().join("rust-mux-session.sock"),
        PathBuf::from,
    )
}

/// Writes one length-prefixed JSON message and flushes it to the peer.
///
/// # Errors
///
/// Returns [`WireError::Json`] when serialization fails and [`WireError::Io`]
/// when the encoded message cannot be written or flushed.
pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<(), WireError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge(payload.len()));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| WireError::FrameTooLarge(payload.len()))?
        .to_be_bytes();
    writer.write_all(&length)?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Reads and decodes one length-prefixed JSON message.
///
/// # Errors
///
/// Returns [`WireError::Closed`] when the peer closes before another message,
/// [`WireError::Io`] when reading fails, and [`WireError::Json`] when the line
/// is not a valid message of the requested type.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<T, WireError> {
    let mut length = [0_u8; 4];
    if let Err(error) = reader.read_exact(&mut length) {
        return if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Err(WireError::Closed)
        } else {
            Err(WireError::Io(error))
        };
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn messages_round_trip_as_length_prefixed_json() {
        let request = ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        };
        let mut bytes = Vec::new();

        write_message(&mut bytes, &request).unwrap();
        let decoded: ClientRequest = read_message(&mut Cursor::new(bytes)).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn seeded_snapshot_has_a_visible_pane() {
        let snapshot = SessionSnapshot::seeded();
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
        assert!(matches!(
            snapshot.workspaces[0].tabs[0].layout,
            PaneLayout::Leaf { .. }
        ));
    }

    #[test]
    fn older_snapshot_without_appearance_fields_uses_harbor_defaults() {
        let snapshot: SessionSnapshot = serde_json::from_str(
            r#"{
                "revision": 3,
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000001",
                    "title": "Old workspace",
                    "tabs": [{
                        "id": "00000000-0000-0000-0000-000000000002",
                        "title": "Shell",
                        "layout": {
                            "kind": "leaf",
                            "pane": {
                                "id": "00000000-0000-0000-0000-000000000003",
                                "title": "Terminal 1",
                                "shell": "zsh"
                            }
                        }
                    }]
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(snapshot.appearance, AppearanceSettings::default());
        assert_eq!(snapshot.workspaces[0].color, None);
        let PaneLayout::Leaf { pane } = &snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected leaf");
        };
        assert_eq!(pane.color, None);
    }

    #[test]
    fn ssh_host_validation_accepts_conservative_config_aliases() {
        for host in [
            "build",
            "build-01",
            "prod_us",
            "host.example.com",
            "192.0.2.10",
        ] {
            assert_eq!(validate_ssh_host(host), Ok(()), "host: {host}");
        }
    }

    #[test]
    fn ssh_host_validation_rejects_option_command_and_shell_injection() {
        for host in [
            "",
            "-A",
            "user@host",
            "host:22",
            "host command",
            "host\nProxyCommand=bad",
            "host;bad",
            "*.example.com",
            "café",
        ] {
            assert!(validate_ssh_host(host).is_err(), "host: {host:?}");
        }
    }
}
