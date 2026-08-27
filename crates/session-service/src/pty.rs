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

use crate::history::{HistoryArchive, HistorySink};
#[cfg(any(test, debug_assertions))]
use crate::process::local_spawn_dir;
use crate::process::{configured_shell, local_shell_command, system_ssh_command};
use crate::tmux::{tmux_local_attach_command, tmux_ssh_attach_command};
use anyhow::{Context, Result, bail};
use hh_protocol::{
    MAX_TERMINAL_CELLS, MAX_TERMINAL_COLUMNS, MAX_TERMINAL_ROWS, MIN_TERMINAL_COLUMNS,
    MIN_TERMINAL_ROWS, NotificationKind, TerminalModes, TerminalModifiers, TerminalMouseAction,
    TerminalMouseButton, TerminalPoint, TerminalScreen, TerminalSelectionKind, TmuxSessionId,
};
use hh_terminal_model::TerminalModel;
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use uuid::Uuid;

use crate::history;

pub(crate) const INITIAL_COLUMNS: u16 = 100;

pub(crate) const INITIAL_ROWS: u16 = 30;

pub(crate) const MAX_INPUT_FRAME: usize = 64 * 1024;

const PTY_INPUT_COMPLETION_BOUND: Duration = Duration::from_secs(5);

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
) -> Result<()> {
    match result.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => bail!(error),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            if input.cancel_if_queued() {
                bail!("terminal input timed out and was cancelled before write after {timeout:?}")
            }
            if input.delivery_is_ambiguous() {
                bail!(
                    "terminal input delivery is ambiguous after {timeout:?}: the writer began before timeout; do not retry automatically"
                )
            }
            match result.try_recv() {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => bail!(error),
                Err(_) => {
                    bail!("terminal input writer stopped without a recoverable delivery outcome")
                }
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            bail!("terminal input writer stopped before acknowledging completion")
        }
    }
}

