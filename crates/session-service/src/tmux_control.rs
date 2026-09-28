//! Private tmux control-mode server and command client.
//!
//! The same client drives HH's local tmux server and, through `ssh`, the
//! tmux server HH keeps on a remote host (see `tmux_remote`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;
use std::time::Duration;

use crate::paste_events::PasteEvents;
use crate::persistence::validate_title;
use crate::process::{configured_shell, is_trusted_executable_file, run_bounded_command};
use crate::pty::{RawPaneEvent, ingest_output};
use crate::registry::PANE_CONNECTION_LOST;
use crate::terminal_images::TerminalImageStore;
use crate::tmux::{TMUX_PROBE_TIMEOUT, system_tmux_binary};
use crate::tmux_remote::{RemoteConnectError, RemoteTmux, classify_ssh_failure};
use anyhow::{Context, Result, anyhow, bail, ensure};
use hh_terminal_model::TerminalModel;
use parking_lot::Mutex;
use uuid::Uuid;

const LOCAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// A command to a remote tmux crosses the network twice.
const REMOTE_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// Reading a pane's whole scrollback on reattach can take seconds on a busy
/// machine; a timeout here must not drop the connection mid-recovery.
const SCROLLBACK_READ_TIMEOUT: Duration = Duration::from_secs(30);
const LOCAL_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Covers ssh connection setup (`ConnectTimeout`) plus the bootstrap.
const REMOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(40);
pub(crate) const ANCHOR_WINDOW_NAME: &str = "hh-anchor";
const MINIMUM_TMUX_VERSION: (u32, u32) = (3, 2);
/// Window user option naming the HH pane a window belongs to, so a window
/// can be found again without saved window ids (remote reconnects) and
/// windows of closed tabs can be told apart from live ones.
pub(crate) const PANE_TAG_OPTION: &str = "@hh-pane";
/// Remote pastes cannot use a local file, so they are sent as hex keys.
const REMOTE_PASTE_CHUNK: usize = 1024;
/// Bytes of ssh diagnostics kept to explain a failed remote connection.
const MAX_STDERR_BYTES: usize = 8 * 1024;
/// First stdout line of the remote bootstrap when tmux cannot run there.
pub(crate) const NO_TMUX_MARKER: &str = "HH-NO-TMUX";

pub(crate) type PaneSinks = Arc<Mutex<HashMap<String, PaneSink>>>;

#[derive(Debug)]
pub(crate) struct PaneSink {
    pub terminal: Arc<Mutex<TerminalModel>>,
    pub revision: Arc<AtomicU64>,
    pub content_revision: Arc<AtomicU64>,
    pub events: Arc<Mutex<VecDeque<RawPaneEvent>>>,
    pub paste_events: Arc<PasteEvents>,
    pub images: Arc<TerminalImageStore>,
    pub exited: Arc<Mutex<Option<String>>>,
    pub bell_count: u64,
    pub window_id: String,
}

struct PendingReply {
    id: u64,
    sender: SyncSender<Result<Vec<String>>>,
}

/// One window of a tmux session, as listed by `list-panes -s`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ListedPane {
    pub window_id: String,
    pub pane_id: String,
    pub pane_pid: u32,
    /// The HH pane this window was created for (`@hh-pane`), if tagged.
    pub tag: Option<Uuid>,
}

pub(crate) struct TmuxControlClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<VecDeque<PendingReply>>>,
    sinks: PaneSinks,
    alive: Arc<AtomicBool>,
    next_reply_id: AtomicU64,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
    command_timeout: Duration,
    remote: bool,
}

