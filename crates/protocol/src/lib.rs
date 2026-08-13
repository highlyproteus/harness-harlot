use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 11;
pub const SOCKET_ENV: &str = "NAH_SOCKET";
pub const STATE_DIR_ENV: &str = "NAH_STATE_DIR";
pub const CONFIG_ENV: &str = "NAH_CONFIG";
pub const PANE_ID_ENV: &str = "NAH_PANE_ID";
/// Marks the separately packaged development desktop build.
///
/// Explicit `NAH_SOCKET`, `NAH_STATE_DIR`, and `NAH_CONFIG` values always
/// override the corresponding Dev defaults, preserving disposable test runs.
pub const DEVELOPMENT_BUILD_ENV: &str = "NAH_DEVELOPMENT_BUILD";
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_SSH_HOST_LEN: usize = 253;
pub const MAX_SSH_INPUT_LEN: usize = MAX_SSH_HOST_LEN + 16;

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
            identity: TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
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
                title: "Workstation 1".to_owned(),
                color: None,
                pinned: false,
                pin_order: 0,
                order: 1,
                active_terminal_count: 1,
                connection: WorkspaceConnection::Local,
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
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub pin_order: u32,
    /// Explicit manual order within the workspace's current pinned group.
    #[serde(default)]
    pub order: u32,
    #[serde(default)]
    pub active_terminal_count: u32,
    #[serde(default)]
    pub connection: WorkspaceConnection,
    pub tabs: Vec<Tab>,
}

