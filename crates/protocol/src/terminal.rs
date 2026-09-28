//! Terminal screen state, streaming cursors, and notifications.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::profile::TerminalProfile;
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

/// Agent that reported a pane's task progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressSource {
    Omp,
    Claude,
    Codex,
}

/// Largest task count a progress report may carry.
pub const MAX_PROGRESS_TASKS: u32 = 1_000;
/// Largest character count of a progress report's `current` or `phase` text.
pub const MAX_PROGRESS_TEXT_CHARS: usize = 200;

/// Task-list progress an agent reported for its pane: `done` of `total`
/// tasks complete. Abandoned tasks are excluded from `total`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneProgress {
    pub done: u32,
    pub total: u32,
    /// The task in progress, if any.
    #[serde(default)]
    pub current: Option<String>,
    /// The phase holding the current (or first unfinished) task, if the
    /// agent groups tasks into phases.
    #[serde(default)]
    pub phase: Option<String>,
    pub source: ProgressSource,
}

impl PaneProgress {
    /// Completed fraction in `0.0..=1.0`; an empty list counts as complete.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            1.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let fraction = self.done as f32 / self.total as f32;
            fraction.clamp(0.0, 1.0)
        }
    }

    /// Checks the bounds every progress report must satisfy.
    ///
    /// # Errors
    ///
    /// Returns a description of the first violated bound.
    pub fn validate(&self) -> Result<(), String> {
        if self.total > MAX_PROGRESS_TASKS {
            return Err(format!("progress total exceeds {MAX_PROGRESS_TASKS}"));
        }
        if self.done > self.total {
            return Err("progress done exceeds total".to_owned());
        }
        for text in [&self.current, &self.phase].into_iter().flatten() {
            if text.chars().count() > MAX_PROGRESS_TEXT_CHARS {
                return Err(format!(
                    "progress text exceeds {MAX_PROGRESS_TEXT_CHARS} characters"
                ));
            }
            if text.chars().any(char::is_control) {
                return Err("progress text contains control characters".to_owned());
            }
        }
        Ok(())
    }
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
    /// Kitty-graphics images this pane can currently display through Unicode
    /// placeholder cells in `lines`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<TerminalImage>,
}

/// One transmitted kitty-graphics image with a virtual (placeholder) placement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalImage {
    /// Image id; placeholder cells carry it as their 24-bit foreground color.
    pub id: u32,
    /// Changes whenever the id is re-transmitted, so renderers can reload.
    pub generation: u64,
    /// Cell box the image is fit into, preserving its aspect ratio.
    pub columns: u16,
    pub rows: u16,
    /// Owner-only PNG file written by the local session service.
    pub path: String,
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
// Independent wire flags, not a state machine.
#[allow(clippy::struct_excessive_bools)]
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
    /// The pane's application enabled kitty paste events (`CSI ? 5522 h`),
    /// so an image paste can go through `ClientRequest::PasteImage`.
    #[serde(default)]
    pub enhanced_paste: bool,
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

/// Window within which a new finish or attention event replaces the same
/// pane's unread event of the same kind instead of adding another row.
pub const NOTIFICATION_DEDUPE_MS: u64 = 5_000;

impl SessionNotification {
    /// Whether `incoming` replaces this stored item: both are unread
    /// `Completed` or `Attention` events of the same kind for the same pane,
    /// at most `NOTIFICATION_DEDUPE_MS` apart in either direction. The
    /// service and the desktop mirror apply the same rule.
    pub fn replaced_by(&self, incoming: &Self) -> bool {
        matches!(
            incoming.kind,
            NotificationKind::Completed | NotificationKind::Attention
        ) && !self.read
            && self.pane_id == incoming.pane_id
            && self.kind == incoming.kind
            && self.at_ms.abs_diff(incoming.at_ms) <= NOTIFICATION_DEDUPE_MS
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: NotificationKind, at_ms: u64) -> SessionNotification {
        SessionNotification {
            id: 1,
            pane_id: Uuid::from_u128(1),
            workspace_id: Uuid::from_u128(2),
            kind,
            message: None,
            pane_title: "omp".to_owned(),
            workspace_title: "Main".to_owned(),
            profile: TerminalProfile::Omp,
            at_ms,
            read: false,
        }
    }

    /// Audit finding 17: an event older than a stored one must not wipe
    /// newer unread items outside the dedupe window.
    #[test]
    fn dedupe_is_symmetric_and_bounded_by_the_window() {
        let stored = event(NotificationKind::Completed, 100_000);
        assert!(stored.replaced_by(&event(NotificationKind::Completed, 104_000)));
        assert!(stored.replaced_by(&event(NotificationKind::Completed, 96_000)));
        assert!(!stored.replaced_by(&event(NotificationKind::Completed, 10_000)));
        assert!(!stored.replaced_by(&event(NotificationKind::Completed, 190_000)));
        assert!(!stored.replaced_by(&event(NotificationKind::Attention, 100_000)));
        assert!(!stored.replaced_by(&event(NotificationKind::Message, 100_000)));
        let mut read = stored.clone();
        read.read = true;
        assert!(!read.replaced_by(&event(NotificationKind::Completed, 100_000)));
        let mut other_pane = event(NotificationKind::Completed, 100_000);
        other_pane.pane_id = Uuid::from_u128(9);
        assert!(!stored.replaced_by(&other_pane));
    }

    fn progress(done: u32, total: u32) -> PaneProgress {
        PaneProgress {
            done,
            total,
            current: None,
            phase: None,
            source: ProgressSource::Omp,
        }
    }

    #[test]
    fn progress_fraction_spans_zero_to_one_and_treats_empty_lists_as_complete() {
        assert!((progress(0, 4).fraction() - 0.0).abs() < f32::EPSILON);
        assert!((progress(1, 4).fraction() - 0.25).abs() < f32::EPSILON);
        assert!((progress(4, 4).fraction() - 1.0).abs() < f32::EPSILON);
        assert!((progress(0, 0).fraction() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn progress_validation_enforces_count_and_text_bounds() {
        assert!(progress(0, 0).validate().is_ok());
        assert!(
            progress(MAX_PROGRESS_TASKS, MAX_PROGRESS_TASKS)
                .validate()
                .is_ok()
        );
        assert!(progress(0, MAX_PROGRESS_TASKS + 1).validate().is_err());
        assert!(progress(5, 4).validate().is_err());

        let with_text = |current: &str, phase: &str| PaneProgress {
            current: Some(current.to_owned()),
            phase: Some(phase.to_owned()),
            ..progress(1, 2)
        };
        let longest = "é".repeat(MAX_PROGRESS_TEXT_CHARS);
        assert!(with_text(&longest, "Phase 1").validate().is_ok());
        assert!(
            with_text(&format!("{longest}x"), "Phase 1")
                .validate()
                .is_err()
        );
        assert!(with_text("ok", &format!("{longest}x")).validate().is_err());
        assert!(with_text("line\nbreak", "Phase 1").validate().is_err());
        assert!(with_text("ok", "tab\there").validate().is_err());
    }
}