pub(crate) struct PtySession {
    pane_id: Uuid,
    master: Mutex<Box<dyn MasterPty + Send>>,
    input_tx: Mutex<Option<std::sync::mpsc::SyncSender<PtyInput>>>,
    writer: Mutex<Option<thread::JoinHandle<()>>>,
    writer_exit: Mutex<std::sync::mpsc::Receiver<()>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    reader: Mutex<Option<thread::JoinHandle<()>>>,
    reader_exit: Mutex<std::sync::mpsc::Receiver<()>>,
    terminal: Arc<Mutex<TerminalModel>>,
    revision: Arc<AtomicU64>,
    events: Arc<Mutex<VecDeque<RawPaneEvent>>>,
    _history: Arc<HistorySink>,
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
        if let Err(error) = terminate_child_bounded(self.child.get_mut().as_mut()) {
            eprintln!(
                "failed to terminate PTY child for pane {}: {error:#}",
                self.pane_id
            );
        }
        self.shutdown_threads_bounded();
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
        cwd: &Path,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        let shell = configured_shell();
        Self::spawn_command(
            pane_id,
            workspace_id,
            local_shell_command(pane_id, cwd),
            &format!("configured shell {shell}"),
            archive,
        )
    }

    pub(crate) fn spawn_ssh(
        pane_id: Uuid,
        workspace_id: Uuid,
        host: &str,
        remote_dir: Option<&str>,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        #[cfg(test)]
        if TEST_LOCAL_SSH_SEAM_ENABLED.load(Ordering::Relaxed) {
            return Self::spawn_local(
                pane_id,
                workspace_id,
                &local_spawn_dir(remote_dir)?,
                archive,
            );
        }
        #[cfg(debug_assertions)]
        if std::env::var_os(LOCAL_SSH_TEST_SEAM_ENV).is_some() {
            return Self::spawn_local(
                pane_id,
                workspace_id,
                &local_spawn_dir(remote_dir)?,
                archive,
            );
        }
        Self::spawn_command(
            pane_id,
            workspace_id,
            system_ssh_command(pane_id, host, remote_dir)?,
            "system OpenSSH",
            archive,
        )
    }

    pub(crate) fn spawn_tmux_local(
        pane_id: Uuid,
        workspace_id: Uuid,
        session_id: &TmuxSessionId,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            workspace_id,
            tmux_local_attach_command(pane_id, session_id)?,
            "tmux session attach",
            archive,
        )
    }

    pub(crate) fn spawn_tmux_ssh(
        pane_id: Uuid,
        workspace_id: Uuid,
        host: &str,
        session_id: &TmuxSessionId,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        Self::spawn_command(
            pane_id,
            workspace_id,
            tmux_ssh_attach_command(pane_id, host, session_id)?,
            "system OpenSSH tmux session attach",
            archive,
        )
    }

    pub(crate) fn spawn_command(
        pane_id: Uuid,
        workspace_id: Uuid,
        command: CommandBuilder,
        description: &str,
        archive: &HistoryArchive,
    ) -> Result<Arc<Self>> {
        // Session registration may wait behind prior disk work, so do it
        // before a child exists. Once the PTY is live, its reader only uses
        // the archive's bounded non-blocking append path.
        let history = Arc::new(archive.start_session(pane_id, workspace_id));
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
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let reader_terminal = Arc::clone(&terminal);
        let reader_revision = Arc::clone(&revision);
        let reader_events = Arc::clone(&events);
        let reader_history = Arc::clone(&history);
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
                        Ok(read) => {
                            let mut terminal = reader_terminal.lock();
                            terminal.process_output(&buffer[..read]);
                            try_enqueue_terminal_notifications(
                                &mut terminal,
                                &reader_events,
                                &mut previous_bell_count,
                            );
                            drop(terminal);
                            reader_revision.fetch_add(1, Ordering::Release);
                            reader_history.record(&buffer[..read]);
                        }
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

        Ok(Arc::new(Self {
            pane_id,
            master: Mutex::new(pair.master),
            input_tx: Mutex::new(Some(input_tx)),
            writer: Mutex::new(Some(writer_thread)),
            writer_exit: Mutex::new(writer_exit),
            child: Mutex::new(child),
            reader: Mutex::new(Some(reader)),
            reader_exit: Mutex::new(reader_exit),
            terminal,
            revision,
            events,
            _history: history,
        }))
    }

    pub(crate) fn write_input(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_INPUT_FRAME {
            bail!("terminal input exceeds {MAX_INPUT_FRAME}-byte frame limit");
        }
        // Typing snaps the viewport back to the live bottom (stock terminal
        // behavior). While `display_offset` is nonzero, `Grid::scroll_up`
        // anchors streaming output to old content and the typed line recedes
        // below the fold. The revision bump makes the next poll deliver the
        // re-anchored screen.
        {
            let mut terminal = self.terminal.lock();
            if terminal.display_offset() != 0 {
                terminal.scroll_bottom();
                drop(terminal);
                self.revision.fetch_add(1, Ordering::Release);
            }
        }
        let Some(input_tx) = self.input_tx.lock().as_ref().cloned() else {
            bail!("terminal is not accepting input");
        };
        // A single bounded channel preserves keystroke/paste ordering while
        // turning a wedged writer (stopped child, full PTY buffer) into an
        // error instead of a frozen handler thread. Clone the sender so the
        // lifecycle lock itself never sits inside the completion bound.
        let deadline = Instant::now() + PTY_INPUT_COMPLETION_BOUND;
        let (input, result) = PtyInput::new(bytes.to_vec());
        let mut queued = input.clone();
        loop {
            match input_tx.try_send(queued) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Full(input)) => queued = input,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    bail!("terminal is not accepting input")
                }
            }
            if Instant::now() >= deadline {
                bail!("terminal is not accepting input");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        await_input_completion(&input, &result, remaining)
    }

    pub(crate) fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        validate_terminal_dimensions(columns, rows)?;
        self.master
            .lock()
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize PTY")?;
        self.terminal
            .lock()
            .resize(usize::from(columns), usize::from(rows));
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
        })
    }

    pub(crate) fn begin_selection(&self, point: TerminalPoint, kind: TerminalSelectionKind) {
        self.terminal.lock().begin_selection(point, kind);
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn update_selection(&self, point: TerminalPoint) {
        self.terminal.lock().update_selection(point);
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn clear_selection(&self) {
        self.terminal.lock().clear_selection();
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        self.terminal.lock().selected_text()
    }

    pub(crate) fn scroll(&self, lines: i32) {
        self.terminal.lock().scroll(lines.clamp(-10_000, 10_000));
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn search_literal(&self, query: &str, forward: bool) -> Result<bool> {
        if query.chars().count() > 256 || query.chars().any(char::is_control) {
            bail!("terminal search must be at most 256 visible characters");
        }
        let found = self.terminal.lock().search_literal(query, forward);
        if found {
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

    pub(crate) fn terminate_and_wait(&self) -> Result<()> {
        let result = terminate_child_bounded(self.child.lock().as_mut());
        self.shutdown_threads_bounded();
        result
    }

    /// Stops and joins the PTY worker threads with a patience bound. Dropping
    /// the input sender lets the writer drain queued input and exit; the
    /// reader exits once the terminal delivers EOF. A thread blocked past the
    /// bound (an orphan still holds the slave side) is detached instead of
    /// blocking teardown.
    fn shutdown_threads_bounded(&self) {
        self.input_tx.lock().take();
        join_thread_bounded(
            &self.writer,
            &self.writer_exit,
            &format!("pty writer for pane {}", self.pane_id),
        );
        join_thread_bounded(
            &self.reader,
            &self.reader_exit,
            &format!("pty reader for pane {}", self.pane_id),
        );
    }

    pub(crate) fn exit_status(&self) -> Result<Option<String>> {
        self.child
            .lock()
            .try_wait()
            .map(|status| status.map(|status| status.to_string()))
            .context("observe PTY child exit")
    }

    /// A successful `spawn` only means the executable started. tmux reports a
    /// missing/dead target by exiting immediately, so do not register a tab
    /// until it survived a short bounded startup window.
    pub(crate) fn confirm_live_for_tmux_attach(&self) -> Result<()> {
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
        self.child.lock().process_id()
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
        self.reader
            .lock()
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
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
                at_ms: history::now_ms(),
            },
        );
    }
    for message in terminal.take_notification_messages() {
        push_raw_pane_event(
            &mut events,
            RawPaneEvent {
                kind: NotificationKind::Message,
                message: Some(message),
                at_ms: history::now_ms(),
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
mod tests {
    use super::*;

    use crate::layout::first_pane_id;
    use crate::registry::SessionRegistry;

    #[derive(Clone)]
    struct StalledWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        started: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl Write for StalledWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            {
                let (started, wake) = &*self.started;
                *started.lock().unwrap() = true;
                wake.notify_all();
            }
            let (released, wake) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.bytes.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cancelled_queued_input_never_executes_after_a_stalled_write_releases() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let writer = StalledWriter {
            bytes: Arc::clone(&bytes),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let (first, first_result) = PtyInput::new(b"first".to_vec());
        let (second, second_result) = PtyInput::new(b"second".to_vec());
        tx.send(first).unwrap();
        tx.send(second.clone()).unwrap();
        drop(tx);
        let worker = thread::spawn(move || run_input_writer(writer, &rx, Uuid::nil()));

        let (did_start, wake) = &*started;
        let mut did_start = did_start.lock().unwrap();
        while !*did_start {
            did_start = wake.wait(did_start).unwrap();
        }
        drop(did_start);
        let timeout = await_input_completion(&second, &second_result, Duration::ZERO).unwrap_err();
        assert!(timeout.to_string().contains("cancelled before write"));
        let (released, wake) = &*release;
        *released.lock().unwrap() = true;
        wake.notify_all();

        assert!(
            first_result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        assert!(
            second_result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        worker.join().unwrap();
        assert_eq!(&*bytes.lock(), b"first");
    }

    #[test]
    fn timeout_after_writer_starts_reports_ambiguous_delivery() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let writer = StalledWriter {
            bytes: Arc::clone(&bytes),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (input, result) = PtyInput::new(b"begun".to_vec());
        tx.send(input.clone()).unwrap();
        drop(tx);
        let worker = thread::spawn(move || run_input_writer(writer, &rx, Uuid::nil()));

        let (did_start, wake) = &*started;
        let mut did_start = did_start.lock().unwrap();
        while !*did_start {
            did_start = wake.wait(did_start).unwrap();
        }
        drop(did_start);
        let timeout = await_input_completion(&input, &result, Duration::ZERO).unwrap_err();
        assert!(timeout.to_string().contains("delivery is ambiguous"));
        assert!(timeout.to_string().contains("do not retry automatically"));

        let (released, wake) = &*release;
        *released.lock().unwrap() = true;
        wake.notify_all();
        worker.join().unwrap();
        assert_eq!(&*bytes.lock(), b"begun");
    }

    #[test]
    fn rejects_oversized_terminal_input_frames() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();

        let error = registry
            .write_input(pane_id, &vec![0; MAX_INPUT_FRAME + 1])
            .unwrap_err();

        assert!(error.to_string().contains("terminal input exceeds"));
    }

    #[test]
    fn configured_shell_pty_accepts_input_and_produces_real_output() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry
            .write_input(pane_id, b"printf 'RMUX_REAL_PTY_TEST\\n'\r")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (_, screens) = registry.state().unwrap();
            let screen = screens
                .iter()
                .find(|screen| screen.pane_id == pane_id)
                .unwrap();
            if screen
                .lines
                .iter()
                .flat_map(|line| &line.runs)
                .any(|run| run.text.contains("RMUX_REAL_PTY_TEST"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shell command output did not arrive"
            );
            thread::sleep(Duration::from_millis(25));
        }
        assert!(registry.pane_process_id(pane_id).unwrap().is_some());
    }

    #[test]
    fn notification_delivery_retries_after_event_lock_contention() {
        let mut terminal = TerminalModel::new(80, 24);
        terminal.process_output(b"\x07\x1b]9;approval needed\x07");
        let events = Mutex::new(VecDeque::new());
        let mut previous_bell_count = 0;

        let lock = events.lock();
        try_enqueue_terminal_notifications(&mut terminal, &events, &mut previous_bell_count);
        assert_eq!(previous_bell_count, 0);
        drop(lock);

        try_enqueue_terminal_notifications(&mut terminal, &events, &mut previous_bell_count);
        let events = events.lock();
        assert_eq!(previous_bell_count, 1);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, NotificationKind::Attention);
        assert_eq!(events[1].kind, NotificationKind::Message);
    }

    /// A grandchild that inherits the slave fd (a backgrounded `sleep`) used
    /// to block `PtySession::Drop`'s reader join forever, wedging every
    /// registry operation behind the state lock. Closing such a pane must
    /// complete within the bounded-join budget.
    #[test]
    fn closing_a_pane_with_an_orphaned_grandchild_does_not_deadlock() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry
            .write_input(
                pane_id,
                b"(sleep 8 0<&1 >/dev/null 2>&1 &); printf 'RMUX_ORPHAN_TEST\\n'\r",
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (_, screens) = registry.state().unwrap();
            let screen = screens
                .iter()
                .find(|screen| screen.pane_id == pane_id)
                .unwrap();
            if screen
                .lines
                .iter()
                .flat_map(|line| &line.runs)
                .any(|run| run.text.contains("RMUX_ORPHAN_TEST"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shell command output did not arrive"
            );
            thread::sleep(Duration::from_millis(25));
        }

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let closer = thread::spawn(move || {
            registry.close_pane(pane_id).unwrap();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "close_pane deadlocked on PTY teardown with an orphaned grandchild"
        );
        closer.join().unwrap();
    }
}
