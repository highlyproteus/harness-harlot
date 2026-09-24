use super::*;
use crate::bots::SavedThread;
use crate::layout::{collect_pane_ids, find_pane_in_snapshot};
use crate::registry::create_owner_only_directory;
use hh_protocol::{
    BotSpec, CodingAgent, DropPlacement, PaneLayout, SessionSnapshot, SplitAxis, Tab, Workspace,
};
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

    fn threads_dir(&self, bot_id: Uuid) -> PathBuf {
        self.root
            .join("state/bots")
            .join(bot_id.to_string())
            .join("threads")
    }

    /// Writes a saved omp session with the current title-slot layout.
    fn save_thread(&self, bot_id: Uuid, id: &str, title: &str) {
        let line = |value: serde_json::Value| format!("{value}\n");
        let contents =
            line(serde_json::json!({"type": "title", "v": 1, "title": title, "pad": "  "}))
                + &line(serde_json::json!({"type": "session", "version": 3, "id": id, "cwd": "/"}));
        std::fs::write(
            self.threads_dir(bot_id)
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

fn find_tab(snapshot: &SessionSnapshot, tab_id: Uuid) -> &Tab {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find(|tab| tab.id == tab_id)
        .unwrap()
}

fn bot_workspace(snapshot: &SessionSnapshot, bot_id: Uuid) -> &Workspace {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == bot_id)
        .unwrap()
}

/// The bot's thread tabs, each with its panes in layout order.
fn thread_tabs(registry: &SessionRegistry, bot_id: Uuid) -> Vec<(Uuid, Vec<Uuid>)> {
    bot_workspace(&registry.snapshot().unwrap(), bot_id)
        .tabs
        .iter()
        .map(|tab| {
            let mut panes = Vec::new();
            collect_pane_ids(&tab.layout, &mut panes);
            (tab.id, panes)
        })
        .collect()
}

/// The bot's live thread panes in tab order and its active one.
fn live(registry: &SessionRegistry, bot_id: Uuid) -> (Vec<Uuid>, Option<Uuid>) {
    let target = registry.state.read().bot_target(bot_id).unwrap();
    (target.panes, target.active_pane)
}

fn spec(registry: &SessionRegistry, bot_id: Uuid) -> BotSpec {
    bot_workspace(&registry.snapshot().unwrap(), bot_id)
        .bot
        .clone()
        .unwrap()
}

fn target(panes: &[(Uuid, Option<&str>)], active: Uuid, pinned: &[&str]) -> BotTarget {
    BotTarget {
        active_pane: Some(active),
        panes: panes.iter().map(|(pane_id, _)| *pane_id).collect(),
        tab_by_pane: panes
            .iter()
            .enumerate()
            .map(|(index, (pane_id, _))| (*pane_id, Uuid::from_u128(index as u128 + 1)))
            .collect(),
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
    for thread in &threads {
        assert_eq!(
            thread.tab_id,
            thread.pane_id.map(|pane_id| target.tab_by_pane[&pane_id]),
            "live threads name their tab, saved ones none"
        );
    }
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
        select_evictions(&threads, Some(panes[5])),
        [panes[4], panes[6], panes[7]]
    );
    assert!(select_evictions(&threads[..5], Some(panes[0])).is_empty());
    let all_busy = threads[..6]
        .iter()
        .map(|thread| LiveThread {
            status: PaneStatus::Working,
            ..*thread
        })
        .collect::<Vec<_>>();
    assert!(select_evictions(&all_busy, Some(panes[0])).is_empty());
}

#[test]
fn opening_threads_focuses_live_ones_and_resumes_saved_ones_in_new_tabs() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, first_tab, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace = bot_workspace(&snapshot, bot_id);
    assert!(workspace.is_bot());
    assert_eq!(workspace.title, "Hive3");
    assert_eq!(workspace.tabs[0].title, "New thread");
    let threads_dir = fixture.threads_dir(bot_id);
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
    let threads = registry.list_bot_threads(bot_id).unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, pane_thread_id(first));
    assert_eq!(threads[0].pane_id, Some(first));
    assert_eq!(threads[0].tab_id, Some(first_tab));

    registry.report_bot_session(first, "s-first").unwrap();
    fixture.save_thread(bot_id, "s-saved", "Fix login");
    let threads = registry.list_bot_threads(bot_id).unwrap();
    assert_eq!(
        threads
            .iter()
            .map(|thread| (thread.id.as_str(), thread.pane_id))
            .collect::<Vec<_>>(),
        [("s-first", Some(first)), ("s-saved", None)],
        "an unsaved live session is newest"
    );

    thread::sleep(Duration::from_millis(5));
    let (resumed_tab, resumed) = registry.open_bot_thread(bot_id, Some("s-saved")).unwrap();
    assert_ne!(resumed, first);
    assert_eq!(
        thread_tabs(registry, bot_id),
        [(first_tab, vec![first]), (resumed_tab, vec![resumed])],
        "a saved thread reopens as a new tab"
    );
    assert_eq!(live(registry, bot_id).1, Some(resumed));
    wait_for_screen(
        registry,
        resumed,
        &format!("--session-dir {} --resume s-saved", threads_dir.display()),
    );
    let snapshot = registry.snapshot().unwrap();
    let pane = find_pane_in_snapshot(&snapshot, resumed).unwrap();
    assert_eq!(pane.profile_override, Some(TerminalProfile::Omp));
    assert_eq!(
        spec(registry, bot_id).thread_panes[&resumed]
            .session
            .as_deref(),
        Some("s-saved")
    );

    thread::sleep(Duration::from_millis(5));
    assert_eq!(
        registry.open_bot_thread(bot_id, Some("s-first")).unwrap(),
        (first_tab, first)
    );
    assert_eq!(
        live(registry, bot_id),
        (vec![first, resumed], Some(first)),
        "a live thread is focused without a new tab"
    );
    assert_eq!(
        registry.open_bot_thread(bot_id, Some("s-saved")).unwrap(),
        (resumed_tab, resumed)
    );

    thread::sleep(Duration::from_millis(5));
    let (fresh_tab, fresh) = registry.open_bot_thread(bot_id, None).unwrap();
    assert_eq!(thread_tabs(registry, bot_id).len(), 3);
    assert_eq!(live(registry, bot_id).1, Some(fresh));
    let text = wait_for_screen(registry, fresh, "OMP_UP");
    assert!(!text.contains("--resume"), "{text}");
    thread::sleep(Duration::from_millis(5));
    registry.open_bot_thread(bot_id, Some("s-first")).unwrap();
    thread::sleep(Duration::from_millis(5));
    assert_eq!(
        registry
            .open_bot_thread(bot_id, Some(&pane_thread_id(fresh)))
            .unwrap(),
        (fresh_tab, fresh)
    );
    assert_eq!(live(registry, bot_id).1, Some(fresh));

    for missing in [
        "s-missing",
        "pane:00000000-0000-0000-0000-000000000000",
        "../x",
    ] {
        assert!(registry.open_bot_thread(bot_id, Some(missing)).is_err());
    }
    assert_eq!(live(registry, bot_id).0.len(), 3);
}

