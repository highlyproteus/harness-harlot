//! PTY session ownership: spawn, IO, resize, and exit tracking.
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::paste_events::PasteEvents;
#[cfg(any(test, debug_assertions))]
use crate::process::local_spawn_dir;
use crate::process::{
    agent_env, apply_agent_env, configured_shell, local_shell_command, system_ssh_command,
};
use crate::registry::PANE_NOT_REATTACHED_PREFIX;
use crate::terminal_images::TerminalImageStore;
use crate::tmux::{tmux_local_attach_command, tmux_ssh_attach_command};
use crate::tmux_control::{PaneSink, TmuxControlClient};
use crate::tmux_remote::RemoteTmux;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    DeliveryDisposition, MAX_TERMINAL_CELLS, MAX_TERMINAL_COLUMNS, MAX_TERMINAL_ROWS,
    MIN_TERMINAL_COLUMNS, MIN_TERMINAL_ROWS, NotificationKind, TerminalModes, TerminalModifiers,
    TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalScreen, TerminalSelectionKind,
    TmuxSessionId,
};
use hh_terminal_model::TerminalModel;
use parking_lot::{Mutex, RwLock};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use uuid::Uuid;

mod input_modes;

pub(crate) const INITIAL_COLUMNS: u16 = 100;

pub(crate) const INITIAL_ROWS: u16 = 30;

pub(crate) const MAX_INPUT_FRAME: usize = 64 * 1024;

const PTY_INPUT_COMPLETION_BOUND: Duration = Duration::from_secs(5);

/// Paste-event replies can carry a multi-megabyte image the application
/// reads at its own pace.
const PTY_REPLY_COMPLETION_BOUND: Duration = Duration::from_mins(1);

/// Replies up to this size go through `send-keys`; larger ones through a
/// tmux paste buffer.
const TMUX_SEND_KEYS_REPLY_LIMIT: usize = 1024;

#[derive(Debug)]
pub(crate) struct InputDeliveryError {
    message: String,
    disposition: DeliveryDisposition,
}

impl InputDeliveryError {
    pub(crate) fn definitely_unsent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            disposition: DeliveryDisposition::DefinitelyUnsent,
        }
    }

    fn indeterminate(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            disposition: DeliveryDisposition::Indeterminate,
        }
    }

    pub(crate) const fn disposition(&self) -> DeliveryDisposition {
        self.disposition
    }
}

impl std::fmt::Display for InputDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InputDeliveryError {}

pub(crate) const MAX_RAW_PANE_EVENTS: usize = 32;

pub(crate) const TMUX_ATTACH_STARTUP_GRACE: Duration = Duration::from_millis(75);

#[cfg(debug_assertions)]
pub(crate) const LOCAL_SSH_TEST_SEAM_ENV: &str = "HH_TEST_LOCAL_SSH_SEAM";

#[cfg(test)]
pub(crate) static TEST_LOCAL_SSH_SEAM_ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub(crate) struct RawPaneEvent {
    pub(crate) kind: NotificationKind,
    pub(crate) message: Option<String>,
    pub(crate) at_ms: u64,
}

const INPUT_QUEUED: u8 = 0;
const INPUT_WRITING: u8 = 1;
const INPUT_COMPLETED: u8 = 2;
const INPUT_CANCELLED: u8 = 3;

#[derive(Clone)]
struct PtyInput {
    inner: Arc<PtyInputInner>,
}

struct PtyInputInner {
    bytes: Vec<u8>,
    state: AtomicU8,
    completion: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
}

impl PtyInput {
    fn new(
        bytes: Vec<u8>,
    ) -> (
        Self,
        std::sync::mpsc::Receiver<std::result::Result<(), String>>,
    ) {
        let (completion, result) = std::sync::mpsc::sync_channel(1);
        (
            Self {
                inner: Arc::new(PtyInputInner {
                    bytes,
                    state: AtomicU8::new(INPUT_QUEUED),
                    completion,
                }),
            },
            result,
        )
    }

