use std::thread;
use std::time::{Duration, Instant};

use rust_mux_protocol::{PaneLayout, PaneRevisionCursor, SplitAxis, TerminalScreen};
use rust_mux_session_service::SessionRegistry;
use rust_mux_terminal_model::SCROLLBACK_HISTORY_LIMIT;
use uuid::Uuid;

#[test]
fn stream_delivers_only_changed_subscribed_panes_without_cross_pane_churn() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane(&registry);
    let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();

    let initial = registry.pane_updates(None, &[], &[first, second]).unwrap();
    assert_eq!(initial.screens.len(), 2);
    let initial_cursors = cursors(&initial.screens);

    let unchanged = registry
        .pane_updates(
            initial.snapshot.as_ref().map(|snapshot| snapshot.revision),
            &initial_cursors,
            &[first, second],
        )
        .unwrap();
    assert!(unchanged.snapshot.is_none());
    assert!(unchanged.screens.is_empty());
    assert_eq!(unchanged.diagnostics.screen_bytes, 0);

    registry
        .write_input(first, b"printf 'RMUX_CHANGED_ONLY_FIRST\\n'\r")
        .unwrap();
    wait_for_revision_after(&registry, first, revision_for(&initial_cursors, first));

    let changed = registry
        .pane_updates(None, &initial_cursors, &[first, second])
        .unwrap();
    assert_eq!(
        changed
            .screens
            .iter()
            .map(|screen| screen.pane_id)
            .collect::<Vec<_>>(),
        vec![first]
    );
    assert_eq!(changed.diagnostics.screens_delivered, 1);
    assert!(changed.diagnostics.screen_bytes > 0);
    assert!(
        changed
            .pane_states
            .iter()
            .find(|state| state.pane_id == second)
            .is_some_and(|state| !state.dirty)
    );
}

#[test]
fn cold_pane_keeps_draining_and_refocus_resyncs_without_output_loss() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane(&registry);
    let initial = registry.pane_updates(None, &[], &[pane_id]).unwrap();
    let initial_revision = initial.screens[0].revision;
    let cursor = [PaneRevisionCursor {
        pane_id,
        revision: initial_revision,
    }];

    registry
        .write_input(
            pane_id,
            b"i=0; while [ $i -lt 2500 ]; do printf 'cold-%04d\\n' $i; i=$((i+1)); done; printf 'RMUX_COLD_FINAL\\n'\r",
        )
        .unwrap();
    wait_for_bounded_screen_text(&registry, pane_id, "RMUX_COLD_FINAL");

    let cold = registry.pane_updates(None, &cursor, &[]).unwrap();
    assert!(cold.screens.is_empty(), "cold panes must not repaint");
    let cold_state = cold
        .pane_states
        .iter()
        .find(|state| state.pane_id == pane_id)
        .unwrap();
    assert!(cold_state.revision > initial_revision);
    assert!(cold_state.dirty);
    assert!(!cold_state.subscribed);

    let (resynced, diagnostics) = registry.pane_snapshot(pane_id).unwrap();
    assert!(screen_contains(&resynced, "RMUX_COLD_FINAL"));
    assert_eq!(
        resynced.history_size,
        u32::try_from(SCROLLBACK_HISTORY_LIMIT).unwrap()
    );
    assert_eq!(diagnostics.screens_delivered, 1);
    assert!(diagnostics.screen_bytes > 0);

    let caught_up = [PaneRevisionCursor {
        pane_id,
        revision: resynced.revision,
    }];
    let live = registry.pane_updates(None, &caught_up, &[pane_id]).unwrap();
    assert!(live.screens.is_empty());
    assert!(live.pane_states.iter().all(|state| !state.dirty));
}

#[test]
fn receiver_reconnect_with_no_cursors_gets_one_current_resync() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane(&registry);
    let second = registry.create_pane(first, SplitAxis::Vertical).unwrap();
    registry
        .write_input(second, b"printf 'RMUX_RECONNECT_SECOND\\n'\r")
        .unwrap();
    wait_for_screen_text(&registry, second, "RMUX_RECONNECT_SECOND");

    let reconnected = registry.pane_updates(None, &[], &[first, second]).unwrap();
    assert!(reconnected.snapshot.is_some());
    assert_eq!(reconnected.screens.len(), 2);
    assert!(
        reconnected
            .screens
            .iter()
            .find(|screen| screen.pane_id == second)
            .is_some_and(|screen| screen_contains(screen, "RMUX_RECONNECT_SECOND"))
    );

    let cursors = cursors(&reconnected.screens);
    let settled = registry
        .pane_updates(
            reconnected
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.revision),
            &cursors,
            &[first, second],
        )
        .unwrap();
    assert!(settled.snapshot.is_none());
    assert!(settled.screens.is_empty());
}