impl std::fmt::Debug for TmuxControlClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TmuxControlClient")
            .field("alive", &self.is_alive())
            .field("remote", &self.remote)
            .field("sinks", &self.sinks.lock().len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TmuxServer {
    pub binary: PathBuf,
    pub socket_name: String,
    pub config_path: PathBuf,
}

impl TmuxServer {
    /// Discovers tmux and prepares the private server owned by `state_dir`,
    /// whose `sessions.json` references that server's window and pane ids.
    pub(crate) fn discover(state_dir: &Path) -> Result<Option<Self>> {
        let binary = match std::env::var_os("HH_TMUX_BINARY") {
            Some(path) => {
                let path =
                    std::fs::canonicalize(PathBuf::from(path)).context("resolve HH_TMUX_BINARY")?;
                ensure!(
                    is_trusted_executable_file(&path),
                    "HH_TMUX_BINARY is not a trusted executable file"
                );
                path
            }
            None => match system_tmux_binary() {
                Ok(binary) => binary,
                Err(_) => return Ok(None),
            },
        };
        if !supported_tmux_version(&binary)? {
            return Ok(None);
        }

        hh_protocol::ensure_private_directory(state_dir)
            .with_context(|| format!("prepare state directory {}", state_dir.display()))?;
        let directory = state_dir.join("tmux");
        hh_protocol::ensure_private_directory(&directory)
            .with_context(|| format!("prepare tmux directory {}", directory.display()))?;
        let config_path = directory.join("hh.conf");
        let mut config = include_str!("../bundled/hh.tmux.conf").to_owned();
        config.push_str("set -g default-shell ");
        config.push_str(&shellquote(&configured_shell()));
        config.push('\n');
        hh_protocol::atomic_write_private(&config_path, config.as_bytes())
            .with_context(|| format!("write tmux config {}", config_path.display()))?;

        Ok(Some(Self {
            binary,
            socket_name: hh_protocol::managed_tmux_socket_name(state_dir),
            config_path,
        }))
    }
}

/// Kills a private test tmux server and removes its socket file when
/// dropped, including on panic. Refuses the app's `hh`/`hh-dev` servers.
#[cfg(test)]
pub(crate) struct PrivateTmuxServerGuard {
    pub binary: PathBuf,
    pub socket_name: String,
}

#[cfg(test)]
impl Drop for PrivateTmuxServerGuard {
    fn drop(&mut self) {
        assert!(
            self.socket_name != "hh" && self.socket_name != "hh-dev",
            "refusing to kill the app's tmux server {}",
            self.socket_name
        );
        let _ = Command::new(&self.binary)
            .args(["-L", &self.socket_name, "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // tmux leaves the socket file behind on macOS after the server exits.
        let _ = std::fs::remove_file(hh_protocol::tmux_socket_path(&self.socket_name));
    }
}

fn supported_tmux_version(binary: &Path) -> Result<bool> {
    let mut command = Command::new(binary);
    command
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded_command(command, TMUX_PROBE_TIMEOUT, "tmux version probe")?;
    if !output.success {
        return Ok(false);
    }
    Ok(parse_tmux_version(&output.stdout).is_some_and(|version| version >= MINIMUM_TMUX_VERSION))
}

fn parse_tmux_version(value: &str) -> Option<(u32, u32)> {
    let version = value.trim().strip_prefix("tmux ")?;
    let (major, minor) = version.split_once('.')?;
    let minor = minor
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// What a control client says when its connection ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloseBehavior {
    /// Local: the registry reconnects and resumes every pane, so a lost
    /// connection says nothing about the panes' programs.
    KeepPanes,
    /// Remote: panes stay running on the host but are unreachable until the
    /// user reconnects the workstation.
    MarkDisconnected,
}

impl TmuxControlClient {
    pub(crate) fn spawn(
        server: &TmuxServer,
        session_name: &str,
        sinks: PaneSinks,
    ) -> Result<Arc<Self>> {
        ensure_control_atom(session_name, "tmux session name")?;
        let mut command = Command::new(&server.binary);
        command
            .args(["-L", &server.socket_name, "-f"])
            .arg(&server.config_path)
            .args(control_session_args(session_name))
            .env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stderr(Stdio::null());
        Self::start(
            command,
            session_name,
            sinks,
            ClientSettings {
                close: CloseBehavior::KeepPanes,
                command_timeout: LOCAL_COMMAND_TIMEOUT,
                startup_timeout: LOCAL_STARTUP_TIMEOUT,
                remote: false,
            },
        )
        .map_err(|failure| anyhow!("tmux control client did not start: {failure}"))
    }

    /// Connects to HH's tmux server on `remote` through `ssh` without any
    /// prompt. Failures carry a `RemoteConnectError` explaining what the user
    /// can do about them.
    pub(crate) fn spawn_remote(
        remote: &RemoteTmux,
        session_name: &str,
        sinks: PaneSinks,
    ) -> Result<Arc<Self>> {
        ensure_control_atom(session_name, "tmux session name")?;
        let mut command = remote.control_command(session_name)?;
        command.stderr(Stdio::piped());
        let client = Self::start(
            command,
            session_name,
            sinks,
            ClientSettings {
                close: CloseBehavior::MarkDisconnected,
                command_timeout: REMOTE_COMMAND_TIMEOUT,
                startup_timeout: REMOTE_STARTUP_TIMEOUT,
                remote: true,
            },
        )
        .map_err(|failure| anyhow::Error::new(failure.into_remote_error()))?;
        client.apply_bundled_options()?;
        Ok(client)
    }

    fn start(
        mut command: Command,
        session_name: &str,
        sinks: PaneSinks,
        settings: ClientSettings,
    ) -> std::result::Result<Arc<Self>, StartFailure> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| StartFailure::Spawn(error.to_string()))?;
        let stdin = child.stdin.take().expect("tmux control stdin is piped");
        let stdout = child.stdout.take().expect("tmux control stdout is piped");
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(pipe) = child.stderr.take() {
            let stderr = Arc::clone(&stderr);
            let _ = thread::Builder::new()
                .name(format!("rmux-tmux-stderr-{session_name}"))
                .spawn(move || collect_stderr(pipe, &stderr));
        }
        let pending = Arc::new(Mutex::new(VecDeque::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let reader_pending = Arc::clone(&pending);
        let reader_sinks = Arc::clone(&sinks);
        let reader_alive = Arc::clone(&alive);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let close = settings.close;
        let reader = thread::Builder::new()
            .name(format!("rmux-tmux-{session_name}"))
            .spawn(move || {
                read_control_output(
                    stdout,
                    &reader_pending,
                    &reader_sinks,
                    &reader_alive,
                    startup_sender,
                    close,
                );
            })
            .map_err(|error| StartFailure::Spawn(error.to_string()))?;
        let client = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            sinks,
            alive,
            next_reply_id: AtomicU64::new(1),
            reader: Mutex::new(Some(reader)),
            command_timeout: settings.command_timeout,
            remote: settings.remote,
        });
        match startup_receiver.recv_timeout(settings.startup_timeout) {
            Ok(Ok(())) => Ok(client),
            Ok(Err(reason)) => {
                client.invalidate("tmux is unavailable");
                Err(StartFailure::NoTmux(reason))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                client.invalidate("tmux control client did not start");
                Err(StartFailure::Exited {
                    stderr: stderr.lock().clone(),
                    timed_out: true,
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Let the stderr collector finish reading what ssh printed.
                let _ = client.child.lock().wait();
                thread::sleep(Duration::from_millis(50));
                client.invalidate("tmux control client exited");
                Err(StartFailure::Exited {
                    stderr: stderr.lock().clone(),
                    timed_out: false,
                })
            }
        }
    }

    /// Applies HH's tmux settings to a remote server, which is started with
    /// no config file so the user's own `~/.tmux.conf` cannot change how
    /// HH's panes behave.
    fn apply_bundled_options(&self) -> Result<()> {
        for line in include_str!("../bundled/hh.tmux.conf").lines() {
            let line = line.trim();
            if line.starts_with("set ") {
                self.run(line, self.command_timeout)
                    .with_context(|| format!("apply remote tmux option `{line}`"))?;
            }
        }
        Ok(())
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.remote
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub(crate) fn run(&self, command: &str, timeout: Duration) -> Result<Vec<String>> {
        ensure!(self.is_alive(), "tmux control client exited");
        ensure_control_command(command)?;
        let id = self.next_reply_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = sync_channel(1);
        {
            let mut stdin = self.stdin.lock();
            self.pending.lock().push_back(PendingReply { id, sender });
            if let Err(error) = writeln!(stdin, "{command}").and_then(|()| stdin.flush()) {
                self.invalidate("tmux control client exited");
                return Err(error).context("write tmux control command");
            }
        }
        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().retain(|reply| reply.id != id);
                self.invalidate("tmux control client timed out");
                bail!("tmux did not answer: {command}")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.invalidate("tmux control client exited");
                bail!("tmux control client exited")
            }
        }
    }

    /// Creates a detached window for HH pane `tag`, started in `cwd` (the
    /// session's directory when `None`), and marks it with `@hh-pane`.
    pub(crate) fn new_window(
        &self,
        name: &str,
        cwd: Option<&str>,
        env: &[(&str, &str)],
        tag: Uuid,
    ) -> Result<(String, String, u32)> {
        validate_title(name, "tmux window")?;
        let mut command = format!(
            "new-window -d -P -F '#{{window_id}} #{{pane_id}} #{{pane_pid}}' -n {}",
            shellquote(name),
        );
        if let Some(cwd) = cwd {
            ensure_control_atom(cwd, "tmux window working directory")?;
            command.push_str(" -c ");
            command.push_str(&shellquote(cwd));
        }
        for (key, value) in env {
            ensure_env_assignment(key, value)?;
            command.push_str(" -e ");
            command.push_str(&shellquote(&format!("{key}={value}")));
        }
        let lines = self.run(&command, self.command_timeout)?;
        let result = lines.last().context("tmux new-window returned no target")?;
        let mut fields = result.split_whitespace();
        let window_id = fields.next().context("tmux omitted window id")?.to_owned();
        let pane_id = fields.next().context("tmux omitted pane id")?.to_owned();
        let process_id = fields
            .next()
            .context("tmux omitted pane pid")?
            .parse()
            .context("tmux returned an invalid pane pid")?;
        ensure!(
            fields.next().is_none(),
            "tmux returned an invalid window target"
        );
        validate_target_id(&window_id, '@', "window")?;
        validate_target_id(&pane_id, '%', "pane")?;
        if let Err(error) = self.run(
            &format!("set-option -w -t {window_id} {PANE_TAG_OPTION} {tag}"),
            self.command_timeout,
        ) {
            let _ = self.kill_window(&window_id);
            return Err(error).context("tag the new tmux window");
        }
        Ok((window_id, pane_id, process_id))
    }

    pub(crate) fn resize_window(&self, window_id: &str, columns: u16, rows: u16) -> Result<()> {
        validate_target_id(window_id, '@', "window")?;
        self.run(
            &format!("resize-window -t {window_id} -x {columns} -y {rows}"),
            self.command_timeout,
        )?;
        Ok(())
    }

    pub(crate) fn send_keys_hex(&self, pane_id: &str, bytes: &[u8]) -> Result<()> {
        validate_target_id(pane_id, '%', "pane")?;
        for chunk in bytes.chunks(256) {
            let mut command = format!("send-keys -t {pane_id} -H");
            for byte in chunk {
                let _ = write!(command, " {byte:02x}");
            }
            self.run(&command, self.command_timeout)?;
        }
        Ok(())
    }

    /// Writes `bytes` to the pane's input in one step. Locally this goes
    /// through a private temporary file and a uniquely named tmux paste
    /// buffer (without `-p` there is no bracketing, and `-r` keeps every byte
    /// as written). A remote tmux cannot read a local file, so the bytes are
    /// sent as hex keys instead.
    pub(crate) fn paste_bytes(&self, pane_id: &str, bytes: &[u8]) -> Result<()> {
        validate_target_id(pane_id, '%', "pane")?;
        if self.remote {
            for chunk in bytes.chunks(REMOTE_PASTE_CHUNK) {
                let mut command = format!("send-keys -t {pane_id} -H");
                for byte in chunk {
                    let _ = write!(command, " {byte:02x}");
                }
                self.run(&command, self.command_timeout)?;
            }
            return Ok(());
        }
        let directory = std::env::temp_dir().join("harness-harlot-tmux-input");
        hh_protocol::ensure_private_directory(&directory)
            .with_context(|| format!("prepare tmux input directory {}", directory.display()))?;
        let buffer = format!("hh-input-{}", uuid::Uuid::new_v4().simple());
        let path = directory.join(&buffer);
        let load = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("create tmux input file {}", path.display()))?;
            file.write_all(bytes).context("write tmux input file")?;
            let path_text = path.to_str().context("tmux input path is not UTF-8")?;
            ensure_control_atom(path_text, "tmux input path")?;
            self.run(
                &format!("load-buffer -b {buffer} {}", shellquote(path_text)),
                self.command_timeout,
            )?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        load?;
        if let Err(error) = self.run(
            &format!("paste-buffer -b {buffer} -d -r -t {pane_id}"),
            self.command_timeout,
        ) {
            let _ = self.run(&format!("delete-buffer -b {buffer}"), self.command_timeout);
            return Err(error);
        }
        Ok(())
    }

    /// Reads pane user option `name` (e.g. `@hh-input-modes`); `None` when unset.
    pub(crate) fn pane_user_option(&self, pane_id: &str, name: &str) -> Result<Option<String>> {
        validate_target_id(pane_id, '%', "pane")?;
        validate_user_option_name(name)?;
        let lines = self.run(
            &format!("show-options -p -q -v -t {pane_id} {name}"),
            self.command_timeout,
        )?;
        Ok(lines.into_iter().next().filter(|value| !value.is_empty()))
    }

    /// Stores `value` (digits and commas only) in pane user option `name`,
    /// or unsets it when `value` is empty. The tmux server keeps it across
    /// session-service restarts.
    pub(crate) fn set_pane_user_option(
        &self,
        pane_id: &str,
        name: &str,
        value: &str,
    ) -> Result<()> {
        validate_target_id(pane_id, '%', "pane")?;
        validate_user_option_name(name)?;
        ensure!(
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b','),
            "tmux pane option value must be digits and commas"
        );
        let command = if value.is_empty() {
            format!("set-option -p -u -t {pane_id} {name}")
        } else {
            format!("set-option -p -t {pane_id} {name} {value}")
        };
        self.run(&command, self.command_timeout)?;
        Ok(())
    }

    pub(crate) fn kill_window(&self, window_id: &str) -> Result<()> {
        validate_target_id(window_id, '@', "window")?;
        self.run(&format!("kill-window -t {window_id}"), self.command_timeout)?;
        Ok(())
    }

    /// Moves window `window_id`, which may live in another session of this
    /// server, into session `session_name`, keeping its processes.
    pub(crate) fn move_window_to_session(&self, window_id: &str, session_name: &str) -> Result<()> {
        validate_target_id(window_id, '@', "window")?;
        ensure_control_atom(session_name, "tmux session name")?;
        self.run(
            &format!(
                "move-window -d -s {window_id} -t {}:",
                shellquote(session_name)
            ),
            self.command_timeout,
        )?;
        Ok(())
    }

    /// Kills session `session_name` of this server and every window in it.
    pub(crate) fn kill_named_session(&self, session_name: &str) -> Result<()> {
        ensure_control_atom(session_name, "tmux session name")?;
        self.run(
            &format!("kill-session -t {}", shellquote(session_name)),
            self.command_timeout,
        )?;
        Ok(())
    }

    pub(crate) fn rename_window(&self, window_id: &str, name: &str) -> Result<()> {
        validate_target_id(window_id, '@', "window")?;
        validate_title(name, "tmux window")?;
        self.run(
            &format!("rename-window -t {window_id} {}", shellquote(name)),
            self.command_timeout,
        )?;
        Ok(())
    }

    /// Every window of this client's session except the anchor.
    pub(crate) fn list_panes(&self) -> Result<Vec<ListedPane>> {
        let lines = self.run(
            &format!(
                "list-panes -s -F '#{{window_id}} #{{pane_id}} #{{pane_pid}} #{{{PANE_TAG_OPTION}}} #{{window_name}}'"
            ),
            self.command_timeout,
        )?;
        let mut listed = Vec::new();
        for line in lines {
            let (pane, name) = parse_listed_pane(&line)?;
            if name != ANCHOR_WINDOW_NAME {
                listed.push(pane);
            }
        }
        Ok(listed)
    }

    /// The pane's full scrollback and screen with styles, for rebuilding a
    /// terminal after a reattach. Allowed to take longer than other commands.
    pub(crate) fn capture_pane(&self, pane_id: &str) -> Result<Vec<u8>> {
        validate_target_id(pane_id, '%', "pane")?;
        let lines = self.run(
            &format!("capture-pane -p -e -S - -t {pane_id}"),
            SCROLLBACK_READ_TIMEOUT.max(self.command_timeout),
        )?;
        Ok(lines.join("\r\n").into_bytes())
    }

    /// Marks every registered pane whose tmux pane no longer exists as
    /// exited, after a reconnect found `listed` windows.
    pub(crate) fn mark_missing_panes_exited(&self, listed: &[ListedPane]) {
        let present = listed
            .iter()
            .map(|pane| pane.pane_id.as_str())
            .collect::<HashSet<_>>();
        for (pane_id, sink) in self.sinks.lock().iter_mut() {
            if !present.contains(pane_id.as_str()) {
                *sink.exited.lock() = Some("exited".to_owned());
            }
        }
    }

    /// Ends this control connection. The session and its windows keep
    /// running on the server.
    pub(crate) fn close(&self) {
        self.invalidate("tmux control client closed");
    }

    pub(crate) fn kill_session(&self) -> Result<()> {
        self.run("kill-session", self.command_timeout)?;
        Ok(())
    }

    pub(crate) fn register_sink(&self, pane_id: &str, sink: PaneSink) -> Result<()> {
        validate_target_id(pane_id, '%', "pane")?;
        ensure!(
            self.sinks.lock().insert(pane_id.to_owned(), sink).is_none(),
            "tmux pane {pane_id} is already registered"
        );
        Ok(())
    }

    pub(crate) fn unregister_sink(&self, pane_id: &str) {
        self.sinks.lock().remove(pane_id);
    }

    fn invalidate(&self, message: &str) {
        if self.alive.swap(false, Ordering::AcqRel) {
            let _ = self.child.lock().kill();
        }
        fail_pending(&self.pending, message);
        if self.remote {
            mark_all_sinks_exited(&self.sinks, PANE_CONNECTION_LOST);
        }
    }
}

