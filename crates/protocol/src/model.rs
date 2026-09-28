//! Desired-state model: snapshots, workspaces, tabs, panes, and tmux types.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::profile::{TerminalIdentity, TerminalProfile};
use crate::terminal::{PaneProgress, PaneStatus};
use crate::validation::ValidationError;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub revision: u64,
    #[serde(default)]
    pub appearance: AppearanceSettings,
    #[serde(default)]
    pub bots: BotSettings,
    /// Ephemeral transport authority projected by the local session service.
    /// Missing entries are intentionally treated as unknown and fail closed.
    #[serde(default)]
    pub terminal_transports: HashMap<Uuid, TerminalTransport>,
    pub workspaces: Vec<Workspace>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BotSettings {
    /// Agent preselected when creating a bot; None picks the first installed agent.
    #[serde(default)]
    pub default_agent: Option<TerminalProfile>,
}

/// Launch configuration of a bot. Each bot is its own `WorkspaceKind::Bot`
/// workspace whose terminals run the configured agent CLI's own interface.
/// An omp bot's tabs hold its live thread panes, one omp conversation each;
/// the user may split and rearrange them like any workstation tab.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BotSpec {
    pub agent: TerminalProfile,
    /// Extra standing instructions appended to the bot's coordinator prompt.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Custom home folder the bot's terminal starts in; None uses the default
    /// `<state>/bots/<bot id>/`.
    #[serde(default)]
    pub home: Option<String>,
    /// Saved thread (agent session) ids the user pinned to the top.
    #[serde(default)]
    pub pinned_threads: Vec<String>,
    /// Live thread panes of this bot, keyed by pane id.
    #[serde(default)]
    pub thread_panes: BTreeMap<Uuid, BotThreadPane>,
}

/// What one live bot pane shows.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BotThreadPane {
    /// Agent session id the pane shows, once known.
    #[serde(default)]
    pub session: Option<String>,
    /// Epoch milliseconds the pane was last activated; 0 = never.
    #[serde(default)]
    pub activated_ms: u64,
    /// The agent launch running in the pane. Its exit hook quotes the id, so
    /// a hook left over from an earlier launch is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<AgentLaunch>,
}

/// One launch of a bot pane's agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLaunch {
    pub id: Uuid,
    /// Epoch milliseconds the launch command was issued.
    pub started_ms: u64,
}

/// One thread of a bot: a saved or live agent conversation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BotThread {
    /// Agent session id, or `pane:<pane id>` for a live pane whose session is
    /// not known yet.
    pub id: String,
    pub title: Option<String>,
    /// Epoch milliseconds of the last change.
    pub updated_ms: u64,
    pub pinned: bool,
    /// The live pane showing this thread.
    pub pane_id: Option<Uuid>,
    /// The bot tab containing the live pane.
    #[serde(default)]
    pub tab_id: Option<Uuid>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalTransport {
    #[default]
    Unknown,
    Local,
    SystemSsh {
        destination: String,
    },
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
            status_changed_at_ms: 0,
            unseen: false,
            progress: None,
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let tab = Tab {
            id: Uuid::new_v4(),
            title: "Shell".to_owned(),
            custom_title: None,
            color: None,
            custom_icon: None,
            pinned: false,
            owner_bot: None,
            owner_thread: None,
            layout: PaneLayout::Leaf { pane },
        };

        Self {
            revision: 0,
            appearance: AppearanceSettings::default(),
            bots: BotSettings::default(),
            terminal_transports: HashMap::new(),
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                title: this_machine_title().to_owned(),
                color: None,
                pinned: false,
                pin_order: 0,
                order: 1,
                active_terminal_count: 1,
                connection: WorkspaceConnection::Local,
                working_dir: None,
                kind: WorkspaceKind::Workstation,
                parent_workstation: None,
                home: true,
                instructions: None,
                owner_bot: None,
                custom_icon: None,
                bot: None,
                tabs: vec![tab],
            }],
        }
    }
}

/// Default title of the undeletable workstation representing this machine.
pub const fn this_machine_title() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else {
        "This Computer"
    }
}

/// A workstation in the sidebar (or, with `WorkspaceKind::Bot`, a bot). Workstations nest
/// through `parent_workstation`; a nested workstation always runs on its parent's machine.
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
    /// Explicit manual order among siblings in the same pinned partition.
    #[serde(default)]
    pub order: u32,
    #[serde(default)]
    pub active_terminal_count: u32,
    #[serde(default)]
    pub connection: WorkspaceConnection,
    /// Root folder new terminals open in. `None` inherits the nearest
    /// ancestor's root; a top-level workstation without one uses the home folder.
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub kind: WorkspaceKind,
    /// Enclosing workstation; `None` for top-level workstations and bots.
    #[serde(default)]
    pub parent_workstation: Option<Uuid>,
    /// The single top-level local workstation representing this machine. It can
    /// be renamed but never deleted or nested.
    #[serde(default)]
    pub home: bool,
    #[serde(default)]
    pub instructions: Option<String>,
    /// Bot whose delegated workers default to this workstation.
    #[serde(default)]
    pub owner_bot: Option<Uuid>,
    #[serde(default)]
    pub custom_icon: Option<String>,
    /// Present exactly on `WorkspaceKind::Bot` workspaces.
    #[serde(default)]
    pub bot: Option<BotSpec>,
    pub tabs: Vec<Tab>,
}

