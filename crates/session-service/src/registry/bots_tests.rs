use super::*;
use crate::layout::{find_pane_in_snapshot, first_pane_id};
use crate::registry::create_owner_only_directory;
use hh_protocol::CodingAgent;
use std::os::unix::fs::PermissionsExt as _;

/// A persistent registry in a private temporary state directory whose agent
/// discovery finds fake Hermes and Aider CLIs that print a marker with the bot
/// they were launched for and their working directory. Dropping it removes
/// both directories.
struct Fixture {
    registry: SessionRegistry,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Canonical, so `$PWD` in a shell matches the paths compared below.
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("hh-bot-tests-{}", Uuid::new_v4()));
        let fakes = root.join("agents");
        create_owner_only_directory(&fakes);
        let agents = [
            (TerminalProfile::Hermes, "hermes", "HERMES_UP"),
            (TerminalProfile::Aider, "aider", "AIDER_UP"),
        ]
        .into_iter()
        .map(|(profile, command, marker)| {
            let path = fakes.join(command);
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho \"{marker}:$HH_BOT_ID:$PWD\"\n"),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            CodingAgent {
                profile,
                command: command.to_owned(),
                path: path.to_string_lossy().into_owned(),
            }
        })
        .collect();
        let state = root.join("state");
        create_owner_only_directory(&state);
        let registry = SessionRegistry::persistent(state.join("sessions.json")).unwrap();
        *registry.coding_agents.lock() = Some(agents);
        Self { registry, root }
    }

    /// A fresh directory under the fixture root.
    fn directory(&self, name: &str) -> String {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn default_home(&self, bot_id: Uuid) -> PathBuf {
        self.root.join("state/bots").join(bot_id.to_string())
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

fn wait_for_screen(registry: &SessionRegistry, pane_id: Uuid, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = screen_text(registry, pane_id);
        if text.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{needle} never appeared; screen:\n{text}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn agents_md(home: &Path) -> String {
    std::fs::read_to_string(home.join("AGENTS.md")).unwrap()
}

#[test]
fn a_bot_starts_in_its_home_with_its_agents_md() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let project = fixture.directory("project");
    let (bot_id, tab_id, pane_id) = registry
        .create_bot(
            Some("Hive3"),
            TerminalProfile::Hermes,
            Some(project.clone()),
            Some("Be brief".to_owned()),
        )
        .unwrap();

    let home = fixture.default_home(bot_id);
    wait_for_screen(
        registry,
        pane_id,
        &format!("HERMES_UP:{bot_id}:{}", home.display()),
    );
    assert_eq!(
        std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let context = agents_md(&home);
    assert!(context.contains("Your name is \"Hive3\""));
    assert!(context.contains(&format!(
        "Your project folder: {project}. Workers you open go there by default."
    )));
    assert!(context.ends_with("## Standing instructions from the user\nBe brief\n"));
    let snapshot = registry.snapshot().unwrap();
    let bot = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == bot_id)
        .unwrap();
    assert!(bot.is_bot());
    assert_eq!(bot.title, "Hive3");
    assert_eq!(bot.working_dir.as_deref(), Some(project.as_str()));
    assert_eq!(bot.tabs.len(), 1);
    assert_eq!(bot.tabs[0].id, tab_id);
    assert_eq!(bot.tabs[0].title, "New thread");
    let mut spec = bot.bot.clone().unwrap();
    let thread = spec.thread_panes.remove(&pane_id).unwrap();
    assert_eq!(thread.session, None);
    assert_eq!(
        spec,
        BotSpec {
            agent: TerminalProfile::Hermes,
            instructions: Some("Be brief".to_owned()),
            home: None,
            pinned_threads: Vec::new(),
            thread_panes: std::collections::BTreeMap::default(),
        }
    );
    let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
    assert_eq!(pane.profile_override, Some(TerminalProfile::Hermes));

    let (second_bot, ..) = registry
        .create_bot(None, TerminalProfile::Aider, None, None)
        .unwrap();
    assert_ne!(second_bot, bot_id, "every bot is its own workspace");
    assert!(!agents_md(&fixture.default_home(second_bot)).contains("Your project folder"));
    let snapshot = registry.snapshot().unwrap();
    let second = snapshot
        .workspaces
        .iter()
        .find(|w| w.id == second_bot)
        .unwrap();
    assert_eq!(second.title, TerminalProfile::Aider.display_name());
    assert_eq!(
        crate::registry::bots::workstation_count(&snapshot),
        1,
        "bots are not workstations"
    );
}

#[test]
fn bots_need_a_persistent_state_directory() {
    let registry = SessionRegistry::new().unwrap();
    let error = registry
        .create_bot(Some("Hive3"), TerminalProfile::Hermes, None, None)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "bots need a persistent session state directory"
    );
}

#[test]
fn a_bot_workspace_is_not_a_workstation() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bots_id, ..) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Hermes, None, None)
        .unwrap();
    let workstation = registry.snapshot().unwrap().workspaces[0].id;

    let error = registry.delete_workspace(workstation).unwrap_err();
    assert_eq!(error.to_string(), "the last workstation cannot be deleted");
    for error in [
        registry.create_workspace_tab(bots_id).unwrap_err(),
        registry.scan_tmux_sessions(bots_id).unwrap_err(),
        registry
            .create_worker(Some(bots_id), None, None, None, None)
            .unwrap_err(),
    ] {
        assert_eq!(
            error.to_string(),
            "a bot only holds its threads; open a new thread instead"
        );
    }
    let (created, _) = registry.create_workspace(None).unwrap();
    let snapshot = registry.snapshot().unwrap();
    let created = snapshot
        .workspaces
        .iter()
        .find(|w| w.id == created)
        .unwrap();
    assert_eq!(created.title, "Workstation 2");
}

