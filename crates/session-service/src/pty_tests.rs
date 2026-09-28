use super::*;

use crate::layout::first_pane_id;
use crate::registry::SessionRegistry;

#[test]
fn selection_revision_preserves_content_revision() {
    let pane_id = Uuid::new_v4();
    let mut command = CommandBuilder::new("/usr/bin/seq");
    command.args(["1", "200"]);
    let session = PtySession::spawn_command(pane_id, command, "selection revision test").unwrap();
    let Transport::Pty {
        reader_exit,
        reader,
        ..
    } = &session.transport
    else {
        unreachable!();
    };
    assert_eq!(
        reader_exit.lock().recv_timeout(Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
    );
    reader.lock().take().unwrap().join().unwrap();
    for lines in [0, -1] {
        let before = session.screen(pane_id).unwrap();
        session.scroll(lines);
        let unchanged = session.screen(pane_id).unwrap();
        assert_eq!(unchanged.content_revision, before.content_revision);
        assert_eq!(unchanged.revision, before.revision);
    }
    let before = session.screen(pane_id).unwrap();
    session.begin_selection(
        TerminalPoint { row: 0, column: 0 },
        TerminalSelectionKind::Simple,
    );
    session.update_selection(TerminalPoint { row: 2, column: 2 });
    let selected = session.screen(pane_id).unwrap();
    assert!(selected.revision > before.revision);
    assert_eq!(selected.content_revision, before.content_revision);
    assert!(selected.selection.is_some());
    session.scroll(1);
    let scrolled = session.screen(pane_id).unwrap();
    assert!(scrolled.revision > selected.revision);
    assert!(scrolled.content_revision > selected.content_revision);
    assert_eq!(scrolled.display_offset, 1);
    session.clear_selection();
    let cleared = session.screen(pane_id).unwrap();
    assert!(cleared.revision > scrolled.revision);
    assert_eq!(cleared.content_revision, scrolled.content_revision);
    assert!(cleared.selection.is_none());
    session.scroll(10_000);
    let oldest = session.screen(pane_id).unwrap();
    assert_eq!(oldest.display_offset, oldest.history_size);
    session.scroll(1);
    let unchanged = session.screen(pane_id).unwrap();
    assert_eq!(unchanged.content_revision, oldest.content_revision);
    assert_eq!(unchanged.revision, oldest.revision);
}

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
fn managed_tmux_transport_streams_and_reattaches_when_available() {
    use std::collections::HashMap;

    use crate::tmux::system_tmux_binary;
    use crate::tmux_control::{PaneSinks, PrivateTmuxServerGuard, TmuxServer};

    let Ok(binary) = system_tmux_binary() else {
        return;
    };
    let fixture = std::env::temp_dir().join(format!("hh-tmux-transport-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&fixture).unwrap();
    let config_path = fixture.join("hh.conf");
    std::fs::write(
        &config_path,
        concat!(
            include_str!("../bundled/hh.tmux.conf"),
            "set -g default-shell '/bin/sh'\n"
        ),
    )
    .unwrap();
    let token = Uuid::new_v4().simple().to_string();
    let socket_name = format!("hh-test-{}", &token[..12]);
    let _server_guard = PrivateTmuxServerGuard {
        binary: binary.clone(),
        socket_name: socket_name.clone(),
    };
    let server = TmuxServer {
        binary,
        socket_name,
        config_path,
    };
    let sinks: PaneSinks = Arc::new(Mutex::new(HashMap::new()));
    let client = TmuxControlClient::spawn(&server, &format!("hh-{}", &token[..12]), sinks).unwrap();
    let pane_id = Uuid::new_v4();
    let session = PtySession::spawn_tmux(
        pane_id,
        Uuid::new_v4(),
        None,
        Path::new("/tmp"),
        &Arc::clone(&client),
    )
    .unwrap();
    session.resize(90, 25).unwrap();
    let second_client = TmuxControlClient::spawn(
        &server,
        &format!("hh-second-{}", &token[..12]),
        Arc::new(Mutex::new(HashMap::new())),
    )
    .unwrap();
    let second_session = PtySession::spawn_tmux(
        Uuid::new_v4(),
        Uuid::new_v4(),
        None,
        Path::new("/tmp"),
        &second_client,
    )
    .unwrap();
    second_session.resize(80, 24).unwrap();
    second_session.terminate_and_wait().unwrap();
    second_client.kill_session().unwrap();
    drop(second_session);
    drop(second_client);
    session
        .write_input(b"printf 'HH_TMUX_TRANSPORT\\n'\r")
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = session.screen(pane_id).unwrap();
        if screen
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .any(|run| run.text.contains("HH_TMUX_TRANSPORT"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "tmux transport output did not arrive"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let (window_id, tmux_pane_id) = session.tmux_ids().unwrap();
    let window_id = window_id.to_owned();
    let tmux_pane_id = tmux_pane_id.to_owned();
    let shell_pid = session.process_id().unwrap();
    drop(session);
    assert!(
        client
            .list_panes()
            .unwrap()
            .iter()
            .any(|pane| pane.window_id == window_id && pane.pane_id == tmux_pane_id)
    );

    let attached_id = Uuid::new_v4();
    let attached = PtySession::attach_tmux(
        attached_id,
        Arc::clone(&client),
        window_id,
        tmux_pane_id,
        shell_pid,
    )
    .unwrap();
    let screen = attached.screen(attached_id).unwrap();
    assert!(
        screen
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .any(|run| run.text.contains("HH_TMUX_TRANSPORT"))
    );
    attached.terminate_and_wait().unwrap();
    client.kill_session().unwrap();
    drop(attached);
    drop(client);
    std::fs::remove_dir_all(fixture).unwrap();
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