    fn begin_write(&self) -> bool {
        self.inner
            .state
            .compare_exchange(
                INPUT_QUEUED,
                INPUT_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn cancel_if_queued(&self) -> bool {
        if self
            .inner
            .state
            .compare_exchange(
                INPUT_QUEUED,
                INPUT_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        let _ = self
            .inner
            .completion
            .send(Err("terminal input cancelled before write".to_owned()));
        true
    }

    fn finish(&self, result: std::result::Result<(), String>) {
        self.inner.state.store(INPUT_COMPLETED, Ordering::Release);
        let _ = self.inner.completion.send(result);
    }

    fn delivery_is_ambiguous(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == INPUT_WRITING
    }
}

fn run_input_writer(
    mut writer: impl Write,
    input_rx: &std::sync::mpsc::Receiver<PtyInput>,
    pane_id: Uuid,
) {
    while let Ok(input) = input_rx.recv() {
        if !input.begin_write() {
            continue;
        }
        if let Err(error) = writer
            .write_all(&input.inner.bytes)
            .and_then(|()| writer.flush())
        {
            let message = format!("write terminal input: {error}");
            input.finish(Err(message));
            eprintln!("pty writer for pane {pane_id} stopped: {error}");
            break;
        }
        input.finish(Ok(()));
    }
}

fn await_input_completion(
    input: &PtyInput,
    result: &std::sync::mpsc::Receiver<std::result::Result<(), String>>,
    timeout: Duration,
) -> std::result::Result<(), InputDeliveryError> {
    match result.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(InputDeliveryError::indeterminate(error)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            if input.cancel_if_queued() {
                return Err(InputDeliveryError::definitely_unsent(format!(
                    "terminal input timed out and was cancelled before write after {timeout:?}"
                )));
            }
            if input.delivery_is_ambiguous() {
                return Err(InputDeliveryError::indeterminate(format!(
                    "terminal input delivery is ambiguous after {timeout:?}: the writer began before timeout; do not retry automatically"
                )));
            }
            match result.try_recv() {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(InputDeliveryError::indeterminate(error)),
                Err(_) => Err(InputDeliveryError::indeterminate(
                    "terminal input writer stopped without a recoverable delivery outcome",
                )),
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(InputDeliveryError::indeterminate(
                "terminal input writer stopped before acknowledging completion",
            ))
        }
    }
}

pub(crate) struct PtySession {
    pane_id: Uuid,
    transport: Transport,
    terminal: Arc<Mutex<TerminalModel>>,
    revision: Arc<AtomicU64>,
    content_revision: Arc<AtomicU64>,
    events: Arc<Mutex<VecDeque<RawPaneEvent>>>,
    paste_events: Arc<PasteEvents>,
    images: Arc<TerminalImageStore>,
    /// Input modes last stored in the tmux pane (see `input_modes`).
    saved_input_modes: Mutex<String>,
}

enum Transport {
    Pty {
        master: Mutex<Box<dyn MasterPty + Send>>,
        input_tx: Mutex<Option<std::sync::mpsc::SyncSender<PtyInput>>>,
        writer: Mutex<Option<thread::JoinHandle<()>>>,
        writer_exit: Mutex<std::sync::mpsc::Receiver<()>>,
        child: Mutex<Box<dyn Child + Send + Sync>>,
        reader: Mutex<Option<thread::JoinHandle<()>>>,
        reader_exit: Mutex<std::sync::mpsc::Receiver<()>>,
    },
    Tmux {
        /// Replaced in place when a lost local connection is re-established.
        client: RwLock<Arc<TmuxControlClient>>,
        window_id: String,
        tmux_pane_id: String,
        pane_pid: u32,
        exited: Arc<Mutex<Option<String>>>,
    },
    /// A saved tmux window that could not be reattached. The window and its
    /// program keep running and its ids stay saved, so the next restart or an
    /// explicit Reattach tries again; nothing ever covers it with a new shell.
    Unattached {
        window_id: String,
        tmux_pane_id: String,
        reason: String,
    },
}

/// Bound for joining PTY worker threads at teardown. A grandchild that kept
/// the slave side open (for example `sleep 300 &` left in a shell) blocks the
/// reader past any patience-bound join, so the thread is detached instead of
/// wedging the caller forever.
const PTY_THREAD_JOIN_BOUND: Duration = Duration::from_secs(2);
const PTY_CHILD_WAIT_BOUND: Duration = Duration::from_secs(2);

fn terminate_child_bounded(child: &mut (dyn Child + Send + Sync)) -> Result<()> {
    if child
        .try_wait()
        .context("observe PTY child before close")?
        .is_some()
    {
        return Ok(());
    }
    child.kill().context("terminate PTY child")?;
    let deadline = Instant::now() + PTY_CHILD_WAIT_BOUND;
    loop {
        if child
            .try_wait()
            .context("observe PTY child exit after close")?
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("PTY child did not exit after termination");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for a PTY worker thread to signal exit (its exit-channel sender
/// drops when the thread returns) and joins it. On timeout the handle is
/// dropped, detaching the thread; it exits once the orphaned child finally
/// closes the terminal.
fn join_thread_bounded(
    handle: &Mutex<Option<thread::JoinHandle<()>>>,
    exit: &Mutex<std::sync::mpsc::Receiver<()>>,
    label: &str,
) {
    match exit.lock().recv_timeout(PTY_THREAD_JOIN_BOUND) {
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            if let Some(handle) = handle.lock().take() {
                let _ = handle.join();
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            if let Some(handle) = handle.lock().take() {
                drop(handle);
                eprintln!("{label} detached: a child process still holds the terminal");
            }
        }
    }
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtySession")
            .field("revision", &self.revision.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        match &mut self.transport {
            Transport::Pty { child, .. } => {
                if let Err(error) = terminate_child_bounded(child.get_mut().as_mut()) {
                    eprintln!(
                        "failed to terminate PTY child for pane {}: {error:#}",
                        self.pane_id
                    );
                }
                self.shutdown_threads_bounded();
            }
            Transport::Tmux {
                client,
                tmux_pane_id,
                ..
            } => client.get_mut().unregister_sink(tmux_pane_id),
            Transport::Unattached { .. } => {}
        }
        self.images.remove_all();
    }
}

pub(crate) fn validate_terminal_dimensions(columns: u16, rows: u16) -> Result<()> {
    if !(MIN_TERMINAL_COLUMNS..=MAX_TERMINAL_COLUMNS).contains(&columns) {
        bail!("terminal columns must be between {MIN_TERMINAL_COLUMNS} and {MAX_TERMINAL_COLUMNS}");
    }
    if !(MIN_TERMINAL_ROWS..=MAX_TERMINAL_ROWS).contains(&rows) {
        bail!("terminal rows must be between {MIN_TERMINAL_ROWS} and {MAX_TERMINAL_ROWS}");
    }
    let cells = u32::from(columns) * u32::from(rows);
    if cells > MAX_TERMINAL_CELLS {
        bail!("terminal dimensions exceed the {MAX_TERMINAL_CELLS}-cell limit");
    }
    Ok(())
}

impl PtySession {
    pub(crate) fn spawn_local(
        pane_id: Uuid,
        workspace_id: Uuid,
        bot_id: Option<Uuid>,
        cwd: &Path,
    ) -> Result<Arc<Self>> {
        let shell = configured_shell();
        let mut command = local_shell_command(pane_id, cwd);
        apply_agent_env(&mut command, workspace_id, bot_id);
        Self::spawn_command(pane_id, command, &format!("configured shell {shell}"))
    }

    // The workspace only feeds the local SSH test seam's agent environment.
    #[cfg_attr(not(any(test, debug_assertions)), allow(unused_variables))]
    pub(crate) fn spawn_ssh(
        pane_id: Uuid,
        workspace_id: Uuid,
        host: &str,
        remote_dir: Option<&str>,
    ) -> Result<Arc<Self>> {
        #[cfg(test)]
        if TEST_LOCAL_SSH_SEAM_ENABLED.load(Ordering::Relaxed) {
            return Self::spawn_local(pane_id, workspace_id, None, &local_spawn_dir(remote_dir)?);
        }
        #[cfg(debug_assertions)]
        if std::env::var_os(LOCAL_SSH_TEST_SEAM_ENV).is_some() {
            return Self::spawn_local(pane_id, workspace_id, None, &local_spawn_dir(remote_dir)?);
        }
        Self::spawn_command(
            pane_id,
            system_ssh_command(pane_id, host, remote_dir)?,
            "system OpenSSH",
        )
    }

    pub(crate) fn spawn_tmux_local(pane_id: Uuid, session_id: &TmuxSessionId) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            tmux_local_attach_command(pane_id, session_id)?,
            "tmux session attach",
        )
    }

    pub(crate) fn spawn_tmux_ssh(
        pane_id: Uuid,
        host: &str,
        session_id: &TmuxSessionId,
    ) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            tmux_ssh_attach_command(pane_id, host, session_id)?,
            "system OpenSSH tmux session attach",
        )
    }
    /// A new window of HH's local tmux server for `pane_id`.
    pub(crate) fn spawn_tmux(
        pane_id: Uuid,
        workspace_id: Uuid,
        bot_id: Option<Uuid>,
        cwd: &Path,
        client: &Arc<TmuxControlClient>,
    ) -> Result<Arc<Self>> {
        let cwd = cwd
            .to_str()
            .context("tmux window working directory is not UTF-8")?;
        let pane_id_text = pane_id.to_string();
        let agent_env = agent_env(workspace_id, bot_id);
        let mut window_env = vec![
            (hh_protocol::pane_id_env(), pane_id_text.as_str()),
            ("COLORTERM", "truecolor"),
        ];
        window_env.extend(agent_env.iter().map(|(key, value)| (*key, value.as_str())));
        Self::spawn_window(pane_id, Some(cwd), &window_env, client)
    }

    /// A new window of HH's tmux server on an SSH host, started in
    /// `remote_dir` (the login directory when `None`). Local paths such as
    /// the service socket mean nothing there, so only terminal settings are
    /// passed.
    pub(crate) fn spawn_remote_tmux(
        pane_id: Uuid,
        remote_dir: Option<&str>,
        client: &Arc<TmuxControlClient>,
    ) -> Result<Arc<Self>> {
        let pane_id_text = pane_id.to_string();
        let window_env = [
            (hh_protocol::pane_id_env(), pane_id_text.as_str()),
            ("COLORTERM", "truecolor"),
            ("PI_FORCE_IMAGE_PROTOCOL", "kitty"),
            ("PI_KITTY_PLACEHOLDERS", "1"),
        ];
        Self::spawn_window(pane_id, remote_dir, &window_env, client)
    }

    fn spawn_window(
        pane_id: Uuid,
        cwd: Option<&str>,
        window_env: &[(&str, &str)],
        client: &Arc<TmuxControlClient>,
    ) -> Result<Arc<Self>> {
        let (window_id, tmux_pane_id, shell_pid) =
            client.new_window("shell", cwd, window_env, pane_id)?;
        let session = Self::new_tmux_transport(
            pane_id,
            Arc::clone(client),
            window_id.clone(),
            tmux_pane_id.clone(),
            shell_pid,
            None,
        );
        if let Err(error) = &session {
            let _ = client.kill_window(&window_id);
            client.unregister_sink(&tmux_pane_id);
            return Err(anyhow::anyhow!("{error:#}"));
        }
        if let Err(error) = client.resize_window(&window_id, INITIAL_COLUMNS, INITIAL_ROWS) {
            client.unregister_sink(&tmux_pane_id);
            let _ = client.kill_window(&window_id);
            return Err(error).context("set initial tmux window size");
        }
        session
    }

    /// The interactive terminal a remote sign-in runs in (see `tmux_remote`).
    pub(crate) fn spawn_sign_in(pane_id: Uuid, remote: &RemoteTmux) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            remote.sign_in_command(pane_id)?,
            "system OpenSSH sign-in",
        )
    }