impl Workspace {
    /// Whether this workspace is a bot; bots are never workstations.
    pub fn is_bot(&self) -> bool {
        self.kind == WorkspaceKind::Bot
    }
}

/// Nesting level of workstation `id`: 1 for a top-level workstation. `None` when the
/// workstation is missing or its parent chain is broken or cyclic.
pub fn workstation_depth(workspaces: &[Workspace], id: Uuid) -> Option<usize> {
    let mut depth = 0;
    let mut current = Some(id);
    while let Some(workstation_id) = current {
        depth += 1;
        if depth > workspaces.len() {
            return None;
        }
        current = workspaces
            .iter()
            .find(|workspace| workspace.id == workstation_id)?
            .parent_workstation;
    }
    Some(depth)
}

/// Root folder new terminals of workstation `id` open in: its own `working_dir` or
/// the nearest ancestor's. `None` means the machine's home folder.
pub fn effective_working_dir(workspaces: &[Workspace], id: Uuid) -> Option<&str> {
    let mut current = Some(id);
    let mut steps = 0;
    while let Some(workstation_id) = current {
        steps += 1;
        if steps > workspaces.len() {
            return None;
        }
        let workspace = workspaces
            .iter()
            .find(|workspace| workspace.id == workstation_id)?;
        if let Some(dir) = workspace.working_dir.as_deref() {
            return Some(dir);
        }
        current = workspace.parent_workstation;
    }
    None
}

/// Every workstation nested anywhere below `id`, parents before their children.
pub fn workstation_descendants(workspaces: &[Workspace], id: Uuid) -> Vec<Uuid> {
    let mut found = Vec::new();
    let mut frontier = vec![id];
    while let Some(parent) = frontier.pop() {
        for workspace in workspaces {
            if workspace.parent_workstation == Some(parent)
                && workspace.id != id
                && !found.contains(&workspace.id)
            {
                found.push(workspace.id);
                frontier.push(workspace.id);
            }
        }
    }
    found
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    #[default]
    Workstation,
    /// A bot: its tabs are the bot's live threads; never shown as a workstation.
    Bot,
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
    pub color: Option<AppearanceColor>,
    #[serde(default)]
    pub custom_icon: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    /// Bot that created this worker tab through `CreateWorker`.
    #[serde(default)]
    pub owner_bot: Option<Uuid>,
    /// Bot pane (thread) that created this worker tab through `CreateWorker`.
    #[serde(default)]
    pub owner_thread: Option<Uuid>,
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
    Gallery,
}

impl PaneKind {
    /// Whether this pane renders a browser view. Exhaustive by design so a
    /// future variant fails compilation exactly here.
    pub fn is_browser(&self) -> bool {
        match self {
            Self::Browser { .. } => true,
            Self::Terminal | Self::Gallery => false,
        }
    }

