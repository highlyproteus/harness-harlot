use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use image::GenericImageView as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const SCHEMA_VERSION: u16 = 1;
const MAX_UI_STATE_BYTES: u64 = 16 * 1024;
const WORKSTATION_BANNER_FILE_NAME: &str = "workstation-banner.png";
const MAX_WORKSTATION_BANNER_BYTES: u64 = 12 * 1024 * 1024;
const MAX_WORKSTATION_BANNER_DIMENSION: u32 = 8_192;
const MAX_WORKSTATION_BANNER_PIXELS: u64 = 24_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedUiState {
    schema_version: u16,
    /// `None` when only visibility has ever been saved; the renderer then uses
    /// its own default width.
    #[serde(default)]
    workspace_sidebar_width: Option<f32>,
    #[serde(default)]
    workstation_banner_hidden: bool,
}

/// Canonical banner PNG plus the pixel dimensions the renderer needs to size
/// the rail header and settings preview without decoding the image again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredBanner {
    pub(crate) png: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
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

    /// `None` when no state file exists yet.
    fn read_state(&self) -> Result<Option<PersistedUiState>> {
        let bytes = match hh_protocol::read_private_file(&self.path, MAX_UI_STATE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read UI state {}", self.path.display()));
            }
        };
        let state: PersistedUiState = serde_json::from_slice(&bytes).context("decode UI state")?;
        if state.schema_version != SCHEMA_VERSION {
            bail!("unsupported UI state schema {}", state.schema_version);
        }
        if let Some(width) = state.workspace_sidebar_width
            && (!width.is_finite() || width <= 0.0)
        {
            bail!("workstation sidebar width must be finite and positive");
        }
        Ok(Some(state))
    }

    /// Rewrites the whole file, preserving every field this caller did not change.
    fn write_state(&self, state: PersistedUiState) -> Result<()> {
        let bytes = serde_json::to_vec(&state).context("encode UI state")?;
        atomic_write_private(&self.path, &bytes, "UI state")
    }

    pub(crate) fn load_workspace_sidebar_width(&self) -> Result<Option<f32>> {
        Ok(self
            .read_state()?
            .and_then(|state| state.workspace_sidebar_width))
    }

    pub(crate) fn save_workspace_sidebar_width(&self, width: f32) -> Result<()> {
        if !width.is_finite() || width <= 0.0 {
            bail!("workstation sidebar width must be finite and positive");
        }
        let mut state = self.read_state()?.unwrap_or(PersistedUiState {
            schema_version: SCHEMA_VERSION,
            workspace_sidebar_width: None,
            workstation_banner_hidden: false,
        });
        state.schema_version = SCHEMA_VERSION;
        state.workspace_sidebar_width = Some(width);
        self.write_state(state)
    }

    pub(crate) fn load_workstation_banner_hidden(&self) -> Result<bool> {
        Ok(self
            .read_state()?
            .is_some_and(|state| state.workstation_banner_hidden))
    }

    pub(crate) fn save_workstation_banner_hidden(&self, hidden: bool) -> Result<()> {
        let mut state = self.read_state()?.unwrap_or(PersistedUiState {
            schema_version: SCHEMA_VERSION,
            workspace_sidebar_width: None,
            workstation_banner_hidden: false,
        });
        state.schema_version = SCHEMA_VERSION;
        state.workstation_banner_hidden = hidden;
        self.write_state(state)
    }

    pub(crate) fn load_workstation_banner(&self) -> Result<Option<StoredBanner>> {
        let path = self.workstation_banner_path()?;
        let bytes = match hh_protocol::read_private_file(&path, MAX_WORKSTATION_BANNER_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read workstation banner {}", path.display()));
            }
        };
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            bail!("saved workstation banner is not a PNG");
        }
        let (width, height) = decode_workstation_banner(&bytes)?.dimensions();
        Ok(Some(StoredBanner {
            png: bytes,
            width,
            height,
        }))
    }

    pub(crate) fn import_workstation_banner(&self, source: &Path) -> Result<StoredBanner> {
        let metadata = fs::symlink_metadata(source).with_context(|| {
            format!("read workstation banner metadata from {}", source.display())
        })?;
        if !metadata.file_type().is_file() {
            bail!("workstation banner must be a regular file, not a symlink");
        }
        if metadata.len() > MAX_WORKSTATION_BANNER_BYTES {
            bail!("workstation banner must be 12 MiB or smaller");
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(source)
            .with_context(|| format!("open workstation banner {}", source.display()))?
            .take(MAX_WORKSTATION_BANNER_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read workstation banner from {}", source.display()))?;
        if bytes.len() as u64 > MAX_WORKSTATION_BANNER_BYTES {
            bail!("workstation banner must be 12 MiB or smaller");
        }
        let stored = canonical_workstation_banner(&bytes)?;
        let path = self.workstation_banner_path()?;
        atomic_write_private(&path, &stored.png, "workstation banner")?;
        Ok(stored)
    }

    pub(crate) fn reset_workstation_banner(&self) -> Result<()> {
        let path = self.workstation_banner_path()?;
        match fs::remove_file(&path) {
            Ok(()) => {
                let parent = path
                    .parent()
                    .context("workstation banner path has no parent directory")?;
                File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .context("sync workstation banner directory")
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("remove workstation banner {}", path.display()))
            }
        }
    }

    fn workstation_banner_path(&self) -> Result<PathBuf> {
        Ok(self
            .path
            .parent()
            .context("UI state path has no parent directory")?
            .join(WORKSTATION_BANNER_FILE_NAME))
    }
}

