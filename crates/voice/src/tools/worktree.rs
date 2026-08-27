use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

pub(super) const MAX_SUBPROCESS_STDERR_BYTES: usize = 16 * 1024;
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(30);
const SUBPROCESS_REAP_TIMEOUT: Duration = Duration::from_secs(1);
const STDERR_COMPLETION_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) fn validate_worktree_branch(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.len() > 100
        || !branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
    {
        bail!("branch must match [A-Za-z0-9._/-]{{1,100}}");
    }
    Ok(())
}

pub(super) fn validate_worktree_base(base: &str) -> Result<()> {
    let grammar_is_safe = !base.is_empty()
        && base.len() <= 200
        && !base.starts_with(['-', '/'])
        && !base.ends_with(['/', '.'])
        && !base.contains("..")
        && !base.contains("//")
        && !base.contains("@{")
        && base
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
        && base.split('/').all(|component| {
            !component.is_empty()
                && Path::new(component)
                    .extension()
                    .is_none_or(|extension| !extension.eq_ignore_ascii_case("lock"))
        });
    ensure!(
        grammar_is_safe,
        "base must be a conservative Git ref using [A-Za-z0-9._/-]"
    );
    Ok(())
}

pub(super) fn worktree_path(repo_dir: &Path, branch: &str) -> Result<PathBuf> {
    validate_worktree_branch(branch)?;
    let repo_name = repo_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("repository directory has no UTF-8 name")?;
    let parent = repo_dir
        .parent()
        .context("repository directory has no parent")?;
    let path_name = branch.replace('/', "-");
    if matches!(path_name.as_str(), "." | "..") {
        bail!("branch does not produce a safe worktree directory name");
    }
    Ok(parent
        .join(format!("{repo_name}-worktrees"))
        .join(path_name))
}

pub(super) fn git_worktree_command(
    repo: &Path,
    target: &Path,
    branch: &str,
    base: Option<&str>,
) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(branch)
        .arg("--")
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(base) = base {
        command.arg(base);
    }
    command
}

pub(super) fn run_git_worktree(
    repo: &Path,
    target: &Path,
    branch: &str,
    base: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut command = git_worktree_command(repo, target, branch, base);
    let (status, stderr) = run_child_with_stderr_timeout(
        &mut command,
        WORKTREE_TIMEOUT,
        "git worktree add",
        cancelled,
    )?;
    if !status.success() {
        bail!("git worktree add failed: {}", stderr.trim());
    }
    Ok(())
}

pub(super) fn run_child_with_stderr_timeout(
    command: &mut Command,
    timeout: Duration,
    operation: &str,
    cancelled: &AtomicBool,
) -> Result<(std::process::ExitStatus, String)> {
    let mut child = command
        .spawn()
        .with_context(|| format!("launch {operation}"))?;
    let mut stderr = child
        .stderr
        .take()
        .with_context(|| format!("capture {operation} stderr"))?;
    let (stderr_tx, stderr_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    thread::spawn(move || {
        let mut captured = 0_usize;
        let mut buffer = [0_u8; 8 * 1024];
        while let Ok(read) = stderr.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let keep = read.min(MAX_SUBPROCESS_STDERR_BYTES.saturating_sub(captured));
            if keep > 0 && stderr_tx.send(buffer[..keep].to_vec()).is_err() {
                break;
            }
            captured = captured.saturating_add(keep);
        }
    });

    let deadline = Instant::now() + timeout;
    let mut captured = Vec::with_capacity(MAX_SUBPROCESS_STDERR_BYTES);
    let status = loop {
        while let Ok(chunk) = stderr_rx.try_recv() {
            captured.extend_from_slice(&chunk);
        }
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for {operation}"))?
        {
            break status;
        }
        if cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
            let reap_deadline = Instant::now() + SUBPROCESS_REAP_TIMEOUT;
            while Instant::now() < reap_deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            bail!("{operation} cancelled");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let reap_deadline = Instant::now() + SUBPROCESS_REAP_TIMEOUT;
            while Instant::now() < reap_deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            bail!("{operation} timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stderr_deadline = Instant::now() + STDERR_COMPLETION_TIMEOUT;
    while captured.len() < MAX_SUBPROCESS_STDERR_BYTES && Instant::now() < stderr_deadline {
        match stderr_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(chunk) => captured.extend_from_slice(&chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    captured.truncate(MAX_SUBPROCESS_STDERR_BYTES);
    Ok((status, String::from_utf8_lossy(&captured).into_owned()))
}