/// Ephemeral metadata returned by an explicit tmux scan.
///
/// This is deliberately not part of the desired-state snapshot: a tmux server
/// and its opaque IDs belong to the host running tmux, not to Not a Harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TmuxSession {
    pub id: String,
    pub name: String,
    pub windows: u32,
    pub attached_clients: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TmuxScanScope {
    Local,
    SystemSsh { destination: String },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceConnection {
    #[default]
    Local,
    SystemSsh {
        destination: String,
        status: WorkspaceConnectionStatus,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceConnectionStatus {
    Connected,
    #[default]
    Offline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePinMove {
    Up,
    Down,
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
    /// Ephemeral resolved identity projected by the local session service.
    /// Only explicit overrides below are included in desired-state recovery.
    #[serde(default)]
    pub identity: TerminalIdentity,
    #[serde(default)]
    pub custom_title: Option<String>,
    #[serde(default)]
    pub profile_override: Option<TerminalProfile>,
}

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
}

impl TerminalProfile {
    pub const ALL: [Self; 11] = [
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
pub const TERMINAL_PROFILE_REGISTRY: [TerminalProfileDefinition; 10] = [
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

/// The last terminal revision a receiver has applied for one pane.
///
/// Cursors contain no terminal contents and are safe to include in local
/// performance diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneRevisionCursor {
    pub pane_id: Uuid,
    pub revision: u64,
}

/// Content-free delivery state for one daemon-owned pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneStreamState {
    pub pane_id: Uuid,
    pub revision: u64,
    pub subscribed: bool,
    pub dirty: bool,
}

/// Per-response, content-free measurements for the pane stream hot path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StreamDiagnostics {
    pub panes_considered: u32,
    pub panes_subscribed: u32,
    pub screens_queued: u32,
    pub screens_delivered: u32,
    pub coalesced_revisions: u64,
    pub snapshot_bytes: u64,
    pub screen_bytes: u64,
    pub preparation_micros: u64,
    /// Filled by the desktop after merging a decoded response. The daemon
    /// leaves this at zero.
    pub desktop_apply_micros: u64,
    pub service_cpu_milli_percent: u32,
    pub service_memory_bytes: u64,
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

/// Local terminal history retention. Selecting a finite duration is an
/// explicit opt-in to deleting closed archive sessions after that age.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryRetention {
    Indefinite,
    Days { days: u32 },
}

/// Behavior when the local archive reaches its configured capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryCleanupPolicy {
    /// Stop accepting archive bytes while keeping the terminal itself live.
    PauseWhenFull,
    /// Explicit opt-in to remove the oldest closed sessions first.
    DeleteOldest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistorySettings {
    pub enabled: bool,
    pub retention: HistoryRetention,
    pub quota_bytes: u64,
    pub cleanup_policy: HistoryCleanupPolicy,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            retention: HistoryRetention::Indefinite,
            quota_bytes: 5 * 1024 * 1024 * 1024,
            cleanup_policy: HistoryCleanupPolicy::PauseWhenFull,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryWarning {
    ApproachingCapacity,
    PausedAtCapacity,
    QueueOverflow,
    CorruptChunk,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryArchiveStatus {
    pub settings: HistorySettings,
    pub live_scrollback_lines: u32,
    pub archived_bytes: u64,
    pub retained_sessions: u32,
    pub oldest_started_ms: Option<u64>,
    pub dropped_bytes: u64,
    pub warning: Option<HistoryWarning>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryClearScope {
    Terminal { pane_id: Uuid },
    Workspace { workspace_id: Uuid },
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub session_id: Uuid,
    pub chunk_index: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryPageDirection {
    Older,
    Newer,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HistoryPageFlags {
    bits: u8,
}

impl HistoryPageFlags {
    pub const HAS_OLDER: u8 = 1 << 0;
    pub const HAS_NEWER: u8 = 1 << 1;
    pub const GAP_BEFORE: u8 = 1 << 2;
    pub const GAP_AFTER: u8 = 1 << 3;
    pub const CORRUPT: u8 = 1 << 4;

    pub const fn new(bits: u8) -> Self {
        Self { bits }
    }

    pub const fn contains(self, flag: u8) -> bool {
        self.bits & flag != 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalHistoryPage {
    pub pane_id: Uuid,
    pub cursor: HistoryCursor,
    pub started_ms: u64,
    pub lines: Vec<String>,
    pub flags: HistoryPageFlags,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    Hello {
        protocol_version: u16,
    },
    GetSnapshot,
    GetUpdates {
        snapshot_revision: Option<u64>,
        pane_revisions: Vec<PaneRevisionCursor>,
        subscribed_panes: Vec<Uuid>,
    },
    GetPaneSnapshot {
        pane_id: Uuid,
    },
    CreatePane {
        target_pane: Uuid,
        axis: SplitAxis,
    },
    CreateTab {
        target_pane: Uuid,
    },
    CreateWorkspaceTerminal {
        workspace_id: Uuid,
    },
    ConnectSsh {
        target_pane: Uuid,
        host: String,
    },
    /// Explicitly reads bounded tmux session metadata for one workstation.
    ScanTmuxSessions {
        workspace_id: Uuid,
    },
    /// Opens one runtime-only tab attached to an existing tmux session.
    AttachTmuxSession {
        workspace_id: Uuid,
        session_id: String,
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
    SetPaneProfile {
        pane_id: Uuid,
        profile: Option<TerminalProfile>,
    },
    ResetPaneIdentity {
        pane_id: Uuid,
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
    CreateSshWorkspace {
        title: Option<String>,
        destination: String,
    },
    RenameWorkspace {
        workspace_id: Uuid,
        title: String,
    },
    SetWorkspacePinned {
        workspace_id: Uuid,
        pinned: bool,
    },
    MovePinnedWorkspace {
        workspace_id: Uuid,
        direction: WorkspacePinMove,
    },
    ReorderWorkspace {
        workspace_id: Uuid,
        target_workspace_id: Uuid,
        after: bool,
    },
    DisconnectWorkspace {
        workspace_id: Uuid,
    },
    ReconnectWorkspace {
        workspace_id: Uuid,
    },
    DeleteWorkspace {
        workspace_id: Uuid,
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
    GetHistoryStatus,
    SetHistorySettings {
        settings: HistorySettings,
    },
    ClearHistory {
        scope: HistoryClearScope,
    },
    LoadHistoryPage {
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    },
    SearchArchivedHistory {
        pane_id: Uuid,
        query: String,
        before: Option<HistoryCursor>,
    },
}

/// Normalizes the single OpenSSH destination accepted from the desktop UI.
///
/// A user may enter a bare `[user@]host` destination or paste the exact command
/// form `ssh [user@]host`. Not a Harness strips only that known executable token;
/// options, extra commands, shell syntax, and other executables remain outside
/// this boundary. OpenSSH remains responsible for resolving normal config and
/// agent behavior after the normalized destination is validated.
///
/// # Errors
///
/// Returns a user-facing validation message when the input is empty, too long,
/// contains control characters, or is not one of the two accepted forms.
pub fn normalize_ssh_input(input: &str) -> Result<String, &'static str> {
    if input.len() > MAX_SSH_INPUT_LEN {
        return Err("SSH destination or command is too long");
    }
    let input = input.trim();
    if input.is_empty() {
        return Err("SSH host, alias, or command is required");
    }
    if input.chars().any(char::is_control) {
        return Err("SSH destination or command may not contain control characters");
    }
    let parts = input.split_ascii_whitespace().collect::<Vec<_>>();
    let destination = match parts.as_slice() {
        [destination] | ["ssh" | "/usr/bin/ssh" | "/bin/ssh", destination] => *destination,
        _ => {
            return Err(
                "Enter one destination or paste `ssh <destination>` without options or extra commands",
            );
        }
    };
    validate_ssh_host(destination)?;
    Ok(destination.to_owned())
}

/// Validates the normalized OpenSSH destination sent to the session service.
///
/// Not a Harness deliberately accepts only a conservative `[user@]host` or SSH
/// config `Host` alias subset. Option prefixes, ports, commands, shell syntax,
/// whitespace, and control characters are not part of this value.
///
/// # Errors
///
/// Returns a user-facing validation message when `host` is empty, too long, or
/// contains anything outside the accepted destination subset.
pub fn validate_ssh_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("SSH host, alias, or destination is required");
    }
    if host.len() > MAX_SSH_HOST_LEN {
        return Err("SSH destination is too long");
    }
    let (user, host) = match host.split_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() && !host.contains('@') => {
            (Some(user), host)
        }
        Some(_) => return Err("SSH destination must contain at most one non-empty `user@` prefix"),
        None => (None, host),
    };
    if let Some(user) = user
        && !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("SSH user may contain only letters, numbers, dots, underscores, and hyphens");
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
    Updates {
        session_revision: u64,
        snapshot: Option<SessionSnapshot>,
        screens: Vec<TerminalScreen>,
        pane_states: Vec<PaneStreamState>,
        diagnostics: StreamDiagnostics,
    },
    PaneSnapshot {
        screen: TerminalScreen,
        diagnostics: StreamDiagnostics,
    },
    PaneCreated {
        pane_id: Uuid,
    },
    WorkspaceCreated {
        workspace_id: Uuid,
        pane_id: Uuid,
    },
    TmuxSessions {
        scope: TmuxScanScope,
        sessions: Vec<TmuxSession>,
        no_server: bool,
    },
    Ack,
    SelectionText {
        text: Option<String>,
    },
    SearchResult {
        found: bool,
    },
    HistoryStatus {
        status: HistoryArchiveStatus,
    },
    HistoryPage {
        page: Option<TerminalHistoryPage>,
    },
    HistorySearchResult {
        page: Option<TerminalHistoryPage>,
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
    std::env::var_os(SOCKET_ENV)
        .map_or_else(|| default_socket_path(development_build()), PathBuf::from)
}

fn default_socket_path(development_build: bool) -> PathBuf {
    let filename = if development_build {
        "nah-dev-session.sock"
    } else {
        "nah-session.sock"
    };
    std::env::temp_dir().join(filename)
}

/// Returns the owner-only Not a Harness state directory.
pub fn state_directory() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os(STATE_DIR_ENV) {
        return Some(PathBuf::from(directory));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let xdg_state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    Some(default_state_directory(
        &home,
        xdg_state_home.as_deref(),
        development_build(),
    ))
}

fn default_state_directory(
    home: &Path,
    xdg_state_home: Option<&Path>,
    development_build: bool,
) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = xdg_state_home;
        let product_directory = if development_build {
            "Not a Harness Dev"
        } else {
            "Not a Harness"
        };
        home.join("Library/Application Support")
            .join(product_directory)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let fallback = home.join(".local/state");
        let base = xdg_state_home.unwrap_or(&fallback);
        base.join(if development_build { "nah-dev" } else { "nah" })
    }
}

/// Returns the optional Not a Harness desktop configuration file.
pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CONFIG_ENV) {
        return Some(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(default_config_path(&base, development_build()))
}

fn default_config_path(base: &Path, development_build: bool) -> PathBuf {
    let product_directory = if development_build { "nah-dev" } else { "nah" };
    base.join(product_directory).join("config.json")
}

fn development_build() -> bool {
    std::env::var(DEVELOPMENT_BUILD_ENV).as_deref() == Ok("1")
}

/// Child terminals receive this stable pane identifier.
pub const fn pane_id_env() -> &'static str {
    PANE_ID_ENV
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
    fn runtime_interface_uses_only_the_nah_prefix() {
        assert_eq!(SOCKET_ENV, "NAH_SOCKET");
        assert_eq!(STATE_DIR_ENV, "NAH_STATE_DIR");
        assert_eq!(CONFIG_ENV, "NAH_CONFIG");
        assert_eq!(DEVELOPMENT_BUILD_ENV, "NAH_DEVELOPMENT_BUILD");
        assert_eq!(pane_id_env(), "NAH_PANE_ID");
        assert!(default_socket_path(false).ends_with("nah-session.sock"));
    }

    #[test]
    fn development_build_defaults_are_isolated_from_stable() {
        assert!(default_socket_path(true).ends_with("nah-dev-session.sock"));
        assert_ne!(default_socket_path(false), default_socket_path(true));

        let home = PathBuf::from("/Users/example");
        assert_ne!(
            default_state_directory(&home, None, false),
            default_state_directory(&home, None, true)
        );
        let config_home = home.join(".config");
        assert_ne!(
            default_config_path(&config_home, false),
            default_config_path(&config_home, true)
        );
    }

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
        assert_eq!(snapshot.workspaces[0].title, "Workstation 1");
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
        assert_eq!(pane.identity, TerminalIdentity::default());
        assert_eq!(pane.custom_title, None);
        assert_eq!(pane.profile_override, None);
    }

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

    #[test]
    fn ssh_host_validation_accepts_conservative_config_aliases() {
        for host in [
            "build",
            "build-01",
            "prod_us",
            "host.example.com",
            "192.0.2.10",
            "admin@build-01",
            "tailscale_user@host.tailnet-name.ts.net",
        ] {
            assert_eq!(validate_ssh_host(host), Ok(()), "host: {host}");
        }
    }

    #[test]
    fn ssh_input_normalizes_bare_destinations_and_exact_system_ssh_commands() {
        for (input, destination) in [
            ("build", "build"),
            (" admin@build-01\n", "admin@build-01"),
            ("ssh prod_us", "prod_us"),
            (
                "/usr/bin/ssh admin@host.example.com",
                "admin@host.example.com",
            ),
            ("/bin/ssh 192.0.2.10", "192.0.2.10"),
        ] {
            assert_eq!(normalize_ssh_input(input), Ok(destination.to_owned()));
        }
    }

    #[test]
    fn ssh_input_rejects_options_extra_commands_and_other_executables() {
        for input in [
            "ssh -A build",
            "ssh -p 22 build",
            "ssh build command",
            "tailscale ssh build",
            "env ssh build",
            "ssh build;bad",
            "ssh\nbuild",
        ] {
            assert!(normalize_ssh_input(input).is_err(), "input: {input:?}");
        }
    }

    #[test]
    fn ssh_host_validation_rejects_option_command_and_shell_injection() {
        for host in [
            "",
            "-A",
            "user@@host",
            "user@",
            "@host",
            "user name@host",
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
