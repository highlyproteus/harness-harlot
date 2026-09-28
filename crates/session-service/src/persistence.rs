use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bots::valid_session_id;
use crate::layout::collect_pane_ids;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    AppearanceColor, AppearanceSettings, BotSettings, BotSpec, MAX_BROWSER_URL_LEN,
    MAX_WORKSTATION_DEPTH, Pane, PaneKind, PaneLayout, PaneProgress, SessionSnapshot, SplitAxis,
    Tab, TerminalIdentity, TerminalProfile, Workspace, WorkspaceConnection,
    WorkspaceConnectionStatus, WorkspaceKind, this_machine_title, validate_ssh_host,
    validate_workspace_dir,
};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Schema 17 added the pane `unseen` flag and task `progress`.
const SCHEMA_VERSION: u16 = 17;
/// Snapshots older than this still carry the retired Harbor Blue defaults.
const DARK_GRAY_DEFAULTS_SCHEMA_VERSION: u16 = 13;
/// Snapshots older than this still carry project tabs and have no home
/// workstation; `migrate_projects_and_home` converts them.
const NESTED_WORKSTATIONS_SCHEMA_VERSION: u16 = 16;
const MIN_SUPPORTED_SCHEMA_VERSION: u16 = 1;
const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024;
/// Most workstations, nested ones included. Every migrated project becomes a
/// nested workstation and holds at least one pane, so the pane limit plus the
/// former workstation limit always fits.
pub(crate) const MAX_WORKSPACES: usize = 64;
pub(crate) const MAX_TABS_PER_WORKSPACE: usize = 32;
pub(crate) const MAX_BOTS: usize = 32;
/// Title of a thread tab created by the per-bot workspace migration.
const MIGRATED_THREAD_TAB_TITLE: &str = "New thread";
const MAX_PANES: usize = 32;
const MAX_LAYOUT_DEPTH: usize = 16;
pub(crate) const MAX_TITLE_CHARS: usize = 80;
const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_INSTRUCTIONS_CHARS: usize = 4096;
pub(crate) const MAX_RECENT_COLORS: usize = 8;