#[test]
fn set_bot_agent_relaunches_the_same_pane_with_the_new_agent() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, tab_id, pane_id) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Hermes, None, None)
        .unwrap();
    wait_for_screen(registry, pane_id, "HERMES_UP");

    registry
        .set_bot_agent(bot_id, TerminalProfile::Aider)
        .unwrap();

    let home = fixture.default_home(bot_id);
    wait_for_screen(
        registry,
        pane_id,
        &format!("AIDER_UP:{bot_id}:{}", home.display()),
    );
    assert!(!screen_text(registry, pane_id).contains("HERMES_UP"));
    let snapshot = registry.snapshot().unwrap();
    let bot = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == bot_id)
        .unwrap();
    assert_eq!(bot.bot.as_ref().unwrap().agent, TerminalProfile::Aider);
    assert_eq!(bot.tabs.len(), 1);
    assert_eq!(bot.tabs[0].id, tab_id);
    let PaneLayout::Leaf { pane } = &bot.tabs[0].layout else {
        panic!("the thread tab holds one terminal");
    };
    assert_eq!(pane.id, pane_id);
    assert_eq!(pane.profile_override, Some(TerminalProfile::Aider));
}

#[test]
fn restarting_a_renamed_bot_rewrites_its_name() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, _, pane_id) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Hermes, None, None)
        .unwrap();
    wait_for_screen(registry, pane_id, "HERMES_UP");

    registry.rename_workspace(bot_id, "Nova").unwrap();
    registry.restart_bot(bot_id).unwrap();

    let context = agents_md(&fixture.default_home(bot_id));
    assert!(context.contains("Your name is \"Nova\""), "{context}");
    assert!(!context.contains("Hive3"));
}

#[test]
fn a_fresh_shell_for_a_bot_types_its_launch_command_again_in_its_home() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, _, pane_id) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Hermes, None, None)
        .unwrap();
    wait_for_screen(registry, pane_id, "HERMES_UP");
    let session = {
        let mut state = registry.state.write();
        let terminal = state.terminal_pane_mut(pane_id).unwrap();
        terminal.exit_status = Some("Exited with code 0".to_owned());
        terminal.last_valid_cwd = PathBuf::from("/");
        Arc::clone(&terminal.session)
    };
    session.terminate_and_wait().unwrap();

    registry.reattach_pane(pane_id).unwrap();

    assert!(!Arc::ptr_eq(&registry.pane(pane_id).unwrap(), &session));
    wait_for_screen(
        registry,
        pane_id,
        &format!(
            "HERMES_UP:{bot_id}:{}",
            fixture.default_home(bot_id).display()
        ),
    );
}

