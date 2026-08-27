//! Clipboard-image materialization and local/managed-SSH file transfer.

use anyhow::{Context as _, Result, bail, ensure};
use gpui::{AppContext as _, Context, Image, ImageFormat};
use hh_protocol::{
    ClientRequest, ServiceResponse, SessionSnapshot, TerminalModes, ensure_private_directory,
    validate_ssh_host,
};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

use crate::HhApp;
use crate::helpers::{find_pane, prepare_paste};
use crate::session::session_call;

const MAX_CLIPBOARD_IMAGE_BYTES: usize = 25 * 1024 * 1024;
const MAX_TRANSFER_FILES: usize = 16;
const MAX_TRANSFER_FILE_BYTES: u64 = 100 * 1024 * 1024;
const SCP_TIMEOUT: Duration = Duration::from_secs(45);
const CLIPBOARD_IMAGE_RETENTION: Duration = Duration::from_hours(24);
const MAX_COMMAND_STDERR_BYTES: usize = 64 * 1024;
const STDERR_COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
enum FileTransferTarget {
    Local,
    SystemSsh(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileTransferAuthority {
    workspace_id: Uuid,
    tab_id: Uuid,
    pane_id: Uuid,
    target: FileTransferTarget,
}

enum FilePasteSource {
    Image(Image),
    Paths(Vec<PathBuf>),
}

fn image_extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
    }
}

fn paste_directory() -> Result<PathBuf> {
    let directory = std::env::temp_dir().join("harness-harlot-paste");
    ensure_private_directory(&directory).context("secure Harness Harlot paste directory")?;
    cleanup_stale_clipboard_images(&directory, SystemTime::now())?;
    Ok(directory)
}

pub(crate) fn cleanup_clipboard_image_cache() -> Result<()> {
    paste_directory().map(|_| ())
}

fn cleanup_stale_clipboard_images(directory: &Path, now: SystemTime) -> Result<()> {
    for entry in fs::read_dir(directory).context("inspect Harness Harlot paste directory")? {
        let entry = entry.context("inspect Harness Harlot paste entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("clipboard-") && !name.starts_with("upload-") {
            continue;
        }
        let metadata = entry
            .metadata()
            .with_context(|| format!("inspect clipboard image {}", entry.path().display()))?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata
            .modified()
            .with_context(|| format!("read clipboard image age {}", entry.path().display()))?;
        if now.duration_since(modified).unwrap_or_default() >= CLIPBOARD_IMAGE_RETENTION {
            fs::remove_file(entry.path())
                .with_context(|| format!("remove stale clipboard image {name}"))?;
        }
    }
    Ok(())
}

pub(crate) fn materialize_clipboard_image(image: &Image) -> Result<PathBuf> {
    ensure!(!image.bytes.is_empty(), "clipboard image is empty");
    ensure!(
        image.bytes.len() <= MAX_CLIPBOARD_IMAGE_BYTES,
        "clipboard image exceeds the 25 MiB limit"
    );
    let path = paste_directory()?.join(format!(
        "clipboard-{}.{}",
        Uuid::new_v4(),
        image_extension(image.format)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create clipboard image {}", path.display()))?;
    std::io::Write::write_all(&mut file, &image.bytes)
        .with_context(|| format!("write clipboard image {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync clipboard image {}", path.display()))?;
    Ok(path)
}

fn validate_source_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "pasted file path must be absolute");
    let value = path
        .to_str()
        .context("pasted file path is not valid UTF-8")?;
    ensure!(
        !value.chars().any(char::is_control),
        "pasted file path contains unsupported control characters"
    );
    Ok(())
}

