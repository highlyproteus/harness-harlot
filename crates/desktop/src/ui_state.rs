use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};

use anyhow::{Context, Result, bail};
use image::GenericImageView as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u16 = 1;
const MAX_UI_STATE_BYTES: u64 = 16 * 1024;
const WORKSTATION_BANNER_FILE_NAME: &str = "workstation-banner.png";
const MAX_WORKSTATION_BANNER_BYTES: u64 = 12 * 1024 * 1024;
const MAX_WORKSTATION_BANNER_DIMENSION: u32 = 8_192;
const MAX_WORKSTATION_BANNER_PIXELS: u64 = 24_000_000;

static UI_STATE_WRITE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn shared_write_lock(path: &Path) -> Arc<Mutex<()>> {
    let mut locks = UI_STATE_WRITE_LOCKS.lock();
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

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
    #[serde(default)]
    /// Legacy; retained only for file compatibility.
    voice_dock_visible: bool,
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
    write_lock: Arc<Mutex<()>>,
}

impl UiStateStore {
    pub(crate) fn from_default_path() -> Result<Self> {
        let path = default_ui_state_path()?;
        Ok(Self {
            write_lock: shared_write_lock(&path),
            path,
        })
    }

    #[cfg(test)]
    fn new(path: PathBuf) -> Self {
        Self {
            write_lock: shared_write_lock(&path),
            path,
        }
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
        hh_protocol::atomic_write_private(&self.path, &bytes).context("write UI state")
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
        let _guard = self.write_lock.lock();
        let mut state = self.read_state()?.unwrap_or(PersistedUiState {
            schema_version: SCHEMA_VERSION,
            workspace_sidebar_width: None,
            workstation_banner_hidden: false,
            voice_dock_visible: false,
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
        let _guard = self.write_lock.lock();
        let mut state = self.read_state()?.unwrap_or(PersistedUiState {
            schema_version: SCHEMA_VERSION,
            workspace_sidebar_width: None,
            workstation_banner_hidden: false,
            voice_dock_visible: false,
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
        let (width, height) = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .context("detect workstation banner image format")?
            .into_dimensions()
            .context("read workstation banner dimensions")?;
        Ok(Some(StoredBanner {
            png: bytes,
            width,
            height,
        }))
    }

    pub(crate) fn import_workstation_banner(&self, source: &Path) -> Result<StoredBanner> {
        let bytes = hh_protocol::read_private_file(source, MAX_WORKSTATION_BANNER_BYTES)
            .with_context(|| format!("read workstation banner from {}", source.display()))?;
        let stored = canonical_workstation_banner(&bytes)?;
        let path = self.workstation_banner_path()?;
        hh_protocol::atomic_write_private(&path, &stored.png)
            .with_context(|| format!("write workstation banner {}", path.display()))?;
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
/// Bounded-decode and canonical-PNG-re-encode limits for one stored image.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CanonicalPngLimits {
    /// Lowercase label used in user-facing error messages.
    pub what: &'static str,
    pub max_dimension: u32,
    pub max_pixels: u64,
    /// Decoder allocation cap in bytes.
    pub max_alloc: u64,
}

/// One bounded-decoded image re-encoded as a canonical PNG.
#[derive(Clone, Debug)]
pub(crate) struct CanonicalPng {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn bounded_image_reader<'a>(
    bytes: &'a [u8],
    limits: &CanonicalPngLimits,
) -> Result<image::ImageReader<Cursor<&'a [u8]>>> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| format!("detect {} image format", limits.what))?;
    let mut image_limits = image::Limits::default();
    image_limits.max_image_width = Some(limits.max_dimension);
    image_limits.max_image_height = Some(limits.max_dimension);
    image_limits.max_alloc = Some(limits.max_alloc);
    reader.limits(image_limits);
    Ok(reader)
}

fn validate_image_dimensions(width: u32, height: u32, limits: &CanonicalPngLimits) -> Result<()> {
    if width == 0
        || height == 0
        || width > limits.max_dimension
        || height > limits.max_dimension
        || u64::from(width) * u64::from(height) > limits.max_pixels
    {
        bail!(
            "{} dimensions must be at most {}x{} and {} pixels",
            limits.what,
            limits.max_dimension,
            limits.max_dimension,
            limits.max_pixels
        );
    }
    Ok(())
}

/// Decodes one bounded raster image and re-encodes it as a canonical PNG.
///
/// Dimensions are validated against `limits` both before decoding (via the
/// decoder's own limits) and after decoding, so an oversized image is
/// rejected before a large allocation can occur.
pub(crate) fn decode_canonical_png(
    bytes: &[u8],
    limits: &CanonicalPngLimits,
) -> Result<CanonicalPng> {
    let dimensions_reader = bounded_image_reader(bytes, limits)?;
    let (header_width, header_height) = dimensions_reader
        .into_dimensions()
        .with_context(|| format!("read bounded {} image dimensions", limits.what))?;
    validate_image_dimensions(header_width, header_height, limits)?;

    let decoded = bounded_image_reader(bytes, limits)?
        .decode()
        .with_context(|| format!("decode bounded {} image", limits.what))?;
    let (width, height) = decoded.dimensions();
    validate_image_dimensions(width, height, limits)?;
    let mut canonical = Cursor::new(Vec::new());
    decoded
        .write_to(&mut canonical, image::ImageFormat::Png)
        .with_context(|| format!("encode canonical {} PNG", limits.what))?;
    Ok(CanonicalPng {
        png: canonical.into_inner(),
        width,
        height,
    })
}

fn canonical_workstation_banner(bytes: &[u8]) -> Result<StoredBanner> {
    let limits = CanonicalPngLimits {
        what: "workstation banner",
        max_dimension: MAX_WORKSTATION_BANNER_DIMENSION,
        max_pixels: MAX_WORKSTATION_BANNER_PIXELS,
        max_alloc: MAX_WORKSTATION_BANNER_PIXELS * 8,
    };
    let canonical = decode_canonical_png(bytes, &limits)?;
    if canonical.png.len() as u64 > MAX_WORKSTATION_BANNER_BYTES {
        bail!("encoded workstation banner must be 12 MiB or smaller");
    }
    Ok(StoredBanner {
        png: canonical.png,
        width: canonical.width,
        height: canonical.height,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use super::{
        CanonicalPngLimits, Cursor, Path, PathBuf, UiStateStore, WORKSTATION_BANNER_FILE_NAME,
        decode_canonical_png,
    };

    use uuid::Uuid;

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
    fn image_pixel_limit_is_enforced_independently_of_dimensions() {
        let bytes = banner_bytes(image::ImageFormat::Png);
        let error = decode_canonical_png(
            &bytes,
            &CanonicalPngLimits {
                what: "test image",
                max_dimension: 300,
                max_pixels: 29_999,
                max_alloc: 1_048_576,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("test image dimensions"));
    }

    fn create_owner_only_directory(path: &Path) {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap();
    }

    #[test]
    fn workstation_banner_is_copied_canonically_and_can_be_reset() {
        let (root, store) = temp_store("banner-round-trip");
        create_owner_only_directory(&root);
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
        create_owner_only_directory(&root);
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
    fn banner_and_sidebar_state_persist_independently() {
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
    fn concurrent_ui_state_saves_preserve_independent_fields() {
        let (root, store) = temp_store("concurrent");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let width_store = UiStateStore::new(root.join("ui-state.json"));
        let width_barrier = Arc::clone(&barrier);
        let width = std::thread::spawn(move || {
            width_barrier.wait();
            for offset in 0_u16..32 {
                width_store
                    .save_workspace_sidebar_width(280.0 + f32::from(offset))
                    .unwrap();
            }
        });
        let banner_store = UiStateStore::new(root.join("ui-state.json"));
        let banner = std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..32 {
                banner_store.save_workstation_banner_hidden(true).unwrap();
            }
        });
        width.join().unwrap();
        banner.join().unwrap();

        assert!(store.load_workspace_sidebar_width().unwrap().is_some());
        assert!(store.load_workstation_banner_hidden().unwrap());
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
