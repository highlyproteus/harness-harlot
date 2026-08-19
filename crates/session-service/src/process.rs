//! Local process plumbing: shells, structured OpenSSH argv, and bounded command IO.
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::tmux::TMUX_PROBE_MAX_BYTES;
use anyhow::{Context, Result, anyhow, bail};
use hh_protocol::{validate_ssh_host, validate_workspace_dir};
use portable_pty::CommandBuilder;
use uuid::Uuid;

pub(crate) fn fallback_cwd() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| valid_local_cwd(path))
        .context("HOME does not name an accessible local directory")?;
    Ok(home)
}

pub(crate) fn local_spawn_dir(dir_override: Option<&str>) -> Result<PathBuf> {
    if let Some(dir) = dir_override {
        let path = Path::new(dir);
        if valid_local_cwd(path) {
            return Ok(path.to_path_buf());
        }
    }
    fallback_cwd()
}

pub(crate) fn valid_local_cwd(path: &Path) -> bool {
    path.is_absolute() && path.metadata().is_ok_and(|metadata| metadata.is_dir())
}

pub(crate) fn configured_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| shell.starts_with('/') && std::path::Path::new(shell).exists())
        .unwrap_or_else(|| "/bin/sh".to_owned())
}

pub(crate) fn local_shell_command(pane_id: Uuid, cwd: &Path) -> CommandBuilder {
    let mut command = command_with_terminal_env([OsString::from(configured_shell())], pane_id);
    command.cwd(cwd);
    command
}

pub(crate) fn system_ssh_command(
    pane_id: Uuid,
    host: &str,
    remote_dir: Option<&str>,
) -> Result<CommandBuilder> {
    system_ssh_command_with(system_ssh_binary()?, pane_id, host, remote_dir)
}

pub(crate) fn system_ssh_command_with(
    binary: impl AsRef<OsStr>,
    pane_id: Uuid,
    host: &str,
    remote_dir: Option<&str>,
) -> Result<CommandBuilder> {
    validate_ssh_host(host).map_err(anyhow::Error::from)?;
    let mut argv = vec![binary.as_ref().to_owned()];
    if let Some(dir) = remote_dir {
        validate_workspace_dir(dir).map_err(anyhow::Error::from)?;
        let quoted = format!("'{}'", dir.replace('\'', "'\\''"));
        let remote = OsString::from(format!(
            "cd {quoted} 2>/dev/null; exec \"${{SHELL:-/bin/sh}}\" -l"
        ));
        argv.extend([
            OsString::from("-tt"),
            OsString::from("--"),
            OsString::from(host),
            remote,
        ]);
    } else {
        argv.extend([OsString::from("--"), OsString::from(host)]);
    }
    Ok(command_with_terminal_env(argv, pane_id))
}

pub(crate) fn command_with_terminal_env(
    argv: impl IntoIterator<Item = OsString>,
    pane_id: Uuid,
) -> CommandBuilder {
    let mut command = CommandBuilder::from_argv(argv.into_iter().collect());
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let pane_id = pane_id.to_string();
    command.env(hh_protocol::pane_id_env(), pane_id);
    if let Some(home) = std::env::var_os("HOME") {
        command.cwd(home);
    }
    command
}

pub(crate) fn system_ssh_binary() -> Result<PathBuf> {
    for path in [Path::new("/usr/bin/ssh"), Path::new("/bin/ssh")] {
        if is_trusted_executable_file(path) {
            return Ok(path.to_path_buf());
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path).filter(|path| path.is_absolute()) {
            let candidate = directory.join("ssh");
            if is_trusted_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    bail!("installed system OpenSSH client was not found")
}

/// Whether the path is an existing file that is executable and not writable
/// by group or world. Used for every externally spawned binary so a
/// tampered, group-writable `ssh`/`tmux` on `PATH` is never trusted.
pub(crate) fn is_trusted_executable_file(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
            && metadata.permissions().mode() & 0o022 == 0
    })
}

#[derive(Debug)]
pub(crate) struct BoundedCommandOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) fn run_bounded_command(
    mut command: Command,
    timeout: Duration,
    operation: &'static str,
) -> Result<BoundedCommandOutput> {
    let mut child = command
        .spawn()
        .with_context(|| format!("start {operation}"))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("{operation} stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .with_context(|| format!("{operation} stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || read_limited_command_output(stdout, operation));
    let stderr_reader = thread::spawn(move || read_limited_command_output(stderr, operation));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("observe {operation}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("{operation} timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = join_command_reader(stdout_reader, operation, "stdout")?;
    let stderr = join_command_reader(stderr_reader, operation, "stderr")?;
    Ok(BoundedCommandOutput {
        success: status.success(),
        stdout: String::from_utf8(stdout)
            .with_context(|| format!("{operation} output was not UTF-8"))?,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

pub(crate) fn read_limited_command_output(
    mut reader: impl Read,
    operation: &'static str,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut overflow = false;
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("read {operation} output"))?;
        if read == 0 {
            break;
        }
        if output.len().saturating_add(read) <= TMUX_PROBE_MAX_BYTES {
            output.extend_from_slice(&buffer[..read]);
        } else {
            overflow = true;
        }
    }
    if overflow {
        bail!("{operation} output exceeded {TMUX_PROBE_MAX_BYTES} bytes");
    }
    Ok(output)
}

pub(crate) fn join_command_reader(
    reader: thread::JoinHandle<Result<Vec<u8>>>,
    operation: &str,
    stream: &str,
) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow!("{operation} {stream} reader panicked"))?
}

pub(crate) fn shell_title() -> String {
    std::path::Path::new(&configured_shell())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("shell")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::process::{configured_shell, local_shell_command, system_ssh_command_with};

    #[test]
    fn local_terminal_command_remains_the_configured_shell_without_arguments() {
        let pane_id = Uuid::nil();
        let cwd = fallback_cwd().unwrap();
        let command = local_shell_command(pane_id, &cwd);

        assert_eq!(
            command.get_argv(),
            &[OsString::from(configured_shell())],
            "the SSH track must not wrap or alter local shell startup"
        );
    }

    #[test]
    fn ssh_command_uses_structured_argv_without_security_overrides() {
        let command =
            system_ssh_command_with("/usr/bin/ssh", Uuid::nil(), "admin@prod-east", None).unwrap();

        assert_eq!(
            command.get_argv(),
            &[
                OsString::from("/usr/bin/ssh"),
                OsString::from("--"),
                OsString::from("admin@prod-east"),
            ]
        );
    }

    #[test]
    fn ssh_command_quotes_configured_remote_directory() {
        let command = system_ssh_command_with(
            "/usr/bin/ssh",
            Uuid::nil(),
            "admin@prod-east",
            Some("/srv/app d'ir"),
        )
        .unwrap();

        assert_eq!(
            command.get_argv(),
            &[
                OsString::from("/usr/bin/ssh"),
                OsString::from("-tt"),
                OsString::from("--"),
                OsString::from("admin@prod-east"),
                OsString::from("cd '/srv/app d'\\''ir' 2>/dev/null; exec \"${SHELL:-/bin/sh}\" -l",),
            ]
        );
    }

    #[test]
    fn invalid_ssh_destinations_are_rejected_before_command_construction() {
        for host in [
            "-oProxyCommand=bad",
            "user@@host",
            "host command",
            "host\n-A",
        ] {
            assert!(
                system_ssh_command_with("/usr/bin/ssh", Uuid::nil(), host, None).is_err(),
                "host: {host:?}"
            );
        }
    }
}