impl Drop for TmuxControlClient {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let child = self.child.get_mut();
        let _ = child.kill();
        let _ = child.wait();
        if let Some(reader) = self.reader.get_mut().take() {
            let _ = reader.join();
        }
    }
}

/// Result of connecting: `Err` carries why tmux cannot run (a remote host
/// without a supported tmux reports this through its bootstrap).
type StartupSender = SyncSender<std::result::Result<(), String>>;

fn read_control_output(
    stdout: impl std::io::Read,
    pending: &Mutex<VecDeque<PendingReply>>,
    sinks: &Mutex<HashMap<String, PaneSink>>,
    alive: &AtomicBool,
    startup_sender: StartupSender,
    close: CloseBehavior,
) {
    let mut startup_sender = Some(startup_sender);
    let mut response: Option<(String, Vec<String>)> = None;
    // tmux passes non-ASCII `%output` bytes through raw and may split a
    // multi-byte character across two notifications, so lines are raw bytes:
    // pane output stays bytes and other lines decode lossily. Only EOF or a
    // read error ends the client.
    let mut reader = BufReader::new(stdout);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if raw.last() == Some(&b'\n') {
            raw.pop();
        }
        if response.is_none()
            && let Some(rest) = raw.strip_prefix(b"%output ")
        {
            if let Some(space) = rest.iter().position(|byte| *byte == b' ')
                && let Ok(pane_id) = std::str::from_utf8(&rest[..space])
                && let Some(sink) = sinks.lock().get_mut(pane_id)
            {
                let bytes = unescape_tmux_output(&rest[space + 1..]);
                ingest_output(
                    &sink.terminal,
                    &sink.events,
                    &sink.paste_events,
                    &sink.images,
                    &sink.revision,
                    &sink.content_revision,
                    &mut sink.bell_count,
                    &bytes,
                );
            }
            continue;
        }
        let line = String::from_utf8_lossy(&raw).into_owned();
        if startup_sender.is_some()
            && response.is_none()
            && let Some(reason) = line.strip_prefix(NO_TMUX_MARKER)
        {
            if let Some(sender) = startup_sender.take() {
                let _ = sender.send(Err(reason.trim().to_owned()));
            }
            continue;
        }
        if let Some((key, lines)) = &mut response {
            let mut fields = line.split_whitespace();
            let keyword = fields.next();
            let identity = fields.next().zip(fields.next());
            if matches!(keyword, Some("%end" | "%error"))
                && identity.is_some()
                && identity == key.split_once(' ')
            {
                let (_, lines) = response.take().expect("response block is active");
                if keyword == Some("%error") {
                    let message = lines.join("\n");
                    let message = if message.is_empty() {
                        "tmux command failed".to_owned()
                    } else {
                        message
                    };
                    if let Some(startup_sender) = startup_sender.take() {
                        let _ = startup_sender.send(Err(message));
                    } else if let Some(reply) = pending.lock().pop_front() {
                        let _ = reply.sender.send(Err(anyhow!(message)));
                    }
                } else if let Some(startup_sender) = startup_sender.take() {
                    let _ = startup_sender.send(Ok(()));
                } else if let Some(reply) = pending.lock().pop_front() {
                    let _ = reply.sender.send(Ok(lines));
                }
            } else {
                lines.push(line);
            }
            continue;
        }
        if let Some(window_id) = line
            .strip_prefix("%window-close ")
            .or_else(|| line.strip_prefix("%unlinked-window "))
        {
            mark_window_exited(sinks, window_id, "exited");
            continue;
        }
        if line == "%exit" || line.starts_with("%exit ") {
            break;
        }
        if line == "%begin" || line.starts_with("%begin ") {
            let mut fields = line.split_whitespace().skip(1);
            let key = fields
                .next()
                .zip(fields.next())
                .map_or_else(String::new, |(time, number)| format!("{time} {number}"));
            response = Some((key, Vec::new()));
        }
    }
    alive.store(false, Ordering::Release);
    fail_pending(pending, "tmux control client exited");
    if close == CloseBehavior::MarkDisconnected {
        mark_all_sinks_exited(sinks, PANE_CONNECTION_LOST);
    }
}

