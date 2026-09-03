use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn install_prefix_for_executable(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if app.file_name()? != "Harness Harlot.app" {
        return None;
    }
    app.parent().map(Path::to_path_buf)
}

pub(super) fn desktop_update_handoff_identity(arguments: &[String]) -> Option<(u32, u64)> {
    if arguments.first().map(String::as_str) != Some("install") {
        return None;
    }
    let process_id = super::optional_string_option(arguments, "--wait-pid")
        .ok()??
        .parse()
        .ok()?;
    let process_start_time = super::optional_string_option(arguments, "--wait-start-time")
        .ok()??
        .parse()
        .ok()?;
    Some((process_id, process_start_time))
}

pub(super) fn desktop_update_relaunch_target(
    arguments: &[String],
    updater_executable: &Path,
) -> Option<(u32, u64, PathBuf)> {
    let (process_id, process_start_time) = desktop_update_handoff_identity(arguments)?;
    let prefix = install_prefix_for_executable(updater_executable)?;
    Some((
        process_id,
        process_start_time,
        prefix.join(super::MACOS_APP_NAME),
    ))
}

#[cfg(target_os = "macos")]
pub(super) fn relaunch_after_failed_desktop_update(arguments: &[String]) {
    let Some((process_id, process_start_time, app)) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(|executable| desktop_update_relaunch_target(arguments, executable))
    else {
        return;
    };
    if let Err(error) = super::wait_for_process_exit(process_id, process_start_time) {
        eprintln!("could not wait to restore Harness Harlot after update failure: {error:#}");
        return;
    }
    if !app.join("Contents/MacOS/hh").is_file() {
        eprintln!(
            "could not restore Harness Harlot after update failure: {} is not launchable",
            app.display()
        );
        return;
    }
    let fixture = arguments.iter().any(|argument| argument == "--fixture");
    if let Err(error) = super::run_status(
        relaunch_program(fixture),
        relaunch_arguments(&app),
        "restore app after failed update",
    ) {
        eprintln!("Harness Harlot update failed and the app could not be reopened: {error:#}");
    }
}

pub(super) struct StagedDirectoryGuard {
    path: PathBuf,
}

impl StagedDirectoryGuard {
    pub(super) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }
}

impl Drop for StagedDirectoryGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "warning: could not remove staged update {}: {error}",
                self.path.display()
            );
        }
    }
}

pub(super) fn relaunch_program(fixture: bool) -> &'static str {
    if fixture { "open" } else { "/usr/bin/open" }
}

pub(super) fn relaunch_arguments(app: &Path) -> Vec<&OsStr> {
    vec![OsStr::new("-n"), app.as_os_str()]
}
