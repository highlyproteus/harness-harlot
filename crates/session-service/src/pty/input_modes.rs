//! Application input modes kept in the tmux pane across session-service
//! restarts. A restarted service rebuilds each terminal model from captured
//! screen text, which cannot show that the application turned on bracketed
//! paste, mouse reporting, or kitty paste events; the tmux server keeps a copy
//! in a pane user option.

use std::sync::atomic::Ordering;

use anyhow::Result;

use super::{PtySession, Transport};

/// tmux pane user option holding the enabled input modes, e.g. `2004,5522`.
pub(super) const INPUT_MODES_OPTION: &str = "@hh-input-modes";

impl PtySession {
    /// Re-enables the input modes a previous service saved for this pane.
    pub(super) fn restore_saved_input_modes(&self) {
        let Transport::Tmux {
            client,
            tmux_pane_id,
            ..
        } = &self.transport
        else {
            return;
        };
        match client.pane_user_option(tmux_pane_id, INPUT_MODES_OPTION) {
            Ok(Some(modes)) => {
                let mut terminal = self.terminal.lock();
                terminal.restore_input_modes(&modes);
                *self.saved_input_modes.lock() = terminal.input_modes();
                drop(terminal);
                self.content_revision.fetch_add(1, Ordering::Release);
                self.revision.fetch_add(1, Ordering::Release);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "failed to read saved input modes for pane {}: {error:#}",
                    self.pane_id
                );
            }
        }
    }

    /// Stores the application's current input modes in its tmux pane when
    /// they changed. PTY-backed sessions end with the service and need no copy.
    pub(crate) fn save_input_modes(&self) -> Result<()> {
        let Transport::Tmux {
            client,
            tmux_pane_id,
            ..
        } = &self.transport
        else {
            return Ok(());
        };
        let current = self.terminal.lock().input_modes();
        let mut saved = self.saved_input_modes.lock();
        if *saved != current {
            client.set_pane_user_option(tmux_pane_id, INPUT_MODES_OPTION, &current)?;
            *saved = current;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use hh_protocol::TerminalModes;
    use parking_lot::Mutex;
    use uuid::Uuid;

    use super::INPUT_MODES_OPTION;
    use crate::pty::PtySession;
    use crate::tmux::system_tmux_binary;
    use crate::tmux_control::{PaneSinks, PrivateTmuxServerGuard, TmuxControlClient, TmuxServer};

    #[test]
    fn input_modes_survive_a_service_restart_reattach() {
        let Ok(binary) = system_tmux_binary() else {
            return;
        };
        let fixture = std::env::temp_dir().join(format!("hh-input-modes-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&fixture).unwrap();
        let config_path = fixture.join("hh.conf");
        std::fs::write(
            &config_path,
            concat!(
                include_str!("../../bundled/hh.tmux.conf"),
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
        let client =
            TmuxControlClient::spawn(&server, &format!("hh-{}", &token[..12]), sinks).unwrap();
        let session = PtySession::spawn_tmux(
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            Path::new("/tmp"),
            &Arc::clone(&client),
        )
        .unwrap();

        // The application turns on bracketed paste and kitty paste events, as
        // omp does at startup, and the service saves them to the tmux pane.
        session
            .write_input(b"printf '\\033[?2004h\\033[?5522h'\r")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.enhanced_paste() {
            assert!(Instant::now() < deadline, "input modes were not observed");
            thread::sleep(Duration::from_millis(25));
        }
        session.save_input_modes().unwrap();
        let (window_id, tmux_pane_id) = session.tmux_ids().unwrap();
        let (window_id, tmux_pane_id) = (window_id.to_owned(), tmux_pane_id.to_owned());
        assert_eq!(
            client
                .pane_user_option(&tmux_pane_id, INPUT_MODES_OPTION)
                .unwrap()
                .as_deref(),
            Some("2004,5522")
        );
        let shell_pid = session.process_id().unwrap();
        drop(session);

        // A restarted service rebuilds the model from screen text, which alone
        // would have lost both modes.
        let attached_id = Uuid::new_v4();
        let attached = PtySession::attach_tmux(
            attached_id,
            Arc::clone(&client),
            window_id,
            tmux_pane_id,
            shell_pid,
        )
        .unwrap();
        assert!(
            attached
                .screen(attached_id)
                .unwrap()
                .modes
                .contains(TerminalModes::BRACKETED_PASTE)
        );
        assert!(attached.enhanced_paste());
        attached.terminate_and_wait().unwrap();
        client.kill_session().unwrap();
        drop(attached);
        drop(client);
        std::fs::remove_dir_all(fixture).unwrap();
    }
}
