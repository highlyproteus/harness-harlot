use super::*;
use crate::bots::SavedThread;
use crate::layout::find_pane_in_snapshot;
use crate::registry::create_owner_only_directory;
use hh_protocol::{BotSpec, CodingAgent, PaneLayout, SessionSnapshot};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// A persistent registry in a private temporary state directory whose agent
/// discovery finds a fake omp that prints the arguments it was launched with.
struct Fixture {
    registry: SessionRegistry,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("hh-bot-thread-tests-{}", Uuid::new_v4()));
        let fakes = root.join("agents");
        create_owner_only_directory(&fakes);
        let omp = fakes.join("omp");
        std::fs::write(&omp, "#!/bin/sh\necho \"OMP_UP $*\"\n").unwrap();
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = root.join("state");
        create_owner_only_directory(&state);
        let registry = SessionRegistry::persistent(state.join("sessions.json")).unwrap();
        *registry.coding_agents.lock() = Some(vec![CodingAgent {
            profile: TerminalProfile::Omp,
            command: "omp".to_owned(),
            path: omp.to_string_lossy().into_owned(),
        }]);
        Self { registry, root }
    }

    fn threads_dir(&self, tab_id: Uuid) -> PathBuf {
        self.root
            .join("state/bots")
            .join(tab_id.to_string())
            .join("threads")
    }

    /// Writes a saved omp session with the current title-slot layout.
    fn save_thread(&self, tab_id: Uuid, id: &str, title: &str) {
        let line = |value: serde_json::Value| format!("{value}\n");
        let contents =
            line(serde_json::json!({"type": "title", "v": 1, "title": title, "pad": "  "}))
                + &line(serde_json::json!({"type": "session", "version": 3, "id": id, "cwd": "/"}));
        std::fs::write(
            self.threads_dir(tab_id)
                .join(format!("2026-01-01_{id}.jsonl")),
            contents,
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn screen_text(registry: &SessionRegistry, pane_id: Uuid) -> String {
    let screen = registry.pane(pane_id).unwrap().screen(pane_id).unwrap();
    screen
        .lines
        .iter()
        .flat_map(|line| &line.runs)
        .map(|run| run.text.as_str())
        .collect()
}

fn wait_for_screen(registry: &SessionRegistry, pane_id: Uuid, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = screen_text(registry, pane_id);
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{needle} never appeared; screen:\n{text}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn bot_tab(snapshot: &SessionSnapshot, tab_id: Uuid) -> &Tab {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find(|tab| tab.id == tab_id)
        .unwrap()
}

/// The bot's live panes and its active one.
fn stack(registry: &SessionRegistry, tab_id: Uuid) -> (Vec<Uuid>, Uuid) {
    match &bot_tab(&registry.snapshot().unwrap(), tab_id).layout {
        PaneLayout::Leaf { pane } => (vec![pane.id], pane.id),
        PaneLayout::Stack { panes, active } => {
            (panes.iter().map(|pane| pane.id).collect(), *active)
        }
        PaneLayout::Split { .. } => panic!("a bot never splits"),
    }
}

fn spec(registry: &SessionRegistry, tab_id: Uuid) -> BotSpec {
    bot_tab(&registry.snapshot().unwrap(), tab_id)
        .bot
        .clone()
        .unwrap()
}

fn target(panes: &[(Uuid, Option<&str>)], active: Uuid, pinned: &[&str]) -> BotTarget {
    BotTarget {
        workspace_id: Uuid::nil(),
        active_pane: active,
        panes: panes.iter().map(|(pane_id, _)| *pane_id).collect(),
        name: "Hive3".to_owned(),
        project_dir: None,
        spec: BotSpec {
            agent: TerminalProfile::Omp,
            instructions: None,
            home: None,
            pinned_threads: pinned.iter().map(|pin| (*pin).to_owned()).collect(),
            thread_panes: panes
                .iter()
                .map(|(pane_id, session)| {
                    (
                        *pane_id,
                        BotThreadPane {
                            session: session.map(str::to_owned),
                            activated_ms: 0,
                        },
                    )
                })
                .collect(),
        },
    }
}

fn saved(id: &str, title: &str, updated_ms: u64) -> SavedThread {
    SavedThread {
        id: id.to_owned(),
        title: Some(title.to_owned()),
        updated_ms,
    }
}

#[test]
fn threads_list_pinned_first_then_newest_and_map_live_panes() {
    let [shows_old, unsaved, fresh] = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let target = target(
        &[
            (shows_old, Some("old")),
            (unsaved, Some("unsaved")),
            (fresh, None),
        ],
        fresh,
        &["oldest"],
    );
    let threads = merge_threads(
        &target,
        vec![
            saved("new", "New", 300),
            saved("old", "Old", 200),
            saved("oldest", "Oldest", 100),
        ],
        1_000,
    );
    let summary = threads
        .iter()
        .map(|thread| {
            (
                thread.id.clone(),
                thread.title.as_deref(),
                thread.pinned,
                thread.pane_id,
            )
        })
        .collect::<Vec<_>>();
    let mut live_now = [
        (pane_thread_id(fresh), None, false, Some(fresh)),
        ("unsaved".to_owned(), None, false, Some(unsaved)),
    ];
    live_now.sort_by(|left, right| left.0.cmp(&right.0));
    let mut expected = vec![("oldest".to_owned(), Some("Oldest"), true, None)];
    expected.extend(live_now);
    expected.extend([
        ("new".to_owned(), Some("New"), false, None),
        ("old".to_owned(), Some("Old"), false, Some(shows_old)),
    ]);
    assert_eq!(summary, expected);
    assert_eq!(threads[1].updated_ms, 1_000, "live unsaved threads are now");
}

#[test]
fn two_panes_on_one_session_list_it_once_on_the_active_pane() {
    let [background, active] = [Uuid::new_v4(), Uuid::new_v4()];
    let target = target(&[(background, Some("s")), (active, Some("s"))], active, &[]);
    let threads = merge_threads(&target, Vec::new(), 5);
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].pane_id, Some(active));
}

#[test]
fn eviction_closes_the_least_recent_idle_panes_but_never_the_active_or_a_busy_one() {
    let panes = (0..8).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
    let statuses = [
        PaneStatus::Working,
        PaneStatus::NeedsApproval,
        PaneStatus::NeedsInput,
        PaneStatus::Attention,
        PaneStatus::Done,
        PaneStatus::Idle,
        PaneStatus::Idle,
        PaneStatus::Idle,
    ];
    let threads = panes
        .iter()
        .zip(statuses)
        .enumerate()
        .map(|(index, (pane_id, status))| LiveThread {
            pane_id: *pane_id,
            activated_ms: index as u64,
            status,
        })
        .collect::<Vec<_>>();
    // The active pane is the least recently activated one here.
    let mut threads = threads;
    threads[5].activated_ms = 0;
    threads[0].activated_ms = 9;
    assert_eq!(
        select_evictions(&threads, panes[5]),
        [panes[4], panes[6], panes[7]]
    );
    assert!(select_evictions(&threads[..5], panes[0]).is_empty());
    let all_busy = threads[..6]
        .iter()
        .map(|thread| LiveThread {
            status: PaneStatus::Working,
            ..*thread
        })
        .collect::<Vec<_>>();
    assert!(select_evictions(&all_busy, panes[0]).is_empty());
}

#[test]
fn opening_threads_activates_live_ones_and_resumes_saved_ones_in_new_panes() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (_, tab_id, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    let threads_dir = fixture.threads_dir(tab_id);
    wait_for_screen(
        registry,
        first,
        &format!(
            "OMP_UP -e {}",
            fixture.root.join("state/bots/hh-omp.ts").display()
        ),
    );
    wait_for_screen(
        registry,
        first,
        &format!("--session-dir {}", threads_dir.display()),
    );
    assert!(!screen_text(registry, first).contains("--resume"));
    let threads = registry.list_bot_threads(tab_id).unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, pane_thread_id(first));
    assert_eq!(threads[0].pane_id, Some(first));

    registry.report_bot_session(first, "s-first").unwrap();
    fixture.save_thread(tab_id, "s-saved", "Fix login");
    let threads = registry.list_bot_threads(tab_id).unwrap();
    assert_eq!(
        threads
            .iter()
            .map(|thread| (thread.id.as_str(), thread.pane_id))
            .collect::<Vec<_>>(),
        [("s-first", Some(first)), ("s-saved", None)],
        "an unsaved live session is newest"
    );

    let resumed = registry.open_bot_thread(tab_id, Some("s-saved")).unwrap();
    assert_ne!(resumed, first);
    assert_eq!(stack(registry, tab_id), (vec![first, resumed], resumed));
    wait_for_screen(
        registry,
        resumed,
        &format!("--session-dir {} --resume s-saved", threads_dir.display()),
    );
    let snapshot = registry.snapshot().unwrap();
    let pane = find_pane_in_snapshot(&snapshot, resumed).unwrap();
    assert_eq!(pane.profile_override, Some(TerminalProfile::Omp));
    assert_eq!(
        spec(registry, tab_id).thread_panes[&resumed]
            .session
            .as_deref(),
        Some("s-saved")
    );

    assert_eq!(
        registry.open_bot_thread(tab_id, Some("s-first")).unwrap(),
        first
    );
    assert_eq!(
        stack(registry, tab_id),
        (vec![first, resumed], first),
        "a live thread is activated without a new pane"
    );
    assert_eq!(
        registry.open_bot_thread(tab_id, Some("s-saved")).unwrap(),
        resumed
    );

    let fresh = registry.open_bot_thread(tab_id, None).unwrap();
    assert_eq!(
        stack(registry, tab_id),
        (vec![first, resumed, fresh], fresh)
    );
    let text = wait_for_screen(registry, fresh, "OMP_UP");
    assert!(!text.contains("--resume"), "{text}");
    registry.open_bot_thread(tab_id, Some("s-first")).unwrap();
    assert_eq!(
        registry
            .open_bot_thread(tab_id, Some(&pane_thread_id(fresh)))
            .unwrap(),
        fresh
    );
    assert_eq!(stack(registry, tab_id).1, fresh);

    for missing in [
        "s-missing",
        "pane:00000000-0000-0000-0000-000000000000",
        "../x",
    ] {
        assert!(registry.open_bot_thread(tab_id, Some(missing)).is_err());
    }
    assert_eq!(stack(registry, tab_id).0.len(), 3);
}