/// Everything a control client needs besides its command line.
#[derive(Clone, Copy, Debug)]
struct ClientSettings {
    close: CloseBehavior,
    command_timeout: Duration,
    startup_timeout: Duration,
    remote: bool,
}

/// Why a control client did not come up.
#[derive(Debug)]
enum StartFailure {
    Spawn(String),
    /// tmux itself refused: missing, too old, or its first command failed.
    NoTmux(String),
    /// The process ended (or hung) before tmux answered.
    Exited {
        stderr: String,
        timed_out: bool,
    },
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "could not start: {error}"),
            Self::NoTmux(reason) => formatter.write_str(reason),
            Self::Exited { stderr, timed_out } => {
                let detail = stderr.trim();
                match (timed_out, detail.is_empty()) {
                    (true, true) => formatter.write_str("timed out"),
                    (true, false) => write!(formatter, "timed out: {detail}"),
                    (false, true) => formatter.write_str("exited"),
                    (false, false) => formatter.write_str(detail),
                }
            }
        }
    }
}

impl StartFailure {
    fn into_remote_error(self) -> RemoteConnectError {
        match self {
            Self::Spawn(error) => RemoteConnectError::Unreachable(format!("ssh: {error}")),
            Self::NoTmux(reason) => RemoteConnectError::NoTmux(reason),
            Self::Exited { stderr, timed_out } => classify_ssh_failure(&stderr, timed_out),
        }
    }
}

