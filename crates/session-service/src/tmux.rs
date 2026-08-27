//! tmux command construction, bounded probes, and scan parsing.
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process::{
    BoundedCommandOutput, command_with_terminal_env, is_trusted_executable_file,
    run_bounded_command, system_ssh_binary,
};
use anyhow::{Context, Result, bail};
use hh_protocol::{
    TmuxSession, TmuxSessionAttachIssue, TmuxSessionId, validate_ssh_host, validate_workspace_dir,
};
use portable_pty::CommandBuilder;
use uuid::Uuid;

pub(crate) const TMUX_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) const TMUX_PROBE_MAX_BYTES: usize = 64 * 1024;

pub(crate) const TMUX_PROBE_MAX_SESSIONS: usize = 64;

pub(crate) const MAX_TMUX_ATTACH_SESSIONS: usize = 32;

pub(crate) const TMUX_SESSION_LIST_FORMAT: &str =
    "S #{session_id} #{session_windows} #{session_attached} #{session_name}";

pub(crate) const TMUX_REMOTE_LIST_COMMAND: &str = "LC_ALL=C exec tmux list-sessions -F 'S #{session_id} #{session_windows} #{session_attached} #{session_name}'";

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TmuxAttachmentPlan {
    pub(crate) launch: Vec<TmuxSession>,
    pub(crate) skipped: Vec<TmuxSessionAttachIssue>,
}

pub(crate) fn plan_tmux_session_attachments(
    session_ids: &[TmuxSessionId],
    already_open: &HashSet<TmuxSessionId>,
    known_sessions: &HashMap<TmuxSessionId, TmuxSession>,
) -> Result<TmuxAttachmentPlan> {
    if session_ids.is_empty() {
        bail!("select at least one tmux session to open");
    }
    if session_ids.len() > MAX_TMUX_ATTACH_SESSIONS {
        bail!("select at most {MAX_TMUX_ATTACH_SESSIONS} tmux sessions at once");
    }
    let mut seen = HashSet::new();
    let mut plan = TmuxAttachmentPlan {
        launch: Vec::new(),
        skipped: Vec::new(),
    };
    for session_id in session_ids {
        let message = if !seen.insert(session_id.clone()) {
            Some("selected more than once")
        } else if already_open.contains(session_id) {
            Some("already open in this workstation")
        } else if !known_sessions.contains_key(session_id) {
            Some("session no longer exists")
        } else {
            None
        };
        if let Some(message) = message {
            plan.skipped.push(TmuxSessionAttachIssue {
                session_id: session_id.clone(),
                message: message.to_owned(),
            });
        } else if let Some(session) = known_sessions.get(session_id) {
            plan.launch.push(session.clone());
        }
    }
    Ok(plan)
}

/// Attaches exactly the way the user would by hand. This deliberately creates
/// no helper session and sets no option: every `set-option` reachable from a
/// directly attached session would persist on the user's own tmux server.
pub(crate) fn tmux_local_attach_command(
    pane_id: Uuid,
    session_id: &TmuxSessionId,
) -> Result<CommandBuilder> {
    Ok(tmux_local_attach_command_with_binary(
        system_tmux_binary()?,
        pane_id,
        session_id,
    ))
}

fn tmux_local_attach_command_with_binary(
    binary: PathBuf,
    pane_id: Uuid,
    session_id: &TmuxSessionId,
) -> CommandBuilder {
    command_with_terminal_env(
        [
            binary.into_os_string(),
            OsString::from("attach-session"),
            OsString::from("-t"),
            OsString::from(session_id.as_str()),
        ],
        pane_id,
    )
}

/// Single-quoting is safe because `TmuxSessionId` is `$` + ASCII digits.
pub(crate) fn tmux_remote_attach_command(session_id: &TmuxSessionId) -> OsString {
    OsString::from(format!("exec tmux attach-session -t '{session_id}'"))
}

pub(crate) fn tmux_ssh_attach_command(
    pane_id: Uuid,
    host: &str,
    session_id: &TmuxSessionId,
) -> Result<CommandBuilder> {
    validate_ssh_host(host).map_err(anyhow::Error::from)?;
    // OpenSSH does not allocate a remote PTY for a supplied command by
    // default. tmux attach requires one, while the metadata-only scan does
    // not, so force it only for this fixed attach path.
    Ok(command_with_terminal_env(
        [
            system_ssh_binary()?.into_os_string(),
            OsString::from("-tt"),
            OsString::from("--"),
            OsString::from(host),
            tmux_remote_attach_command(session_id),
        ],
        pane_id,
    ))
}

pub(crate) fn system_tmux_binary() -> Result<PathBuf> {
    for path in [
        Path::new("/opt/homebrew/bin/tmux"),
        Path::new("/usr/local/bin/tmux"),
        Path::new("/usr/bin/tmux"),
    ] {
        let Ok(resolved) = std::fs::canonicalize(path) else {
            continue;
        };
        if is_trusted_executable_file(&resolved) {
            return Ok(resolved);
        }
    }
    bail!("trusted tmux executable was not found in a supported system location")
}

