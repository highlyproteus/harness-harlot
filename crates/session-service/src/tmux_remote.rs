//! HH's tmux server on an SSH host.
//!
//! An SSH workstation's terminals are windows of a private tmux server on the
//! remote host (`tmux -L <socket>`, the same socket name HH uses locally), so
//! they keep running when the connection drops, the app quits, or the service
//! restarts. HH drives that server in control mode over `ssh`, exactly like
//! the local one, which gives remote tabs full scrollback on reattach.
//!
//! Control mode uses ssh's stdin and stdout for the tmux protocol, so ssh can
//! never prompt. A host that needs a password, passphrase, second factor, or a
//! host-key confirmation is signed in once in a visible terminal that opens a
//! shared connection (`ControlMaster`) owned by HH; control connections then
//! reuse it without prompting.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use hh_protocol::validate_ssh_host;
use portable_pty::CommandBuilder;
use uuid::Uuid;

use crate::process::{command_with_terminal_env, system_ssh_binary};
use crate::tmux_control::ANCHOR_WINDOW_NAME;

/// Seconds ssh waits for the TCP connection before giving up.
const CONNECT_TIMEOUT_SECONDS: u32 = 15;

/// Why a remote tmux connection could not be made.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum RemoteConnectError {
    /// ssh needs a prompt answered (password, passphrase, second factor, or
    /// an unknown host key): the user signs in once in a terminal.
    SignInRequired(String),
    /// The host has no tmux 3.2+; its tabs fall back to plain SSH shells.
    NoTmux(String),
    /// The host could not be reached (DNS, refused, timed out, …).
    Unreachable(String),
}

impl std::fmt::Display for RemoteConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignInRequired(detail) => write!(formatter, "sign-in required: {detail}"),
            Self::NoTmux(reason) => write!(formatter, "tmux unavailable: {reason}"),
            Self::Unreachable(detail) => write!(formatter, "host unreachable: {detail}"),
        }
    }
}

impl std::error::Error for RemoteConnectError {}

/// Classifies ssh's diagnostics after a failed non-interactive connection.
pub(crate) fn classify_ssh_failure(stderr: &str, timed_out: bool) -> RemoteConnectError {
    let detail = last_meaningful_line(stderr);
    let lower = stderr.to_ascii_lowercase();
    let needs_prompt = [
        "permission denied",
        "host key verification failed",
        "no more authentication methods",
        "too many authentication failures",
        "passphrase",
        "keyboard-interactive",
        "authentication failed",
        "no matching host key",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if needs_prompt {
        RemoteConnectError::SignInRequired(detail)
    } else if timed_out && detail.is_empty() {
        RemoteConnectError::Unreachable("the connection timed out".to_owned())
    } else if detail.is_empty() {
        RemoteConnectError::Unreachable("ssh exited without explaining why".to_owned())
    } else {
        RemoteConnectError::Unreachable(detail)
    }
}

fn last_meaningful_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

/// HH's private tmux server on `destination`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteTmux {
    pub destination: String,
    /// `hh`, `hh-dev`, or `hh-<hash>`: the local socket name, so a dev build
    /// or a test never touches the stable app's remote sessions.
    pub socket_name: String,
}

impl RemoteTmux {
    pub(crate) fn new(destination: &str, socket_name: &str) -> Result<Self> {
        validate_ssh_host(destination).map_err(anyhow::Error::from)?;
        ensure!(
            is_atom(socket_name),
            "tmux socket name must be letters, digits and dashes"
        );
        Ok(Self {
            destination: destination.to_owned(),
            socket_name: socket_name.to_owned(),
        })
    }

