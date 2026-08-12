use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const SCHEMA_VERSION: u16 = 1;
const MAX_UI_STATE_BYTES: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedUiState {
    schema_version: u16,
    workspace_sidebar_width: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct UiStateStore {
    path: PathBuf,
}

impl UiStateStore {
    pub(crate) fn from_default_path() -> Result<Self> {
        Ok(Self {
            path: default_ui_state_path()?,
        })
    }

    #[cfg(test)]
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn load_workspace_sidebar_width(&self) -> Result<Option<f32>> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect UI state {}", self.path.display()));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("UI state must be a regular file");
        }
        if metadata.len() > MAX_UI_STATE_BYTES {
            bail!("UI state exceeds {MAX_UI_STATE_BYTES} bytes");
        }

        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        OpenOptions::new()
            .read(true)
            .open(&self.path)
            .with_context(|| format!("open UI state {}", self.path.display()))?
            .take(MAX_UI_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("read UI state")?;
        if bytes.len() as u64 > MAX_UI_STATE_BYTES {
            bail!("UI state exceeds {MAX_UI_STATE_BYTES} bytes");
        }

        let state: PersistedUiState = serde_json::from_slice(&bytes).context("decode UI state")?;
        if state.schema_version != SCHEMA_VERSION {
            bail!("unsupported UI state schema {}", state.schema_version);
        }
        if !state.workspace_sidebar_width.is_finite() || state.workspace_sidebar_width <= 0.0 {
            bail!("workspace sidebar width must be finite and positive");
        }
        Ok(Some(state.workspace_sidebar_width))
    }

    pub(crate) fn save_workspace_sidebar_width(&self, width: f32) -> Result<()> {
        if !width.is_finite() || width <= 0.0 {
            bail!("workspace sidebar width must be finite and positive");
        }
        let state = PersistedUiState {
            schema_version: SCHEMA_VERSION,
            workspace_sidebar_width: width,
        };
        let bytes = serde_json::to_vec(&state).context("encode UI state")?;
        let parent = self
            .path
            .parent()
            .context("UI state path has no parent directory")?;
        ensure_private_directory(parent)?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("ui-state.json");
        let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
        let write_result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| format!("create temporary UI state {}", temporary.display()))?;
            file.write_all(&bytes).context("write UI state")?;
            file.sync_all().context("sync UI state contents")?;
            fs::rename(&temporary, &self.path).with_context(|| {
                format!(
                    "atomically replace {} with {}",
                    self.path.display(),
                    temporary.display()
                )
            })?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .context("sync UI state directory")?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        write_result
    }
}

fn default_ui_state_path() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("RUST_MUX_STATE_DIR") {
        return Ok(PathBuf::from(directory).join("ui-state.json"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    #[cfg(target_os = "macos")]
    let directory = home.join("Library/Application Support/Rust Mux");
    #[cfg(not(target_os = "macos"))]
    let directory = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"))
        .join("rust-mux");
    Ok(directory.join("ui-state.json"))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create UI state directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect UI state directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("UI state directory must be a real directory");
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict UI state directory {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(label: &str) -> (PathBuf, UiStateStore) {
        let root = std::env::temp_dir().join(format!("rust-mux-ui-{label}-{}", Uuid::new_v4()));
        let store = UiStateStore::new(root.join("ui-state.json"));
        (root, store)
    }

    #[test]
    fn sidebar_width_round_trips_across_store_instances() {
        let (root, store) = temp_store("round-trip");
        assert_eq!(store.load_workspace_sidebar_width().unwrap(), None);
        store.save_workspace_sidebar_width(287.5).unwrap();

        let restarted = UiStateStore::new(root.join("ui-state.json"));
        assert_eq!(
            restarted.load_workspace_sidebar_width().unwrap(),
            Some(287.5)
        );
        assert_eq!(
            fs::metadata(root.join("ui-state.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_or_unrecognized_state_is_rejected_without_overwriting_it() {
        let (root, store) = temp_store("invalid");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("ui-state.json"),
            br#"{"schema_version":2,"workspace_sidebar_width":220.0}"#,
        )
        .unwrap();

        assert!(store.load_workspace_sidebar_width().is_err());
        assert!(root.join("ui-state.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