fn stage_remote_source(path: &Path) -> Result<PathBuf> {
    validate_source_path(path)?;
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| {
            format!(
                "open pasted file without following links {}",
                path.display()
            )
        })?;
    let metadata = source
        .metadata()
        .with_context(|| format!("inspect opened pasted file {}", path.display()))?;
    ensure!(metadata.is_file(), "pasted item is not a regular file");
    ensure!(
        metadata.len() <= MAX_TRANSFER_FILE_BYTES,
        "pasted file exceeds the 100 MiB limit"
    );

    let generated_path = remote_paste_path(path, Uuid::new_v4());
    let extension = generated_path
        .rsplit_once('.')
        .map_or("bin", |(_, extension)| extension);
    let staged = paste_directory()?.join(format!("upload-{}.{}", Uuid::new_v4(), extension));
    let result = (|| {
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)
            .with_context(|| format!("create staged paste file {}", staged.display()))?;
        let copied = std::io::copy(
            &mut std::io::Read::by_ref(&mut source).take(MAX_TRANSFER_FILE_BYTES + 1),
            &mut destination,
        )
        .with_context(|| format!("stage pasted file {}", path.display()))?;
        ensure!(
            copied <= MAX_TRANSFER_FILE_BYTES,
            "pasted file grew beyond the 100 MiB limit"
        );
        destination
            .sync_all()
            .with_context(|| format!("sync staged paste file {}", staged.display()))?;
        Ok(staged.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn stage_remote_sources(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    ensure!(!paths.is_empty(), "no files were provided for paste");
    ensure!(
        paths.len() <= MAX_TRANSFER_FILES,
        "at most 16 files can be pasted at once"
    );
    let mut staged = Vec::with_capacity(paths.len());
    for path in paths {
        match stage_remote_source(&path) {
            Ok(path) => staged.push(path),
            Err(error) => {
                for path in &staged {
                    let _ = fs::remove_file(path);
                }
                return Err(error);
            }
        }
    }
    Ok(staged)
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn shell_join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| shell_quote_path(path))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn remote_paste_path(local_path: &Path, id: Uuid) -> String {
    let extension = local_path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 10
                && value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "bin".to_owned());
    format!("{}/file.{extension}", remote_paste_directory(id))
}

fn remote_paste_directory(id: Uuid) -> String {
    format!("/tmp/hh-paste-{id}")
}

fn trusted_scp_binary() -> Result<PathBuf> {
    for path in [Path::new("/usr/bin/scp"), Path::new("/bin/scp")] {
        if path.metadata().is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.permissions().mode() & 0o111 != 0
                && metadata.permissions().mode() & 0o022 == 0
        }) {
            return Ok(path.to_path_buf());
        }
    }
    bail!("installed system OpenSSH scp client was not found")
}

fn trusted_ssh_binary() -> Result<PathBuf> {
    for path in [Path::new("/usr/bin/ssh"), Path::new("/bin/ssh")] {
        if path.metadata().is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.permissions().mode() & 0o111 != 0
                && metadata.permissions().mode() & 0o022 == 0
        }) {
            return Ok(path.to_path_buf());
        }
    }
    bail!("installed system OpenSSH client was not found")
}

fn validate_remote_paste_directory(path: &str) -> Result<()> {
    let id = path
        .strip_prefix("/tmp/hh-paste-")
        .context("generated remote paste directory is invalid")?;
    ensure!(
        Uuid::parse_str(id).is_ok(),
        "generated remote paste directory is invalid"
    );
    Ok(())
}

fn validate_remote_paste_path(path: &str) -> Result<()> {
    let (directory, file_name) = path
        .rsplit_once('/')
        .context("generated remote paste path is invalid")?;
    validate_remote_paste_directory(directory)?;
    ensure!(
        !file_name.is_empty()
            && file_name.len() <= 64
            && file_name.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
            ),
        "generated remote paste path is invalid"
    );
    Ok(())
}

fn ssh_remote_command_with(
    binary: &Path,
    destination: &str,
    command_name: &str,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Result<Command> {
    validate_ssh_host(destination).map_err(anyhow::Error::from)?;
    let mut command = Command::new(binary);
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=6",
        "-o",
        "ServerAliveInterval=20",
        "-o",
        "ServerAliveCountMax=2",
        "--",
    ]);
    command.arg(destination);
    command.arg(command_name);
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::piped());
    Ok(command)
}

pub(crate) fn ssh_private_directory_command_with(
    binary: &Path,
    destination: &str,
    remote_directory: &str,
) -> Result<Command> {
    validate_remote_paste_directory(remote_directory)?;
    ssh_remote_command_with(
        binary,
        destination,
        "mkdir",
        ["-m", "700", "--", remote_directory],
    )
}

pub(crate) fn ssh_private_files_command_with(
    binary: &Path,
    destination: &str,
    remote_paths: &[String],
) -> Result<Command> {
    ensure!(!remote_paths.is_empty(), "no remote paste files to protect");
    for path in remote_paths {
        validate_remote_paste_path(path)?;
    }
    ssh_remote_command_with(
        binary,
        destination,
        "chmod",
        ["600", "--"]
            .into_iter()
            .map(String::from)
            .chain(remote_paths.iter().cloned()),
    )
}

