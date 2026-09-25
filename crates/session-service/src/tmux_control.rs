//! Private tmux control-mode server and command client.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
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
use crate::terminal_images::TerminalImageStore;
use crate::tmux::{TMUX_PROBE_TIMEOUT, system_tmux_binary};
use anyhow::{Context, Result, anyhow, bail, ensure};
use hh_terminal_model::TerminalModel;
use parking_lot::Mutex;

const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const ANCHOR_WINDOW_NAME: &str = "hh-anchor";
const MINIMUM_TMUX_VERSION: (u32, u32) = (3, 2);

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

pub(crate) struct TmuxControlClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<VecDeque<PendingReply>>>,
    sinks: PaneSinks,
    alive: Arc<AtomicBool>,
    next_reply_id: AtomicU64,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for TmuxControlClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TmuxControlClient")
            .field("alive", &self.is_alive())
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
            socket_name: managed_tmux_socket_name(state_dir),
            config_path,
        }))
    }
}

/// Names the private tmux server (`tmux -L <name>`) owned by `state_dir`.
///
/// The default install keeps the readable `hh` (release) or `hh-dev` (debug)
/// socket. Any other state directory, including every `HH_STATE_DIR`
/// override, gets `hh-<fnv1a64 of the canonical path>` so disposable test and
/// custom-state services never share or disrupt the app's live server. The
/// digest is fixed (not `DefaultHasher`) so a restarted or updated service
/// finds the same server again.
#[must_use]
pub fn managed_tmux_socket_name(state_dir: &Path) -> String {
    let canonical = canonical_state_dir(state_dir);
    let is_default_install = std::env::var_os(hh_protocol::STATE_DIR_ENV).is_none()
        && hh_protocol::state_directory()
            .is_some_and(|default| canonical_state_dir(&default) == canonical);
    if is_default_install {
        let name = if cfg!(debug_assertions) {
            "hh-dev"
        } else {
            "hh"
        };
        return name.to_owned();
    }
    format!(
        "hh-{:016x}",
        fnv1a64(canonical.as_os_str().as_encoded_bytes())
    )
}