#[test]
fn thread_panes_split_side_by_side_stay_threads_but_generic_creation_is_refused() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, first_tab, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    registry.report_bot_session(first, "s-first").unwrap();
    let (_, second) = registry.open_bot_thread(bot_id, None).unwrap();

    registry
        .move_pane_to_split(second, first, DropPlacement::Right)
        .unwrap();
    assert_eq!(
        thread_tabs(registry, bot_id),
        [(first_tab, vec![first, second])],
        "the emptied tab is gone and both threads share one tab"
    );
    let snapshot = registry.snapshot().unwrap();
    assert!(matches!(
        &bot_workspace(&snapshot, bot_id).tabs[0].layout,
        PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ..
        }
    ));
    let threads = registry.list_bot_threads(bot_id).unwrap();
    let mut mapped = threads
        .iter()
        .map(|thread| (thread.pane_id, thread.tab_id))
        .collect::<Vec<_>>();
    mapped.sort();
    let mut expected = vec![
        (Some(first), Some(first_tab)),
        (Some(second), Some(first_tab)),
    ];
    expected.sort();
    assert_eq!(mapped, expected);
    assert_eq!(spec(registry, bot_id).thread_panes.len(), 2);

    // Moving a thread back out into its own tab works too.
    registry
        .move_pane_to_new_tab(second, first_tab, true, None)
        .unwrap();
    assert_eq!(thread_tabs(registry, bot_id).len(), 2);

    let workstation = registry.snapshot().unwrap().workspaces[0].id;
    let plain = crate::layout::first_pane_id(&registry.snapshot().unwrap()).unwrap();
    assert!(registry.create_pane(first, SplitAxis::Vertical).is_err());
    assert!(registry.create_group_terminal(first).is_err());
    assert!(registry.create_group_browser(first, None).is_err());
    assert!(registry.create_workspace_tab(bot_id).is_err());
    assert!(registry.create_browser_tab(bot_id, None).is_err());
    assert!(
        registry
            .move_pane_to_split(first, first, DropPlacement::Left)
            .is_err(),
        "a self-drop would spawn a plain shell in the bot"
    );
    assert!(
        registry
            .move_pane_to_split(plain, first, DropPlacement::Left)
            .is_err(),
        "panes never cross between a bot and a workstation"
    );
    assert!(
        registry
            .move_pane_to_group(first, registry.snapshot().unwrap().workspaces[0].tabs[0].id)
            .is_err()
    );
    assert_eq!(live(registry, bot_id).0.len(), 2);
    assert_eq!(
        crate::registry::bots::workstation_count(&registry.snapshot().unwrap()),
        1
    );
    registry.create_workspace_tab(workstation).unwrap();
}