pub(crate) fn tmux_local_probe_command() -> Result<Command> {
    let mut command = Command::new(system_tmux_binary()?);
    command
        .args(["list-sessions", "-F", TMUX_SESSION_LIST_FORMAT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

pub(crate) fn tmux_ssh_probe_command(destination: &str) -> Result<Command> {
    validate_ssh_host(destination).map_err(anyhow::Error::from)?;
    let mut command = Command::new(system_ssh_binary()?);
    // This is intentionally a single fixed remote command, not an arbitrary
    // user string. With piped stdout, OpenSSH does not allocate a tty.
    // Pin the locale inside the fixed remote command: a local process
    // environment is not guaranteed to pass through sshd AcceptEnv.
    command
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "ServerAliveInterval=2",
            "-o",
            "ServerAliveCountMax=1",
            "--",
        ])
        .arg(destination)
        .arg(TMUX_REMOTE_LIST_COMMAND)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

pub(crate) fn remote_directory_command(destination: &str, path: &str) -> Result<Command> {
    validate_ssh_host(destination).map_err(anyhow::Error::from)?;
    validate_workspace_dir(path).map_err(anyhow::Error::from)?;
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    let mut command = Command::new(system_ssh_binary()?);
    command
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "--"])
        .arg(destination)
        .arg(format!("ls -1p -- {quoted}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

pub(crate) fn run_tmux_probe(command: Command) -> Result<BoundedCommandOutput> {
    run_tmux_probe_with_timeout(command, TMUX_PROBE_TIMEOUT)
}

pub(crate) fn run_tmux_probe_with_timeout(
    command: Command,
    timeout: Duration,
) -> Result<BoundedCommandOutput> {
    run_bounded_command(command, timeout, "tmux scan")
}

pub(crate) fn parse_tmux_scan(output: &str) -> Result<Vec<TmuxSession>> {
    let mut sessions = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        // tmux sanitizes control characters in format output on current
        // releases, turning tab delimiters into underscores. Keep the three
        // machine fields first and split only four times so session names may
        // still contain spaces.
        let mut fields = line.splitn(5, ' ');
        match fields.next() {
            Some("S") => {
                if sessions.len() >= TMUX_PROBE_MAX_SESSIONS {
                    bail!("tmux scan returned more than {TMUX_PROBE_MAX_SESSIONS} sessions");
                }
                let (Some(id), Some(window_count), Some(attached), Some(name), None) = (
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                ) else {
                    bail!("tmux scan returned malformed metadata");
                };
                let id = TmuxSessionId::try_from(id.to_owned()).map_err(anyhow::Error::from)?;
                if name.is_empty()
                    || name.chars().count() > 80
                    || name.chars().any(char::is_control)
                {
                    bail!("tmux scan returned an unsafe session label");
                }
                let window_count = window_count
                    .parse::<u32>()
                    .context("tmux session window count was invalid")?;
                let attached_clients = attached
                    .parse::<u32>()
                    .context("tmux session attached-client count was invalid")?;
                if sessions
                    .iter()
                    .any(|session: &TmuxSession| session.id == id)
                {
                    bail!("tmux scan returned a duplicate session ID");
                }
                sessions.push(TmuxSession {
                    id,
                    name: name.to_owned(),
                    windows: window_count,
                    attached_clients,
                });
            }
            _ => bail!("tmux scan returned malformed metadata"),
        }
    }
    Ok(sessions)
}

pub(crate) fn tmux_reports_no_server(stderr: &str) -> bool {
    stderr.contains("no server running") || stderr.contains("error connecting to")
}

pub(crate) fn probe_error_summary(stderr: &str) -> String {
    let message = stderr.lines().next().unwrap_or("unknown error");
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect()
}

#[cfg(test)]
pub(crate) fn tmux_session(id: &str, name: &str) -> TmuxSession {
    TmuxSession {
        id: TmuxSessionId::try_from(id.to_owned()).unwrap(),
        name: name.to_owned(),
        windows: 1,
        attached_clients: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    use crate::process::system_ssh_binary;
    use crate::tmux::{
        parse_tmux_scan, plan_tmux_session_attachments, tmux_session, tmux_ssh_attach_command,
        tmux_ssh_probe_command,
    };

    #[test]
    fn tmux_attach_uses_only_fixed_structured_commands() {
        let target = TmuxSessionId::try_from("$42".to_owned()).unwrap();
        let pane_id = Uuid::nil();
        let trusted_binary = PathBuf::from("/usr/bin/tmux");
        let local = tmux_local_attach_command_with_binary(trusted_binary.clone(), pane_id, &target);
        assert!(trusted_binary.is_absolute());
        assert_eq!(
            local.get_argv(),
            &[
                trusted_binary.into_os_string(),
                OsString::from("attach-session"),
                OsString::from("-t"),
                OsString::from("$42"),
            ]
        );
        // Attaching must never mutate the user's own tmux server: no option is
        // set and no session is created, only a plain attach.
        for mutation in [
            "set-option",
            "new-session",
            "window-size",
            "aggressive-resize",
        ] {
            assert!(
                !local.get_argv().contains(&OsString::from(mutation)),
                "local attach mutates the user's tmux server: {mutation}"
            );
        }

        let expected_remote = "exec tmux attach-session -t '$42'";
        let remote = tmux_remote_attach_command(&target);
        assert_eq!(remote, OsString::from(expected_remote));
        let remote_ssh = tmux_ssh_attach_command(pane_id, "admin@build-node", &target).unwrap();
        assert_eq!(
            remote_ssh.get_argv(),
            &[
                system_ssh_binary().unwrap().into_os_string(),
                OsString::from("-tt"),
                OsString::from("--"),
                OsString::from("admin@build-node"),
                OsString::from(expected_remote),
            ]
        );
        for target in ["name", "$42;bad", "$4 2", "$-1", "42", "$42'bad"] {
            assert!(
                TmuxSessionId::try_from(target.to_owned()).is_err(),
                "target: {target:?}"
            );
        }
    }

    #[cfg(test)]
    #[test]
    fn tmux_attachment_plan_opens_each_unique_selection_and_skips_invalid_targets() {
        let first = tmux_session("$1", "editor");
        let second = tmux_session("$2", "server");
        let missing = TmuxSessionId::try_from("$3".to_owned()).unwrap();
        let already_open = HashSet::from([second.id.clone()]);
        let known_sessions = HashMap::from([
            (first.id.clone(), first.clone()),
            (second.id.clone(), second.clone()),
        ]);
        let plan = plan_tmux_session_attachments(
            &[
                first.id.clone(),
                second.id.clone(),
                first.id.clone(),
                missing.clone(),
            ],
            &already_open,
            &known_sessions,
        )
        .unwrap();
        assert_eq!(plan.launch, vec![first.clone()]);
        assert_eq!(
            plan.skipped,
            vec![
                TmuxSessionAttachIssue {
                    session_id: second.id,
                    message: "already open in this workstation".to_owned(),
                },
                TmuxSessionAttachIssue {
                    session_id: first.id,
                    message: "selected more than once".to_owned(),
                },
                TmuxSessionAttachIssue {
                    session_id: missing,
                    message: "session no longer exists".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn tmux_scan_parser_accepts_printable_metadata_from_real_tmux() {
        let sessions = parse_tmux_scan("S $9 3 1 build shell\n").unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.as_str(), "$9");
        assert_eq!(sessions[0].name, "build shell");
        assert_eq!(sessions[0].windows, 3);
        assert_eq!(sessions[0].attached_clients, 1);
    }

    #[test]
    fn tmux_scan_parser_bounds_and_rejects_malicious_metadata() {
        let sessions = parse_tmux_scan("S $1 2 1 build\nS $2 1 0 research\n").unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id.as_str(), "$1");
        assert_eq!(sessions[0].windows, 2);
        assert_eq!(sessions[1].name, "research");
        assert_eq!(sessions[1].attached_clients, 0);

        for output in [
            "build $1 1 0 name\n",
            "S $1 1 0\n",
            "S $1 1 0 bad\u{0007}name\n",
            "S $1;bad 1 0 name\n",
            "S $1 not-a-number 0 name\n",
            "S $1 1 0 name\nS $1 1 0 other\n",
            "S $1 1 0 name\nW $1 1 0 editor\n",
        ] {
            assert!(parse_tmux_scan(output).is_err(), "output: {output:?}");
        }
        assert!(tmux_reports_no_server("no server running on /tmp/tmux"));
        assert!(tmux_reports_no_server(
            "error connecting to /tmp/tmux (No such file or directory)"
        ));
    }

    #[test]
    fn remote_tmux_probe_is_a_fixed_command_not_a_user_command() {
        let command = tmux_ssh_probe_command("admin@build-node").unwrap();
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                OsStr::new("-o"),
                OsStr::new("BatchMode=yes"),
                OsStr::new("-o"),
                OsStr::new("ConnectTimeout=3"),
                OsStr::new("-o"),
                OsStr::new("ServerAliveInterval=2"),
                OsStr::new("-o"),
                OsStr::new("ServerAliveCountMax=1"),
                OsStr::new("--"),
                OsStr::new("admin@build-node"),
                OsStr::new(TMUX_REMOTE_LIST_COMMAND),
            ]
        );
        assert!(tmux_ssh_probe_command("build;whoami").is_err());
    }

    #[test]
    fn tmux_probe_timeout_is_bounded_and_reports_an_error() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let error = run_tmux_probe_with_timeout(command, Duration::from_millis(20)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }
}