pub(crate) fn scp_upload_command_with(
    binary: &Path,
    destination: &str,
    local_path: &Path,
    remote_path: &str,
) -> Result<Command> {
    validate_ssh_host(destination).map_err(anyhow::Error::from)?;
    validate_remote_paste_path(remote_path)?;
    let mut command = Command::new(binary);
    command.args([
        "-q",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=6",
        "-o",
        "ServerAliveInterval=20",
        "-o",
        "ServerAliveCountMax=2",
        "--",
    ]);
    command.arg(local_path);
    command.arg(format!("{destination}:{remote_path}"));
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::piped());
    Ok(command)
}

pub(crate) fn ssh_cleanup_command_with(
    binary: &Path,
    destination: &str,
    remote_paths: &[String],
) -> Result<Command> {
    ensure!(
        !remote_paths.is_empty(),
        "no remote paste paths to clean up"
    );
    let mut directories = Vec::new();
    for path in remote_paths {
        validate_remote_paste_path(path)?;
        let directory = path
            .rsplit_once('/')
            .map(|(directory, _)| directory.to_owned())
            .context("generated remote paste path has no directory")?;
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    ssh_remote_command_with(
        binary,
        destination,
        "rm",
        ["-rf", "--"]
            .into_iter()
            .map(String::from)
            .chain(directories),
    )
}

fn read_bounded_stderr(mut reader: impl Read, max_bytes: usize) -> Result<String> {
    let mut captured = Vec::with_capacity(max_bytes.min(4096));
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("read remote command stderr")?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&captured).into_owned())
}

fn receive_stderr_with_timeout(
    receiver: &Receiver<Result<String>>,
    timeout: Duration,
) -> Result<Option<String>> {
    match receiver.recv_timeout(timeout) {
        Ok(result) => result.map(Some),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => bail!("remote stderr reader disconnected"),
    }
}

fn run_scp_upload(mut command: Command) -> Result<()> {
    let mut child = command.spawn().context("start remote image upload")?;
    let stderr = child
        .stderr
        .take()
        .context("remote image upload stderr was not piped")?;
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = stderr_sender.send(read_bounded_stderr(stderr, MAX_COMMAND_STDERR_BYTES));
    });
    let deadline = Instant::now() + SCP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = receive_stderr_with_timeout(&stderr_receiver, STDERR_COMPLETION_TIMEOUT);
                return Err(error).context("observe remote image upload");
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = receive_stderr_with_timeout(&stderr_receiver, STDERR_COMPLETION_TIMEOUT);
            bail!("remote image upload timed out after 45 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stderr = receive_stderr_with_timeout(&stderr_receiver, STDERR_COMPLETION_TIMEOUT)?
        .unwrap_or_default();
    if !status.success() {
        let detail = stderr.lines().find(|line| !line.trim().is_empty());
        bail!(
            "remote image upload failed{}",
            detail.map_or_else(String::new, |line| format!(": {}", line.trim()))
        );
    }
    Ok(())
}

fn run_remote_cleanup(command: Command) -> Result<()> {
    run_scp_upload(command).context("remove partial remote uploads")
}

fn transfer_authority_for_pane(
    snapshot: &SessionSnapshot,
    pane_id: Uuid,
) -> Result<FileTransferAuthority> {
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            let Some(pane) = find_pane(&tab.layout, pane_id) else {
                continue;
            };
            ensure!(pane.kind.is_terminal(), "pane {pane_id} is not terminal");
            let target = match snapshot.terminal_transports.get(&pane_id) {
                Some(hh_protocol::TerminalTransport::Local) => FileTransferTarget::Local,
                Some(hh_protocol::TerminalTransport::SystemSsh { destination }) => {
                    FileTransferTarget::SystemSsh(destination.clone())
                }
                Some(hh_protocol::TerminalTransport::Unknown) | None => {
                    bail!("pane {pane_id} has no authoritative terminal transport")
                }
            };
            return Ok(FileTransferAuthority {
                workspace_id: workspace.id,
                tab_id: tab.id,
                pane_id,
                target,
            });
        }
    }
    bail!("pane {pane_id} does not exist")
}