#[test]
fn opening_more_than_five_threads_closes_the_least_recent_idle_one() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, first_tab, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    let mut opened = vec![first];
    for _ in 0..MAX_LIVE_THREADS {
        // Distinct activation times make the eviction order deterministic.
        thread::sleep(Duration::from_millis(5));
        opened.push(registry.open_bot_thread(bot_id, None).unwrap().1);
    }
    let (panes, active) = live(registry, bot_id);
    assert_eq!(active, opened.last().copied());
    assert_eq!(panes, opened[1..]);
    assert!(
        registry.pane(first).is_err(),
        "the evicted pane's process ended"
    );
    assert!(
        thread_tabs(registry, bot_id)
            .iter()
            .all(|(tab_id, _)| *tab_id != first_tab),
        "the evicted thread's emptied tab is removed"
    );
    assert!(!spec(registry, bot_id).thread_panes.contains_key(&first));
}

#[test]
fn reported_sessions_follow_the_pane_and_survive_a_fresh_shell() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, _, pane_id) = registry
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
        spec(registry, bot_id).thread_panes[&pane_id]
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

    registry.restart_bot(bot_id).unwrap();
    wait_for_screen(registry, pane_id, "--resume s2");
}

#[test]
fn pins_persist_and_only_saved_threads_can_be_pinned() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, _, pane_id) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    fixture.save_thread(bot_id, "s-old", "Old");
    registry
        .set_bot_thread_pinned(bot_id, "s-old", true)
        .unwrap();
    registry
        .set_bot_thread_pinned(bot_id, "s-old", true)
        .unwrap();
    assert_eq!(spec(registry, bot_id).pinned_threads, ["s-old"]);
    let threads = registry.list_bot_threads(bot_id).unwrap();
    assert_eq!(threads[0].id, "s-old");
    assert!(threads[0].pinned);
    assert!(
        registry
            .set_bot_thread_pinned(bot_id, &pane_thread_id(pane_id), true)
            .is_err()
    );
    registry
        .set_bot_thread_pinned(bot_id, "s-old", false)
        .unwrap();
    assert!(spec(registry, bot_id).pinned_threads.is_empty());
}

#[test]
fn workers_record_the_thread_pane_that_opened_them() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot, _, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    let (_, second) = registry.open_bot_thread(bot, None).unwrap();
    let (_, worker, _) = registry
        .create_worker(None, None, None, None, Some(second))
        .unwrap();
    let plain = crate::layout::first_pane_id(&registry.snapshot().unwrap()).unwrap();
    let workstation = registry.snapshot().unwrap().workspaces[0].id;
    let (_, unowned, _) = registry
        .create_worker(Some(workstation), None, None, None, Some(plain))
        .unwrap();
    let snapshot = registry.snapshot().unwrap();
    assert_eq!(find_tab(&snapshot, worker).owner_bot, Some(bot));
    assert_eq!(find_tab(&snapshot, worker).owner_thread, Some(second));
    assert_eq!(find_tab(&snapshot, unowned).owner_thread, None);
    assert_ne!(first, second);
}

#[test]
fn switching_a_bot_to_another_agent_keeps_only_its_active_thread() {
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
    let (bot_id, _, first) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Omp, None, None)
        .unwrap();
    registry.report_bot_session(first, "s1").unwrap();
    thread::sleep(Duration::from_millis(5));
    let (second_tab, second) = registry.open_bot_thread(bot_id, None).unwrap();

    registry
        .set_bot_agent(bot_id, TerminalProfile::Hermes)
        .unwrap();

    assert_eq!(thread_tabs(registry, bot_id), [(second_tab, vec![second])]);
    wait_for_screen(registry, second, "HERMES_UP");
    assert!(registry.list_bot_threads(bot_id).unwrap().is_empty());
    let spec = spec(registry, bot_id);
    assert_eq!(spec.thread_panes.len(), 1);
    assert_eq!(spec.thread_panes[&second].session, None);
    assert!(
        registry
            .open_bot_thread(bot_id, Some("s1"))
            .unwrap_err()
            .to_string()
            .contains("only omp bots keep threads")
    );
    assert!(Path::new(&fixture.threads_dir(bot_id)).is_dir());
}
