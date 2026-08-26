use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use hh_protocol::{
    AppearanceColor, AppearanceSettings, MAX_BROWSER_URL_LEN, Pane, PaneKind, PaneLayout,
    SessionSnapshot, SplitAxis, Tab, TerminalIdentity, TerminalProfile, Workspace,
    WorkspaceConnection, WorkspaceConnectionStatus, validate_ssh_host, validate_workspace_dir,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const SCHEMA_VERSION: u16 = 10;
const MIN_SUPPORTED_SCHEMA_VERSION: u16 = 1;
const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024;
pub(crate) const MAX_WORKSPACES: usize = 16;
pub(crate) const MAX_TABS_PER_WORKSPACE: usize = 32;
const MAX_PANES: usize = 32;
const MAX_LAYOUT_DEPTH: usize = 16;
pub(crate) const MAX_TITLE_CHARS: usize = 80;
const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_RECENT_COLORS: usize = 8;

#[derive(Clone, Debug)]
pub(crate) struct RecoveredState {
    pub snapshot: SessionSnapshot,
    pub cwd_by_pane: HashMap<Uuid, PathBuf>,
    pub offline_panes: HashSet<Uuid>,
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
        let desired: DesiredState =
            serde_json::from_slice(&bytes).context("decode recovery snapshot")?;
        desired.validate()?;
        Ok(desired.into_runtime())
    }

    pub(crate) fn encode_with_offline(
        snapshot: &SessionSnapshot,
        cwd_by_pane: &HashMap<Uuid, PathBuf>,
        offline_panes: &HashSet<Uuid>,
    ) -> Result<Vec<u8>> {
        let desired = DesiredState::from_runtime(snapshot, cwd_by_pane, offline_panes)?;
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
        let bytes = Self::encode_with_offline(snapshot, cwd_by_pane, &HashSet::new())?;
        self.write_snapshot(&bytes)
    }

    fn quarantine(&self) -> Result<PathBuf> {
        let parent = self.path.parent().context("snapshot path has no parent")?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let quarantined = parent.join(format!(
            "sessions.corrupt-{timestamp}-{}.json",
            Uuid::new_v4()
        ));
        fs::rename(&self.path, &quarantined).with_context(|| {
            format!(
                "quarantine corrupt snapshot {} as {}",
                self.path.display(),
                quarantined.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&quarantined)
            .context("inspect quarantined snapshot without following links")?;
        if metadata.is_file() {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&quarantined)
                .context("open quarantined snapshot without following links")?;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .context("restrict opened quarantined snapshot")?;
        }
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .context("sync quarantine directory")?;
        Ok(quarantined)
    }

    #[cfg(test)]
    pub(crate) fn inject_failure_before_replace(&self, enabled: bool) {
        self.fail_before_replace
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }
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
    #[serde(default, skip_serializing)]
    #[expect(dead_code, reason = "parsed only so pre-removal snapshots still load")]
    tmux: RetiredTmuxSettings,
    workspaces: Vec<DesiredWorkspace>,
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
    tabs: Vec<DesiredTab>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredTab {
    id: Uuid,
    title: String,
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    color: Option<AppearanceColor>,
    #[serde(default)]
    custom_icon: Option<String>,
    #[serde(default)]
    parent_tab: Option<Uuid>,
    #[serde(default)]
    pinned: bool,
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
    kind: PaneKind,
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
}

impl DesiredState {
    fn from_runtime(
        snapshot: &SessionSnapshot,
        cwd_by_pane: &HashMap<Uuid, PathBuf>,
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
                    tabs: workspace
                        .tabs
                        .iter()
                        .map(|tab| {
                            Ok(DesiredTab {
                                id: tab.id,
                                title: tab.title.clone(),
                                custom_title: tab.custom_title.clone(),
                                project_dir: tab.project_dir.clone(),
                                color: tab.color,
                                custom_icon: tab.custom_icon.clone(),
                                parent_tab: tab.parent_tab,
                                pinned: tab.pinned,
                                layout: DesiredLayout::from_runtime(
                                    &tab.layout,
                                    cwd_by_pane,
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
            tmux: RetiredTmuxSettings::default(),
            workspaces,
        })
    }

    fn into_runtime(self) -> RecoveredState {
        let mut appearance = self.appearance;
        if self.schema_version < SCHEMA_VERSION {
            if appearance.default_terminal_accent == AppearanceColor::HARBOR_BLUE {
                appearance.default_terminal_accent = AppearanceColor::DARK_GRAY;
            }
            if appearance.default_workspace_color == AppearanceColor::HARBOR_BLUE {
                appearance.default_workspace_color = AppearanceColor::DARK_GRAY;
            }
        }
        let mut cwd_by_pane = HashMap::new();
        let mut offline_panes = HashSet::new();
        let workspaces = self
            .workspaces
            .into_iter()
            .map(|workspace| Workspace {
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
                tabs: workspace
                    .tabs
                    .into_iter()
                    .map(|tab| Tab {
                        id: tab.id,
                        title: tab.title,
                        custom_title: tab.custom_title,
                        project_dir: tab.project_dir,
                        color: tab.color,
                        custom_icon: tab.custom_icon,
                        parent_tab: tab.parent_tab,
                        pinned: tab.pinned,
                        layout: tab
                            .layout
                            .into_runtime(&mut cwd_by_pane, &mut offline_panes),
                    })
                    .collect(),
            })
            .collect();
        RecoveredState {
            snapshot: SessionSnapshot {
                revision: self.revision.saturating_add(1),
                appearance,
                workspaces,
            },
            cwd_by_pane,
            offline_panes,
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
        if self.workspaces.is_empty() || self.workspaces.len() > MAX_WORKSPACES {
            bail!("snapshot must contain 1 to {MAX_WORKSPACES} workstations");
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
            if workspace.tabs.len() > MAX_TABS_PER_WORKSPACE {
                bail!("workstation must contain at most {MAX_TABS_PER_WORKSPACE} tabs");
            }
            let tabs_by_id = workspace
                .tabs
                .iter()
                .map(|tab| (tab.id, tab))
                .collect::<HashMap<_, _>>();
            for tab in &workspace.tabs {
                validate_id(tab.id, &mut ids)?;
                validate_title(&tab.title, "tab")?;
                if let Some(name) = &tab.custom_title {
                    validate_title(name, "group")?;
                }
                if let Some(project_dir) = &tab.project_dir {
                    validate_workspace_dir(project_dir).map_err(anyhow::Error::from)?;
                }
                if let Some(icon) = &tab.custom_icon {
                    validate_custom_icon_id(icon)?;
                }
                if let Some(parent_id) = tab.parent_tab {
                    let valid_parent = parent_id != tab.id
                        && tab.project_dir.is_none()
                        && tabs_by_id.get(&parent_id).is_some_and(|parent| {
                            parent.parent_tab.is_none() && parent.project_dir.is_some()
                        });
                    if !valid_parent {
                        bail!("tab {} has an invalid parent tab", tab.id);
                    }
                }
                tab.layout.validate(1, &mut ids, &mut panes)?;
            }
        }
        if panes > MAX_PANES {
            bail!("snapshot must contain at most {MAX_PANES} panes");
        }
        Ok(())
    }
}

impl DesiredLayout {
    fn from_runtime(
        layout: &PaneLayout,
        cwd_by_pane: &HashMap<Uuid, PathBuf>,
        allow_offline: bool,
        offline_panes: &HashSet<Uuid>,
    ) -> Result<Self> {
        Ok(match layout {
            PaneLayout::Leaf { pane } => Self::Leaf {
                pane: DesiredPane::from_runtime(
                    pane,
                    cwd_by_pane,
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
                    allow_offline,
                    offline_panes,
                )?),
                second: Box::new(Self::from_runtime(
                    second,
                    cwd_by_pane,
                    allow_offline,
                    offline_panes,
                )?),
            },
        })
    }

    fn into_runtime(
        self,
        cwd_by_pane: &mut HashMap<Uuid, PathBuf>,
        offline_panes: &mut HashSet<Uuid>,
    ) -> PaneLayout {
        match self {
            Self::Leaf { pane } => PaneLayout::Leaf {
                pane: pane.into_runtime(cwd_by_pane, offline_panes),
            },
            Self::Stack { panes, active } => PaneLayout::Stack {
                panes: panes
                    .into_iter()
                    .map(|pane| pane.into_runtime(cwd_by_pane, offline_panes))
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
                first: Box::new(first.into_runtime(cwd_by_pane, offline_panes)),
                second: Box::new(second.into_runtime(cwd_by_pane, offline_panes)),
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
        allow_offline: bool,
    ) -> Result<Self> {
        let local_cwd = matches!(pane.kind, PaneKind::Terminal)
            .then(|| cwd_by_pane.get(&pane.id).cloned())
            .flatten();
        if matches!(pane.kind, PaneKind::Terminal) && local_cwd.is_none() && !allow_offline {
            bail!("pane {} has no safe local CWD metadata", pane.id);
        }
        Ok(Self {
            id: pane.id,
            kind: pane.kind.clone(),
            title: pane
                .custom_title
                .clone()
                .unwrap_or_else(|| match &pane.kind {
                    PaneKind::Terminal => "Terminal".to_owned(),
                    PaneKind::Browser { .. } | PaneKind::Assistant => pane.title.clone(),
                }),
            color: pane.color,
            custom_title: pane.custom_title.clone(),
            profile_override: pane.profile_override,
            custom_icon: pane.custom_icon.clone(),
            local_cwd,
        })
    }

    fn into_runtime(
        self,
        cwd_by_pane: &mut HashMap<Uuid, PathBuf>,
        offline_panes: &mut HashSet<Uuid>,
    ) -> Pane {
        if matches!(self.kind, PaneKind::Terminal) {
            if let Some(local_cwd) = self.local_cwd {
                cwd_by_pane.insert(self.id, local_cwd);
            } else {
                offline_panes.insert(self.id);
            }
        }
        let custom_title = self.custom_title.or_else(|| {
            matches!(self.kind, PaneKind::Terminal)
                .then(|| legacy_custom_title(&self.title))
                .flatten()
        });
        let title = custom_title
            .clone()
            .or_else(|| match &self.kind {
                PaneKind::Terminal => self
                    .profile_override
                    .map(|profile| profile.display_name().to_owned()),
                PaneKind::Browser { .. } | PaneKind::Assistant => Some(self.title.clone()),
            })
            .unwrap_or_else(|| "Terminal".to_owned());
        Pane {
            id: self.id,
            kind: self.kind,
            title,
            shell: String::new(),
            color: self.color,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title,
            profile_override: self.profile_override,
            custom_icon: self.custom_icon,
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
        match &self.kind {
            PaneKind::Terminal => {}
            PaneKind::Browser { url } => {
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
            PaneKind::Assistant => {
                if self.local_cwd.is_some() {
                    bail!("assistant panes may not persist terminal CWD metadata");
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
mod tests {
    use super::*;

    fn test_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hh-{label}-{}", Uuid::new_v4()))
    }

    fn create_owner_only_directory(path: &Path) {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap();
    }

    fn cwd_map(snapshot: &SessionSnapshot) -> HashMap<Uuid, PathBuf> {
        let pane_id = match &snapshot.workspaces[0].tabs[0].layout {
            PaneLayout::Leaf { pane } => pane.id,
            _ => panic!("seeded snapshot should contain one leaf"),
        };
        HashMap::from([(pane_id, std::env::temp_dir())])
    }

    #[test]
    fn snapshot_contains_only_explicit_safe_desired_state() {
        let directory = test_directory("safe-schema");
        let path = directory.join("sessions.json");
        let store = SnapshotStore::new(path.clone());
        let snapshot = SessionSnapshot::seeded();
        store.save(&snapshot, &cwd_map(&snapshot)).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("local_cwd"));
        for forbidden in [
            "terminal_output",
            "identity",
            "identity_source",
            "environment",
            "process_id",
            "socket",
            "credential",
            "secret",
            "shell",
        ] {
            assert!(
                !text.contains(forbidden),
                "persisted forbidden field {forbidden}"
            );
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deliberately_empty_local_workspace_round_trips_without_creating_a_terminal() {
        let directory = test_directory("empty-local");
        let store = SnapshotStore::new(directory.join("sessions.json"));
        let mut snapshot = SessionSnapshot::seeded();
        snapshot.workspaces[0].tabs.clear();
        snapshot.workspaces[0].active_terminal_count = 0;

        store.save(&snapshot, &HashMap::new()).unwrap();
        let recovered = store.load().unwrap();

        assert_eq!(recovered.snapshot.workspaces.len(), 1);
        assert!(recovered.snapshot.workspaces[0].tabs.is_empty());
        assert!(recovered.cwd_by_pane.is_empty());
        assert!(recovered.offline_panes.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ssh_workspace_layout_recovers_offline_without_runtime_or_secret_material() {
        let directory = test_directory("ssh-layout");
        let path = directory.join("sessions.json");
        let store = SnapshotStore::new(path.clone());
        let mut snapshot = SessionSnapshot::seeded();
        let workspace = &mut snapshot.workspaces[0];
        let first = match &workspace.tabs[0].layout {
            PaneLayout::Leaf { pane } => pane.clone(),
            _ => panic!("seeded snapshot should contain one leaf"),
        };
        let second = Pane {
            id: Uuid::new_v4(),
            kind: hh_protocol::PaneKind::Terminal,
            title: "Remote two".to_owned(),
            shell: "ssh".to_owned(),
            color: None,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let first_id = first.id;
        let second_id = second.id;
        workspace.title = "Tailnet build".to_owned();
        workspace.pinned = true;
        workspace.pin_order = 1;
        workspace.connection = WorkspaceConnection::SystemSsh {
            destination: "admin@build-node".to_owned(),
            status: WorkspaceConnectionStatus::Connected,
        };
        workspace.tabs[0].layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.4,
            first: Box::new(PaneLayout::Leaf { pane: first }),
            second: Box::new(PaneLayout::Leaf {
                pane: second.clone(),
            }),
        };

        store.save(&snapshot, &HashMap::new()).unwrap();
        let recovered = store.load_or_quarantine().unwrap().unwrap();
        let recovered_workspace = &recovered.snapshot.workspaces[0];

        assert_eq!(recovered_workspace.title, "Tailnet build");
        assert!(recovered_workspace.pinned);
        assert_eq!(recovered_workspace.pin_order, 1);
        let PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } = &recovered_workspace.tabs[0].layout
        else {
            panic!("saved SSH layout did not retain its split shape");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert!((*ratio - 0.4).abs() < f32::EPSILON);
        assert!(matches!(first.as_ref(), PaneLayout::Leaf { pane } if pane.id == first_id));
        assert!(matches!(second.as_ref(), PaneLayout::Leaf { pane } if pane.id == second_id));
        assert_eq!(
            recovered_workspace.connection,
            WorkspaceConnection::SystemSsh {
                destination: "admin@build-node".to_owned(),
                status: WorkspaceConnectionStatus::Offline,
            }
        );
        assert_eq!(recovered.offline_panes.len(), 2);
        assert!(recovered.offline_panes.contains(&second_id));
        let text = fs::read_to_string(path).unwrap();
        for forbidden in ["password", "private_key", "agent_material", "known_hosts"] {
            assert!(!text.contains(forbidden));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_replace_preserves_last_complete_snapshot() {
        let directory = test_directory("atomic-fault");
        let path = directory.join("sessions.json");
        let store = SnapshotStore::new(path.clone());
        let mut snapshot = SessionSnapshot::seeded();
        let cwd_by_pane = cwd_map(&snapshot);
        store.save(&snapshot, &cwd_by_pane).unwrap();
        let original = fs::read(&path).unwrap();

        snapshot.revision = 42;
        store.inject_failure_before_replace(true);
        assert!(store.save(&snapshot, &cwd_by_pane).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            1
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn appearance_defaults_and_overrides_round_trip_with_old_snapshot_fallback() {
        let directory = test_directory("appearance-round-trip");
        let path = directory.join("sessions.json");
        let store = SnapshotStore::new(path);
        let mut snapshot = SessionSnapshot::seeded();
        snapshot.appearance.default_terminal_accent = AppearanceColor::new(0x95, 0xcc, 0x7f);
        snapshot.appearance.default_workspace_color = AppearanceColor::new(0xc9, 0x90, 0xe5);
        snapshot.appearance.recent_colors = vec![AppearanceColor::new(0xef, 0x71, 0x7a)];
        snapshot.workspaces[0].color = Some(AppearanceColor::new(0xe4, 0xbd, 0x72));
        let PaneLayout::Leaf { pane } = &mut snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected leaf");
        };
        pane.color = Some(AppearanceColor::new(0x67, 0xc8, 0xc6));
        pane.title = "Live-detected Claude".to_owned();
        pane.identity = hh_protocol::TerminalIdentity {
            profile: TerminalProfile::Claude,
            source: hh_protocol::TerminalIdentitySource::Command,
        };
        pane.status = hh_protocol::PaneStatus::Working;
        pane.custom_title = Some("Release shell".to_owned());
        pane.profile_override = Some(TerminalProfile::Gemini);
        pane.custom_icon = Some("00000000-0000-4000-8000-000000000001.png".to_owned());

        store.save(&snapshot, &cwd_map(&snapshot)).unwrap();
        let recovered = store.load().unwrap().snapshot;

        assert_eq!(recovered.appearance, snapshot.appearance);
        assert_eq!(recovered.workspaces[0].color, snapshot.workspaces[0].color);
        let PaneLayout::Leaf {
            pane: recovered_pane,
        } = &recovered.workspaces[0].tabs[0].layout
        else {
            panic!("expected recovered leaf");
        };
        assert_eq!(
            recovered_pane.color,
            Some(AppearanceColor::new(0x67, 0xc8, 0xc6))
        );
        assert_eq!(recovered_pane.title, "Release shell");
        assert_eq!(
            recovered_pane.custom_title.as_deref(),
            Some("Release shell")
        );
        assert_eq!(
            recovered_pane.profile_override,
            Some(TerminalProfile::Gemini)
        );
        assert_eq!(
            recovered_pane.custom_icon.as_deref(),
            Some("00000000-0000-4000-8000-000000000001.png")
        );
        assert_eq!(recovered_pane.identity, TerminalIdentity::default());
        assert_eq!(recovered_pane.status, hh_protocol::PaneStatus::Idle);

        let old: DesiredState = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "revision": 1,
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000011",
                    "title": "Old workspace",
                    "tabs": [{
                        "id": "00000000-0000-0000-0000-000000000012",
                        "title": "Terminals",
                        "layout": {
                            "kind": "leaf",
                            "pane": {
                                "id": "00000000-0000-0000-0000-000000000013",
                                "title": "Terminal 1",
                                "local_cwd": "/tmp"
                            }
                        }
                    }]
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(old.appearance, AppearanceSettings::default());
        assert_eq!(old.workspaces[0].color, None);
        let old_runtime = old.into_runtime().snapshot;
        let PaneLayout::Leaf { pane: old_pane } = &old_runtime.workspaces[0].tabs[0].layout else {
            panic!("expected old leaf");
        };
        assert_eq!(old_pane.custom_title, None);
        assert_eq!(old_pane.profile_override, None);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn schema_six_harbor_blue_defaults_migrate_to_dark_gray() {
        let snapshot = SessionSnapshot::seeded();
        let mut desired =
            DesiredState::from_runtime(&snapshot, &cwd_map(&snapshot), &HashSet::new()).unwrap();
        desired.schema_version = 6;
        desired.appearance.default_terminal_accent = AppearanceColor::HARBOR_BLUE;
        desired.appearance.default_workspace_color = AppearanceColor::HARBOR_BLUE;

        let recovered = desired.into_runtime().snapshot;

        assert_eq!(
            recovered.appearance.default_terminal_accent,
            AppearanceColor::DARK_GRAY
        );
        assert_eq!(
            recovered.appearance.default_workspace_color,
            AppearanceColor::DARK_GRAY
        );
    }

    #[test]
    fn schema_v1_custom_names_migrate_to_explicit_overrides() {
        let desired: DesiredState = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "revision": 4,
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000021",
                    "title": "Workspace",
                    "tabs": [{
                        "id": "00000000-0000-0000-0000-000000000022",
                        "title": "Terminals",
                        "layout": {
                            "kind": "leaf",
                            "pane": {
                                "id": "00000000-0000-0000-0000-000000000023",
                                "title": "Deploy console",
                                "local_cwd": "/tmp"
                            }
                        }
                    }]
                }]
            }"#,
        )
        .unwrap();

        let runtime = desired.into_runtime().snapshot;
        let PaneLayout::Leaf { pane } = &runtime.workspaces[0].tabs[0].layout else {
            panic!("expected leaf");
        };
        assert_eq!(pane.custom_title.as_deref(), Some("Deploy console"));
        assert_eq!(pane.title, "Deploy console");
    }

    #[test]
    fn schema_v4_snapshot_with_retired_tmux_setting_loads_and_stops_being_written() {
        let stored: DesiredState = serde_json::from_str(
            r#"{
                "schema_version": 4,
                "revision": 7,
                "tmux": {"hide_status_bar": true},
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000031",
                    "title": "Workstation",
                    "tabs": [{
                        "id": "00000000-0000-0000-0000-000000000032",
                        "title": "Terminals",
                        "layout": {
                            "kind": "leaf",
                            "pane": {
                                "id": "00000000-0000-0000-0000-000000000033",
                                "title": "Terminal 1",
                                "local_cwd": "/tmp"
                            }
                        }
                    }]
                }]
            }"#,
        )
        .unwrap();
        stored.validate().unwrap();

        let recovered = stored.into_runtime();
        assert_eq!(recovered.snapshot.workspaces[0].title, "Workstation");

        let rewritten = DesiredState::from_runtime(
            &recovered.snapshot,
            &recovered.cwd_by_pane,
            &HashSet::new(),
        )
        .unwrap();
        let encoded = serde_json::to_string(&rewritten).unwrap();
        assert!(!encoded.contains("tmux"), "encoded: {encoded}");
        assert!(!encoded.contains("hide_status_bar"), "encoded: {encoded}");
        serde_json::from_str::<DesiredState>(&encoded).unwrap();
    }

    #[test]
    fn corrupt_or_unknown_state_is_quarantined() {
        let directory = test_directory("quarantine");
        create_owner_only_directory(&directory);
        let path = directory.join("sessions.json");
        fs::write(
            &path,
            br#"{"schema_version":999,"revision":0,"workspaces":[]}"#,
        )
        .unwrap();
        let store = SnapshotStore::new(path.clone());

        assert!(store.load_or_quarantine().unwrap().is_none());
        assert!(!path.exists());
        assert!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("sessions.corrupt-")
                })
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn symlink_snapshot_is_quarantined_without_following_its_target() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("symlink-quarantine");
        create_owner_only_directory(&directory);
        let target = directory.join("outside-target");
        fs::write(&target, b"do not touch").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let path = directory.join("sessions.json");
        symlink(&target, &path).unwrap();
        let store = SnapshotStore::new(path.clone());

        assert!(store.load_or_quarantine().unwrap().is_none());
        assert_eq!(fs::read(&target).unwrap(), b"do not touch");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!path.exists());
        assert!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry.file_type().is_ok_and(|kind| kind.is_symlink())
                        && entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("sessions.corrupt-")
                })
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_ratio_and_duplicate_ids_are_rejected() {
        let snapshot = SessionSnapshot::seeded();
        let mut desired =
            DesiredState::from_runtime(&snapshot, &cwd_map(&snapshot), &HashSet::new()).unwrap();
        let pane = match &desired.workspaces[0].tabs[0].layout {
            DesiredLayout::Leaf { pane } => pane.clone(),
            _ => panic!("expected leaf"),
        };
        desired.workspaces[0].tabs[0].layout = DesiredLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: f32::NAN,
            first: Box::new(DesiredLayout::Leaf { pane: pane.clone() }),
            second: Box::new(DesiredLayout::Leaf { pane }),
        };
        assert!(desired.validate().is_err());
    }
}