/// `new-session` arguments that attach the control client to `session_name`
/// (creating it with an idle anchor window that keeps the session alive).
pub(crate) fn control_session_args(session_name: &str) -> [&str; 11] {
    [
        "-C",
        "new-session",
        "-A",
        "-s",
        session_name,
        "-n",
        ANCHOR_WINDOW_NAME,
        "--",
        "/bin/sh",
        "-c",
        "while :; do sleep 3600; done",
    ]
}

fn collect_stderr(mut pipe: impl Read, stderr: &Mutex<String>) {
    let mut buffer = [0_u8; 1024];
    while let Ok(read) = pipe.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let mut stderr = stderr.lock();
        if stderr.len() < MAX_STDERR_BYTES {
            stderr.push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    }
}

fn fail_pending(pending: &Mutex<VecDeque<PendingReply>>, message: &str) {
    for reply in pending.lock().drain(..) {
        let _ = reply.sender.send(Err(anyhow!(message.to_owned())));
    }
}

fn mark_window_exited(sinks: &Mutex<HashMap<String, PaneSink>>, window_id: &str, message: &str) {
    for sink in sinks
        .lock()
        .values_mut()
        .filter(|sink| sink.window_id == window_id)
    {
        *sink.exited.lock() = Some(message.to_owned());
    }
}

