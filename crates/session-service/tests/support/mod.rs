//! Shared integration-test fixtures.

use std::ffi::OsString;
use std::fs;
use std::ops::Deref;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hh_protocol::{managed_tmux_socket_name, tmux_socket_path};
use uuid::Uuid;

/// Owner-only temporary state directory for one test registry.
///
/// Dropping it, including during a panic, kills the private tmux server the
/// registry started for this directory, removes its socket file, and deletes
/// the directory. It never targets the app's `hh`/`hh-dev` servers.
#[derive(Debug)]
pub struct TestStateDir {
    path: PathBuf,
    socket_name: String,
}

impl TestStateDir {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("hh-integration-{label}-{}", Uuid::new_v4()));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .unwrap();
        // The directory exists, so the name uses the same canonical path the
        // registry hashes.
        let socket_name = managed_tmux_socket_name(&path);
        assert_private_socket_name(&socket_name);
        Self { path, socket_name }
    }

    #[allow(dead_code, reason = "only the tmux isolation tests address the server")]
    pub fn socket_name(&self) -> &str {
        &self.socket_name
    }
}

impl Deref for TestStateDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestStateDir {
    fn drop(&mut self) {
        assert_private_socket_name(&self.socket_name);
        let _ = Command::new(tmux_binary())
            .args(["-L", &self.socket_name, "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // tmux leaves the socket file behind on macOS after the server exits.
        let _ = fs::remove_file(tmux_socket_path(&self.socket_name));
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn tmux_binary() -> OsString {
    std::env::var_os("HH_TMUX_BINARY").unwrap_or_else(|| OsString::from("tmux"))
}

fn assert_private_socket_name(name: &str) {
    assert!(
        name != "hh" && name != "hh-dev",
        "test state directory resolved to the app's tmux server {name}"
    );
}