fn default_ui_state_path() -> Result<PathBuf> {
    let directory = hh_protocol::state_directory().context("HOME is not set")?;
    Ok(directory.join("ui-state.json"))
}
fn decode_workstation_banner(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("detect workstation banner image format")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_WORKSTATION_BANNER_DIMENSION);
    limits.max_image_height = Some(MAX_WORKSTATION_BANNER_DIMENSION);
    limits.max_alloc = Some(MAX_WORKSTATION_BANNER_PIXELS * 8);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .context("decode bounded workstation banner image")?;
    let (width, height) = decoded.dimensions();
    if width == 0
        || height == 0
        || width > MAX_WORKSTATION_BANNER_DIMENSION
        || height > MAX_WORKSTATION_BANNER_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_WORKSTATION_BANNER_PIXELS
    {
        bail!(
            "workstation banner dimensions must be at most {MAX_WORKSTATION_BANNER_DIMENSION}x{MAX_WORKSTATION_BANNER_DIMENSION} and {MAX_WORKSTATION_BANNER_PIXELS} pixels"
        );
    }
    Ok(decoded)
}

fn canonical_workstation_banner(bytes: &[u8]) -> Result<StoredBanner> {
    let decoded = decode_workstation_banner(bytes)?;
    let (width, height) = decoded.dimensions();
    let mut canonical = Cursor::new(Vec::new());
    decoded
        .write_to(&mut canonical, image::ImageFormat::Png)
        .context("encode canonical workstation banner PNG")?;
    let canonical = canonical.into_inner();
    if canonical.len() as u64 > MAX_WORKSTATION_BANNER_BYTES {
        bail!("encoded workstation banner must be 12 MiB or smaller");
    }
    Ok(StoredBanner {
        png: canonical,
        width,
        height,
    })
}

fn atomic_write_private(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} path has no parent directory"))?;
    ensure_private_directory(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("private-state");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create temporary {label} {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {label}"))?;
        file.sync_all()
            .with_context(|| format!("sync {label} contents"))?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "atomically replace {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync {label} directory"))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    write_result
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
        let root = std::env::temp_dir().join(format!("hh-ui-{label}-{}", Uuid::new_v4()));
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

    fn banner_bytes(format: image::ImageFormat) -> Vec<u8> {
        let image = image::DynamicImage::new_rgb8(300, 100);
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, format).unwrap();
        encoded.into_inner()
    }

    #[test]
    fn workstation_banner_is_copied_canonically_and_can_be_reset() {
        let (root, store) = temp_store("banner-round-trip");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("selected.jpg");
        fs::write(&source, banner_bytes(image::ImageFormat::Jpeg)).unwrap();

        let saved = store.import_workstation_banner(&source).unwrap();
        fs::remove_file(source).unwrap();
        assert!(saved.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!((saved.width, saved.height), (300, 100));
        assert_eq!(store.load_workstation_banner().unwrap(), Some(saved));
        assert_eq!(
            fs::metadata(root.join(WORKSTATION_BANNER_FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        store.reset_workstation_banner().unwrap();
        assert_eq!(store.load_workstation_banner().unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_banner_does_not_replace_the_saved_selection() {
        let (root, store) = temp_store("banner-invalid");
        fs::create_dir_all(&root).unwrap();
        let valid = root.join("valid.png");
        fs::write(&valid, banner_bytes(image::ImageFormat::Png)).unwrap();
        let saved = store.import_workstation_banner(&valid).unwrap();
        let invalid = root.join("invalid.png");
        fs::write(&invalid, b"not an image").unwrap();

        assert!(store.import_workstation_banner(&invalid).is_err());
        assert_eq!(store.load_workstation_banner().unwrap(), Some(saved));
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

    #[test]
    fn banner_visibility_and_sidebar_width_persist_independently() {
        let (root, store) = temp_store("visibility");
        assert!(!store.load_workstation_banner_hidden().unwrap());

        store.save_workspace_sidebar_width(287.5).unwrap();
        store.save_workstation_banner_hidden(true).unwrap();

        let restarted = UiStateStore::new(root.join("ui-state.json"));
        assert_eq!(
            restarted.load_workspace_sidebar_width().unwrap(),
            Some(287.5)
        );
        assert!(restarted.load_workstation_banner_hidden().unwrap());

        restarted.save_workspace_sidebar_width(300.0).unwrap();
        assert!(restarted.load_workstation_banner_hidden().unwrap());
        restarted.save_workstation_banner_hidden(false).unwrap();
        assert_eq!(
            restarted.load_workspace_sidebar_width().unwrap(),
            Some(300.0)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ui_state_written_before_the_visibility_field_still_loads() {
        let (root, store) = temp_store("legacy");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("ui-state.json"),
            br#"{"schema_version":1,"workspace_sidebar_width":220.0}"#,
        )
        .unwrap();

        assert_eq!(store.load_workspace_sidebar_width().unwrap(), Some(220.0));
        assert!(!store.load_workstation_banner_hidden().unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