fn mark_all_sinks_exited(sinks: &Mutex<HashMap<String, PaneSink>>, message: &str) {
    for sink in sinks.lock().values_mut() {
        *sink.exited.lock() = Some(message.to_owned());
    }
}

/// Parses `window pane pid tag name` from `list_panes`; `tag` is empty for
/// windows HH did not tag. The name is only compared with the anchor's.
fn parse_listed_pane(line: &str) -> Result<(ListedPane, &str)> {
    let mut fields = line.splitn(5, ' ');
    let window_id = fields.next().context("tmux omitted window id")?.to_owned();
    let pane_id = fields.next().context("tmux omitted pane id")?.to_owned();
    let process_id = fields
        .next()
        .context("tmux omitted pane pid")?
        .parse()
        .context("tmux returned an invalid pane pid")?;
    let tag = fields.next().context("tmux omitted window tag")?;
    let name = fields.next().context("tmux omitted window name")?;
    validate_target_id(&window_id, '@', "window")?;
    validate_target_id(&pane_id, '%', "pane")?;
    Ok((
        ListedPane {
            window_id,
            pane_id,
            pane_pid: process_id,
            tag: Uuid::parse_str(tag).ok(),
        },
        name,
    ))
}

fn validate_target_id(value: &str, sigil: char, label: &str) -> Result<()> {
    ensure!(
        value.len() >= 2
            && value.starts_with(sigil)
            && value[1..].bytes().all(|byte| byte.is_ascii_digit()),
        "tmux returned an invalid {label} id"
    );
    Ok(())
}