    /// Reattaches to an existing window, rebuilding the terminal from its
    /// whole scrollback. If only reading the scrollback fails while the
    /// connection is still up, the window is attached with a blank screen:
    /// its program keeps running and redraws on the next output or resize.
    pub(crate) fn attach_tmux(
        pane_id: Uuid,
        client: Arc<TmuxControlClient>,
        window_id: String,
        tmux_pane_id: String,
        shell_pid: u32,
    ) -> Result<Arc<Self>> {
        let captured = match client.capture_pane(&tmux_pane_id) {
            Ok(captured) => Some(captured),
            Err(error) if client.is_alive() => {
                eprintln!("attaching pane {pane_id} without its scrollback: {error:#}");
                None
            }
            Err(error) => return Err(error).context("read the window's scrollback"),
        };
        let session = Self::new_tmux_transport(
            pane_id,
            client,
            window_id,
            tmux_pane_id,
            shell_pid,
            captured,
        )?;
        session.restore_saved_input_modes();
        Ok(session)
    }

    /// A pane whose saved window exists but could not be attached; see
    /// `Transport::Unattached`.
    pub(crate) fn unattached_tmux(
        pane_id: Uuid,
        window_id: String,
        tmux_pane_id: String,
        reason: String,
    ) -> Arc<Self> {
        let terminal = Arc::new(Mutex::new(TerminalModel::new(
            usize::from(INITIAL_COLUMNS),
            usize::from(INITIAL_ROWS),
        )));
        {
            let mut model = terminal.lock();
            model.process_output(
                format!(
                    "\r\nHarness Harlot could not reattach this terminal: {reason}\r\n\
                     Its program is still running in tmux window {window_id}.\r\n\
                     Use Reattach Exited Terminal to try again.\r\n"
                )
                .as_bytes(),
            );
        }
        Arc::new(Self {
            pane_id,
            transport: Transport::Unattached {
                window_id,
                tmux_pane_id,
                reason,
            },
            terminal,
            revision: Arc::new(AtomicU64::new(1)),
            content_revision: Arc::new(AtomicU64::new(1)),
            events: Arc::new(Mutex::new(VecDeque::new())),
            paste_events: Arc::new(PasteEvents::default()),
            images: Arc::new(TerminalImageStore::for_pane(pane_id)),
            saved_input_modes: Mutex::new(String::new()),
        })
    }