#[test]
fn local_activity_matrix_smoke_reports_bounded_change_only_delivery() {
    let registry = SessionRegistry::new().unwrap();
    let first = first_pane(&registry);
    let panes = [
        first,
        registry.create_pane(first, SplitAxis::Horizontal).unwrap(),
        registry.create_pane(first, SplitAxis::Vertical).unwrap(),
        registry.create_tab(first).unwrap(),
    ];
    for (index, pane_id) in panes.iter().enumerate() {
        registry
            .resize_pane(
                *pane_id,
                80 + u16::try_from(index).unwrap() * 20,
                24 + u16::try_from(index).unwrap() * 8,
            )
            .unwrap();
    }
    thread::sleep(Duration::from_millis(200));

    let initial = registry.pane_updates(None, &[], &panes).unwrap();
    let matrix_cursors = cursors(&initial.screens);
    assert_eq!(initial.screens.len(), panes.len());
    assert!(initial.diagnostics.service_memory_bytes > 0);

    let idle = registry
        .pane_updates(
            initial.snapshot.as_ref().map(|snapshot| snapshot.revision),
            &matrix_cursors,
            &panes,
        )
        .unwrap();
    assert_eq!(idle.diagnostics.screen_bytes, 0);

    registry
        .write_input(first, b"printf 'RMUX_MATRIX_BURST\\n'\r")
        .unwrap();
    wait_for_revision_after(&registry, first, revision_for(&matrix_cursors, first));
    let active = registry
        .pane_updates(None, &matrix_cursors, &[first])
        .unwrap();
    assert_eq!(active.screens.len(), 1);
    assert_eq!(active.screens[0].pane_id, first);

    let cold_cursors = cursors(&active.screens);
    let cold = registry.pane_updates(None, &cold_cursors, &[]).unwrap();
    assert_eq!(cold.diagnostics.screen_bytes, 0);
    assert_eq!(cold.diagnostics.screens_delivered, 0);

    let refocus_started = Instant::now();
    let (_, refocus) = registry.pane_snapshot(first).unwrap();
    let refocus_micros = u64::try_from(refocus_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    assert!(refocus.screen_bytes > 0);
    assert!(refocus_micros < 500_000, "local refocus exceeded 500 ms");

    eprintln!(
        "pane-stream-smoke panes={} initial_bytes={} idle_bytes={} active_bytes={} cold_bytes={} refocus_micros={} preparation_micros={} memory_bytes={} cpu_milli_percent={}",
        panes.len(),
        initial.diagnostics.screen_bytes,
        idle.diagnostics.screen_bytes,
        active.diagnostics.screen_bytes,
        cold.diagnostics.screen_bytes,
        refocus_micros,
        active.diagnostics.preparation_micros,
        active.diagnostics.service_memory_bytes,
        active.diagnostics.service_cpu_milli_percent,
    );
}

fn first_pane(registry: &SessionRegistry) -> Uuid {
    let snapshot = registry.snapshot().unwrap();
    match &snapshot.workspaces[0].tabs[0].layout {
        PaneLayout::Leaf { pane } => pane.id,
        other => panic!("unexpected initial layout: {other:?}"),
    }
}

fn cursors(screens: &[TerminalScreen]) -> Vec<PaneRevisionCursor> {
    screens
        .iter()
        .map(|screen| PaneRevisionCursor {
            pane_id: screen.pane_id,
            revision: screen.revision,
        })
        .collect()
}

fn revision_for(cursors: &[PaneRevisionCursor], pane_id: Uuid) -> u64 {
    cursors
        .iter()
        .find(|cursor| cursor.pane_id == pane_id)
        .unwrap()
        .revision
}

fn wait_for_revision_after(registry: &SessionRegistry, pane_id: Uuid, revision: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let update = registry.pane_updates(None, &[], &[]).unwrap();
        if update
            .pane_states
            .iter()
            .find(|state| state.pane_id == pane_id)
            .is_some_and(|state| state.revision > revision)
        {
            return;
        }
        assert!(Instant::now() < deadline, "pane revision did not advance");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_screen_text(registry: &SessionRegistry, pane_id: Uuid, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (screen, _) = registry.pane_snapshot(pane_id).unwrap();
        if screen_contains(&screen, expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "terminal did not drain expected output"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_bounded_screen_text(registry: &SessionRegistry, pane_id: Uuid, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (screen, _) = registry.pane_snapshot(pane_id).unwrap();
        if screen.history_size == u32::try_from(SCROLLBACK_HISTORY_LIMIT).unwrap()
            && screen_contains(&screen, expected)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "terminal did not drain through bounded history"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn screen_contains(screen: &TerminalScreen, expected: &str) -> bool {
    screen
        .lines
        .iter()
        .flat_map(|line| &line.runs)
        .any(|run| run.text.contains(expected))
}