#[test]
fn opening_more_than_five_threads_closes_the_least_recent_idle_one() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (_, tab_id, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    let mut opened = vec![first];
    for _ in 0..MAX_LIVE_THREADS {
        // Distinct activation times make the eviction order deterministic.
        thread::sleep(Duration::from_millis(5));
        opened.push(registry.open_bot_thread(tab_id, None).unwrap());
    }
    let (panes, active) = stack(registry, tab_id);
    assert_eq!(active, *opened.last().unwrap());
    assert_eq!(panes, opened[1..]);
    assert!(
        registry.pane(first).is_err(),
        "the evicted pane's process ended"
    );
    assert!(!spec(registry, tab_id).thread_panes.contains_key(&first));
}

#[test]
fn reported_sessions_follow_the_pane_and_survive_a_fresh_shell() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (_, tab_id, pane_id) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    let plain = crate::layout::first_pane_id(&registry.snapshot().unwrap()).unwrap();
    assert_ne!(plain, pane_id);
    let error = registry.report_bot_session(plain, "s1").unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("pane {plain} is not a bot terminal")
    );
    assert!(registry.report_bot_session(pane_id, "bad id").is_err());

    registry.report_bot_session(pane_id, "s1").unwrap();
    registry.report_bot_session(pane_id, "s2").unwrap();
    assert_eq!(
        spec(registry, tab_id).thread_panes[&pane_id]
            .session
            .as_deref(),
        Some("s2"),
        "/new and /resume replace the pane's session"
    );

    // A fresh shell (as after a service restart) resumes the pane's session.
    let session = {
        let mut state = registry.state.write();
        let terminal = state.terminal_pane_mut(pane_id).unwrap();
        terminal.exit_status = Some("Exited with code 0".to_owned());
        Arc::clone(&terminal.session)
    };
    session.terminate_and_wait().unwrap();
    registry.reattach_pane(pane_id).unwrap();
    wait_for_screen(registry, pane_id, "--resume s2");

    registry.restart_bot(tab_id).unwrap();
    wait_for_screen(registry, pane_id, "--resume s2");
}