    /// Where HH keeps the shared connection a sign-in opens. `%C` is ssh's
    /// hash of the destination, which keeps the path short enough for a
    /// Unix socket.
    fn control_path(&self) -> Result<String> {
        let directory = control_directory();
        hh_protocol::ensure_private_directory(&directory)
            .with_context(|| format!("prepare ssh control directory {}", directory.display()))?;
        let path = directory.join(format!("{}-%C", self.socket_name));
        path.into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("ssh control path is not UTF-8"))
    }

    /// ssh options shared by every non-interactive command: never prompt,
    /// notice a dead network within about 30 seconds, and ride HH's shared
    /// connection when a sign-in opened one (ssh connects directly when it
    /// does not exist).
    fn batch_command(&self) -> Result<Command> {
        let mut command = Command::new(system_ssh_binary()?);
        command
            .args(["-T", "-o", "BatchMode=yes"])
            .arg("-o")
            .arg(format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"))
            .args([
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ControlMaster=no",
            ])
            .arg("-o")
            .arg(format!("ControlPath={}", self.control_path()?))
            .arg("--")
            .arg(&self.destination)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        Ok(command)
    }

    /// Attaches a control client to session `session_name` on the host,
    /// starting HH's server there if needed.
    pub(crate) fn control_command(&self, session_name: &str) -> Result<Command> {
        ensure!(
            is_atom(session_name),
            "tmux session name must be letters, digits and dashes"
        );
        let mut command = self.batch_command()?;
        command.arg(remote_shell_command(&format!(
            "{FIND_TMUX}exec \"$t\" -L {socket} -f /dev/null -C new-session -A -s {session_name} \
             -n {ANCHOR_WINDOW_NAME} -- /bin/sh -c \"while :; do sleep 3600; done\"",
            socket = self.socket_name,
        )));
        Ok(command)
    }

    /// Kills session `session_name` on the host without a control
    /// connection, for a workstation deleted while offline.
    pub(crate) fn kill_session_command(&self, session_name: &str) -> Result<Command> {
        ensure!(
            is_atom(session_name),
            "tmux session name must be letters, digits and dashes"
        );
        let mut command = self.batch_command()?;
        command
            .arg(remote_shell_command(&format!(
                "{FIND_TMUX}\"$t\" -L {socket} kill-session -t ={session_name} 2>/dev/null; exit 0",
                socket = self.socket_name,
            )))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(command)
    }

    /// The interactive terminal a sign-in runs in: answers ssh's prompts
    /// once and leaves a shared connection behind for the control client.
    pub(crate) fn sign_in_command(&self, pane_id: Uuid) -> Result<CommandBuilder> {
        let argv: Vec<OsString> = vec![
            system_ssh_binary()?.into_os_string(),
            "-tt".into(),
            "-o".into(),
            "ControlMaster=yes".into(),
            "-o".into(),
            "ControlPersist=yes".into(),
            "-o".into(),
            format!("ControlPath={}", self.control_path()?).into(),
            "--".into(),
            self.destination.clone().into(),
            "echo; echo Signed in. Harness Harlot is opening your terminals.".into(),
        ];
        let mut command = command_with_terminal_env(argv, pane_id);
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
        Ok(command)
    }
}

/// Owner-only directory for HH's ssh shared connections. `/tmp` keeps the
/// socket path well under the 104-byte limit.
fn control_directory() -> PathBuf {
    PathBuf::from("/tmp").join(format!("hh-ssh-{}", rustix::process::getuid().as_raw()))
}

/// Finds a tmux 3.2+ on the host, or prints the no-tmux marker and exits.
/// POSIX sh without single quotes, so it fits in `sh -c '…'`.
const FIND_TMUX: &str = concat!(
    "t=; for c in \"$(command -v tmux 2>/dev/null)\" /opt/homebrew/bin/tmux /usr/local/bin/tmux ",
    "/usr/bin/tmux \"$HOME/.local/bin/tmux\" /home/linuxbrew/.linuxbrew/bin/tmux; do ",
    "if [ -n \"$c\" ] && [ -x \"$c\" ]; then t=$c; break; fi; done; ",
    "if [ -z \"$t\" ]; then echo \"HH-NO-TMUX tmux is not installed on this host\"; exit 3; fi; ",
    "v=$(\"$t\" -V 2>/dev/null); m=${v#tmux }; M=${m%%.*}; n=${m#*.}; n=${n%%[!0-9]*}; ",
    "case $M in \"\"|*[!0-9]*) M=0;; esac; case $n in \"\"|*[!0-9]*) n=0;; esac; ",
    "if [ \"$M\" -lt 3 ] || { [ \"$M\" -eq 3 ] && [ \"$n\" -lt 2 ]; }; then ",
    "echo \"HH-NO-TMUX tmux 3.2 or newer is needed; this host has ${v:-an unknown version}\"; exit 3; fi; ",
);

/// Runs `script` under `/bin/sh` whatever the user's login shell is.
fn remote_shell_command(script: &str) -> String {
    debug_assert!(!script.contains('\''));
    format!("exec /bin/sh -c '{script}'")
}

fn is_atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_mean_sign_in_and_network_failures_mean_unreachable() {
        assert!(matches!(
            classify_ssh_failure(
                "user@devbox: Permission denied (publickey,password).\n",
                false
            ),
            RemoteConnectError::SignInRequired(_)
        ));
        assert!(matches!(
            classify_ssh_failure("Host key verification failed.\n", false),
            RemoteConnectError::SignInRequired(_)
        ));
        assert_eq!(
            classify_ssh_failure(
                "ssh: Could not resolve hostname nowhere: nodename nor servname provided\n",
                false
            ),
            RemoteConnectError::Unreachable(
                "ssh: Could not resolve hostname nowhere: nodename nor servname provided"
                    .to_owned()
            )
        );
        assert!(matches!(
            classify_ssh_failure("", true),
            RemoteConnectError::Unreachable(_)
        ));
        assert!(FIND_TMUX.contains(&format!("{} ", crate::tmux_control::NO_TMUX_MARKER)));
    }

    #[test]
    fn remote_commands_never_prompt_and_only_embed_validated_atoms() {
        let remote = RemoteTmux::new("admin@build-node", "hh-dev").unwrap();
        let command = remote.control_command("hh-0f0f").unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert!(args.contains(&"-T".to_owned()));
        let script = args.last().unwrap();
        assert!(script.starts_with("exec /bin/sh -c '"));
        assert!(script.contains("-L hh-dev -f /dev/null -C new-session -A -s hh-0f0f"));
        assert_eq!(script.matches('\'').count(), 2);
        assert!(remote.control_command("hh; rm -rf ~").is_err());
        assert!(RemoteTmux::new("admin@build-node", "hh dev").is_err());
    }
}