fn canonical_state_dir(state_dir: &Path) -> PathBuf {
    std::fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
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
        let base =
            std::env::var_os("TMUX_TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let uid = rustix::process::getuid().as_raw();
        let _ = std::fs::remove_file(base.join(format!("tmux-{uid}")).join(&self.socket_name));
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

impl TmuxControlClient {
    pub(crate) fn spawn(
        server: &TmuxServer,
        session_name: &str,
        sinks: PaneSinks,
    ) -> Result<Arc<Self>> {
        ensure_control_atom(session_name, "tmux session name")?;
        let mut child = Command::new(&server.binary)
            .args(["-L", &server.socket_name, "-f"])
            .arg(&server.config_path)
            .args([
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
            ])
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("start tmux control client for {session_name}"))?;
        let stdin = child
            .stdin
            .take()
            .context("tmux control stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("tmux control stdout was not piped")?;
        let pending = Arc::new(Mutex::new(VecDeque::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let reader_pending = Arc::clone(&pending);
        let reader_sinks = Arc::clone(&sinks);
        let reader_alive = Arc::clone(&alive);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let reader = thread::Builder::new()
            .name(format!("rmux-tmux-{session_name}"))
            .spawn(move || {
                read_control_output(
                    stdout,
                    &reader_pending,
                    &reader_sinks,
                    &reader_alive,
                    startup_sender,
                );
            })
            .context("start tmux control reader")?;
        let client = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            sinks,
            alive,
            next_reply_id: AtomicU64::new(1),
            reader: Mutex::new(Some(reader)),
        });
        if startup_receiver
            .recv_timeout(DEFAULT_COMMAND_TIMEOUT)
            .is_err()
        {
            client.invalidate("tmux control client did not start");
            bail!("tmux control client did not start");
        }
        Ok(client)
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

    pub(crate) fn new_window(
        &self,
        name: &str,
        cwd: &Path,
        env: &[(&str, &str)],
    ) -> Result<(String, String, u32)> {
        validate_title(name, "tmux window")?;
        let cwd = cwd
            .to_str()
            .context("tmux window working directory is not UTF-8")?;
        ensure_control_atom(cwd, "tmux window working directory")?;
        let mut command = format!(
            "new-window -d -P -F '#{{window_id}} #{{pane_id}} #{{pane_pid}}' -n {} -c {}",
            shellquote(name),
            shellquote(cwd)
        );
        for (key, value) in env {
            ensure_env_assignment(key, value)?;
            command.push_str(" -e ");
            command.push_str(&shellquote(&format!("{key}={value}")));
        }
        let lines = self.run(&command, DEFAULT_COMMAND_TIMEOUT)?;
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
        Ok((window_id, pane_id, process_id))
    }

    pub(crate) fn resize_window(&self, window_id: &str, columns: u16, rows: u16) -> Result<()> {
        validate_target_id(window_id, '@', "window")?;
        self.run(
            &format!("resize-window -t {window_id} -x {columns} -y {rows}"),
            DEFAULT_COMMAND_TIMEOUT,
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
            self.run(&command, DEFAULT_COMMAND_TIMEOUT)?;
        }
        Ok(())
    }

    /// Writes `bytes` to the pane's input in one step through a private
    /// temporary file and a uniquely named tmux paste buffer. Without `-p`
    /// there is no bracketing, and `-r` keeps every byte as written.
    pub(crate) fn paste_bytes(&self, pane_id: &str, bytes: &[u8]) -> Result<()> {
        validate_target_id(pane_id, '%', "pane")?;
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
                DEFAULT_COMMAND_TIMEOUT,
            )?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        load?;
        if let Err(error) = self.run(
            &format!("paste-buffer -b {buffer} -d -r -t {pane_id}"),
            DEFAULT_COMMAND_TIMEOUT,
        ) {
            let _ = self.run(
                &format!("delete-buffer -b {buffer}"),
                DEFAULT_COMMAND_TIMEOUT,
            );
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
            DEFAULT_COMMAND_TIMEOUT,
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
        self.run(&command, DEFAULT_COMMAND_TIMEOUT)?;
        Ok(())
    }

    pub(crate) fn kill_window(&self, window_id: &str) -> Result<()> {
        validate_target_id(window_id, '@', "window")?;
        self.run(
            &format!("kill-window -t {window_id}"),
            DEFAULT_COMMAND_TIMEOUT,
        )?;
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
            DEFAULT_COMMAND_TIMEOUT,
        )?;
        Ok(())
    }

    /// Kills session `session_name` of this server and every window in it.
    pub(crate) fn kill_named_session(&self, session_name: &str) -> Result<()> {
        ensure_control_atom(session_name, "tmux session name")?;
        self.run(
            &format!("kill-session -t {}", shellquote(session_name)),
            DEFAULT_COMMAND_TIMEOUT,
        )?;
        Ok(())
    }

    pub(crate) fn rename_window(&self, window_id: &str, name: &str) -> Result<()> {
        validate_target_id(window_id, '@', "window")?;
        validate_title(name, "tmux window")?;
        self.run(
            &format!("rename-window -t {window_id} {}", shellquote(name)),
            DEFAULT_COMMAND_TIMEOUT,
        )?;
        Ok(())
    }

    pub(crate) fn list_panes(&self) -> Result<Vec<(String, String, u32, String)>> {
        let lines = self.run(
            "list-panes -s -F '#{window_id} #{pane_id} #{pane_pid} #{window_name}'",
            DEFAULT_COMMAND_TIMEOUT,
        )?;
        lines
            .into_iter()
            .map(|line| parse_listed_pane(&line))
            .filter_map(|result| match result {
                Ok((_, _, _, name)) if name == ANCHOR_WINDOW_NAME => None,
                result => Some(result),
            })
            .collect()
    }

    pub(crate) fn capture_pane(&self, pane_id: &str) -> Result<Vec<u8>> {
        validate_target_id(pane_id, '%', "pane")?;
        let lines = self.run(
            &format!("capture-pane -p -e -S - -t {pane_id}"),
            DEFAULT_COMMAND_TIMEOUT,
        )?;
        Ok(lines.join("\r\n").into_bytes())
    }

    pub(crate) fn kill_session(&self) -> Result<()> {
        self.run("kill-session", DEFAULT_COMMAND_TIMEOUT)?;
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
        mark_all_sinks_exited(&self.sinks, message);
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

fn read_control_output(
    stdout: impl std::io::Read,
    pending: &Mutex<VecDeque<PendingReply>>,
    sinks: &Mutex<HashMap<String, PaneSink>>,
    alive: &AtomicBool,
    startup_sender: SyncSender<()>,
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
                    if let Some(reply) = pending.lock().pop_front() {
                        let _ = reply.sender.send(Err(anyhow!(if message.is_empty() {
                            "tmux command failed".to_owned()
                        } else {
                            message
                        })));
                    }
                } else if let Some(startup_sender) = startup_sender.take() {
                    let _ = startup_sender.send(());
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
    mark_all_sinks_exited(sinks, "tmux control client exited");
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

fn parse_listed_pane(line: &str) -> Result<(String, String, u32, String)> {
    let mut fields = line.splitn(4, ' ');
    let window_id = fields.next().context("tmux omitted window id")?.to_owned();
    let pane_id = fields.next().context("tmux omitted pane id")?.to_owned();
    let process_id = fields
        .next()
        .context("tmux omitted pane pid")?
        .parse()
        .context("tmux returned an invalid pane pid")?;
    let name = fields
        .next()
        .context("tmux omitted window name")?
        .to_owned();
    validate_target_id(&window_id, '@', "window")?;
    validate_target_id(&pane_id, '%', "pane")?;
    validate_title(&name, "tmux window")?;
    Ok((window_id, pane_id, process_id, name))
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
        );
        startup_receiver.try_recv().unwrap();
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

    #[test]
    fn output_splitting_a_utf8_character_neither_ends_the_client_nor_loses_bytes() {
        let alive = AtomicBool::new(true);
        let (sender, reply) = sync_channel(1);
        let pending = Mutex::new(VecDeque::from([PendingReply { id: 1, sender }]));
        let (startup_sender, _startup) = sync_channel(1);
        let terminal = Arc::new(Mutex::new(TerminalModel::new(80, 24)));
        let exited = Arc::new(Mutex::new(None));
        let sink = PaneSink {
            terminal: Arc::clone(&terminal),
            revision: Arc::default(),
            content_revision: Arc::default(),
            events: Arc::default(),
            paste_events: Arc::default(),
            images: Arc::new(TerminalImageStore::in_directory(std::env::temp_dir())),
            exited: Arc::clone(&exited),
            bell_count: 0,
            window_id: "@1".to_owned(),
        };
        let sinks = Mutex::new(HashMap::from([("%1".to_owned(), sink)]));
        // "é" is 0xC3 0xA9; tmux emitted its two bytes in separate notifications.
        let mut stream = b"%begin 1 1 0\n%end 1 1 0\n".to_vec();
        stream.extend_from_slice(b"%output %1 caf\xc3\n%output %1 \xa9!\n");
        stream.extend_from_slice(b"%begin 2 2 1\nstill reading\n%end 2 2 1\n");
        read_control_output(stream.as_slice(), &pending, &sinks, &alive, startup_sender);

        assert_eq!(reply.try_recv().unwrap().unwrap(), ["still reading"]);
        let screen = terminal
            .lock()
            .styled_lines()
            .iter()
            .flat_map(|line| line.runs.iter().map(|run| run.text.clone()))
            .collect::<String>();
        assert!(screen.contains("café!"), "{screen}");
        // EOF ends the client only after every line was processed.
        assert_eq!(exited.lock().as_deref(), Some("tmux control client exited"));
    }

    #[test]
    fn quotes_single_quotes_for_tmux_commands() {
        assert_eq!(shellquote("don't"), "'don'\\''t'");
    }

    #[test]
    fn parses_listed_panes_without_losing_spaces_in_names() {
        assert_eq!(
            parse_listed_pane("@12 %34 567 Agent shell").unwrap(),
            (
                "@12".to_owned(),
                "%34".to_owned(),
                567,
                "Agent shell".to_owned()
            )
        );
    }
}