    fn new_tmux_transport(
        pane_id: Uuid,
        client: Arc<TmuxControlClient>,
        window_id: String,
        tmux_pane_id: String,
        shell_pid: u32,
        captured: Option<Vec<u8>>,
    ) -> Result<Arc<Self>> {
        let terminal = Arc::new(Mutex::new(TerminalModel::new(
            usize::from(INITIAL_COLUMNS),
            usize::from(INITIAL_ROWS),
        )));
        let revision = Arc::new(AtomicU64::new(0));
        let content_revision = Arc::new(AtomicU64::new(0));
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let exited = Arc::new(Mutex::new(None));
        let paste_events = Arc::new(PasteEvents::default());
        let images = Arc::new(TerminalImageStore::for_pane(pane_id));
        let mut bell_count = 0;
        if let Some(captured) = captured {
            ingest_output(
                &terminal,
                &events,
                &paste_events,
                &images,
                &revision,
                &content_revision,
                &mut bell_count,
                &captured,
            );
        }
        client.register_sink(
            &tmux_pane_id,
            PaneSink {
                terminal: Arc::clone(&terminal),
                revision: Arc::clone(&revision),
                content_revision: Arc::clone(&content_revision),
                events: Arc::clone(&events),
                paste_events: Arc::clone(&paste_events),
                images: Arc::clone(&images),
                exited: Arc::clone(&exited),
                bell_count,
                window_id: window_id.clone(),
            },
        )?;
        let session = Arc::new(Self {
            pane_id,
            transport: Transport::Tmux {
                client: RwLock::new(client),
                window_id,
                tmux_pane_id,
                pane_pid: shell_pid,
                exited,
            },
            terminal,
            revision,
            content_revision,
            events,
            paste_events,
            images,
            saved_input_modes: Mutex::new(String::new()),
        });
        session.paste_events.bind(&session);
        Ok(session)
    }