#[test]
fn pins_persist_and_only_saved_threads_can_be_pinned() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (_, tab_id, pane_id) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    fixture.save_thread(tab_id, "s-old", "Old");
    registry
        .set_bot_thread_pinned(tab_id, "s-old", true)
        .unwrap();
    registry
        .set_bot_thread_pinned(tab_id, "s-old", true)
        .unwrap();
    assert_eq!(spec(registry, tab_id).pinned_threads, ["s-old"]);
    let threads = registry.list_bot_threads(tab_id).unwrap();
    assert_eq!(threads[0].id, "s-old");
    assert!(threads[0].pinned);
    assert!(
        registry
            .set_bot_thread_pinned(tab_id, &pane_thread_id(pane_id), true)
            .is_err()
    );
    registry
        .set_bot_thread_pinned(tab_id, "s-old", false)
        .unwrap();
    assert!(spec(registry, tab_id).pinned_threads.is_empty());
}

#[test]
fn workers_record_the_thread_pane_that_opened_them() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (_, bot, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    let second = registry.open_bot_thread(bot, None).unwrap();
    let (_, worker, _) = registry
        .create_worker(None, None, None, None, Some(second))
        .unwrap();
    let plain = crate::layout::first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let workstation = registry.snapshot().unwrap().workspaces[0].id;
    let (_, unowned, _) = registry
        .create_worker(Some(workstation), None, None, None, Some(plain))
        .unwrap();
    let snapshot = registry.snapshot().unwrap();
    assert_eq!(bot_tab(&snapshot, worker).owner_bot, Some(bot));
    assert_eq!(bot_tab(&snapshot, worker).owner_thread, Some(second));
    assert_eq!(bot_tab(&snapshot, unowned).owner_thread, None);
    assert_ne!(first, second);
}