#[derive(Clone, Debug)]
pub(crate) struct RecoveredState {
    pub snapshot: SessionSnapshot,
    pub cwd_by_pane: HashMap<Uuid, PathBuf>,
    pub tmux_by_pane: HashMap<Uuid, (String, String)>,
    pub offline_panes: HashSet<Uuid>,
    /// Panes whose tmux windows still live in another workstation's session,
    /// mapped to that workstation's id: panes of the retired shared Bots
    /// workspace and panes of projects migrated into nested workstations.
    pub legacy_tmux_workspace: HashMap<Uuid, Uuid>,
    /// `(from, to)` workstation pairs whose gallery directory contents must be
    /// copied because migrated project tabs moved gallery panes into `to`.
    pub gallery_copies: Vec<(Uuid, Uuid)>,
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotStore {
    path: PathBuf,
    #[cfg(test)]
    fail_before_replace: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SnapshotStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(test)]
            fail_before_replace: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub(crate) fn load_or_quarantine(&self) -> Result<Option<RecoveredState>> {
        let Some(parent) = self.path.parent() else {
            bail!("snapshot path has no parent: {}", self.path.display());
        };
        ensure_private_directory(parent)?;
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect snapshot {}", self.path.display()));
            }
        }

        match self.load() {
            Ok(state) => Ok(Some(state)),
            Err(error) => {
                let quarantined = self.quarantine().with_context(|| {
                    format!("snapshot was invalid ({error:#}) and could not be quarantined")
                })?;
                eprintln!(
                    "quarantined invalid Harness Harlot recovery snapshot at {}: {error:#}",
                    quarantined.display()
                );
                Ok(None)
            }
        }
    }

    fn load(&self) -> Result<RecoveredState> {
        let bytes = hh_protocol::read_private_file(&self.path, MAX_SNAPSHOT_BYTES)
            .with_context(|| format!("read snapshot {}", self.path.display()))?;
        let mut desired: DesiredState =
            serde_json::from_slice(&bytes).context("decode recovery snapshot")?;
        desired.migrate();
        desired.validate()?;
        Ok(desired.into_runtime())
    }

    pub(crate) fn encode_with_offline(
        snapshot: &SessionSnapshot,
        cwd_by_pane: &HashMap<Uuid, PathBuf>,
        tmux_by_pane: &HashMap<Uuid, (String, String)>,
        offline_panes: &HashSet<Uuid>,
    ) -> Result<Vec<u8>> {
        let desired =
            DesiredState::from_runtime(snapshot, cwd_by_pane, tmux_by_pane, offline_panes)?;
        desired.validate()?;
        let bytes = serde_json::to_vec(&desired).context("encode recovery snapshot")?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            bail!("encoded snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes");
        }
        Ok(bytes)
    }

    /// Atomically writes the recovery snapshot: a fresh `0o600` temporary in
    /// the same directory is written, synced, renamed over the target, and
    /// the parent directory is synced.
    ///
    /// This must stay behaviorally in sync with
    /// `hh_protocol::paths::atomic_write_private`. Consolidation is
    /// intentionally skipped: this copy carries the `#[cfg(test)]`
    /// injected-failure hook (`fail_before_replace`) that cannot cross the
    /// crate boundary.
    pub(crate) fn write_snapshot(&self, bytes: &[u8]) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("snapshot path has no parent directory")?;
        ensure_private_directory(parent)?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sessions.json");
        let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
        let write_result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| format!("create temporary snapshot {}", temporary.display()))?;
            file.write_all(bytes).context("write recovery snapshot")?;
            file.sync_all().context("sync recovery snapshot contents")?;

            #[cfg(test)]
            if self
                .fail_before_replace
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                bail!("injected failure before atomic snapshot replace");
            }

            fs::rename(&temporary, &self.path).with_context(|| {
                format!(
                    "atomically replace {} with {}",
                    self.path.display(),
                    temporary.display()
                )
            })?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .context("sync recovery snapshot directory")?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    #[cfg(test)]
    fn save(&self, snapshot: &SessionSnapshot, cwd_by_pane: &HashMap<Uuid, PathBuf>) -> Result<()> {
        let bytes =
            Self::encode_with_offline(snapshot, cwd_by_pane, &HashMap::new(), &HashSet::new())?;
        self.write_snapshot(&bytes)
    }

    fn quarantine(&self) -> Result<PathBuf> {
        quarantine_private_file(&self.path, "sessions")
    }

    #[cfg(test)]
    pub(crate) fn inject_failure_before_replace(&self, enabled: bool) {
        self.fail_before_replace
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Moves an invalid state file aside as `<stem>.corrupt-<ms>-<uuid>.json` in
/// its directory, restricting the moved file to its owner, so the service can
/// start fresh while the bytes stay available for inspection.
pub(crate) fn quarantine_private_file(path: &Path, stem: &str) -> Result<PathBuf> {
    let parent = path.parent().context("state file path has no parent")?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let quarantined = parent.join(format!(
        "{stem}.corrupt-{timestamp}-{}.json",
        Uuid::new_v4()
    ));
    fs::rename(path, &quarantined).with_context(|| {
        format!(
            "quarantine corrupt state file {} as {}",
            path.display(),
            quarantined.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&quarantined)
        .context("inspect quarantined state file without following links")?;
    if metadata.is_file() {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&quarantined)
            .context("open quarantined state file without following links")?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("restrict opened quarantined state file")?;
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .context("sync quarantine directory")?;
    Ok(quarantined)
}

pub(crate) fn default_snapshot_path() -> Result<PathBuf> {
    let directory = hh_protocol::state_directory().context("HOME is not set")?;
    Ok(directory.join("sessions.json"))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    hh_protocol::ensure_private_directory(path)
        .with_context(|| format!("prepare recovery directory {}", path.display()))
}

/// Retained only so a snapshot written before the tmux status-bar setting was
/// removed still parses under `deny_unknown_fields`. Never written back.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RetiredTmuxSettings {
    #[serde(default)]
    hide_status_bar: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredState {
    schema_version: u16,
    revision: u64,
    #[serde(default)]
    appearance: AppearanceSettings,
    #[serde(default)]
    bots: BotSettings,
    /// Settings of the removed pi Assistant in schema-13 snapshots. Never written back.
    #[serde(default, skip_serializing)]
    #[expect(dead_code, reason = "parsed only so pre-removal snapshots still load")]
    assistant: Option<IgnoredAny>,
    #[serde(default, skip_serializing)]
    #[expect(dead_code, reason = "parsed only so pre-removal snapshots still load")]
    tmux: RetiredTmuxSettings,
    workspaces: Vec<DesiredWorkspace>,
    /// Filled by `split_legacy_bots` and `migrate_projects`; see
    /// `RecoveredState::legacy_tmux_workspace`.
    #[serde(skip)]
    legacy_tmux_workspace: HashMap<Uuid, Uuid>,
    /// Filled by `migrate_projects`; see `RecoveredState::gallery_copies`.
    #[serde(skip)]
    gallery_copies: Vec<(Uuid, Uuid)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredWorkspace {
    id: Uuid,
    title: String,
    #[serde(default)]
    color: Option<AppearanceColor>,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    pin_order: u32,
    #[serde(default)]
    order: u32,
    #[serde(default)]
    connection: WorkspaceConnection,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    kind: DesiredWorkspaceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_workstation: Option<Uuid>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    home: bool,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    owner_bot: Option<Uuid>,
    #[serde(default)]
    custom_icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bot: Option<BotSpec>,
    tabs: Vec<DesiredTab>,
}

/// Persisted workspace kinds, including the removed Assistant kind that
/// schema-13 snapshots may contain until `drop_legacy_assistants` runs, and
/// the retired shared Bots workspace of schema-14 snapshots, split into one
/// Bot workspace per bot by `split_legacy_bots`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DesiredWorkspaceKind {
    #[default]
    Workstation,
    Bot,
    Bots,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredTab {
    id: Uuid,
    title: String,
    #[serde(default)]
    custom_title: Option<String>,
    /// Project directory of a schema-15 project tab, turned into a nested
    /// workstation by `migrate_projects`. Never written back.
    #[serde(default, skip_serializing)]
    project_dir: Option<String>,
    #[serde(default)]
    color: Option<AppearanceColor>,
    #[serde(default)]
    custom_icon: Option<String>,
    /// Project owning a schema-15 child tab. Never written back.
    #[serde(default, skip_serializing)]
    parent_tab: Option<Uuid>,
    #[serde(default)]
    pinned: bool,
    /// A bot tab of the retired shared Bots workspace (schema 14). Never
    /// written back.
    #[serde(default, skip_serializing)]
    bot: Option<BotSpec>,
    #[serde(default)]
    owner_bot: Option<Uuid>,
    #[serde(default)]
    owner_thread: Option<Uuid>,
    layout: DesiredLayout,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DesiredLayout {
    Leaf {
        pane: DesiredPane,
    },
    Stack {
        panes: Vec<DesiredPane>,
        active: Uuid,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredPane {
    id: Uuid,
    #[serde(default)]
    kind: DesiredPaneKind,
    /// Compatibility fallback for schema-v1 readers. Live detected identity is
    /// deliberately projected to "Terminal" instead of being persisted here.
    title: String,
    #[serde(default)]
    color: Option<AppearanceColor>,
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    profile_override: Option<TerminalProfile>,
    #[serde(default)]
    custom_icon: Option<String>,
    local_cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmux_window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmux_pane: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unseen: bool,
    /// Boxed: rarely present, and it would dominate the layout enum's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress: Option<Box<PaneProgress>>,
}

/// Persisted pane kinds, including the removed Assistant kind that
/// schema-13 snapshots may contain until `drop_legacy_assistants` runs.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DesiredPaneKind {
    #[default]
    Terminal,
    Browser {
        url: String,
    },
    Gallery,
    Assistant,
}

impl From<&PaneKind> for DesiredPaneKind {
    fn from(kind: &PaneKind) -> Self {
        match kind {
            PaneKind::Terminal => Self::Terminal,
            PaneKind::Browser { url } => Self::Browser { url: url.clone() },
            PaneKind::Gallery => Self::Gallery,
        }
    }
}

impl DesiredPaneKind {
    fn into_runtime(self) -> PaneKind {
        match self {
            Self::Terminal => PaneKind::Terminal,
            Self::Browser { url } => PaneKind::Browser { url },
            Self::Gallery => PaneKind::Gallery,
            Self::Assistant => unreachable!("legacy assistant panes are dropped before recovery"),
        }
    }
}

impl DesiredState {
    /// Upgrades an older snapshot to the current schema in memory.
    fn migrate(&mut self) {
        self.drop_legacy_assistants();
        self.split_legacy_bots();
        if self.schema_version < NESTED_WORKSTATIONS_SCHEMA_VERSION {
            self.migrate_projects();
            self.choose_home();
            for tab in self
                .workspaces
                .iter_mut()
                .flat_map(|workspace| &mut workspace.tabs)
            {
                if tab
                    .custom_title
                    .as_deref()
                    .is_some_and(|title| numbered_title(title, "Group"))
                {
                    tab.custom_title = None;
                }
            }
        }
    }

    /// Removes the retired pi Assistant from a schema-13 snapshot: its
    /// workspaces, its panes (collapsing their layouts) and tabs left empty.
    /// A snapshot left without workstations gains an empty home later, in
    /// `choose_home`.
    fn drop_legacy_assistants(&mut self) {
        self.workspaces
            .retain(|workspace| workspace.kind != DesiredWorkspaceKind::Assistant);
        for workspace in &mut self.workspaces {
            workspace.tabs = std::mem::take(&mut workspace.tabs)
                .into_iter()
                .filter_map(|mut tab| {
                    tab.layout = tab.layout.without_assistants()?;
                    Some(tab)
                })
                .collect();
        }
    }

    /// Turns every schema-15 project tab of a workstation into a nested
    /// workstation holding the project tab and its child tabs, rooted at the
    /// project directory and running on the parent's machine.
    fn migrate_projects(&mut self) {
        let mut next_order = self
            .workspaces
            .iter()
            .filter(|workspace| !workspace.pinned)
            .map(|workspace| workspace.order)
            .max()
            .unwrap_or(0);
        let mut next_pin_order = self
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned)
            .map(|workspace| workspace.pin_order)
            .max()
            .unwrap_or(0);
        let mut nested = Vec::new();
        for workspace in &mut self.workspaces {
            let tabs = std::mem::take(&mut workspace.tabs);
            if workspace.kind != DesiredWorkspaceKind::Workstation {
                workspace.tabs = tabs
                    .into_iter()
                    .map(|mut tab| {
                        tab.project_dir = None;
                        tab.parent_tab = None;
                        tab
                    })
                    .collect();
                continue;
            }
            let project_ids = tabs
                .iter()
                .filter(|tab| tab.parent_tab.is_none() && tab.project_dir.is_some())
                .map(|tab| tab.id)
                .collect::<HashSet<_>>();
            let mut projects = Vec::new();
            let mut children: HashMap<Uuid, Vec<DesiredTab>> = HashMap::new();
            for mut tab in tabs {
                let parent = tab.parent_tab.take();
                if parent.is_none() && tab.project_dir.is_some() {
                    projects.push(tab);
                } else if let Some(parent) = parent.filter(|parent| project_ids.contains(parent)) {
                    tab.project_dir = None;
                    children.entry(parent).or_default().push(tab);
                } else {
                    tab.project_dir = None;
                    workspace.tabs.push(tab);
                }
            }
            for mut root in projects {
                let working_dir = root.project_dir.take();
                let title = root
                    .custom_title
                    .take()
                    .unwrap_or_else(|| root.title.clone());
                let pinned = std::mem::take(&mut root.pinned);
                let (order, pin_order) = if pinned {
                    next_pin_order = next_pin_order.saturating_add(1);
                    (0, next_pin_order)
                } else {
                    next_order = next_order.saturating_add(1);
                    (next_order, 0)
                };
                let mut child = DesiredWorkspace {
                    id: Uuid::new_v4(),
                    title,
                    color: root.color.take(),
                    pinned,
                    pin_order,
                    order,
                    connection: match &workspace.connection {
                        WorkspaceConnection::Local => WorkspaceConnection::Local,
                        WorkspaceConnection::SystemSsh { destination, .. } => {
                            WorkspaceConnection::SystemSsh {
                                destination: destination.clone(),
                                status: WorkspaceConnectionStatus::Offline,
                            }
                        }
                    },
                    working_dir,
                    kind: DesiredWorkspaceKind::Workstation,
                    parent_workstation: Some(workspace.id),
                    home: false,
                    instructions: None,
                    owner_bot: None,
                    custom_icon: root.custom_icon.take(),
                    bot: None,
                    tabs: Vec::new(),
                };
                let root_children = children.remove(&root.id).unwrap_or_default();
                child.tabs.push(root);
                child.tabs.extend(root_children);
                let mut moves_gallery = false;
                for tab in &child.tabs {
                    tab.layout.visit_panes(&mut |pane| {
                        if pane.tmux_window.is_some() {
                            self.legacy_tmux_workspace.insert(pane.id, workspace.id);
                        }
                        moves_gallery |= pane.kind == DesiredPaneKind::Gallery;
                    });
                }
                if moves_gallery {
                    self.gallery_copies.push((workspace.id, child.id));
                }
                nested.push(child);
            }
        }
        self.workspaces.extend(nested);
    }

    /// Marks the first local top-level workstation in display order as this
    /// machine's home, or appends an empty one when there is none.
    fn choose_home(&mut self) {
        if self.workspaces.iter().any(|workspace| workspace.home) {
            return;
        }
        let home = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| {
                workspace.kind == DesiredWorkspaceKind::Workstation
                    && workspace.connection == WorkspaceConnection::Local
                    && workspace.parent_workstation.is_none()
                    && workspace.owner_bot.is_none()
            })
            .min_by_key(|(index, workspace)| {
                (
                    !workspace.pinned,
                    if workspace.pinned {
                        workspace.pin_order
                    } else {
                        workspace.order
                    },
                    *index,
                )
            })
            .map(|(index, _)| index);
        if let Some(index) = home {
            let workspace = &mut self.workspaces[index];
            workspace.home = true;
            if numbered_title(&workspace.title, "Workstation") {
                this_machine_title().clone_into(&mut workspace.title);
            }
            return;
        }
        let order = self
            .workspaces
            .iter()
            .filter(|workspace| !workspace.pinned)
            .map(|workspace| workspace.order)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.workspaces.push(DesiredWorkspace {
            id: Uuid::new_v4(),
            title: this_machine_title().to_owned(),
            color: None,
            pinned: false,
            pin_order: 0,
            order,
            connection: WorkspaceConnection::Local,
            working_dir: None,
            kind: DesiredWorkspaceKind::Workstation,
            parent_workstation: None,
            home: true,
            instructions: None,
            owner_bot: None,
            custom_icon: None,
            bot: None,
            tabs: Vec::new(),
        });
    }

    /// Splits the retired shared Bots workspace of a schema-14 snapshot into
    /// one Bot workspace per bot tab. The bot keeps its tab id as its
    /// workspace id, so its home folder, threads and every `owner_bot`
    /// reference stay valid; each of its thread panes becomes its own tab.
    fn split_legacy_bots(&mut self) {
        while let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.kind == DesiredWorkspaceKind::Bots)
        {
            let legacy = self.workspaces.remove(index);
            for tab in legacy.tabs {
                let Some(spec) = tab.bot else {
                    continue;
                };
                let name = tab.custom_title.unwrap_or(tab.title);
                let mut panes = Vec::new();
                tab.layout.into_panes(&mut panes);
                let tabs = panes
                    .into_iter()
                    .map(|mut pane| {
                        if pane.tmux_window.is_some() {
                            self.legacy_tmux_workspace.insert(pane.id, legacy.id);
                        }
                        // Bot panes carried the bot's name; threads show their own.
                        if pane.custom_title.as_deref() == Some(name.as_str()) {
                            pane.custom_title = None;
                            "Terminal".clone_into(&mut pane.title);
                        }
                        DesiredTab {
                            id: Uuid::new_v4(),
                            title: MIGRATED_THREAD_TAB_TITLE.to_owned(),
                            custom_title: None,
                            project_dir: None,
                            color: None,
                            custom_icon: None,
                            parent_tab: None,
                            pinned: false,
                            bot: None,
                            owner_bot: None,
                            owner_thread: None,
                            layout: DesiredLayout::Leaf { pane },
                        }
                    })
                    .collect();
                let order = self
                    .workspaces
                    .iter()
                    .filter(|workspace| !workspace.pinned)
                    .map(|workspace| workspace.order)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                self.workspaces.push(DesiredWorkspace {
                    id: tab.id,
                    title: name,
                    color: tab.color,
                    pinned: false,
                    pin_order: 0,
                    order,
                    connection: WorkspaceConnection::Local,
                    working_dir: tab.project_dir,
                    kind: DesiredWorkspaceKind::Bot,
                    parent_workstation: None,
                    home: false,
                    instructions: None,
                    owner_bot: None,
                    custom_icon: tab.custom_icon,
                    bot: Some(spec),
                    tabs,
                });
            }
        }
    }

    fn from_runtime(
        snapshot: &SessionSnapshot,
        cwd_by_pane: &HashMap<Uuid, PathBuf>,
        tmux_by_pane: &HashMap<Uuid, (String, String)>,
        offline_panes: &HashSet<Uuid>,
    ) -> Result<Self> {
        let workspaces = snapshot
            .workspaces
            .iter()
            .map(|workspace| {
                let allow_offline =
                    matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. });
                Ok(DesiredWorkspace {
                    id: workspace.id,
                    title: workspace.title.clone(),
                    color: workspace.color,
                    pinned: workspace.pinned,
                    pin_order: workspace.pin_order,
                    order: workspace.order,
                    connection: match &workspace.connection {
                        WorkspaceConnection::Local => WorkspaceConnection::Local,
                        WorkspaceConnection::SystemSsh { destination, .. } => {
                            WorkspaceConnection::SystemSsh {
                                destination: destination.clone(),
                                status: WorkspaceConnectionStatus::Offline,
                            }
                        }
                    },
                    working_dir: workspace.working_dir.clone(),
                    kind: match workspace.kind {
                        WorkspaceKind::Workstation => DesiredWorkspaceKind::Workstation,
                        WorkspaceKind::Bot => DesiredWorkspaceKind::Bot,
                    },
                    parent_workstation: workspace.parent_workstation,
                    home: workspace.home,
                    instructions: workspace.instructions.clone(),
                    owner_bot: workspace.owner_bot,
                    custom_icon: workspace.custom_icon.clone(),
                    bot: workspace.bot.clone(),
                    tabs: workspace
                        .tabs
                        .iter()
                        .map(|tab| {
                            Ok(DesiredTab {
                                id: tab.id,
                                title: tab.title.clone(),
                                custom_title: tab.custom_title.clone(),
                                project_dir: None,
                                color: tab.color,
                                custom_icon: tab.custom_icon.clone(),
                                parent_tab: None,
                                pinned: tab.pinned,
                                bot: None,
                                owner_bot: tab.owner_bot,
                                owner_thread: tab.owner_thread,
                                layout: DesiredLayout::from_runtime(
                                    &tab.layout,
                                    cwd_by_pane,
                                    tmux_by_pane,
                                    allow_offline,
                                    offline_panes,
                                )?,
                            })
                        })
                        .collect::<Result<_>>()?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            revision: snapshot.revision,
            appearance: snapshot.appearance.clone(),
            bots: snapshot.bots.clone(),
            assistant: None,
            tmux: RetiredTmuxSettings::default(),
            workspaces,
            legacy_tmux_workspace: HashMap::new(),
            gallery_copies: Vec::new(),
        })
    }

    fn into_runtime(self) -> RecoveredState {
        let mut appearance = self.appearance;
        if self.schema_version < DARK_GRAY_DEFAULTS_SCHEMA_VERSION {
            if appearance.default_terminal_accent == AppearanceColor::HARBOR_BLUE {
                appearance.default_terminal_accent = AppearanceColor::DARK_GRAY;
            }
            if appearance.default_workspace_color == AppearanceColor::HARBOR_BLUE {
                appearance.default_workspace_color = AppearanceColor::DARK_GRAY;
            }
        }
        let mut cwd_by_pane = HashMap::new();
        let mut tmux_by_pane = HashMap::new();
        let mut offline_panes = HashSet::new();
        let workspaces = self
            .workspaces
            .into_iter()
            .map(|workspace| {
                let tabs = workspace
                    .tabs
                    .into_iter()
                    .map(|tab| Tab {
                        layout: tab.layout.into_runtime(
                            &mut cwd_by_pane,
                            &mut tmux_by_pane,
                            &mut offline_panes,
                        ),
                        id: tab.id,
                        title: tab.title,
                        custom_title: tab.custom_title,
                        color: tab.color,
                        custom_icon: tab.custom_icon,
                        pinned: tab.pinned,
                        owner_bot: tab.owner_bot,
                        owner_thread: tab.owner_thread,
                    })
                    .collect::<Vec<_>>();
                let bot = workspace.bot.map(|mut bot| {
                    // Threads of panes that did not survive live on only as
                    // saved sessions.
                    let mut live = Vec::new();
                    for tab in &tabs {
                        collect_pane_ids(&tab.layout, &mut live);
                    }
                    bot.thread_panes.retain(|pane_id, _| live.contains(pane_id));
                    bot
                });
                Workspace {
                    id: workspace.id,
                    title: workspace.title,
                    color: workspace.color,
                    pinned: workspace.pinned,
                    pin_order: workspace.pin_order,
                    order: workspace.order,
                    active_terminal_count: 0,
                    connection: match workspace.connection {
                        WorkspaceConnection::Local => WorkspaceConnection::Local,
                        WorkspaceConnection::SystemSsh { destination, .. } => {
                            WorkspaceConnection::SystemSsh {
                                destination,
                                status: WorkspaceConnectionStatus::Offline,
                            }
                        }
                    },
                    working_dir: workspace.working_dir,
                    kind: match workspace.kind {
                        DesiredWorkspaceKind::Workstation => WorkspaceKind::Workstation,
                        DesiredWorkspaceKind::Bot => WorkspaceKind::Bot,
                        DesiredWorkspaceKind::Bots | DesiredWorkspaceKind::Assistant => {
                            unreachable!("legacy workspaces are migrated before recovery")
                        }
                    },
                    parent_workstation: workspace.parent_workstation,
                    home: workspace.home,
                    instructions: workspace.instructions,
                    owner_bot: workspace.owner_bot,
                    custom_icon: workspace.custom_icon,
                    bot,
                    tabs,
                }
            })
            .collect();
        RecoveredState {
            snapshot: SessionSnapshot {
                revision: self.revision.saturating_add(1),
                appearance,
                bots: self.bots,
                terminal_transports: std::collections::HashMap::new(),
                workspaces,
            },
            cwd_by_pane,
            tmux_by_pane,
            offline_panes,
            legacy_tmux_workspace: self.legacy_tmux_workspace,
            gallery_copies: self.gallery_copies,
        }
    }

    fn validate(&self) -> Result<()> {
        if !(MIN_SUPPORTED_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&self.schema_version) {
            bail!(
                "unsupported recovery schema {}, expected {MIN_SUPPORTED_SCHEMA_VERSION} to {SCHEMA_VERSION}",
                self.schema_version
            );
        }
        if self.appearance.recent_colors.len() > MAX_RECENT_COLORS {
            bail!("appearance recent colors exceed {MAX_RECENT_COLORS}");
        }
        let count_kind = |kind| {
            self.workspaces
                .iter()
                .filter(|workspace| workspace.kind == kind)
                .count()
        };
        let workstations = count_kind(DesiredWorkspaceKind::Workstation);
        if workstations == 0 || workstations > MAX_WORKSPACES {
            bail!("snapshot must contain 1 to {MAX_WORKSPACES} workstations");
        }
        self.validate_nesting()?;
        if count_kind(DesiredWorkspaceKind::Bots) > 0 {
            bail!("the legacy Bots workspace must be split before validation");
        }
        if count_kind(DesiredWorkspaceKind::Bot) > MAX_BOTS {
            bail!("snapshot must contain at most {MAX_BOTS} bots");
        }
        if count_kind(DesiredWorkspaceKind::Assistant) > 0 {
            bail!("legacy assistant workspaces must be dropped before validation");
        }
        let mut ids = HashSet::new();
        let mut panes = 0;
        for workspace in &self.workspaces {
            validate_id(workspace.id, &mut ids)?;
            validate_title(&workspace.title, "workspace")?;
            match &workspace.connection {
                WorkspaceConnection::SystemSsh { destination, .. } => {
                    validate_ssh_host(destination).map_err(anyhow::Error::from)?;
                }
                WorkspaceConnection::Local => {}
            }
            if let Some(working_dir) = &workspace.working_dir {
                validate_workspace_dir(working_dir).map_err(anyhow::Error::from)?;
            }
            if let Some(icon) = &workspace.custom_icon {
                validate_custom_icon_id(icon)?;
            }
            if workspace
                .instructions
                .as_deref()
                .is_some_and(|instructions| instructions.chars().count() > MAX_INSTRUCTIONS_CHARS)
            {
                bail!("workstation instructions too long");
            }
            if workspace.tabs.len() > MAX_TABS_PER_WORKSPACE {
                bail!("workstation must contain at most {MAX_TABS_PER_WORKSPACE} tabs");
            }
            if workspace.bot.is_some() != (workspace.kind == DesiredWorkspaceKind::Bot) {
                bail!(
                    "workspace {} must carry a bot exactly when it is a bot",
                    workspace.id
                );
            }
            if let Some(bot) = &workspace.bot {
                validate_bot(bot)?;
            }
            for tab in &workspace.tabs {
                validate_id(tab.id, &mut ids)?;
                validate_title(&tab.title, "tab")?;
                if let Some(name) = &tab.custom_title {
                    validate_title(name, "tab")?;
                }
                if tab.project_dir.is_some() || tab.parent_tab.is_some() {
                    bail!("project tab {} must be migrated before validation", tab.id);
                }
                if let Some(icon) = &tab.custom_icon {
                    validate_custom_icon_id(icon)?;
                }
                if tab.bot.is_some() {
                    bail!(
                        "legacy bot tab {} must be migrated before validation",
                        tab.id
                    );
                }
                tab.layout.validate(1, &mut ids, &mut panes)?;
            }
        }
        if panes > MAX_PANES {
            bail!("snapshot must contain at most {MAX_PANES} panes");
        }
        Ok(())
    }

    /// Checks the home workstation and the nesting tree: exactly one home
    /// (local, top-level, not a bot); every parent exists and is a
    /// workstation on the same machine; no cycles; depth within the limit.
    fn validate_nesting(&self) -> Result<()> {
        let by_id = self
            .workspaces
            .iter()
            .map(|workspace| (workspace.id, workspace))
            .collect::<HashMap<_, _>>();
        let mut homes = 0;
        for workspace in &self.workspaces {
            let is_workstation = workspace.kind == DesiredWorkspaceKind::Workstation;
            if workspace.home {
                homes += 1;
                if !is_workstation
                    || workspace.connection != WorkspaceConnection::Local
                    || workspace.parent_workstation.is_some()
                {
                    bail!("the home workstation must be a local top-level workstation");
                }
            }
            let Some(parent_id) = workspace.parent_workstation else {
                continue;
            };
            if !is_workstation || workspace.owner_bot.is_some() {
                bail!("workspace {} cannot be nested", workspace.id);
            }
            let parent = by_id
                .get(&parent_id)
                .filter(|parent| parent.kind == DesiredWorkspaceKind::Workstation)
                .with_context(|| format!("workstation {} has an invalid parent", workspace.id))?;
            let same_machine = match (&parent.connection, &workspace.connection) {
                (WorkspaceConnection::Local, WorkspaceConnection::Local) => true,
                (
                    WorkspaceConnection::SystemSsh {
                        destination: parent_destination,
                        ..
                    },
                    WorkspaceConnection::SystemSsh { destination, .. },
                ) => parent_destination == destination,
                _ => false,
            };
            if !same_machine {
                bail!(
                    "workstation {} must run on its parent's machine",
                    workspace.id
                );
            }
            let mut depth = 1;
            let mut current = Some(parent_id);
            while let Some(id) = current {
                depth += 1;
                if id == workspace.id || depth > MAX_WORKSTATION_DEPTH {
                    bail!(
                        "workstation {} nests deeper than {MAX_WORKSTATION_DEPTH} levels",
                        workspace.id
                    );
                }
                current = by_id.get(&id).and_then(|parent| parent.parent_workstation);
            }
        }
        if homes != 1 {
            bail!("snapshot must contain exactly one home workstation");
        }
        Ok(())
    }
}

/// Whether `title` is `<prefix> <number>`, e.g. a generated "Workstation 2".
fn numbered_title(title: &str, prefix: &str) -> bool {
    title
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix(' '))
        .is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

impl DesiredLayout {
    fn from_runtime(
        layout: &PaneLayout,
        cwd_by_pane: &HashMap<Uuid, PathBuf>,
        tmux_by_pane: &HashMap<Uuid, (String, String)>,
        allow_offline: bool,
        offline_panes: &HashSet<Uuid>,
    ) -> Result<Self> {
        Ok(match layout {
            PaneLayout::Leaf { pane } => Self::Leaf {
                pane: DesiredPane::from_runtime(
                    pane,
                    cwd_by_pane,
                    tmux_by_pane,
                    allow_offline || offline_panes.contains(&pane.id),
                )?,
            },
            PaneLayout::Stack { panes, active } => Self::Stack {
                panes: panes
                    .iter()
                    .map(|pane| {
                        DesiredPane::from_runtime(
                            pane,
                            cwd_by_pane,
                            tmux_by_pane,
                            allow_offline || offline_panes.contains(&pane.id),
                        )
                    })
                    .collect::<Result<_>>()?,
                active: *active,
            },
            PaneLayout::Split {
                axis,
                ratio,
                first,
                second,
            } => Self::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(Self::from_runtime(
                    first,
                    cwd_by_pane,
                    tmux_by_pane,
                    allow_offline,
                    offline_panes,
                )?),
                second: Box::new(Self::from_runtime(
                    second,
                    cwd_by_pane,
                    tmux_by_pane,
                    allow_offline,
                    offline_panes,
                )?),
            },
        })
    }

    fn into_runtime(
        self,
        cwd_by_pane: &mut HashMap<Uuid, PathBuf>,
        tmux_by_pane: &mut HashMap<Uuid, (String, String)>,
        offline_panes: &mut HashSet<Uuid>,
    ) -> PaneLayout {
        match self {
            Self::Leaf { pane } => PaneLayout::Leaf {
                pane: pane.into_runtime(cwd_by_pane, tmux_by_pane, offline_panes),
            },
            Self::Stack { panes, active } => PaneLayout::Stack {
                panes: panes
                    .into_iter()
                    .map(|pane| pane.into_runtime(cwd_by_pane, tmux_by_pane, offline_panes))
                    .collect(),
                active,
            },
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => PaneLayout::Split {
                axis,
                ratio,
                first: Box::new(first.into_runtime(cwd_by_pane, tmux_by_pane, offline_panes)),
                second: Box::new(second.into_runtime(cwd_by_pane, tmux_by_pane, offline_panes)),
            },
        }
    }

    /// Calls `visit` for every pane of this layout, in layout order.
    fn visit_panes(&self, visit: &mut impl FnMut(&DesiredPane)) {
        match self {
            Self::Leaf { pane } => visit(pane),
            Self::Stack { panes, .. } => panes.iter().for_each(visit),
            Self::Split { first, second, .. } => {
                first.visit_panes(visit);
                second.visit_panes(visit);
            }
        }
    }

    /// Every pane of this layout, in layout order.
    fn into_panes(self, panes: &mut Vec<DesiredPane>) {
        match self {
            Self::Leaf { pane } => panes.push(pane),
            Self::Stack { panes: stacked, .. } => panes.extend(stacked),
            Self::Split { first, second, .. } => {
                first.into_panes(panes);
                second.into_panes(panes);
            }
        }
    }

    /// This layout without legacy assistant panes; `None` when nothing remains.
    fn without_assistants(self) -> Option<Self> {
        match self {
            Self::Leaf { pane } => {
                (pane.kind != DesiredPaneKind::Assistant).then_some(Self::Leaf { pane })
            }
            Self::Stack { panes, active } => {
                let mut panes = panes
                    .into_iter()
                    .filter(|pane| pane.kind != DesiredPaneKind::Assistant)
                    .collect::<Vec<_>>();
                match panes.len() {
                    0 => None,
                    1 => panes.pop().map(|pane| Self::Leaf { pane }),
                    _ => {
                        let active = if panes.iter().any(|pane| pane.id == active) {
                            active
                        } else {
                            panes[0].id
                        };
                        Some(Self::Stack { panes, active })
                    }
                }
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.without_assistants(), second.without_assistants()) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            },
        }
    }

    fn validate(
        &self,
        depth: usize,
        ids: &mut HashSet<Uuid>,
        pane_count: &mut usize,
    ) -> Result<()> {
        if depth > MAX_LAYOUT_DEPTH {
            bail!("layout nesting exceeds {MAX_LAYOUT_DEPTH}");
        }
        match self {
            Self::Leaf { pane } => pane.validate(ids, pane_count),
            Self::Stack { panes, active } => {
                if panes.len() < 2 || panes.len() > MAX_PANES {
                    bail!("pane stack must contain 2 to {MAX_PANES} panes");
                }
                if !panes.iter().any(|pane| pane.id == *active) {
                    bail!("pane stack active ID is not present");
                }
                for pane in panes {
                    pane.validate(ids, pane_count)?;
                }
                Ok(())
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || !(0.05..=0.95).contains(ratio) {
                    bail!("split ratio must be finite and between 0.05 and 0.95");
                }
                first.validate(depth + 1, ids, pane_count)?;
                second.validate(depth + 1, ids, pane_count)
            }
        }
    }
}

impl DesiredPane {
    fn from_runtime(
        pane: &Pane,
        cwd_by_pane: &HashMap<Uuid, PathBuf>,
        tmux_by_pane: &HashMap<Uuid, (String, String)>,
        allow_offline: bool,
    ) -> Result<Self> {
        let local_cwd = pane
            .kind
            .is_terminal()
            .then(|| cwd_by_pane.get(&pane.id).cloned())
            .flatten();
        if matches!(pane.kind, PaneKind::Terminal) && local_cwd.is_none() && !allow_offline {
            bail!("pane {} has no safe local CWD metadata", pane.id);
        }
        Ok(Self {
            id: pane.id,
            kind: DesiredPaneKind::from(&pane.kind),
            title: pane
                .custom_title
                .clone()
                .unwrap_or_else(|| match &pane.kind {
                    PaneKind::Terminal => "Terminal".to_owned(),
                    PaneKind::Browser { .. } | PaneKind::Gallery => pane.title.clone(),
                }),
            color: pane.color,
            custom_title: pane.custom_title.clone(),
            profile_override: pane.profile_override,
            custom_icon: pane.custom_icon.clone(),
            local_cwd,
            tmux_window: tmux_by_pane.get(&pane.id).map(|(window, _)| window.clone()),
            tmux_pane: tmux_by_pane.get(&pane.id).map(|(_, pane)| pane.clone()),
            unseen: pane.unseen,
            progress: pane.progress.clone().map(Box::new),
        })
    }

    fn into_runtime(
        self,
        cwd_by_pane: &mut HashMap<Uuid, PathBuf>,
        tmux_by_pane: &mut HashMap<Uuid, (String, String)>,
        offline_panes: &mut HashSet<Uuid>,
    ) -> Pane {
        if self.kind == DesiredPaneKind::Terminal {
            if let Some(local_cwd) = self.local_cwd {
                cwd_by_pane.insert(self.id, local_cwd);
            } else {
                offline_panes.insert(self.id);
            }
        }
        if let (Some(window_id), Some(pane_id)) = (&self.tmux_window, &self.tmux_pane) {
            tmux_by_pane.insert(self.id, (window_id.clone(), pane_id.clone()));
        }
        let kind = self.kind.into_runtime();
        let custom_title = self.custom_title.or_else(|| {
            kind.is_terminal()
                .then(|| legacy_custom_title(&self.title))
                .flatten()
        });
        let title = custom_title
            .clone()
            .or_else(|| match &kind {
                PaneKind::Terminal => self
                    .profile_override
                    .map(|profile| profile.display_name().to_owned()),
                PaneKind::Browser { .. } | PaneKind::Gallery => Some(self.title.clone()),
            })
            .unwrap_or_else(|| "Terminal".to_owned());
        Pane {
            id: self.id,
            kind,
            title,
            shell: String::new(),
            color: self.color,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            status_changed_at_ms: 0,
            custom_title,
            profile_override: self.profile_override,
            custom_icon: self.custom_icon,
            unseen: self.unseen,
            progress: self.progress.map(|progress| *progress),
        }
    }

    fn validate(&self, ids: &mut HashSet<Uuid>, pane_count: &mut usize) -> Result<()> {
        validate_id(self.id, ids)?;
        validate_title(&self.title, "pane")?;
        if let Some(custom_title) = &self.custom_title {
            validate_title(custom_title, "custom terminal")?;
        }
        if let Some(custom_icon) = &self.custom_icon {
            validate_custom_icon_id(custom_icon)?;
        }
        let has_tmux = match (&self.tmux_window, &self.tmux_pane) {
            (Some(window), Some(pane))
                if window.len() >= 2
                    && window.starts_with('@')
                    && window[1..].bytes().all(|byte| byte.is_ascii_digit())
                    && pane.len() >= 2
                    && pane.starts_with('%')
                    && pane[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                true
            }
            (None, None) => false,
            _ => bail!("persisted tmux pane target is invalid"),
        };
        if has_tmux && self.kind != DesiredPaneKind::Terminal {
            bail!("only terminal panes may persist tmux targets");
        }
        if let Some(progress) = &self.progress {
            if self.kind != DesiredPaneKind::Terminal {
                bail!("only terminal panes may persist task progress");
            }
            progress
                .validate()
                .map_err(|error| anyhow::anyhow!("persisted {error}"))?;
        }
        match &self.kind {
            DesiredPaneKind::Terminal => {}
            DesiredPaneKind::Browser { url } => {
                if url.len() > MAX_BROWSER_URL_LEN {
                    bail!("browser URL exceeds the {MAX_BROWSER_URL_LEN}-byte limit");
                }
                if url != "about:blank" {
                    let parsed =
                        url::Url::parse(url).context("persisted browser URL is invalid")?;
                    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                        bail!("persisted browser URL must be an HTTP(S) URL with a host");
                    }
                }
                if url.chars().any(char::is_whitespace) || url.chars().any(char::is_control) {
                    bail!("persisted browser URL contains invalid whitespace");
                }
                if self.local_cwd.is_some() {
                    bail!("browser panes may not persist terminal CWD metadata");
                }
            }
            DesiredPaneKind::Assistant => {
                bail!("legacy assistant panes must be dropped before validation");
            }
            DesiredPaneKind::Gallery => {
                if self.local_cwd.is_some() {
                    bail!("gallery panes may not persist terminal CWD metadata");
                }
            }
        }
        if let Some(local_cwd) = &self.local_cwd {
            if !local_cwd.is_absolute() {
                bail!("local CWD must be absolute");
            }
            if local_cwd.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES {
                bail!("local CWD exceeds {MAX_PATH_BYTES} bytes");
            }
        }
        *pane_count += 1;
        if *pane_count > MAX_PANES {
            bail!("snapshot exceeds {MAX_PANES} panes");
        }
        Ok(())
    }
}

