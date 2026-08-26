//! Desired-state model: snapshots, workspaces, tabs, panes, and tmux types.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::profile::{TerminalIdentity, TerminalProfile};
use crate::terminal::PaneStatus;
use crate::validation::ValidationError;

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
    pub const DARK_GRAY: Self = Self::new(0x3b, 0x42, 0x4f);

    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub const fn as_rgb(self) -> u32 {
        ((self.red as u32) << 16) | ((self.green as u32) << 8) | self.blue as u32
    }
}

impl Default for AppearanceColor {
    fn default() -> Self {
        Self::DARK_GRAY
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
            default_terminal_accent: AppearanceColor::DARK_GRAY,
            default_workspace_color: AppearanceColor::DARK_GRAY,
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
            kind: PaneKind::Terminal,
            color: None,
            identity: TerminalIdentity::default(),
            status: PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let tab = Tab {
            id: Uuid::new_v4(),
            title: "Shell".to_owned(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
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
                working_dir: None,
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
    #[serde(default)]
    pub working_dir: Option<String>,
    pub tabs: Vec<Tab>,
}

const MAX_TMUX_ID_LEN: usize = 32;

fn validate_tmux_id(value: &str, sigil: char, label: &'static str) -> Result<(), ValidationError> {
    if value.len() < 2
        || value.len() > MAX_TMUX_ID_LEN
        || !value.starts_with(sigil)
        || !value[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ValidationError::TmuxTargetId { label });
    }
    Ok(())
}

/// Opaque tmux session ID (`$` + ASCII digits) reported by a bounded scan.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TmuxSessionId(String);

impl TmuxSessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TmuxSessionId {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_tmux_id(&value, '$', "session")?;
        Ok(Self(value))
    }
}

impl FromStr for TmuxSessionId {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value.to_owned())
    }
}

impl From<TmuxSessionId> for String {
    fn from(value: TmuxSessionId) -> Self {
        value.0
    }
}

impl std::fmt::Display for TmuxSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Ephemeral metadata returned by an explicit tmux scan.
///
/// This is deliberately not part of the desired-state snapshot: a tmux server
/// and its opaque IDs belong to the host running tmux, not to Harness Harlot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TmuxSession {
    pub id: TmuxSessionId,
    pub name: String,
    pub windows: u32,
    pub attached_clients: u32,
}

/// One selected tmux session which was not opened.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TmuxSessionAttachIssue {
    pub session_id: TmuxSessionId,
    pub message: String,
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
    #[serde(default)]
    pub custom_title: Option<String>,
    #[serde(default)]
    pub project_dir: Option<String>,
    #[serde(default)]
    pub color: Option<AppearanceColor>,
    #[serde(default)]
    pub custom_icon: Option<String>,
    #[serde(default)]
    pub parent_tab: Option<Uuid>,
    #[serde(default)]
    pub pinned: bool,
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneKind {
    #[default]
    Terminal,
    Browser {
        url: String,
    },
    Assistant,
}

impl PaneKind {
    /// Whether this pane renders a browser view. Exhaustive by design so a
    /// future variant fails compilation exactly here.
    pub fn is_browser(&self) -> bool {
        match self {
            Self::Browser { .. } => true,
            Self::Terminal | Self::Assistant => false,
        }
    }

    /// Whether this pane renders a terminal view. Exhaustive by design so a
    /// future variant fails compilation exactly here.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Terminal => true,
            Self::Browser { .. } | Self::Assistant => false,
        }
    }

    /// Whether this pane renders a voice assistant view. Exhaustive by design
    /// so a future variant fails compilation exactly here.
    pub fn is_assistant(&self) -> bool {
        match self {
            Self::Assistant => true,
            Self::Terminal | Self::Browser { .. } => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: Uuid,
    pub title: String,
    pub shell: String,
    #[serde(default)]
    pub kind: PaneKind,
    #[serde(default)]
    pub color: Option<AppearanceColor>,
    /// Ephemeral resolved identity projected by the local session service.
    /// Only explicit overrides below are included in desired-state recovery.
    #[serde(default)]
    pub identity: TerminalIdentity,
    /// Ephemeral activity state projected by the local session service.
    /// It is intentionally reset during desired-state recovery.
    #[serde(default)]
    pub status: PaneStatus,
    #[serde(default)]
    pub custom_title: Option<String>,
    #[serde(default)]
    pub profile_override: Option<TerminalProfile>,
    /// Stable filename of an image copied into the application's custom icon store.
    #[serde(default)]
    pub custom_icon: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_kinds_use_stable_tagged_json_and_round_trip() {
        let terminal = PaneKind::Terminal;
        assert_eq!(
            serde_json::to_value(&terminal).unwrap(),
            serde_json::json!({ "type": "terminal" })
        );
        assert_eq!(
            serde_json::from_value::<PaneKind>(serde_json::to_value(&terminal).unwrap()).unwrap(),
            terminal
        );

        let browser = PaneKind::Browser {
            url: "https://example.com/path".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&browser).unwrap(),
            serde_json::json!({
                "type": "browser",
                "url": "https://example.com/path",
            })
        );
        assert_eq!(
            serde_json::from_value::<PaneKind>(serde_json::to_value(&browser).unwrap()).unwrap(),
            browser
        );

        let assistant = PaneKind::Assistant;
        assert_eq!(
            serde_json::to_value(&assistant).unwrap(),
            serde_json::json!({ "type": "assistant" })
        );
        assert_eq!(
            serde_json::from_value::<PaneKind>(serde_json::to_value(&assistant).unwrap()).unwrap(),
            assistant
        );
    }

    #[test]
    fn browser_pane_kind_round_trips_on_the_pane_model() {
        let pane: Pane = serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000002",
            "title": "Example",
            "shell": "",
            "kind": {
                "type": "browser",
                "url": "https://example.com",
            },
        }))
        .unwrap();

        assert_eq!(
            pane.kind,
            PaneKind::Browser {
                url: "https://example.com".to_owned(),
            }
        );
        assert_eq!(
            serde_json::from_value::<Pane>(serde_json::to_value(&pane).unwrap()).unwrap(),
            pane
        );
    }

    #[test]
    fn seeded_snapshot_has_a_visible_pane() {
        let snapshot = SessionSnapshot::seeded();
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].title, "Workstation 1");
        assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
        let PaneLayout::Leaf { pane } = &snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected leaf");
        };
        assert_eq!(pane.kind, PaneKind::Terminal);
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
                                "shell": "zsh",
                                "kind": { "type": "terminal" }
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
        assert_eq!(pane.kind, PaneKind::Terminal);
        assert_eq!(pane.identity, TerminalIdentity::default());
        assert_eq!(pane.custom_title, None);
        assert_eq!(pane.profile_override, None);
        assert_eq!(snapshot.workspaces[0].working_dir, None);
        assert_eq!(snapshot.workspaces[0].tabs[0].project_dir, None);
    }

    #[test]
    fn workspace_and_project_directories_round_trip() {
        let mut snapshot = SessionSnapshot::seeded();
        snapshot.workspaces[0].working_dir = Some("/srv/workstation".to_owned());
        snapshot.workspaces[0].tabs[0].project_dir = Some("/srv/project".to_owned());

        let restored: SessionSnapshot =
            serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();

        assert_eq!(restored, snapshot);
    }
}