fn ensure_control_atom(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && !value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0')),
        "{label} contains an invalid control character"
    );
    Ok(())
}

fn validate_user_option_name(name: &str) -> Result<()> {
    ensure!(
        name.len() > 1
            && name.starts_with('@')
            && name[1..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "tmux user option name is invalid"
    );
    Ok(())
}

fn ensure_control_command(command: &str) -> Result<()> {
    ensure_control_atom(command, "tmux command")
}

fn ensure_env_assignment(key: &str, value: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "tmux environment key is invalid"
    );
    ensure_control_atom(value, "tmux environment value")
}

fn shellquote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn unescape_tmux_output(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 1 < bytes.len() && bytes[index + 1] == b'\\' {
            decoded.push(b'\\');
            index += 2;
            continue;
        }
        if index + 3 < bytes.len()
            && bytes[index + 1..=index + 3]
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'7'))
        {
            let byte = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            decoded.push(byte);
            index += 4;
            continue;
        }
        decoded.push(b'\\');
        index += 1;
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_output_containing_control_keywords_stays_in_the_block() {
        struct CheckBeforeEof<'a> {
            bytes: &'a [u8],
            alive: &'a AtomicBool,
            reply: std::sync::mpsc::Receiver<Result<Vec<String>>>,
        }

        impl std::io::Read for CheckBeforeEof<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.bytes.is_empty() {
                    assert_eq!(
                        self.reply.try_recv().unwrap().unwrap(),
                        ["%exit", "%audit-percent-line", "%end 999 999 1", "AFTER"]
                    );
                    assert!(self.alive.load(Ordering::Acquire));
                }
                self.bytes.read(buffer)
            }
        }

        let alive = AtomicBool::new(true);
        let (sender, reply) = sync_channel(1);
        let pending = Mutex::new(VecDeque::from([PendingReply { id: 1, sender }]));
        let sinks = Mutex::new(HashMap::new());
        let (startup_sender, startup_receiver) = sync_channel(1);
        read_control_output(
            CheckBeforeEof {
                bytes: b"%begin 1 1 0\n%end 1 1 0\n%begin 1789779917 5 1\n%exit\n%audit-percent-line\n%end 999 999 1\nAFTER\n%end 1789779917 5 1\n",
                alive: &alive,
                reply,
            },
            &pending,
            &sinks,
            &alive,
            startup_sender,
            CloseBehavior::KeepPanes,
        );
        startup_receiver.try_recv().unwrap().unwrap();
        assert!(!alive.load(Ordering::Acquire));
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn parses_supported_tmux_versions_with_patch_suffixes() {
        assert_eq!(parse_tmux_version("tmux 3.2\n"), Some((3, 2)));
        assert_eq!(parse_tmux_version("tmux 3.6b"), Some((3, 6)));
        assert_eq!(parse_tmux_version("tmux next-3.6"), None);
    }

    #[test]
    fn unescapes_control_mode_pty_bytes() {
        assert_eq!(
            unescape_tmux_output(br"hello\040\033[31mred\033[0m\\tail\015\012"),
            b"hello \x1b[31mred\x1b[0m\\tail\r\n"
        );
    }

    fn test_sink(
        terminal: &Arc<Mutex<TerminalModel>>,
        exited: &Arc<Mutex<Option<String>>>,
    ) -> PaneSink {
        PaneSink {
            terminal: Arc::clone(terminal),
            revision: Arc::default(),
            content_revision: Arc::default(),
            events: Arc::default(),
            paste_events: Arc::default(),
            images: Arc::new(TerminalImageStore::in_directory(std::env::temp_dir())),
            exited: Arc::clone(exited),
            bell_count: 0,
            window_id: "@1".to_owned(),
        }
    }

    #[test]
    fn output_splitting_a_utf8_character_neither_ends_the_client_nor_loses_bytes() {
        let alive = AtomicBool::new(true);
        let (sender, reply) = sync_channel(1);
        let pending = Mutex::new(VecDeque::from([PendingReply { id: 1, sender }]));
        let (startup_sender, _startup) = sync_channel(1);
        let terminal = Arc::new(Mutex::new(TerminalModel::new(80, 24)));
        let exited = Arc::new(Mutex::new(None));
        let sinks = Mutex::new(HashMap::from([(
            "%1".to_owned(),
            test_sink(&terminal, &exited),
        )]));
        // "é" is 0xC3 0xA9; tmux emitted its two bytes in separate notifications.
        let mut stream = b"%begin 1 1 0\n%end 1 1 0\n".to_vec();
        stream.extend_from_slice(b"%output %1 caf\xc3\n%output %1 \xa9!\n");
        stream.extend_from_slice(b"%begin 2 2 1\nstill reading\n%end 2 2 1\n");
        read_control_output(
            stream.as_slice(),
            &pending,
            &sinks,
            &alive,
            startup_sender,
            CloseBehavior::MarkDisconnected,
        );

        assert_eq!(reply.try_recv().unwrap().unwrap(), ["still reading"]);
        let screen = terminal
            .lock()
            .styled_lines()
            .iter()
            .flat_map(|line| line.runs.iter().map(|run| run.text.clone()))
            .collect::<String>();
        assert!(screen.contains("café!"), "{screen}");
        // EOF ends the client only after every line was processed.
        assert_eq!(exited.lock().as_deref(), Some(PANE_CONNECTION_LOST));
    }

    #[test]
    fn a_lost_local_connection_leaves_panes_running_for_the_reconnect() {
        let alive = AtomicBool::new(true);
        let pending = Mutex::new(VecDeque::new());
        let (startup_sender, _startup) = sync_channel(1);
        let terminal = Arc::new(Mutex::new(TerminalModel::new(80, 24)));
        let exited = Arc::new(Mutex::new(None));
        let sinks = Mutex::new(HashMap::from([(
            "%1".to_owned(),
            test_sink(&terminal, &exited),
        )]));
        read_control_output(
            b"%begin 1 1 0\n%end 1 1 0\n".as_slice(),
            &pending,
            &sinks,
            &alive,
            startup_sender,
            CloseBehavior::KeepPanes,
        );
        assert!(!alive.load(Ordering::Acquire));
        assert_eq!(*exited.lock(), None);

        let (startup_sender, _startup) = sync_channel(1);
        read_control_output(
            b"%begin 1 1 0\n%end 1 1 0\n%window-close @1\n".as_slice(),
            &pending,
            &sinks,
            &alive,
            startup_sender,
            CloseBehavior::KeepPanes,
        );
        assert_eq!(exited.lock().as_deref(), Some("exited"));
    }

    #[test]
    fn a_host_without_tmux_is_reported_before_startup() {
        let alive = AtomicBool::new(true);
        let pending = Mutex::new(VecDeque::new());
        let sinks = Mutex::new(HashMap::new());
        let (startup_sender, startup) = sync_channel(1);
        read_control_output(
            b"HH-NO-TMUX tmux 3.2 or newer is not installed\n".as_slice(),
            &pending,
            &sinks,
            &alive,
            startup_sender,
            CloseBehavior::MarkDisconnected,
        );
        assert_eq!(
            startup.try_recv().unwrap(),
            Err("tmux 3.2 or newer is not installed".to_owned())
        );
    }

    #[test]
    fn quotes_single_quotes_for_tmux_commands() {
        assert_eq!(shellquote("don't"), "'don'\\''t'");
    }

    #[test]
    fn parses_listed_panes_with_and_without_tags() {
        let tag = Uuid::new_v4();
        let line = format!("@12 %34 567 {tag} Agent shell");
        let (pane, name) = parse_listed_pane(&line).unwrap();
        assert_eq!(
            pane,
            ListedPane {
                window_id: "@12".to_owned(),
                pane_id: "%34".to_owned(),
                pane_pid: 567,
                tag: Some(tag),
            }
        );
        assert_eq!(name, "Agent shell");
        let (pane, name) = parse_listed_pane("@0 %0 8  hh-anchor").unwrap();
        assert_eq!(pane.tag, None);
        assert_eq!(name, ANCHOR_WINDOW_NAME);
    }
}