fn legacy_custom_title(title: &str) -> Option<String> {
    let generated = title == "Terminal"
        || title.strip_prefix("Terminal ").is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        });
    (!generated).then(|| title.to_owned())
}

/// Most pinned or live threads a persisted bot may list.
const MAX_BOT_THREAD_ENTRIES: usize = 500;

fn validate_bot(bot: &BotSpec) -> Result<()> {
    if bot
        .instructions
        .as_deref()
        .is_some_and(|instructions| instructions.chars().count() > MAX_INSTRUCTIONS_CHARS)
    {
        bail!("bot instructions too long");
    }
    if let Some(home) = bot.home.as_deref() {
        validate_workspace_dir(home).map_err(anyhow::Error::from)?;
    }
    if bot.pinned_threads.len() > MAX_BOT_THREAD_ENTRIES
        || bot.thread_panes.len() > MAX_BOT_THREAD_ENTRIES
    {
        bail!("bot lists too many threads");
    }
    let sessions = bot
        .thread_panes
        .values()
        .filter_map(|thread| thread.session.as_deref());
    if !bot
        .pinned_threads
        .iter()
        .map(String::as_str)
        .chain(sessions)
        .all(valid_session_id)
    {
        bail!("bot thread id is invalid");
    }
    Ok(())
}

fn validate_id(id: Uuid, ids: &mut HashSet<Uuid>) -> Result<()> {
    if id.is_nil() {
        bail!("IDs may not be nil");
    }
    if !ids.insert(id) {
        bail!("duplicate ID {id}");
    }
    Ok(())
}

pub(crate) fn validate_title(title: &str, kind: &str) -> Result<()> {
    let length = title.chars().count();
    if length == 0 || length > MAX_TITLE_CHARS || title.chars().any(char::is_control) {
        bail!("{kind} title must be 1 to {MAX_TITLE_CHARS} visible characters");
    }
    Ok(())
}

pub(super) fn validate_custom_icon_id(icon: &str) -> Result<()> {
    let Some((stem, extension)) = icon.split_once('.') else {
        bail!("custom icon ID is malformed");
    };
    if Uuid::parse_str(stem).is_err()
        || !matches!(extension, "png" | "jpg" | "webp" | "gif")
        || icon.contains(['/', '\\'])
    {
        bail!("custom icon ID is malformed");
    }
    Ok(())
}

#[cfg(test)]
#[path = "persistence_tests.rs"]
mod tests;
