use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

use crate::MAX_UNIX_SOCKET_PATH_BYTES;
use rustix::process::geteuid;
use uuid::Uuid;
pub const SOCKET_ENV: &str = "HH_SOCKET";
pub const STATE_DIR_ENV: &str = "HH_STATE_DIR";
pub const CONFIG_ENV: &str = "HH_CONFIG";
pub const PANE_ID_ENV: &str = "HH_PANE_ID";
/// Marks the separately packaged development desktop build.
///
/// Explicit `HH_SOCKET`, `HH_STATE_DIR`, and `HH_CONFIG` values always
/// override the corresponding Dev defaults, preserving disposable test runs.
pub const DEVELOPMENT_BUILD_ENV: &str = "HH_DEVELOPMENT_BUILD";

/// Returns the configured socket path, or the private runtime-directory
/// default. Unix-domain socket paths must fit in macOS's 104-byte `sun_path`
/// field including its trailing NUL.
///
/// # Errors
///
/// Returns an error when no state directory is available or when the selected
/// path cannot fit in a macOS Unix-domain socket address.
pub fn socket_path() -> io::Result<PathBuf> {
    let path = std::env::var_os(SOCKET_ENV).map_or_else(
        || {
            runtime_directory()
                .map(|directory| default_socket_path(&directory, development_build()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
        },
        |path| Ok(PathBuf::from(path)),
    )?;
    validate_socket_path_length(&path)?;
    Ok(path)
}

/// Returns the pre-hardening default socket path while no explicit socket
/// override is configured. Clients use this only to preserve live PTYs across
/// the one-time runtime-directory migration.
pub fn legacy_socket_path() -> Option<PathBuf> {
    if std::env::var_os(SOCKET_ENV).is_some() {
        return None;
    }
    let path = std::env::temp_dir().join(socket_filename(development_build()));
    validate_socket_path_length(&path).ok()?;
    Some(path)
}

fn validate_socket_path_length(path: &Path) -> io::Result<()> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length > MAX_UNIX_SOCKET_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "session socket path is {length} bytes; maximum is {MAX_UNIX_SOCKET_PATH_BYTES}. Set {SOCKET_ENV} to a shorter owner-only path"
            ),
        ));
    }
    Ok(())
}

fn socket_filename(development_build: bool) -> &'static str {
    if development_build {
        "hh-dev-session.sock"
    } else {
        "hh-session.sock"
    }
}

/// Returns the owner-only runtime directory used for the session socket.
pub fn runtime_directory() -> Option<PathBuf> {
    state_directory().map(|directory| directory.join("run"))
}

fn default_socket_path(runtime_directory: &Path, development_build: bool) -> PathBuf {
    runtime_directory.join(socket_filename(development_build))
}

/// Returns the owner-only Harness Harlot state directory.
pub fn state_directory() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os(STATE_DIR_ENV) {
        return Some(PathBuf::from(directory));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let xdg_state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    Some(default_state_directory(
        &home,
        xdg_state_home.as_deref(),
        development_build(),
    ))
}

fn default_state_directory(
    home: &Path,
    xdg_state_home: Option<&Path>,
    development_build: bool,
) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = xdg_state_home;
        let product_directory = if development_build {
            "Harness Harlot Dev"
        } else {
            "Harness Harlot"
        };
        home.join("Library/Application Support")
            .join(product_directory)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let fallback = home.join(".local/state");
        let base = xdg_state_home.unwrap_or(&fallback);
        base.join(if development_build { "hh-dev" } else { "hh" })
    }
}

/// Returns the optional Harness Harlot desktop configuration file.
pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CONFIG_ENV) {
        return Some(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(default_config_path(&base, development_build()))
}

fn default_config_path(base: &Path, development_build: bool) -> PathBuf {
    let product_directory = if development_build { "hh-dev" } else { "hh" };
    base.join(product_directory).join("config.json")
}
/// Reads one owner-only regular file without following a final symlink.
///
/// The opened descriptor, rather than a pre-open path check, supplies metadata
/// and bytes. Permissions are reasserted to `0600`.
///
/// # Errors
///
/// Returns an error for symlinks, non-regular files, foreign ownership, files
/// larger than `max_bytes`, or files that grow past the limit while read.
pub fn read_private_file(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private path is not a regular file",
        ));
    }
    if metadata.uid() != geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private file is not owned by the current user",
        ));
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("private file exceeds {max_bytes} bytes"),
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let capacity = usize::try_from(metadata.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("private file grew past {max_bytes} bytes while reading"),
        ));
    }
    Ok(bytes)
}

fn development_build() -> bool {
    std::env::var(DEVELOPMENT_BUILD_ENV).as_deref() == Ok("1")
}