fn revalidate_transfer_authority(
    snapshot: &SessionSnapshot,
    expected: &FileTransferAuthority,
) -> Result<()> {
    let current = transfer_authority_for_pane(snapshot, expected.pane_id)?;
    ensure!(
        current.workspace_id == expected.workspace_id,
        "pane {} changed workspace",
        expected.pane_id
    );
    ensure!(
        current.tab_id == expected.tab_id,
        "pane {} changed tab",
        expected.pane_id
    );
    ensure!(
        current.target == expected.target,
        "pane {} terminal transport or destination changed",
        expected.pane_id
    );
    Ok(())
}

#[cfg(test)]
fn transfer_target_for_pane(
    snapshot: &SessionSnapshot,
    pane_id: Uuid,
) -> Option<FileTransferTarget> {
    transfer_authority_for_pane(snapshot, pane_id)
        .ok()
        .map(|authority| authority.target)
}

fn validate_transfer_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    ensure!(!paths.is_empty(), "no files were provided for paste");
    ensure!(
        paths.len() <= MAX_TRANSFER_FILES,
        "at most 16 files can be pasted at once"
    );
    paths
        .into_iter()
        .map(|path| {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("resolve pasted file {}", path.display()))?;
            let metadata = canonical
                .metadata()
                .with_context(|| format!("inspect pasted file {}", canonical.display()))?;
            ensure!(metadata.is_file(), "pasted item is not a regular file");
            let value = canonical
                .to_str()
                .context("pasted file path is not valid UTF-8")?;
            ensure!(
                !value.chars().any(char::is_control),
                "pasted file path contains unsupported control characters"
            );
            ensure!(
                metadata.len() <= MAX_TRANSFER_FILE_BYTES,
                "pasted file exceeds the 100 MiB limit"
            );
            Ok(canonical)
        })
        .collect()
}