#[test]
fn workers_from_a_bot_open_in_the_bot_workstation_and_project_folder() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let project = fixture.directory("project");
    let (bot, _, bot_pane) = registry
        .create_bot(
            Some("Hive3"),
            TerminalProfile::Hermes,
            Some(project.clone()),
            None,
        )
        .unwrap();
    let home = fixture.default_home(bot);
    assert!(home.is_dir());

    let (workstation, tab_id, pane_id) = registry
        .create_worker(
            None,
            None,
            Some("api-fix"),
            Some("echo HH_WORKER_$((6 * 7)):$PWD"),
            Some(bot_pane),
        )
        .unwrap();

    wait_for_screen(registry, pane_id, &format!("HH_WORKER_42:{project}"));
    let snapshot = registry.snapshot().unwrap();
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workstation)
        .unwrap();
    assert_eq!(workspace.kind, WorkspaceKind::Workstation);
    assert_eq!(workspace.title, "Hive3");
    assert_eq!(workspace.owner_bot, Some(bot));
    assert_eq!(workspace.working_dir.as_deref(), Some(project.as_str()));
    let tab = workspace.tabs.iter().find(|tab| tab.id == tab_id).unwrap();
    assert_eq!(tab.owner_bot, Some(bot));
    assert_eq!(tab.custom_title.as_deref(), Some("api-fix"));

    let (reused, second_tab, _) = registry
        .create_worker(None, None, None, None, Some(bot_pane))
        .unwrap();
    assert_eq!(reused, workstation);
    let (explicit, explicit_tab, _) = registry
        .create_worker(
            Some(snapshot.workspaces[0].id),
            None,
            None,
            None,
            Some(bot_pane),
        )
        .unwrap();
    assert_eq!(explicit, snapshot.workspaces[0].id);
    let snapshot = registry.snapshot().unwrap();
    let owner_of = |tab_id: Uuid| {
        snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .find(|tab| tab.id == tab_id)
            .unwrap()
            .owner_bot
    };
    assert_eq!(owner_of(second_tab), Some(bot));
    assert_eq!(owner_of(explicit_tab), Some(bot));

    registry.delete_workspace(bot).unwrap();
    let snapshot = registry.snapshot().unwrap();
    assert!(
        snapshot
            .workspaces
            .iter()
            .all(|workspace| workspace.owner_bot.is_none()
                && workspace.tabs.iter().all(|tab| tab.owner_bot.is_none())),
        "a deleted bot leaves no owner references"
    );
    assert!(!home.exists(), "a deleted bot's home is removed");
    assert!(Path::new(&project).is_dir(), "the project folder stays");
}

#[test]
fn workers_from_a_bot_without_a_project_folder_start_in_the_users_home() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (_, _, bot_pane) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Hermes, None, None)
        .unwrap();
    let (workstation, _, pane_id) = registry
        .create_worker(
            None,
            None,
            None,
            Some("echo HH_WORKER:$PWD"),
            Some(bot_pane),
        )
        .unwrap();

    let home = fallback_cwd().unwrap();
    wait_for_screen(registry, pane_id, &format!("HH_WORKER:{}", home.display()));
    let snapshot = registry.snapshot().unwrap();
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workstation)
        .unwrap();
    assert_eq!(workspace.working_dir, None);
}

#[test]
fn a_custom_home_hosts_the_bot_keeps_the_users_agents_md_and_survives_deletion() {
    let fixture = Fixture::new();
    let registry = &fixture.registry;
    let (bot_id, _, pane_id) = registry
        .create_bot(Some("Hive3"), TerminalProfile::Hermes, None, None)
        .unwrap();
    wait_for_screen(registry, pane_id, "HERMES_UP");
    let custom = fixture.directory("repo");
    let own = b"# Repo rules\nUse tabs.\n";
    std::fs::write(Path::new(&custom).join("AGENTS.md"), own).unwrap();

    let error = registry
        .set_bot_home(
            bot_id,
            Some(fixture.root.join("missing").to_string_lossy().into_owned()),
        )
        .unwrap_err();
    assert!(
        error.to_string().ends_with("is not an existing directory"),
        "{error}"
    );
    registry.set_bot_home(bot_id, Some(custom.clone())).unwrap();

    wait_for_screen(registry, pane_id, &format!("HERMES_UP:{bot_id}:{custom}"));
    assert_eq!(
        std::fs::read(Path::new(&custom).join("AGENTS.md")).unwrap(),
        own
    );
    let notifications = registry.notifications().unwrap();
    assert!(
        notifications.iter().any(|notification| notification.pane_id == pane_id
            && notification.message.as_deref()
                == Some(&*format!(
                    "Hive3 did not get its instructions: {custom} already has its own AGENTS.md."
                ))),
        "{notifications:?}"
    );
    let snapshot = registry.snapshot().unwrap();
    let spec = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == bot_id)
        .and_then(|workspace| workspace.bot.clone())
        .unwrap();
    assert_eq!(spec.home.as_deref(), Some(custom.as_str()));

    registry.set_bot_home(bot_id, None).unwrap();
    let home = fixture.default_home(bot_id);
    wait_for_screen(
        registry,
        pane_id,
        &format!("HERMES_UP:{bot_id}:{}", home.display()),
    );
    assert!(agents_md(&home).contains("Your name is \"Hive3\""));

    registry.set_bot_home(bot_id, Some(custom.clone())).unwrap();
    registry.delete_workspace(bot_id).unwrap();
    assert_eq!(
        std::fs::read(Path::new(&custom).join("AGENTS.md")).unwrap(),
        own
    );
    assert!(!home.exists(), "only the default home is removed");
}

#[test]
fn workers_need_a_workstation_unless_a_bot_requests_them() {
    let registry = SessionRegistry::new().unwrap();
    let plain_pane = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    for requester in [None, Some(plain_pane)] {
        let error = registry
            .create_worker(None, None, None, Some("true"), requester)
            .unwrap_err();
        assert!(error.to_string().contains("workspace_id"), "{error}");
    }
}
