use super::*;

fn test_directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hh-{label}-{}", Uuid::new_v4()))
}

fn create_owner_only_directory(path: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}

fn cwd_map(snapshot: &SessionSnapshot) -> HashMap<Uuid, PathBuf> {
    let pane_id = match &snapshot.workspaces[0].tabs[0].layout {
        PaneLayout::Leaf { pane } => pane.id,
        _ => panic!("seeded snapshot should contain one leaf"),
    };
    HashMap::from([(pane_id, std::env::temp_dir())])
}

#[test]
fn snapshot_contains_only_explicit_safe_desired_state() {
    let directory = test_directory("safe-schema");
    let path = directory.join("sessions.json");
    let store = SnapshotStore::new(path.clone());
    let snapshot = SessionSnapshot::seeded();
    store.save(&snapshot, &cwd_map(&snapshot)).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("local_cwd"));
    for forbidden in [
        "terminal_output",
        "identity",
        "identity_source",
        "environment",
        "process_id",
        "socket",
        "credential",
        "secret",
        "shell",
    ] {
        assert!(
            !text.contains(forbidden),
            "persisted forbidden field {forbidden}"
        );
    }
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn deliberately_empty_local_workspace_round_trips_without_creating_a_terminal() {
    let directory = test_directory("empty-local");
    let store = SnapshotStore::new(directory.join("sessions.json"));
    let mut snapshot = SessionSnapshot::seeded();
    snapshot.workspaces[0].tabs.clear();
    snapshot.workspaces[0].active_terminal_count = 0;

    store.save(&snapshot, &HashMap::new()).unwrap();
    let recovered = store.load().unwrap();

    assert_eq!(recovered.snapshot.workspaces.len(), 1);
    assert!(recovered.snapshot.workspaces[0].tabs.is_empty());
    assert!(recovered.cwd_by_pane.is_empty());
    assert!(recovered.offline_panes.is_empty());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn bot_workspaces_and_owners_round_trip() {
    let directory = test_directory("bots");
    let store = SnapshotStore::new(directory.join("sessions.json"));
    let mut snapshot = SessionSnapshot::seeded();
    snapshot.bots.default_agent = Some(TerminalProfile::Claude);
    let bot_id = Uuid::new_v4();
    let mut bot_pane = crate::layout::pane_fixture(Uuid::new_v4());
    bot_pane.profile_override = Some(TerminalProfile::Omp);
    let live_thread = hh_protocol::BotThreadPane {
        session: Some("0193-live".to_owned()),
        activated_ms: 42,
    };
    let spec = BotSpec {
        pinned_threads: vec!["0193-pinned".to_owned()],
        thread_panes: [
            (bot_pane.id, live_thread.clone()),
            (Uuid::new_v4(), hh_protocol::BotThreadPane::default()),
        ]
        .into(),
        agent: TerminalProfile::Omp,
        instructions: Some("Prefer small PRs".to_owned()),
        home: Some("/tmp".to_owned()),
    };
    snapshot.workspaces[0].owner_bot = Some(bot_id);
    snapshot.workspaces[0].tabs[0].owner_bot = Some(bot_id);
    snapshot.workspaces[0].tabs[0].owner_thread = Some(bot_pane.id);
    let mut bot = snapshot.workspaces[0].clone();
    bot.id = bot_id;
    bot.title = "Hive3".to_owned();
    bot.kind = WorkspaceKind::Bot;
    bot.bot = Some(spec.clone());
    bot.owner_bot = None;
    bot.working_dir = Some("/tmp".to_owned());
    bot.tabs = vec![Tab {
        owner_thread: None,
        id: Uuid::new_v4(),
        title: "New thread".to_owned(),
        custom_title: None,
        project_dir: None,
        color: None,
        custom_icon: None,
        parent_tab: None,
        pinned: false,
        owner_bot: None,
        layout: PaneLayout::Leaf {
            pane: bot_pane.clone(),
        },
    }];
    snapshot.workspaces.push(bot);
    let mut cwd_by_pane = cwd_map(&snapshot);
    cwd_by_pane.insert(bot_pane.id, std::env::temp_dir());

    store.save(&snapshot, &cwd_by_pane).unwrap();
    let recovered = store.load().unwrap().snapshot;

    assert_eq!(recovered.bots, snapshot.bots);
    assert_eq!(recovered.workspaces[0].owner_bot, Some(bot_id));
    assert_eq!(recovered.workspaces[0].tabs[0].owner_bot, Some(bot_id));
    assert_eq!(
        recovered.workspaces[0].tabs[0].owner_thread,
        Some(bot_pane.id)
    );
    assert_eq!(recovered.workspaces[0].bot, None);
    let bot = &recovered.workspaces[1];
    assert!(bot.is_bot());
    assert_eq!(bot.id, bot_id);
    assert_eq!(bot.title, "Hive3");
    let mut live_spec = spec.clone();
    live_spec.thread_panes = [(bot_pane.id, live_thread)].into();
    assert_eq!(
        bot.bot.as_ref(),
        Some(&live_spec),
        "threads of panes that are gone are dropped"
    );
    assert_eq!(bot.working_dir.as_deref(), Some("/tmp"));
    let PaneLayout::Leaf { pane } = &bot.tabs[0].layout else {
        panic!("thread tab is not a leaf");
    };
    assert_eq!(pane.profile_override, Some(TerminalProfile::Omp));
    fs::remove_dir_all(directory).unwrap();
}

/// A schema-14 snapshot as the previous release wrote it: one shared Bots
/// workspace whose bot tabs hold their thread panes in a stack.
fn schema_v14_bots_snapshot(ids: &[Uuid; 10], cwd: &str) -> String {
    let ids = ids.map(|id| id.to_string());
    let [
        workstation,
        worker_tab,
        worker_pane,
        bots,
        hive,
        first,
        second,
        nova,
        nova_pane,
        other,
    ] = &ids;
    serde_json::json!({
        "schema_version": 14,
        "revision": 41,
        "appearance": {},
        "bots": {"default_agent": "omp"},
        "workspaces": [
            {
                "id": workstation, "title": "Workstation 1", "order": 1,
                "kind": "workstation", "owner_bot": hive,
                "tabs": [{
                    "id": worker_tab, "title": "api-fix", "custom_title": "api-fix",
                    "owner_bot": hive, "owner_thread": second,
                    "layout": {"kind": "leaf", "pane": {
                        "id": worker_pane, "kind": {"type": "terminal"},
                        "title": "Terminal", "local_cwd": cwd,
                    }},
                }],
            },
            {
                "id": bots, "title": "Bots", "order": 2, "kind": "bots",
                "tabs": [
                    {
                        "id": hive, "title": "Hive3", "custom_title": "Hive 3",
                        "project_dir": "/srv/app",
                        "bot": {
                            "agent": "omp", "instructions": "Prefer small PRs",
                            "home": "/tmp", "pinned_threads": ["0193-pinned"],
                            "thread_panes": {
                                first: {"session": "0193-first", "activated_ms": 7},
                                second: {"session": null, "activated_ms": 9},
                            },
                        },
                        "layout": {"kind": "stack", "active": second, "panes": [
                            {
                                "id": first, "kind": {"type": "terminal"},
                                "title": "Hive 3", "custom_title": "Hive 3",
                                "profile_override": "omp", "local_cwd": cwd,
                                "tmux_window": "@3", "tmux_pane": "%4",
                            },
                            {
                                "id": second, "kind": {"type": "terminal"},
                                "title": "Hive 3", "custom_title": "Hive 3",
                                "profile_override": "omp", "local_cwd": cwd,
                            },
                        ]},
                    },
                    {
                        "id": nova, "title": "Hermes",
                        "bot": {"agent": "hermes"},
                        "layout": {"kind": "leaf", "pane": {
                            "id": nova_pane, "kind": {"type": "terminal"},
                            "title": "Hermes", "custom_title": "Hermes",
                            "profile_override": "hermes", "local_cwd": cwd,
                        }},
                    },
                ],
            },
            {
                "id": other, "title": "Workstation 2", "order": 3,
                "kind": "workstation", "tabs": [],
            },
        ],
    })
    .to_string()
}

#[test]
fn schema_v14_shared_bots_workspace_splits_into_one_workspace_per_bot() {
    let directory = test_directory("legacy-bots");
    create_owner_only_directory(&directory);
    let path = directory.join("sessions.json");
    let ids: [Uuid; 10] = std::array::from_fn(|_| Uuid::new_v4());
    let [
        workstation,
        worker_tab,
        _,
        bots,
        hive,
        first,
        second,
        nova,
        nova_pane,
        _,
    ] = ids;
    let cwd = std::env::temp_dir();
    fs::write(&path, schema_v14_bots_snapshot(&ids, cwd.to_str().unwrap())).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let store = SnapshotStore::new(path.clone());

    let recovered = store
        .load_or_quarantine()
        .unwrap()
        .expect("a valid schema-14 snapshot is migrated, never quarantined");

    assert_eq!(
        fs::read_dir(&directory).unwrap().count(),
        1,
        "nothing was quarantined"
    );
    let snapshot = &recovered.snapshot;
    assert_eq!(snapshot.bots.default_agent, Some(TerminalProfile::Omp));
    assert!(
        snapshot
            .workspaces
            .iter()
            .all(|workspace| workspace.id != bots)
    );
    let kinds = snapshot
        .workspaces
        .iter()
        .map(|workspace| (workspace.id, workspace.kind))
        .collect::<Vec<_>>();
    assert_eq!(kinds[0], (workstation, WorkspaceKind::Workstation));
    assert_eq!(kinds[2], (hive, WorkspaceKind::Bot));
    assert_eq!(kinds[3], (nova, WorkspaceKind::Bot));
    assert_eq!(
        snapshot.workspaces[0].owner_bot,
        Some(hive),
        "owner references keep pointing at the bot, now its workspace"
    );
    let worker = &snapshot.workspaces[0].tabs[0];
    assert_eq!(worker.id, worker_tab);
    assert_eq!(
        (worker.owner_bot, worker.owner_thread),
        (Some(hive), Some(second))
    );

    let hive_workspace = &snapshot.workspaces[2];
    assert_eq!(
        hive_workspace.title, "Hive 3",
        "the rename becomes the name"
    );
    assert_eq!(hive_workspace.working_dir.as_deref(), Some("/srv/app"));
    assert!(!hive_workspace.pinned);
    assert!(
        hive_workspace.order > snapshot.workspaces[1].order,
        "migrated bots follow the existing workspaces"
    );
    let spec = hive_workspace.bot.as_ref().unwrap();
    assert_eq!(spec.agent, TerminalProfile::Omp);
    assert_eq!(spec.instructions.as_deref(), Some("Prefer small PRs"));
    assert_eq!(spec.home.as_deref(), Some("/tmp"));
    assert_eq!(spec.pinned_threads, ["0193-pinned"]);
    assert_eq!(
        spec.thread_panes[&first].session.as_deref(),
        Some("0193-first")
    );
    assert_eq!(spec.thread_panes[&second].activated_ms, 9);
    let thread_tabs = hive_workspace
        .tabs
        .iter()
        .map(|tab| match &tab.layout {
            PaneLayout::Leaf { pane } => (tab.title.as_str(), pane.id, pane.custom_title.clone()),
            _ => panic!("every stacked thread becomes its own single-pane tab"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        thread_tabs,
        [("New thread", first, None), ("New thread", second, None)]
    );
    assert!(
        hive_workspace
            .tabs
            .iter()
            .all(|tab| tab.id != hive && tab.id != first && tab.id != second)
    );
    assert_eq!(
        recovered.tmux_by_pane.get(&first),
        Some(&("@3".to_owned(), "%4".to_owned()))
    );
    assert_eq!(
        recovered.legacy_tmux_workspace,
        HashMap::from([(first, bots)]),
        "the migrated tmux window is adopted from the retired Bots session"
    );

    let nova_workspace = &snapshot.workspaces[3];
    assert_eq!(nova_workspace.title, "Hermes");
    assert_eq!(nova_workspace.tabs.len(), 1);
    assert_eq!(
        nova_workspace.bot.as_ref().unwrap().agent,
        TerminalProfile::Hermes
    );
    let PaneLayout::Leaf { pane } = &nova_workspace.tabs[0].layout else {
        panic!("expected leaf");
    };
    assert_eq!(pane.id, nova_pane);

    // The migrated state writes back as schema 15 and loads unchanged.
    let bytes = SnapshotStore::encode_with_offline(
        snapshot,
        &recovered.cwd_by_pane,
        &recovered.tmux_by_pane,
        &recovered.offline_panes,
    )
    .unwrap();
    let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(written["schema_version"], 15);
    assert_eq!(written["workspaces"][2]["kind"], "bot");
    assert!(written["workspaces"][2]["tabs"][0].get("bot").is_none());
    store.write_snapshot(&bytes).unwrap();
    let reloaded = store.load().unwrap();
    assert!(reloaded.legacy_tmux_workspace.is_empty());
    assert_eq!(
        reloaded.snapshot.workspaces[2].tabs,
        snapshot.workspaces[2].tabs
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn schema_v13_assistant_workspaces_panes_and_settings_load_without_them() {
    let directory = test_directory("legacy-assistant");
    create_owner_only_directory(&directory);
    let path = directory.join("sessions.json");
    let pane = |id: &str, kind: &str| serde_json::json!({"id": id, "kind": {"type": kind}, "title": "Pane", "local_cwd": if kind == "terminal" { Some("/tmp") } else { None }});
    let legacy = serde_json::json!({
        "schema_version": 13,
        "revision": 4,
        "assistant": {"access": "confirm", "model": "openai/gpt-5", "preferred_agent": "omp"},
        "workspaces": [
            {
                "id": "00000000-0000-0000-0000-000000000041",
                "title": "Workstation 1",
                "tabs": [
                    {
                        "id": "00000000-0000-0000-0000-000000000042",
                        "title": "Split",
                        "layout": {
                            "kind": "split", "axis": "horizontal", "ratio": 0.5,
                            "first": {"kind": "leaf", "pane": pane("00000000-0000-0000-0000-000000000043", "terminal")},
                            "second": {"kind": "leaf", "pane": pane("00000000-0000-0000-0000-000000000044", "assistant")}
                        }
                    },
                    {
                        "id": "00000000-0000-0000-0000-000000000045",
                        "title": "Group",
                        "layout": {
                            "kind": "stack", "active": "00000000-0000-0000-0000-000000000047",
                            "panes": [
                                pane("00000000-0000-0000-0000-000000000046", "terminal"),
                                pane("00000000-0000-0000-0000-000000000047", "assistant"),
                                pane("00000000-0000-0000-0000-000000000048", "terminal")
                            ]
                        }
                    },
                    {
                        "id": "00000000-0000-0000-0000-000000000049",
                        "title": "Assistant",
                        "layout": {"kind": "leaf", "pane": pane("00000000-0000-0000-0000-00000000004a", "assistant")}
                    }
                ]
            },
            {
                "id": "00000000-0000-0000-0000-00000000004b",
                "title": "Research",
                "kind": "assistant",
                "instructions": "Answer tersely",
                "tabs": [{
                    "id": "00000000-0000-0000-0000-00000000004c",
                    "title": "Thread 1",
                    "layout": {"kind": "leaf", "pane": pane("00000000-0000-0000-0000-00000000004d", "assistant")}
                }]
            }
        ]
    });
    fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let recovered = SnapshotStore::new(path.clone())
        .load_or_quarantine()
        .unwrap()
        .expect("legacy snapshot loads instead of being quarantined");

    let id = |suffix: &str| {
        Uuid::parse_str(&format!("00000000-0000-0000-0000-0000000000{suffix}")).unwrap()
    };
    let workspaces = &recovered.snapshot.workspaces;
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].tabs.len(), 2);
    let PaneLayout::Leaf { pane } = &workspaces[0].tabs[0].layout else {
        panic!("the split collapses to its remaining terminal");
    };
    assert_eq!(pane.id, id("43"));
    let PaneLayout::Stack { panes, active } = &workspaces[0].tabs[1].layout else {
        panic!("the group keeps its two terminals");
    };
    assert_eq!(
        panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
        [id("46"), id("48")]
    );
    assert_eq!(*active, id("46"));
    assert_eq!(recovered.snapshot.bots, BotSettings::default());
    assert!(path.exists());
    assert_eq!(
        fs::read_dir(&directory).unwrap().count(),
        1,
        "nothing was quarantined"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn schema_v13_snapshot_holding_only_an_assistant_workspace_gains_an_empty_workstation() {
    let mut desired: DesiredState = serde_json::from_value(serde_json::json!({
        "schema_version": 13,
        "revision": 1,
        "workspaces": [{
            "id": "00000000-0000-0000-0000-000000000051",
            "title": "Assistant 1",
            "kind": "assistant",
            "tabs": [{
                "id": "00000000-0000-0000-0000-000000000052",
                "title": "Thread 1",
                "layout": {"kind": "leaf", "pane": {"id": "00000000-0000-0000-0000-000000000053", "kind": {"type": "assistant"}, "title": "Assistant", "local_cwd": null}}
            }]
        }]
    }))
    .unwrap();
    desired.drop_legacy_assistants();
    desired.validate().unwrap();
    let workspaces = desired.into_runtime().snapshot.workspaces;
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].kind, WorkspaceKind::Workstation);
    assert!(workspaces[0].tabs.is_empty());
}

#[test]
fn ssh_workspace_layout_recovers_offline_without_runtime_or_secret_material() {
    let directory = test_directory("ssh-layout");
    let path = directory.join("sessions.json");
    let store = SnapshotStore::new(path.clone());
    let mut snapshot = SessionSnapshot::seeded();
    let workspace = &mut snapshot.workspaces[0];
    let first = match &workspace.tabs[0].layout {
        PaneLayout::Leaf { pane } => pane.clone(),
        _ => panic!("seeded snapshot should contain one leaf"),
    };
    let second = Pane {
        id: Uuid::new_v4(),
        kind: hh_protocol::PaneKind::Terminal,
        title: "Remote two".to_owned(),
        shell: "ssh".to_owned(),
        color: None,
        identity: TerminalIdentity::default(),
        status: hh_protocol::PaneStatus::default(),
        status_changed_at_ms: 0,
        custom_title: None,
        profile_override: None,
        custom_icon: None,
    };
    let first_id = first.id;
    let second_id = second.id;
    workspace.title = "Tailnet build".to_owned();
    workspace.pinned = true;
    workspace.pin_order = 1;
    workspace.connection = WorkspaceConnection::SystemSsh {
        destination: "admin@build-node".to_owned(),
        status: WorkspaceConnectionStatus::Connected,
    };
    workspace.tabs[0].layout = PaneLayout::Split {
        axis: SplitAxis::Horizontal,
        ratio: 0.4,
        first: Box::new(PaneLayout::Leaf { pane: first }),
        second: Box::new(PaneLayout::Leaf {
            pane: second.clone(),
        }),
    };

    store.save(&snapshot, &HashMap::new()).unwrap();
    let recovered = store.load_or_quarantine().unwrap().unwrap();
    let recovered_workspace = &recovered.snapshot.workspaces[0];

    assert_eq!(recovered_workspace.title, "Tailnet build");
    assert!(recovered_workspace.pinned);
    assert_eq!(recovered_workspace.pin_order, 1);
    let PaneLayout::Split {
        axis,
        ratio,
        first,
        second,
    } = &recovered_workspace.tabs[0].layout
    else {
        panic!("saved SSH layout did not retain its split shape");
    };
    assert_eq!(*axis, SplitAxis::Horizontal);
    assert!((*ratio - 0.4).abs() < f32::EPSILON);
    assert!(matches!(first.as_ref(), PaneLayout::Leaf { pane } if pane.id == first_id));
    assert!(matches!(second.as_ref(), PaneLayout::Leaf { pane } if pane.id == second_id));
    assert_eq!(
        recovered_workspace.connection,
        WorkspaceConnection::SystemSsh {
            destination: "admin@build-node".to_owned(),
            status: WorkspaceConnectionStatus::Offline,
        }
    );
    assert_eq!(recovered.offline_panes.len(), 2);
    assert!(recovered.offline_panes.contains(&second_id));
    let text = fs::read_to_string(path).unwrap();
    for forbidden in ["password", "private_key", "agent_material", "known_hosts"] {
        assert!(!text.contains(forbidden));
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn failed_replace_preserves_last_complete_snapshot() {
    let directory = test_directory("atomic-fault");
    let path = directory.join("sessions.json");
    let store = SnapshotStore::new(path.clone());
    let mut snapshot = SessionSnapshot::seeded();
    let cwd_by_pane = cwd_map(&snapshot);
    store.save(&snapshot, &cwd_by_pane).unwrap();
    let original = fs::read(&path).unwrap();

    snapshot.revision = 42;
    store.inject_failure_before_replace(true);
    assert!(store.save(&snapshot, &cwd_by_pane).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(
        fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        1
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn appearance_defaults_and_overrides_round_trip_with_old_snapshot_fallback() {
    let directory = test_directory("appearance-round-trip");
    let path = directory.join("sessions.json");
    let store = SnapshotStore::new(path);
    let mut snapshot = SessionSnapshot::seeded();
    snapshot.appearance.default_terminal_accent = AppearanceColor::new(0x95, 0xcc, 0x7f);
    snapshot.appearance.default_workspace_color = AppearanceColor::new(0xc9, 0x90, 0xe5);
    snapshot.appearance.recent_colors = vec![AppearanceColor::new(0xef, 0x71, 0x7a)];
    snapshot.workspaces[0].color = Some(AppearanceColor::new(0xe4, 0xbd, 0x72));
    let PaneLayout::Leaf { pane } = &mut snapshot.workspaces[0].tabs[0].layout else {
        panic!("expected leaf");
    };
    pane.color = Some(AppearanceColor::new(0x67, 0xc8, 0xc6));
    pane.title = "Live-detected Claude".to_owned();
    pane.identity = hh_protocol::TerminalIdentity {
        profile: TerminalProfile::Claude,
        source: hh_protocol::TerminalIdentitySource::Command,
    };
    pane.status = hh_protocol::PaneStatus::Working;
    pane.custom_title = Some("Release shell".to_owned());
    pane.profile_override = Some(TerminalProfile::Gemini);
    pane.custom_icon = Some("00000000-0000-4000-8000-000000000001.png".to_owned());

    store.save(&snapshot, &cwd_map(&snapshot)).unwrap();
    let recovered = store.load().unwrap().snapshot;

    assert_eq!(recovered.appearance, snapshot.appearance);
    assert_eq!(recovered.workspaces[0].color, snapshot.workspaces[0].color);
    let PaneLayout::Leaf {
        pane: recovered_pane,
    } = &recovered.workspaces[0].tabs[0].layout
    else {
        panic!("expected recovered leaf");
    };
    assert_eq!(
        recovered_pane.color,
        Some(AppearanceColor::new(0x67, 0xc8, 0xc6))
    );
    assert_eq!(recovered_pane.title, "Release shell");
    assert_eq!(
        recovered_pane.custom_title.as_deref(),
        Some("Release shell")
    );
    assert_eq!(
        recovered_pane.profile_override,
        Some(TerminalProfile::Gemini)
    );
    assert_eq!(
        recovered_pane.custom_icon.as_deref(),
        Some("00000000-0000-4000-8000-000000000001.png")
    );
    assert_eq!(recovered_pane.identity, TerminalIdentity::default());
    assert_eq!(recovered_pane.status, hh_protocol::PaneStatus::Idle);

    let old: DesiredState = serde_json::from_str(
        r#"{
            "schema_version": 1,
            "revision": 1,
            "workspaces": [{
                "id": "00000000-0000-0000-0000-000000000011",
                "title": "Old workspace",
                "tabs": [{
                    "id": "00000000-0000-0000-0000-000000000012",
                    "title": "Terminals",
                    "layout": {
                        "kind": "leaf",
                        "pane": {
                            "id": "00000000-0000-0000-0000-000000000013",
                            "title": "Terminal 1",
                            "local_cwd": "/tmp"
                        }
                    }
                }]
            }]
        }"#,
    )
    .unwrap();
    assert_eq!(old.appearance, AppearanceSettings::default());
    assert_eq!(old.workspaces[0].color, None);
    let old_runtime = old.into_runtime().snapshot;
    let PaneLayout::Leaf { pane: old_pane } = &old_runtime.workspaces[0].tabs[0].layout else {
        panic!("expected old leaf");
    };
    assert_eq!(old_pane.custom_title, None);
    assert_eq!(old_pane.profile_override, None);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn schema_six_harbor_blue_defaults_migrate_to_dark_gray() {
    let snapshot = SessionSnapshot::seeded();
    let mut desired = DesiredState::from_runtime(
        &snapshot,
        &cwd_map(&snapshot),
        &HashMap::new(),
        &HashSet::new(),
    )
    .unwrap();
    desired.schema_version = 6;
    desired.appearance.default_terminal_accent = AppearanceColor::HARBOR_BLUE;
    desired.appearance.default_workspace_color = AppearanceColor::HARBOR_BLUE;

    let recovered = desired.into_runtime().snapshot;

    assert_eq!(
        recovered.appearance.default_terminal_accent,
        AppearanceColor::DARK_GRAY
    );
    assert_eq!(
        recovered.appearance.default_workspace_color,
        AppearanceColor::DARK_GRAY
    );
}

#[test]
fn schema_v1_custom_names_migrate_to_explicit_overrides() {
    let desired: DesiredState = serde_json::from_str(
        r#"{
            "schema_version": 1,
            "revision": 4,
            "workspaces": [{
                "id": "00000000-0000-0000-0000-000000000021",
                "title": "Workspace",
                "tabs": [{
                    "id": "00000000-0000-0000-0000-000000000022",
                    "title": "Terminals",
                    "layout": {
                        "kind": "leaf",
                        "pane": {
                            "id": "00000000-0000-0000-0000-000000000023",
                            "title": "Deploy console",
                            "local_cwd": "/tmp"
                        }
                    }
                }]
            }]
        }"#,
    )
    .unwrap();

    let runtime = desired.into_runtime().snapshot;
    let PaneLayout::Leaf { pane } = &runtime.workspaces[0].tabs[0].layout else {
        panic!("expected leaf");
    };
    assert_eq!(pane.custom_title.as_deref(), Some("Deploy console"));
    assert_eq!(pane.title, "Deploy console");
}

#[test]
fn schema_v4_snapshot_with_retired_tmux_setting_loads_and_stops_being_written() {
    let stored: DesiredState = serde_json::from_str(
        r#"{
            "schema_version": 4,
            "revision": 7,
            "tmux": {"hide_status_bar": true},
            "workspaces": [{
                "id": "00000000-0000-0000-0000-000000000031",
                "title": "Workstation",
                "tabs": [{
                    "id": "00000000-0000-0000-0000-000000000032",
                    "title": "Terminals",
                    "layout": {
                        "kind": "leaf",
                        "pane": {
                            "id": "00000000-0000-0000-0000-000000000033",
                            "title": "Terminal 1",
                            "local_cwd": "/tmp"
                        }
                    }
                }]
            }]
        }"#,
    )
    .unwrap();
    stored.validate().unwrap();

    let recovered = stored.into_runtime();
    assert_eq!(recovered.snapshot.workspaces[0].title, "Workstation");

    let rewritten = DesiredState::from_runtime(
        &recovered.snapshot,
        &recovered.cwd_by_pane,
        &HashMap::new(),
        &HashSet::new(),
    )
    .unwrap();
    let encoded = serde_json::to_string(&rewritten).unwrap();
    assert!(!encoded.contains("tmux"), "encoded: {encoded}");
    assert!(!encoded.contains("hide_status_bar"), "encoded: {encoded}");
    serde_json::from_str::<DesiredState>(&encoded).unwrap();
}

#[test]
fn corrupt_or_unknown_state_is_quarantined() {
    let directory = test_directory("quarantine");
    create_owner_only_directory(&directory);
    let path = directory.join("sessions.json");
    fs::write(
        &path,
        br#"{"schema_version":999,"revision":0,"workspaces":[]}"#,
    )
    .unwrap();
    let store = SnapshotStore::new(path.clone());

    assert!(store.load_or_quarantine().unwrap().is_none());
    assert!(!path.exists());
    assert!(
        fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sessions.corrupt-")
            })
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn symlink_snapshot_is_quarantined_without_following_its_target() {
    use std::os::unix::fs::symlink;

    let directory = test_directory("symlink-quarantine");
    create_owner_only_directory(&directory);
    let target = directory.join("outside-target");
    fs::write(&target, b"do not touch").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    let path = directory.join("sessions.json");
    symlink(&target, &path).unwrap();
    let store = SnapshotStore::new(path.clone());

    assert!(store.load_or_quarantine().unwrap().is_none());
    assert_eq!(fs::read(&target).unwrap(), b"do not touch");
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(!path.exists());
    assert!(
        fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry.file_type().is_ok_and(|kind| kind.is_symlink())
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("sessions.corrupt-")
            })
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_ratio_and_duplicate_ids_are_rejected() {
    let snapshot = SessionSnapshot::seeded();
    let mut desired = DesiredState::from_runtime(
        &snapshot,
        &cwd_map(&snapshot),
        &HashMap::new(),
        &HashSet::new(),
    )
    .unwrap();
    let pane = match &desired.workspaces[0].tabs[0].layout {
        DesiredLayout::Leaf { pane } => pane.clone(),
        _ => panic!("expected leaf"),
    };
    desired.workspaces[0].tabs[0].layout = DesiredLayout::Split {
        axis: SplitAxis::Horizontal,
        ratio: f32::NAN,
        first: Box::new(DesiredLayout::Leaf { pane: pane.clone() }),
        second: Box::new(DesiredLayout::Leaf { pane }),
    };
    assert!(desired.validate().is_err());
}
#[test]
fn managed_tmux_targets_round_trip_and_require_a_complete_pair() {
    let snapshot = SessionSnapshot::seeded();
    let pane_id = crate::layout::first_pane_id(&snapshot).unwrap();
    let tmux_by_pane = HashMap::from([(pane_id, ("@12".to_owned(), "%34".to_owned()))]);
    let desired = DesiredState::from_runtime(
        &snapshot,
        &cwd_map(&snapshot),
        &tmux_by_pane,
        &HashSet::new(),
    )
    .unwrap();
    desired.validate().unwrap();
    let recovered = desired.into_runtime();
    assert_eq!(recovered.tmux_by_pane, tmux_by_pane);

    let mut invalid = DesiredState::from_runtime(
        &snapshot,
        &cwd_map(&snapshot),
        &HashMap::new(),
        &HashSet::new(),
    )
    .unwrap();
    let DesiredLayout::Leaf { pane } = &mut invalid.workspaces[0].tabs[0].layout else {
        panic!("expected leaf");
    };
    pane.tmux_window = Some("@12".to_owned());
    assert!(invalid.validate().is_err());
}

#[test]
fn overlong_bot_instructions_are_rejected() {
    let snapshot = SessionSnapshot::seeded();
    let mut desired = DesiredState::from_runtime(
        &snapshot,
        &cwd_map(&snapshot),
        &HashMap::new(),
        &HashSet::new(),
    )
    .unwrap();
    let mut bot = desired.workspaces[0].clone();
    bot.id = Uuid::new_v4();
    bot.kind = DesiredWorkspaceKind::Bot;
    bot.tabs.clear();
    bot.bot = Some(BotSpec {
        pinned_threads: Vec::new(),
        thread_panes: std::collections::BTreeMap::default(),
        agent: TerminalProfile::Omp,
        instructions: Some("x".repeat(MAX_INSTRUCTIONS_CHARS + 1)),
        home: None,
    });
    desired.workspaces.push(bot);
    assert_eq!(
        desired.validate().unwrap_err().to_string(),
        "bot instructions too long"
    );
}