#[test]
fn switching_a_bot_to_another_agent_collapses_it_to_one_fresh_pane() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let hermes = fixture.root.join("agents/hermes");
    std::fs::write(&hermes, "#!/bin/sh\necho \"HERMES_UP $*\"\n").unwrap();
    std::fs::set_permissions(&hermes, std::fs::Permissions::from_mode(0o755)).unwrap();
    registry
        .coding_agents
        .lock()
        .as_mut()
        .unwrap()
        .push(CodingAgent {
            profile: TerminalProfile::Hermes,
            command: "hermes".to_owned(),
            path: hermes.to_string_lossy().into_owned(),
        });
    let (_, tab_id, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    registry.report_bot_session(first, "s1").unwrap();
    let second = registry.open_bot_thread(tab_id, None).unwrap();

    registry
        .set_bot_agent(tab_id, TerminalProfile::Hermes)
        .unwrap();

    assert_eq!(stack(registry, tab_id), (vec![second], second));
    wait_for_screen(registry, second, "HERMES_UP");
    assert!(registry.list_bot_threads(tab_id).unwrap().is_empty());
    let spec = spec(registry, tab_id);
    assert_eq!(spec.thread_panes.len(), 1);
    assert_eq!(spec.thread_panes[&second].session, None);
    assert!(
        registry
            .open_bot_thread(tab_id, Some("s1"))
            .unwrap_err()
            .to_string()
            .contains("only omp bots keep threads")
    );
    assert_eq!(registry.open_bot_thread(tab_id, None).unwrap(), second);
    assert_eq!(stack(registry, tab_id).0, [second]);
    assert!(Path::new(&fixture.threads_dir(tab_id)).is_dir());
}