    /// Whether this pane renders a terminal view. Exhaustive by design so a
    /// future variant fails compilation exactly here.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Terminal => true,
            Self::Browser { .. } | Self::Gallery => false,
        }
    }

    /// Whether this pane renders an image gallery.
    pub const fn is_gallery(&self) -> bool {
        match self {
            Self::Gallery => true,
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
    /// Ephemeral epoch milliseconds of the last `status` transition; 0 = never.
    #[serde(default)]
    pub status_changed_at_ms: u64,
    /// The pane reached done, needs input, needs approval, or attention and
    /// the user has not opened it since (`ClientRequest::MarkPaneSeen`).
    /// Owned by the session service and kept across restarts.
    #[serde(default)]
    pub unseen: bool,
    /// Task-list progress last reported by the pane's agent; cleared when its
    /// process exits.
    #[serde(default)]
    pub progress: Option<PaneProgress>,
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

        let gallery = PaneKind::Gallery;
        assert_eq!(
            serde_json::to_value(&gallery).unwrap(),
            serde_json::json!({ "type": "gallery" })
        );
        assert_eq!(
            serde_json::from_value::<PaneKind>(serde_json::to_value(&gallery).unwrap()).unwrap(),
            gallery
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
    fn seeded_snapshot_is_the_home_workstation_with_a_visible_pane() {
        let snapshot = SessionSnapshot::seeded();
        assert_eq!(snapshot.workspaces.len(), 1);
        let home = &snapshot.workspaces[0];
        assert_eq!(home.title, this_machine_title());
        assert!(home.home && home.parent_workstation.is_none());
        assert_eq!(home.kind, WorkspaceKind::Workstation);
        assert_eq!(home.connection, WorkspaceConnection::Local);
        assert_eq!(home.tabs.len(), 1);
        let PaneLayout::Leaf { pane } = &home.tabs[0].layout else {
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
        assert_eq!(snapshot.workspaces[0].parent_workstation, None);
        assert!(!snapshot.workspaces[0].home);
    }

    fn nested(parent: Option<&Workspace>, working_dir: Option<&str>) -> Workspace {
        let mut workstation = SessionSnapshot::seeded().workspaces.remove(0);
        workstation.home = false;
        workstation.parent_workstation = parent.map(|parent| parent.id);
        workstation.working_dir = working_dir.map(str::to_owned);
        workstation
    }

    #[test]
    fn nested_workstations_inherit_the_nearest_root_and_report_depth_and_descendants() {
        let root = nested(None, Some("/srv"));
        let child = nested(Some(&root), None);
        let grandchild = nested(Some(&child), Some("/srv/app"));
        let leaf = nested(Some(&grandchild), None);
        let other = nested(None, None);
        let workstations = vec![
            root.clone(),
            child.clone(),
            grandchild.clone(),
            leaf.clone(),
            other.clone(),
        ];

        assert_eq!(effective_working_dir(&workstations, child.id), Some("/srv"));
        assert_eq!(
            effective_working_dir(&workstations, leaf.id),
            Some("/srv/app")
        );
        assert_eq!(effective_working_dir(&workstations, other.id), None);
        assert_eq!(workstation_depth(&workstations, root.id), Some(1));
        assert_eq!(workstation_depth(&workstations, leaf.id), Some(4));
        assert_eq!(workstation_depth(&workstations, Uuid::new_v4()), None);
        assert_eq!(
            workstation_descendants(&workstations, root.id),
            vec![child.id, grandchild.id, leaf.id]
        );
        assert!(workstation_descendants(&workstations, other.id).is_empty());
    }

    #[test]
    fn cyclic_parent_chains_terminate() {
        let mut first = nested(None, None);
        let mut second = nested(Some(&first), None);
        first.parent_workstation = Some(second.id);
        second.parent_workstation = Some(first.id);
        let workstations = vec![first.clone(), second];
        assert_eq!(workstation_depth(&workstations, first.id), None);
        assert_eq!(effective_working_dir(&workstations, first.id), None);
    }
}

#[cfg(test)]
mod bot_thread_tests {
    use super::*;

    #[test]
    fn bot_specs_and_tabs_without_thread_fields_still_load_and_new_fields_round_trip() {
        let legacy: BotSpec = serde_json::from_value(serde_json::json!({"agent": "omp"})).unwrap();
        assert!(legacy.pinned_threads.is_empty());
        assert!(legacy.thread_panes.is_empty());

        let pane = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let spec = BotSpec {
            agent: TerminalProfile::Omp,
            instructions: None,
            home: None,
            pinned_threads: vec!["0193-abc".to_owned()],
            thread_panes: BTreeMap::from([(
                pane,
                BotThreadPane {
                    session: Some("0193-abc".to_owned()),
                    activated_ms: 7,
                    launch: None,
                },
            )]),
        };
        let encoded = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            encoded["thread_panes"],
            serde_json::json!({ pane.to_string(): {"session": "0193-abc", "activated_ms": 7} })
        );
        assert_eq!(serde_json::from_value::<BotSpec>(encoded).unwrap(), spec);
        let mut launched = spec.clone();
        launched.thread_panes.get_mut(&pane).unwrap().launch = Some(AgentLaunch {
            id: pane,
            started_ms: 9,
        });
        let encoded = serde_json::to_value(&launched).unwrap();
        assert_eq!(
            encoded["thread_panes"][pane.to_string()]["launch"],
            serde_json::json!({"id": pane, "started_ms": 9})
        );
        assert_eq!(
            serde_json::from_value::<BotSpec>(encoded).unwrap(),
            launched
        );

        let mut snapshot = SessionSnapshot::seeded();
        assert_eq!(snapshot.workspaces[0].tabs[0].owner_thread, None);
        assert_eq!(snapshot.workspaces[0].bot, None);
        snapshot.workspaces[0].tabs[0].owner_thread = Some(pane);
        let mut bot = snapshot.workspaces[0].clone();
        bot.id = Uuid::new_v4();
        bot.kind = WorkspaceKind::Bot;
        bot.bot = Some(spec);
        snapshot.workspaces.push(bot);
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(encoded["workspaces"][1]["kind"], "bot");
        assert_eq!(encoded["workspaces"][1]["bot"]["agent"], "omp");
        let restored: SessionSnapshot = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored, snapshot);
        assert!(restored.workspaces[1].is_bot() && !restored.workspaces[0].is_bot());
    }
}
