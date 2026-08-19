//! Terminal history settings, archive status, and archived pages.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
