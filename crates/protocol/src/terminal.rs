//! Terminal screen state, streaming cursors, and notifications.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::profile::TerminalProfile;
/// Service-owned assistant process state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantStatus {
    Starting,
    Idle,
    Streaming,
    Compacting,
    /// pi exited; restarting respawns it on the same session file.
    Exited {
        message: String,
    },
    /// pi could not be started.
    Unavailable {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantEntry {
    User {
        text: String,
        image_count: u32,
        timestamp_ms: u64,
    },
    Assistant {
        text: String,
        final_: bool,
        timestamp_ms: u64,
    },
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        summary: String,
        output: String,
        done: bool,
        is_error: bool,
        target_pane: Option<Uuid>,
    },
    Notice {
        message: String,
        level: AssistantNoticeLevel,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantNoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssistantApproval {
    pub request_id: String,
    pub title: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssistantThreadView {
    pub pane_id: Uuid,
    pub revision: u64,
    pub status: AssistantStatus,
    pub model: Option<String>,
    pub entries: Vec<AssistantEntry>,
    /// Entries dropped from the front to honour `MAX_ASSISTANT_ENTRIES`.
    pub truncated_entries: u32,
    pub pending_approval: Option<AssistantApproval>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssistantImage {
    pub mime_type: String,
    pub base64: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssistantModel {
    pub provider: String,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CodingAgent {
    pub profile: TerminalProfile,
    /// Command name as found on the login PATH, e.g. "claude".
    pub command: String,
    /// Canonical absolute executable path, for display only.
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserAction {
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload,
    /// Raw Chrome `DevTools` Protocol call executed against the pane's browser.
    DevTools {
        method: String,
        params: serde_json::Value,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BrowserCommandRequest {
    pub request_id: u64,
    pub pane_id: Uuid,
    pub action: BrowserAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserCommandOutcome {
    /// `result` is the CDP `result` object for `DevTools`, `null` for the other actions.
    Ok {
        result: serde_json::Value,
    },
    Error {
        message: String,
    },
}

/// Ephemeral activity state projected by the local session service.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneStatus {
    #[default]
    Idle,
    Working,
    NeedsApproval,
    NeedsInput,
    Attention,
    Done,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalScreen {
    pub pane_id: Uuid,
    pub revision: u64,
    /// Advances only when visible text, dimensions, or viewport offset change;
    /// selection changes advance `revision` alone. Renderers key glyph caches on this.
    pub content_revision: u64,
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
    /// The pane's process has exited: its terminal is frozen and input goes
    /// nowhere. Runtime-only panes (tmux attach, SSH) can be reattached.
    #[serde(default)]
    pub exited: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Completed,
    Attention,
    Message,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionNotification {
    pub id: u64,
    pub pane_id: Uuid,
    pub workspace_id: Uuid,
    pub kind: NotificationKind,
    pub message: Option<String>,
    pub pane_title: String,
    pub workspace_title: String,
    pub profile: TerminalProfile,
    pub at_ms: u64,
    pub read: bool,
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
    /// Number of terminal grid cells occupied by this run. Every producer
    /// populates this field.
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