fn remove_staged_sources(paths: &[PathBuf]) -> Result<()> {
    let mut first_error = None;
    for path in paths {
        if let Err(error) = fs::remove_file(path)
            && first_error.is_none()
        {
            first_error = Some(
                anyhow::Error::new(error)
                    .context(format!("remove staged paste file {}", path.display())),
            );
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn rollback_remote_paths(destination: &str, paths: &[PathBuf]) -> Result<()> {
    let paths = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let command = ssh_cleanup_command_with(&trusted_ssh_binary()?, destination, &paths)?;
    run_remote_cleanup(command)
}

fn reconcile_remote_source_cleanup(
    destination: &str,
    transfer_result: Result<Vec<PathBuf>>,
    source_cleanup: Result<()>,
    rollback: impl FnOnce(&str, &[PathBuf]) -> Result<()>,
) -> Result<Vec<PathBuf>> {
    match (transfer_result, source_cleanup) {
        (Ok(paths), Ok(())) => Ok(paths),
        (Err(upload_error), Ok(())) => Err(upload_error),
        (Err(upload_error), Err(cleanup_error)) => Err(upload_error.context(format!(
            "owned clipboard source cleanup also failed: {cleanup_error:#}"
        ))),
        (Ok(paths), Err(cleanup_error)) => match rollback(destination, &paths) {
            Ok(()) => Err(cleanup_error.context(
                "remote upload was rolled back because owned clipboard source cleanup failed",
            )),
            Err(rollback_error) => Err(cleanup_error.context(format!(
                "owned clipboard source cleanup failed and remote rollback also failed: {rollback_error:#}"
            ))),
        },
    }
}

fn transfer_paths(paths: Vec<PathBuf>, target: FileTransferTarget) -> Result<Vec<PathBuf>> {
    match target {
        FileTransferTarget::Local => validate_transfer_paths(paths),
        FileTransferTarget::SystemSsh(destination) => {
            validate_ssh_host(&destination).map_err(anyhow::Error::from)?;
            let staged = stage_remote_sources(paths)?;
            let upload_result = (|| {
                let binary = trusted_scp_binary()?;
                let cleanup_binary = trusted_ssh_binary()?;
                let mut remote_paths = Vec::with_capacity(staged.len());
                for local_path in &staged {
                    let id = Uuid::new_v4();
                    let remote_directory = remote_paste_directory(id);
                    let remote_path = remote_paste_path(local_path, id);
                    remote_paths.push(remote_path.clone());
                    let step = (|| {
                        let setup = ssh_private_directory_command_with(
                            &cleanup_binary,
                            &destination,
                            &remote_directory,
                        )?;
                        run_scp_upload(setup).context("create private remote paste directory")?;
                        let upload = scp_upload_command_with(
                            &binary,
                            &destination,
                            local_path,
                            &remote_path,
                        )?;
                        run_scp_upload(upload)?;
                        let protect = ssh_private_files_command_with(
                            &cleanup_binary,
                            &destination,
                            std::slice::from_ref(&remote_path),
                        )?;
                        run_scp_upload(protect).context("protect remote pasted file")
                    })();
                    if let Err(upload_error) = step {
                        let cleanup =
                            ssh_cleanup_command_with(&cleanup_binary, &destination, &remote_paths)
                                .and_then(run_remote_cleanup);
                        return match cleanup {
                            Ok(()) => Err(upload_error),
                            Err(cleanup_error) => Err(upload_error.context(format!(
                                "partial remote upload cleanup also failed: {cleanup_error:#}"
                            ))),
                        };
                    }
                }
                Ok(remote_paths
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>())
            })();
            let staged_cleanup = remove_staged_sources(&staged);
            match (upload_result, staged_cleanup) {
                (Ok(remote_paths), Ok(())) => Ok(remote_paths),
                (Err(upload_error), Ok(())) => Err(upload_error),
                (Err(upload_error), Err(cleanup_error)) => Err(upload_error.context(format!(
                    "local staging cleanup also failed: {cleanup_error:#}"
                ))),
                (Ok(remote_paths), Err(cleanup_error)) => {
                    let rollback = rollback_remote_paths(&destination, &remote_paths);
                    match rollback {
                        Ok(()) => Err(cleanup_error.context(
                            "remote upload was rolled back because local staging cleanup failed",
                        )),
                        Err(rollback_error) => Err(cleanup_error.context(format!(
                            "local staging cleanup failed and remote rollback also failed: {rollback_error:#}"
                        ))),
                    }
                }
            }
        }
    }
}

fn cleanup_aborted_transfer(
    authority: &FileTransferAuthority,
    paths: &[PathBuf],
    owned_local_path: Option<&Path>,
) -> Result<()> {
    match &authority.target {
        FileTransferTarget::SystemSsh(destination) => rollback_remote_paths(destination, paths),
        FileTransferTarget::Local => match owned_local_path {
            Some(path) => fs::remove_file(path)
                .with_context(|| format!("remove aborted clipboard image {}", path.display())),
            None => Ok(()),
        },
    }
}

impl HhApp {
    pub(crate) fn paste_image_to_terminal(
        &mut self,
        pane_id: Uuid,
        image: Image,
        cx: &mut Context<Self>,
    ) {
        self.start_file_paste(pane_id, FilePasteSource::Image(image), cx);
    }

    pub(crate) fn paste_paths_to_terminal(
        &mut self,
        pane_id: Uuid,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.start_file_paste(pane_id, FilePasteSource::Paths(paths), cx);
    }

    fn start_file_paste(&mut self, pane_id: Uuid, source: FilePasteSource, cx: &mut Context<Self>) {
        let Some(screen) = self.session.screens.get(&pane_id) else {
            return;
        };
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return;
        };
        let Ok(authority) = transfer_authority_for_pane(snapshot, pane_id) else {
            return;
        };
        let bracketed = screen.modes.contains(TerminalModes::BRACKETED_PASTE);
        let control_client = self.control_client_handle();

        cx.spawn(async move |this, cx| {
            let transfer_target = authority.target.clone();
            let result = cx
                .background_spawn(async move {
                    let (paths, owned_clipboard_image) = match source {
                        FilePasteSource::Image(image) => {
                            let path = materialize_clipboard_image(&image)?;
                            (vec![path.clone()], Some(path))
                        }
                        FilePasteSource::Paths(paths) => (paths, None),
                    };
                    let remote_destination = match &transfer_target {
                        FileTransferTarget::SystemSsh(destination) => Some(destination.clone()),
                        FileTransferTarget::Local => None,
                    };
                    let result = transfer_paths(paths, transfer_target);
                    if let (Some(destination), Some(path)) =
                        (remote_destination, owned_clipboard_image.as_ref())
                    {
                        let cleanup = fs::remove_file(path).with_context(|| {
                            format!("remove uploaded clipboard image {}", path.display())
                        });
                        return reconcile_remote_source_cleanup(
                            &destination,
                            result,
                            cleanup,
                            rollback_remote_paths,
                        )
                        .map(|paths| (paths, None));
                    }
                    result.map(|paths| (paths, owned_clipboard_image))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result.and_then(|(paths, owned_local_path)| {
                    let response = session_call(&control_client, &ClientRequest::GetSnapshot)
                        .context("refresh transfer authority before terminal path insertion")?;
                    let ServiceResponse::Snapshot { snapshot } = response else {
                        bail!("unexpected GetSnapshot response before terminal path insertion: {response:?}");
                    };
                    if let Err(authority_error) =
                        revalidate_transfer_authority(&snapshot, &authority)
                    {
                        return match cleanup_aborted_transfer(
                            &authority,
                            &paths,
                            owned_local_path.as_deref(),
                        ) {
                            Ok(()) => Err(authority_error),
                            Err(cleanup_error) => Err(authority_error.context(format!(
                                "aborted transfer cleanup also failed: {cleanup_error:#}"
                            ))),
                        };
                    }
                    prepare_paste(&shell_join_paths(&paths), bracketed).map_err(anyhow::Error::msg)
                }) {
                    Ok(bytes) => {
                        this.dispatch_control(ClientRequest::WriteInput { pane_id, bytes });
                        this.session.connection_error = None;
                    }
                    Err(error) => this.session.connection_error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLIPBOARD_IMAGE_RETENTION, FileTransferTarget, cleanup_stale_clipboard_images,
        materialize_clipboard_image, read_bounded_stderr, receive_stderr_with_timeout,
        reconcile_remote_source_cleanup, remote_paste_path, revalidate_transfer_authority,
        scp_upload_command_with, shell_join_paths, ssh_cleanup_command_with,
        ssh_private_directory_command_with, ssh_private_files_command_with, stage_remote_source,
        transfer_authority_for_pane, transfer_target_for_pane, validate_transfer_paths,
    };
    use gpui::{Image, ImageFormat};
    use hh_protocol::{
        PaneKind, PaneLayout, SessionSnapshot, TerminalTransport, WorkspaceConnection,
        WorkspaceConnectionStatus,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    #[test]
    fn transferred_path_insertion_rejects_transport_or_pane_change() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = match &snapshot.workspaces[0].tabs[0].layout {
            PaneLayout::Leaf { pane } => pane.id,
            _ => unreachable!(),
        };
        snapshot.terminal_transports.insert(
            pane_id,
            TerminalTransport::SystemSsh {
                destination: "developer@build-node".to_owned(),
            },
        );
        let authority = transfer_authority_for_pane(&snapshot, pane_id).unwrap();

        snapshot.terminal_transports.insert(
            pane_id,
            TerminalTransport::SystemSsh {
                destination: "developer@other-node".to_owned(),
            },
        );
        assert!(revalidate_transfer_authority(&snapshot, &authority).is_err());

        snapshot.terminal_transports.insert(
            pane_id,
            TerminalTransport::SystemSsh {
                destination: "developer@build-node".to_owned(),
            },
        );
        let PaneLayout::Leaf { pane } = &mut snapshot.workspaces[0].tabs[0].layout else {
            unreachable!()
        };
        pane.kind = PaneKind::Browser {
            url: "https://example.com".to_owned(),
        };
        assert!(revalidate_transfer_authority(&snapshot, &authority).is_err());
    }

    #[test]
    fn managed_ssh_workstation_routes_pane_files_to_its_remote_host() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = match &snapshot.workspaces[0].tabs[0].layout {
            PaneLayout::Leaf { pane } => pane.id,
            _ => unreachable!("seeded snapshot starts with one leaf pane"),
        };
        snapshot.terminal_transports.insert(
            pane_id,
            TerminalTransport::SystemSsh {
                destination: "developer@build-node".to_owned(),
            },
        );
        snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
            destination: "developer@build-node".to_owned(),
            status: WorkspaceConnectionStatus::Offline,
        };

        assert_eq!(
            transfer_target_for_pane(&snapshot, pane_id),
            Some(FileTransferTarget::SystemSsh(
                "developer@build-node".to_owned()
            ))
        );
    }

    #[test]
    fn local_runtime_pane_inside_ssh_workstation_stays_local() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = match &snapshot.workspaces[0].tabs[0].layout {
            PaneLayout::Leaf { pane } => pane.id,
            _ => unreachable!("seeded snapshot starts with one leaf pane"),
        };
        snapshot
            .terminal_transports
            .insert(pane_id, TerminalTransport::Local);
        snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
            destination: "developer@build-node".to_owned(),
            status: WorkspaceConnectionStatus::Connected,
        };

        assert_eq!(
            transfer_target_for_pane(&snapshot, pane_id),
            Some(FileTransferTarget::Local)
        );
    }

    #[test]
    fn browser_panes_never_resolve_to_remote_file_transfer_targets() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = match &mut snapshot.workspaces[0].tabs[0].layout {
            PaneLayout::Leaf { pane } => {
                pane.kind = PaneKind::Browser {
                    url: "https://example.com".to_owned(),
                };
                pane.id
            }
            _ => unreachable!("seeded snapshot starts with one leaf pane"),
        };
        snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
            destination: "developer@build-node".to_owned(),
            status: WorkspaceConnectionStatus::Connected,
        };

        assert_eq!(transfer_target_for_pane(&snapshot, pane_id), None);
    }

    #[test]
    fn clipboard_images_are_materialized_as_private_encoded_files() {
        let image = Image::from_bytes(ImageFormat::Png, b"fixture-png".to_vec());
        let path = materialize_clipboard_image(&image).unwrap();

        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("png")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"fixture-png");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stale_owned_clipboard_images_are_removed_without_touching_other_files() {
        let directory =
            std::env::temp_dir().join(format!("hh-image-cleanup-{}", uuid::Uuid::new_v4()));
        hh_protocol::ensure_private_directory(&directory).unwrap();
        let stale = directory.join("clipboard-old.png");
        let unrelated = directory.join("keep.txt");
        std::fs::write(&stale, b"old").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();
        let modified = std::fs::metadata(&stale).unwrap().modified().unwrap();

        cleanup_stale_clipboard_images(
            &directory,
            modified + CLIPBOARD_IMAGE_RETENTION + std::time::Duration::from_secs(1),
        )
        .unwrap();

        assert!(!stale.exists());
        assert!(unrelated.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn terminal_path_insertion_rejects_control_character_filenames() {
        let directory =
            std::env::temp_dir().join(format!("hh-image-path-{}", uuid::Uuid::new_v4()));
        hh_protocol::ensure_private_directory(&directory).unwrap();
        let path = directory.join("bad\nname.png");
        std::fs::write(&path, b"fixture").unwrap();

        let error = validate_transfer_paths(vec![path]).unwrap_err();

        assert!(error.to_string().contains("control characters"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn remote_staging_copies_from_an_open_descriptor_and_rejects_symlinks() {
        let directory =
            std::env::temp_dir().join(format!("hh-image-stage-{}", uuid::Uuid::new_v4()));
        hh_protocol::ensure_private_directory(&directory).unwrap();
        let source = directory.join("source.png");
        std::fs::write(&source, b"original").unwrap();

        let staged = stage_remote_source(&source).unwrap();
        std::fs::write(&source, b"replacement").unwrap();

        assert_eq!(std::fs::read(&staged).unwrap(), b"original");
        assert_eq!(
            std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let symlink = directory.join("source-link.png");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        assert!(stage_remote_source(&symlink).is_err());

        std::fs::remove_file(staged).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stderr_reader_drains_beyond_its_bounded_capture() {
        let input = vec![b'x'; 32 * 1024];
        let captured = read_bounded_stderr(std::io::Cursor::new(input), 1024).unwrap();

        assert_eq!(captured.len(), 1024);
    }

    #[test]
    fn stderr_completion_wait_is_bounded_without_eof() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let started = std::time::Instant::now();

        let stderr =
            receive_stderr_with_timeout(&receiver, std::time::Duration::from_millis(10)).unwrap();

        assert!(stderr.is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn remote_source_cleanup_failure_rolls_back_uploaded_paths() {
        let paths = vec![PathBuf::from(
            "/tmp/hh-paste-12345678-1234-1234-1234-1234567890ab/file.png",
        )];
        let mut rolled_back = false;

        let error = reconcile_remote_source_cleanup(
            "developer@build-node",
            Ok(paths.clone()),
            Err(anyhow::anyhow!("local cleanup failed")),
            |destination, rollback_paths| {
                rolled_back = true;
                assert_eq!(destination, "developer@build-node");
                assert_eq!(rollback_paths, paths);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(rolled_back);
        assert!(format!("{error:#}").contains("local cleanup failed"));
    }

    #[test]
    fn upload_and_source_cleanup_errors_are_both_preserved() {
        let error = reconcile_remote_source_cleanup(
            "developer@build-node",
            Err(anyhow::anyhow!("upload failed")),
            Err(anyhow::anyhow!("local cleanup failed")),
            |_, _| unreachable!("no remote paths exist to roll back"),
        )
        .unwrap_err();
        let detail = format!("{error:#}");

        assert!(detail.contains("upload failed"));
        assert!(detail.contains("local cleanup failed"));
    }

    #[test]
    fn pasted_paths_are_shell_escaped_without_losing_spaces_or_quotes() {
        assert_eq!(
            shell_join_paths(&[
                PathBuf::from("/tmp/Screen Shot.png"),
                PathBuf::from("/tmp/designer's note.jpg"),
            ]),
            "'/tmp/Screen Shot.png' '/tmp/designer'\\''s note.jpg'"
        );
    }

    #[test]
    fn remote_paths_keep_only_a_safe_lowercase_extension() {
        let path = remote_paste_path(
            Path::new("/Users/test/Screen Shot.PNG"),
            uuid::Uuid::parse_str("12345678-1234-1234-1234-1234567890ab").unwrap(),
        );
        assert_eq!(
            path,
            "/tmp/hh-paste-12345678-1234-1234-1234-1234567890ab/file.png"
        );
    }

    #[test]
    fn scp_upload_is_structured_argv_not_a_shell_command() {
        let command = scp_upload_command_with(
            Path::new("/usr/bin/scp"),
            "developer@build-node",
            Path::new("/tmp/Screen Shot.png"),
            "/tmp/hh-paste-12345678-1234-1234-1234-1234567890ab/first.png",
        )
        .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), "/usr/bin/scp");
        assert_eq!(
            args,
            vec![
                "-q",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=6",
                "-o",
                "ServerAliveInterval=20",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                "/tmp/Screen Shot.png",
                "developer@build-node:/tmp/hh-paste-12345678-1234-1234-1234-1234567890ab/first.png",
            ]
        );
    }

    #[test]
    fn remote_upload_establishes_private_directory_and_file_permissions() {
        let directory = "/tmp/hh-paste-12345678-1234-1234-1234-1234567890ab";
        let path = format!("{directory}/first.png");
        let setup = ssh_private_directory_command_with(
            Path::new("/usr/bin/ssh"),
            "developer@build-node",
            directory,
        )
        .unwrap();
        let finalize = ssh_private_files_command_with(
            Path::new("/usr/bin/ssh"),
            "developer@build-node",
            &[path],
        )
        .unwrap();
        let setup_args = setup
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let finalize_args = finalize
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(setup_args.ends_with(&[
            "developer@build-node".to_owned(),
            "mkdir".to_owned(),
            "-m".to_owned(),
            "700".to_owned(),
            "--".to_owned(),
            directory.to_owned(),
        ]));
        assert!(finalize_args.ends_with(&[
            "developer@build-node".to_owned(),
            "chmod".to_owned(),
            "600".to_owned(),
            "--".to_owned(),
            format!("{directory}/first.png"),
        ]));
    }

    #[test]
    fn partial_remote_upload_cleanup_is_structured_and_bounded() {
        let command = ssh_cleanup_command_with(
            Path::new("/usr/bin/ssh"),
            "developer@build-node",
            &[
                "/tmp/hh-paste-12345678-1234-1234-1234-1234567890ab/first.png".to_owned(),
                "/tmp/hh-paste-abcdefab-cdef-abcd-efab-cdefabcdefab/second.png".to_owned(),
            ],
        )
        .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), "/usr/bin/ssh");
        assert_eq!(
            args,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=6",
                "-o",
                "ServerAliveInterval=20",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                "developer@build-node",
                "rm",
                "-rf",
                "--",
                "/tmp/hh-paste-12345678-1234-1234-1234-1234567890ab",
                "/tmp/hh-paste-abcdefab-cdef-abcd-efab-cdefabcdefab",
            ]
        );
    }
}