/// Child terminals receive this stable pane identifier.
pub const fn pane_id_env() -> &'static str {
    PANE_ID_ENV
}

/// Verifies owner-only permission on one already-inspected entry: owned by
/// the current effective user with no group or other access bits.
pub fn validate_private_ownership(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == geteuid().as_raw() && metadata.mode().trailing_zeros() >= 6
}

/// Creates or validates one owner-only directory.
///
/// A missing path (including parents) is created with mode `0o700`. An
/// existing entry is accepted only when it is a real directory — symlinks are
/// rejected without being followed — owned by the current effective user with
/// no group or other access bits. Permissions are reasserted to `0o700`.
///
/// # Errors
///
/// Returns an error when the path exists but is not a real directory, when it
/// is owned by another user or exposes group/other access bits, or when any
/// filesystem operation fails.
pub fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "private directory must be a real directory: {}",
                    path.display()
                ),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path)?;
        }
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "private directory must be a real directory: {}",
                path.display()
            ),
        ));
    }
    if !validate_private_ownership(&metadata) || metadata.mode() & 0o700 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private directory must be owned by the current user with mode 700: {} (run chmod 700)",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Atomically writes one owner-only file: a fresh `0o600` temporary in the
/// same directory is written, synced, renamed over the target, and the parent
/// directory is synced. The parent directory is created or validated with
/// [`ensure_private_directory`] first. No temporary file survives a failure.
///
/// This must stay behaviorally in sync with the snapshot writer in
/// `hh_session_service::persistence` (`write_snapshot`). Consolidation is
/// intentionally skipped: the service copy carries a `#[cfg(test)]`
/// injected-failure hook that cannot cross the crate boundary.
///
/// # Errors
///
/// Returns an error when the target has no parent directory, when the parent
/// cannot be made owner-only, or when any write, sync, or rename fails.
pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "private write target has no parent directory: {}",
                path.display()
            ),
        )
    })?;
    ensure_private_directory(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("private-state");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_interface_uses_only_the_hh_prefix() {
        assert_eq!(SOCKET_ENV, "HH_SOCKET");
        assert_eq!(STATE_DIR_ENV, "HH_STATE_DIR");
        assert_eq!(CONFIG_ENV, "HH_CONFIG");
        assert_eq!(DEVELOPMENT_BUILD_ENV, "HH_DEVELOPMENT_BUILD");
        assert_eq!(pane_id_env(), "HH_PANE_ID");
        assert!(default_socket_path(Path::new("/private/run"), false).ends_with("hh-session.sock"));
    }

    #[test]
    fn development_build_defaults_are_isolated_from_stable() {
        assert!(
            default_socket_path(Path::new("/private/run"), true).ends_with("hh-dev-session.sock")
        );
        assert_ne!(
            default_socket_path(Path::new("/private/run"), false),
            default_socket_path(Path::new("/private/run"), true)
        );

        let home = PathBuf::from("/Users/example");
        assert_ne!(
            default_state_directory(&home, None, false),
            default_state_directory(&home, None, true)
        );
        let config_home = home.join(".config");
        assert_ne!(
            default_config_path(&config_home, false),
            default_config_path(&config_home, true)
        );
    }

    #[test]
    fn socket_paths_are_bounded_for_macos_sun_path() {
        let accepted = PathBuf::from("a".repeat(crate::MAX_UNIX_SOCKET_PATH_BYTES));
        let rejected = PathBuf::from("a".repeat(crate::MAX_UNIX_SOCKET_PATH_BYTES + 1));
        assert!(validate_socket_path_length(&accepted).is_ok());
        assert!(validate_socket_path_length(&rejected).is_err());
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hh-protocol-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn ensure_private_directory_rejects_an_existing_regular_file() {
        let path = temp_path("regular-file");
        fs::write(&path, b"occupied").unwrap();

        let error = ensure_private_directory(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        assert_eq!(fs::read(&path).unwrap(), b"occupied");
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn ensure_private_directory_rejects_a_symlink_to_a_directory() {
        let real = temp_path("symlink-real");
        let link = temp_path("symlink-link");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let error = ensure_private_directory(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        fs::remove_file(&link).unwrap();
    }
    #[test]
    fn ensure_private_directory_creates_fresh_owner_only_directories() {
        let root = temp_path("fresh-dir");
        let path = root.join("nested").join("leaf");

        ensure_private_directory(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // A second call over the existing directory still succeeds.
        ensure_private_directory(&path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_write_private_publishes_bytes_without_leaving_temporaries() {
        let root = temp_path("atomic-write");
        let target = root.join("state.json");

        atomic_write_private(&target, b"payload").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"payload");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root).unwrap();
    }
}
