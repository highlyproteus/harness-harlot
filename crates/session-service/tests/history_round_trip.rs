use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use hh_protocol::{
    HistoryClearScope, HistoryPageDirection, HistorySettings, PaneLayout, PaneRevisionCursor,
    TerminalProfile,
};
use hh_session_service::SessionRegistry;
use uuid::Uuid;

#[test]
fn real_pty_output_is_archived_and_lazily_loaded_without_expanding_live_history() {
    let directory = test_directory("pty-archive");
    let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
    let history_settings = HistorySettings {
        enabled: true,
        ..HistorySettings::default()
    };
    registry.set_history_settings(history_settings).unwrap();
    let snapshot = registry.snapshot().unwrap();
    let pane_id = leaf(&snapshot.workspaces[0].tabs[0].layout);
    let initial = registry
        .pane_updates(None, &[], &[pane_id], true, 0)
        .unwrap();
    let initial_cursor = PaneRevisionCursor {
        pane_id,
        revision: initial.screens[0].revision,
    };
    registry
        .set_pane_profile(pane_id, Some(TerminalProfile::Hermes))
        .unwrap();
    registry
        .write_input(pane_id, b"printf 'RMUX_ARCHIVE_ROUND_TRIP\\n'\r")
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let page = loop {
        if let Some(page) = registry
            .load_history_page(pane_id, None, HistoryPageDirection::Older)
            .unwrap()
            && page
                .lines
                .iter()
                .any(|line| line.contains("RMUX_ARCHIVE_ROUND_TRIP"))
        {
            break page;
        }
        assert!(
            Instant::now() < deadline,
            "real PTY output did not reach the local archive"
        );
        thread::sleep(Duration::from_millis(25));
    };

    assert_eq!(page.pane_id, pane_id);
    let integrated = registry
        .pane_updates(
            initial.snapshot.as_ref().map(|snapshot| snapshot.revision),
            &[initial_cursor],
            &[pane_id],
            true,
            0,
        )
        .unwrap();
    let integrated_snapshot = integrated.snapshot.expect("identity changed desired state");
    let integrated_pane = pane(&integrated_snapshot.workspaces[0].tabs[0].layout);
    assert_eq!(
        integrated_pane.profile_override,
        Some(TerminalProfile::Hermes)
    );
    assert_eq!(integrated_pane.identity.profile, TerminalProfile::Hermes);
    assert!(integrated.screens.iter().any(|screen| {
        screen.pane_id == pane_id
            && screen
                .lines
                .iter()
                .flat_map(|line| &line.runs)
                .any(|run| run.text.contains("RMUX_ARCHIVE_ROUND_TRIP"))
    }));
    let status = registry.history_status();
    assert_eq!(status.live_scrollback_lines, 2_000);
    assert!(status.archived_bytes > 0);
    assert_eq!(status.retained_sessions, 1);
    assert!(status.oldest_started_ms.is_some());

    registry
        .clear_history(HistoryClearScope::Terminal { pane_id })
        .unwrap();
    assert!(
        registry
            .load_history_page(pane_id, None, HistoryPageDirection::Older)
            .unwrap()
            .is_none()
    );

    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

fn leaf(layout: &PaneLayout) -> Uuid {
    pane(layout).id
}

fn pane(layout: &PaneLayout) -> &hh_protocol::Pane {
    match layout {
        PaneLayout::Leaf { pane } => pane,
        _ => panic!("expected a leaf pane"),
    }
}

fn test_directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hh-integration-{label}-{}", Uuid::new_v4()))
}