    pub(crate) fn spawn_command(
        pane_id: Uuid,
        command: CommandBuilder,
        description: &str,
    ) -> Result<Arc<Self>> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: INITIAL_ROWS,
                cols: INITIAL_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open native PTY")?;

        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawn {description}"))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("take PTY writer")?;
        let terminal = Arc::new(Mutex::new(TerminalModel::new(
            usize::from(INITIAL_COLUMNS),
            usize::from(INITIAL_ROWS),
        )));
        let revision = Arc::new(AtomicU64::new(0));
        let content_revision = Arc::new(AtomicU64::new(0));
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let reader_terminal = Arc::clone(&terminal);
        let reader_revision = Arc::clone(&revision);
        let reader_content_revision = Arc::clone(&content_revision);
        let reader_events = Arc::clone(&events);
        let paste_events = Arc::new(PasteEvents::default());
        let reader_paste_events = Arc::clone(&paste_events);
        let images = Arc::new(TerminalImageStore::for_pane(pane_id));
        let reader_images = Arc::clone(&images);
        let (reader_exit_tx, reader_exit) = std::sync::mpsc::channel::<()>();
        let reader = thread::Builder::new()
            .name(format!("rmux-pty-{pane_id}"))
            .spawn(move || {
                // Dropping this sender when the thread returns is the exit
                // signal for the bounded join in `shutdown_threads_bounded`.
                let _reader_exit = reader_exit_tx;
                let mut buffer = [0_u8; 16 * 1024];
                let mut previous_bell_count = 0;
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => ingest_output(
                            &reader_terminal,
                            &reader_events,
                            &reader_paste_events,
                            &reader_images,
                            &reader_revision,
                            &reader_content_revision,
                            &mut previous_bell_count,
                            &buffer[..read],
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
            .context("spawn PTY reader thread")?;

        // Input flows through a dedicated writer thread so a stopped child
        // with a full PTY buffer can never wedge a request handler: the
        // bounded channel below turns a stuck write into a timeout error.
        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<PtyInput>(64);
        let (writer_exit_tx, writer_exit) = std::sync::mpsc::channel::<()>();
        let writer_thread = thread::Builder::new()
            .name(format!("rmux-pty-writer-{pane_id}"))
            .spawn(move || {
                let _writer_exit = writer_exit_tx;
                run_input_writer(writer, &input_rx, pane_id);
            })
            .context("spawn PTY writer thread")?;

        let session = Arc::new(Self {
            pane_id,
            transport: Transport::Pty {
                master: Mutex::new(pair.master),
                input_tx: Mutex::new(Some(input_tx)),
                writer: Mutex::new(Some(writer_thread)),
                writer_exit: Mutex::new(writer_exit),
                child: Mutex::new(child),
                reader: Mutex::new(Some(reader)),
                reader_exit: Mutex::new(reader_exit),
            },
            terminal,
            revision,
            content_revision,
            events,
            paste_events,
            images,
            saved_input_modes: Mutex::new(String::new()),
        });
        session.paste_events.bind(&session);
        Ok(session)
    }

    pub(crate) fn write_input(&self, bytes: &[u8]) -> std::result::Result<(), InputDeliveryError> {
        if bytes.len() > MAX_INPUT_FRAME {
            return Err(InputDeliveryError::definitely_unsent(format!(
                "terminal input exceeds {MAX_INPUT_FRAME}-byte frame limit"
            )));
        }
        {
            let mut terminal = self.terminal.lock();
            if terminal.display_offset() != 0 {
                terminal.scroll_bottom();
                self.content_revision.fetch_add(1, Ordering::Release);
                self.revision.fetch_add(1, Ordering::Release);
            }
        }
        match &self.transport {
            Transport::Tmux {
                client,
                tmux_pane_id,
                exited,
                ..
            } => {
                if exited.lock().is_some() {
                    return Err(InputDeliveryError::definitely_unsent(
                        "terminal process has exited",
                    ));
                }
                let client = Arc::clone(&client.read());
                client.send_keys_hex(tmux_pane_id, bytes).map_err(|error| {
                    let message = format!("write terminal input through tmux: {error:#}");
                    if error.to_string().starts_with("tmux did not answer") {
                        InputDeliveryError::indeterminate(message)
                    } else if !client.is_alive() {
                        InputDeliveryError::definitely_unsent(message)
                    } else {
                        InputDeliveryError::indeterminate(message)
                    }
                })
            }
            Transport::Unattached { .. } => Err(InputDeliveryError::definitely_unsent(
                "terminal is not attached",
            )),
            Transport::Pty { .. } => self.write_pty(bytes.to_vec(), PTY_INPUT_COMPLETION_BOUND),
        }
    }

    /// Writes a terminal reply (DECRQM report or OSC 5522 packets) to the
    /// pane's input as one uninterrupted unit. Unlike typed input it has no
    /// frame limit and leaves the viewport where it is.
    pub(crate) fn write_reply(
        &self,
        bytes: Vec<u8>,
    ) -> std::result::Result<(), InputDeliveryError> {
        match &self.transport {
            Transport::Tmux {
                client,
                tmux_pane_id,
                exited,
                ..
            } => {
                if exited.lock().is_some() {
                    return Err(InputDeliveryError::definitely_unsent(
                        "terminal process has exited",
                    ));
                }
                let client = Arc::clone(&client.read());
                let result = if bytes.len() <= TMUX_SEND_KEYS_REPLY_LIMIT {
                    client.send_keys_hex(tmux_pane_id, &bytes)
                } else {
                    client.paste_bytes(tmux_pane_id, &bytes)
                };
                result.map_err(|error| {
                    InputDeliveryError::indeterminate(format!(
                        "write terminal reply through tmux: {error:#}"
                    ))
                })
            }
            Transport::Unattached { .. } => Err(InputDeliveryError::definitely_unsent(
                "terminal is not attached",
            )),
            Transport::Pty { .. } => self.write_pty(bytes, PTY_REPLY_COMPLETION_BOUND),
        }
    }

    fn write_pty(
        &self,
        bytes: Vec<u8>,
        bound: Duration,
    ) -> std::result::Result<(), InputDeliveryError> {
        let Transport::Pty {
            child, input_tx, ..
        } = &self.transport
        else {
            return Err(InputDeliveryError::definitely_unsent(
                "terminal is not a PTY",
            ));
        };
        match child.lock().try_wait() {
            Ok(Some(_)) => {
                return Err(InputDeliveryError::definitely_unsent(
                    "terminal process has exited",
                ));
            }
            Ok(None) => {}
            Err(error) => {
                return Err(InputDeliveryError::indeterminate(format!(
                    "observe terminal process before input delivery: {error}"
                )));
            }
        }
        let Some(input_tx) = input_tx.lock().as_ref().cloned() else {
            return Err(InputDeliveryError::definitely_unsent(
                "terminal is not accepting input",
            ));
        };
        let deadline = Instant::now() + bound;
        let (input, result) = PtyInput::new(bytes);
        let mut queued = input.clone();
        loop {
            match input_tx.try_send(queued) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Full(input)) => queued = input,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(InputDeliveryError::definitely_unsent(
                        "terminal is not accepting input",
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(InputDeliveryError::definitely_unsent(
                    "terminal is not accepting input",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        await_input_completion(&input, &result, remaining)
    }

    pub(crate) fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        validate_terminal_dimensions(columns, rows)?;
        match &self.transport {
            Transport::Pty { master, .. } => master
                .lock()
                .resize(PtySize {
                    rows,
                    cols: columns,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize PTY")?,
            Transport::Tmux {
                client,
                window_id,
                exited,
                ..
            } => {
                // An exited or disconnected window keeps its last screen.
                if exited.lock().is_none() {
                    client.read().resize_window(window_id, columns, rows)?;
                }
            }
            Transport::Unattached { .. } => {}
        }
        let mut terminal = self.terminal.lock();
        terminal.resize(usize::from(columns), usize::from(rows));
        self.content_revision.fetch_add(1, Ordering::Release);
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    pub(crate) fn screen(&self, pane_id: Uuid) -> Result<TerminalScreen> {
        let terminal = self.terminal.lock();
        let (columns, rows) = terminal.dimensions();
        let mut mode_bits = 0;
        for (enabled, mode) in [
            (terminal.bracketed_paste(), TerminalModes::BRACKETED_PASTE),
            (terminal.mouse_reporting(), TerminalModes::MOUSE_REPORTING),
            (terminal.mouse_motion(), TerminalModes::MOUSE_MOTION),
            (terminal.sgr_mouse(), TerminalModes::SGR_MOUSE),
        ] {
            if enabled {
                mode_bits |= mode;
            }
        }
        Ok(TerminalScreen {
            pane_id,
            revision: self.revision.load(Ordering::Acquire),
            content_revision: self.content_revision.load(Ordering::Acquire),
            columns: u16::try_from(columns).context("terminal columns exceed protocol range")?,
            rows: u16::try_from(rows).context("terminal rows exceed protocol range")?,
            lines: terminal.styled_lines(),
            cursor: terminal.cursor(),
            selection: terminal.selection(),
            display_offset: u32::try_from(terminal.display_offset())
                .context("terminal display offset exceeds protocol range")?,
            history_size: u32::try_from(terminal.history_size())
                .context("terminal history exceeds protocol range")?,
            modes: TerminalModes::new(mode_bits),
            images: self.images.screen_images(terminal.placed_images()),
        })
    }

    pub(crate) fn begin_selection(&self, point: TerminalPoint, kind: TerminalSelectionKind) {
        let mut terminal = self.terminal.lock();
        terminal.begin_selection(point, kind);
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn update_selection(&self, point: TerminalPoint) {
        let mut terminal = self.terminal.lock();
        terminal.update_selection(point);
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn clear_selection(&self) {
        let mut terminal = self.terminal.lock();
        terminal.clear_selection();
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        self.terminal.lock().selected_text()
    }

    pub(crate) fn scroll(&self, lines: i32) {
        let mut terminal = self.terminal.lock();
        let previous_offset = terminal.display_offset();
        terminal.scroll(lines.clamp(-10_000, 10_000));
        if terminal.display_offset() != previous_offset {
            self.content_revision.fetch_add(1, Ordering::Release);
            self.revision.fetch_add(1, Ordering::Release);
        }
    }

    pub(crate) fn search_literal(&self, query: &str, forward: bool) -> Result<bool> {
        if query.chars().count() > 256 || query.chars().any(char::is_control) {
            bail!("terminal search must be at most 256 visible characters");
        }
        let mut terminal = self.terminal.lock();
        let found = terminal.search_literal(query, forward);
        if found {
            self.content_revision.fetch_add(1, Ordering::Release);
            self.revision.fetch_add(1, Ordering::Release);
        }
        Ok(found)
    }

    pub(crate) fn mouse_input(
        &self,
        point: TerminalPoint,
        button: TerminalMouseButton,
        action: TerminalMouseAction,
        modifiers: TerminalModifiers,
    ) -> Result<()> {
        let report = self
            .terminal
            .lock()
            .mouse_report(point, button, action, modifiers);
        if let Some(report) = report {
            self.write_input(&report)?;
        }
        Ok(())
    }

    /// Ends the pane for good: the user closed it. A tmux window (local or
    /// remote) is killed with its program. See `detach` for leaving it running.
    pub(crate) fn terminate_and_wait(&self) -> Result<()> {
        match &self.transport {
            Transport::Pty { child, .. } => {
                let result = terminate_child_bounded(child.lock().as_mut());
                self.shutdown_threads_bounded();
                result
            }
            Transport::Tmux {
                client,
                window_id,
                tmux_pane_id,
                exited,
                ..
            } => {
                let client = Arc::clone(&client.read());
                client.unregister_sink(tmux_pane_id);
                if exited.lock().is_some() {
                    return Ok(());
                }
                client.kill_window(window_id)?;
                *exited.lock() = Some("exited".to_owned());
                Ok(())
            }
            // A later restart's cleanup removes the window once its tab is gone.
            Transport::Unattached { .. } => Ok(()),
        }
    }

    /// Stops showing the pane without ending its program: a tmux window
    /// keeps running (a disconnect, or a reconnect that failed part-way).
    /// A plain PTY cannot outlive the service, so it is terminated.
    pub(crate) fn detach(&self, reason: &str) -> Result<()> {
        match &self.transport {
            Transport::Tmux {
                client,
                tmux_pane_id,
                exited,
                ..
            } => {
                client.read().unregister_sink(tmux_pane_id);
                exited.lock().get_or_insert_with(|| reason.to_owned());
                Ok(())
            }
            Transport::Unattached { .. } => Ok(()),
            Transport::Pty { .. } => self.terminate_and_wait(),
        }
    }

    /// Stops and joins the PTY worker threads with a patience bound. Dropping
    /// the input sender lets the writer drain queued input and exit; the
    /// reader exits once the terminal delivers EOF. A thread blocked past the
    /// bound (an orphan still holds the slave side) is detached instead of
    /// blocking teardown.
    fn shutdown_threads_bounded(&self) {
        let Transport::Pty {
            input_tx,
            writer,
            writer_exit,
            reader,
            reader_exit,
            ..
        } = &self.transport
        else {
            return;
        };
        input_tx.lock().take();
        join_thread_bounded(
            writer,
            writer_exit,
            &format!("pty writer for pane {}", self.pane_id),
        );
        join_thread_bounded(
            reader,
            reader_exit,
            &format!("pty reader for pane {}", self.pane_id),
        );
    }

    pub(crate) fn exit_status(&self) -> Result<Option<String>> {
        match &self.transport {
            Transport::Pty { child, .. } => child
                .lock()
                .try_wait()
                .map(|status| status.map(|status| status.to_string()))
                .context("observe PTY child exit"),
            Transport::Tmux { exited, .. } => Ok(exited.lock().clone()),
            Transport::Unattached { reason, .. } => {
                Ok(Some(format!("{PANE_NOT_REATTACHED_PREFIX} {reason}")))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn terminate_child_for_test(&self) -> Result<()> {
        match &self.transport {
            Transport::Pty { child, .. } => terminate_child_bounded(child.lock().as_mut()),
            Transport::Tmux { .. } | Transport::Unattached { .. } => {
                bail!("test termination is only available for PTY panes")
            }
        }
    }

    /// A successful `spawn` only means the executable started. tmux reports a
    /// missing/dead target by exiting immediately, so do not register a tab
    /// until it survived a short bounded startup window.
    pub(crate) fn confirm_live_for_tmux_attach(&self) -> Result<()> {
        if matches!(
            self.transport,
            Transport::Tmux { .. } | Transport::Unattached { .. }
        ) {
            bail!("HH-managed tmux windows do not use the attach startup check");
        }
        let deadline = Instant::now() + TMUX_ATTACH_STARTUP_GRACE;
        loop {
            if let Some(status) = self.exit_status()? {
                bail!("tmux attach exited before the terminal became live ({status})");
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn process_id(&self) -> Option<u32> {
        match &self.transport {
            Transport::Pty { child, .. } => child.lock().process_id(),
            Transport::Tmux { pane_pid, .. } => Some(*pane_pid),
            Transport::Unattached { .. } => None,
        }
    }

    /// The tmux window and pane this terminal belongs to, kept for an
    /// unattached pane so its window is found again later.
    pub(crate) fn tmux_ids(&self) -> Option<(&str, &str)> {
        match &self.transport {
            Transport::Tmux {
                window_id,
                tmux_pane_id,
                ..
            }
            | Transport::Unattached {
                window_id,
                tmux_pane_id,
                ..
            } => Some((window_id, tmux_pane_id)),
            Transport::Pty { .. } => None,
        }
    }

    /// The control connection a live tmux pane talks through.
    pub(crate) fn tmux_client(&self) -> Option<Arc<TmuxControlClient>> {
        match &self.transport {
            Transport::Tmux { client, .. } => Some(Arc::clone(&client.read())),
            Transport::Pty { .. } | Transport::Unattached { .. } => None,
        }
    }

    /// Points this pane at a re-established connection whose sinks already
    /// hold this pane (the old and new clients share one sink map).
    pub(crate) fn replace_tmux_client(&self, replacement: &Arc<TmuxControlClient>) {
        if let Transport::Tmux { client, .. } = &self.transport {
            *client.write() = Arc::clone(replacement);
        }
    }

    pub(crate) fn rename_tmux_window(&self, title: &str) -> Result<()> {
        if let Transport::Tmux {
            client, window_id, ..
        } = &self.transport
        {
            client.read().rename_window(window_id, title)?;
        }
        Ok(())
    }

    /// Whether the pane's application currently accepts kitty paste events.
    pub(crate) fn enhanced_paste(&self) -> bool {
        self.terminal.lock().enhanced_paste()
    }

    /// Offers a PNG (and optional text) as a paste event. Fails when the
    /// application has not enabled enhanced paste or the pane has exited.
    pub(crate) fn paste_image(&self, png: Vec<u8>, text: Option<String>) -> Result<()> {
        if !self.enhanced_paste() || self.exit_status()?.is_some() {
            bail!("the pane's application has not enabled enhanced paste");
        }
        self.paste_events.offer(self, png, text)
    }

    pub(crate) fn terminal_title(&self) -> Option<String> {
        self.terminal.lock().terminal_title()
    }

    /// Whether the pane's event queue holds at least one raw event.
    ///
    /// Uses `try_lock` so streaming checks never block on a writer.
    pub(crate) fn has_pending_events(&self) -> bool {
        self.events
            .try_lock()
            .is_some_and(|events| !events.is_empty())
    }

    /// Whether the output reader thread has finished draining the PTY.
    pub(crate) fn reader_is_finished(&self) -> bool {
        match &self.transport {
            Transport::Pty { reader, .. } => reader
                .lock()
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished),
            Transport::Tmux { exited, .. } => exited.lock().is_some(),
            Transport::Unattached { .. } => true,
        }
    }

    /// Drains all queued raw events without blocking on a concurrent writer.
    pub(crate) fn try_drain_events(&self) -> Option<Vec<RawPaneEvent>> {
        let mut events = self.events.try_lock()?;
        Some(events.drain(..).collect())
    }

    /// The current terminal revision, loaded with acquire ordering to match
    /// the reader thread's release-store updates.
    pub(crate) fn current_revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}

// Each argument is one of the pane's independently shared handles.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ingest_output(
    terminal: &Mutex<TerminalModel>,
    events: &Mutex<VecDeque<RawPaneEvent>>,
    paste_events: &Arc<PasteEvents>,
    images: &TerminalImageStore,
    revision: &AtomicU64,
    content_revision: &AtomicU64,
    bell_count: &mut u64,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let mut terminal = terminal.lock();
    terminal.process_output(bytes);
    // Written before the revision advances so no screen names a missing file.
    let image_events = terminal.take_image_events();
    if !image_events.is_empty() {
        images.apply(image_events);
    }
    try_enqueue_terminal_notifications(&mut terminal, events, bell_count);
    let requests = terminal.take_terminal_requests();
    content_revision.fetch_add(1, Ordering::Release);
    revision.fetch_add(1, Ordering::Release);
    drop(terminal);
    paste_events.enqueue(requests);
}

fn try_enqueue_terminal_notifications(
    terminal: &mut TerminalModel,
    events: &Mutex<VecDeque<RawPaneEvent>>,
    previous_bell_count: &mut u64,
) {
    let bell_count = terminal.bell_count();
    let Some(mut events) = events.try_lock() else {
        return;
    };
    if bell_count > *previous_bell_count {
        push_raw_pane_event(
            &mut events,
            RawPaneEvent {
                kind: NotificationKind::Attention,
                message: None,
                at_ms: crate::now_ms(),
            },
        );
    }
    for message in terminal.take_notification_messages() {
        push_raw_pane_event(
            &mut events,
            RawPaneEvent {
                kind: NotificationKind::Message,
                message: Some(message),
                at_ms: crate::now_ms(),
            },
        );
    }
    *previous_bell_count = bell_count;
}

pub(crate) fn push_raw_pane_event(events: &mut VecDeque<RawPaneEvent>, event: RawPaneEvent) {
    if events.len() == MAX_RAW_PANE_EVENTS {
        events.pop_front();
    }
    events.push_back(event);
}

#[cfg(test)]
#[path = "pty_tests.rs"]
mod tests;
