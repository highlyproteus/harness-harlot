use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn notification(id: u64) -> SessionNotification {
    SessionNotification {
        id,
        pane_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        kind: hh_protocol::NotificationKind::Message,
        message: None,
        pane_title: "pane".to_owned(),
        workspace_title: "workspace".to_owned(),
        profile: hh_protocol::TerminalProfile::Terminal,
        at_ms: 0,
        read: false,
    }
}

#[test]
fn initial_cursor_skips_the_existing_notification_backlog() {
    assert_eq!(initial_notification_cursor(&[]), 0);
    assert_eq!(
        initial_notification_cursor(&[notification(3), notification(9), notification(5)]),
        9
    );
}

#[test]
fn model_cannot_resolve_approvals() {
    assert!(
        tool_schemas()
            .iter()
            .all(|schema| schema["name"] != "approve_action")
    );
    assert!(classify_tool("approve_action").is_err());
}

#[test]
fn every_terminal_mutation_and_launch_requires_independent_approval() {
    for name in [
        "create_workstation",
        "open_terminal_tab",
        "rename_tab",
        "open_project_tab",
        "create_worktree_tab",
        "launch_agent",
        "send_input",
        "send_keys",
        "close_tab",
        "close_workstation",
    ] {
        assert_eq!(classify_tool(name).unwrap(), TrustTier::T2, "tool={name}");
    }
}

#[test]
fn tool_schemas_require_only_non_optional_parameters() {
    let schemas = tool_schemas();
    let required = |name: &str| {
        schemas
            .iter()
            .find(|schema| schema["name"] == name)
            .unwrap()["parameters"]["required"]
            .clone()
    };
    assert_eq!(required("read_pane"), json!(["pane_id"]));
    assert_eq!(required("create_workstation"), json!(["title"]));
    assert_eq!(required("open_terminal_tab"), json!(["workspace_id"]));
    assert_eq!(required("rename_tab"), json!(["tab_id", "title"]));
    assert_eq!(
        required("create_worktree_tab"),
        json!(["workspace_id", "repo_dir", "branch"])
    );
    assert_eq!(required("send_input"), json!(["pane_id", "text"]));
    assert_eq!(required("list_directory"), json!([]));
    assert_eq!(required("find_directory"), json!(["query"]));
    assert_eq!(required("list_threads"), json!([]));
    assert_eq!(required("read_thread"), json!(["thread_id"]));
}

#[test]
fn snapshot_summary_omits_workspaces_outside_authorized_boundary() {
    let mut snapshot = SessionSnapshot::seeded();
    let authorized_id = snapshot.workspaces[0].id;
    let mut unrelated = snapshot.workspaces[0].clone();
    unrelated.id = Uuid::new_v4();
    unrelated.title = "Unrelated".to_owned();
    snapshot.workspaces.push(unrelated);

    let summary = snapshot_summary(&snapshot, &HashMap::new(), &HashSet::from([authorized_id]));
    assert_eq!(summary["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(summary["workspaces"][0]["id"], authorized_id.to_string());
}

#[test]
fn terminal_workspace_rejects_assistant_workspace() {
    let mut snapshot = SessionSnapshot::seeded();
    let workspace_id = snapshot.workspaces[0].id;
    assert_eq!(
        terminal_workspace(&snapshot, workspace_id).unwrap().id,
        workspace_id
    );

    snapshot.workspaces[0].kind = hh_protocol::WorkspaceKind::Assistant;
    let error = terminal_workspace(&snapshot, workspace_id).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "workspace {workspace_id} is an assistant workspace; choose a kind=workstation target from list_workstations"
        )
    );
}

#[test]
fn thread_access_requires_matching_authorized_workspace() {
    let authorized = Uuid::new_v4();
    let unrelated = Uuid::new_v4();
    let boundary = HashSet::from([authorized]);
    assert!(thread_workspace_is_authorized(Some(authorized), &boundary));
    assert!(!thread_workspace_is_authorized(Some(unrelated), &boundary));
    assert!(!thread_workspace_is_authorized(None, &boundary));
}

#[test]
fn directory_access_stays_within_authorized_root() {
    let root = std::env::temp_dir().join(format!("hh-boundary-{}", Uuid::new_v4()));
    let allowed = root.join("allowed");
    let outside = std::env::temp_dir().join(format!("hh-outside-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    assert_eq!(
        canonical_directory_within(allowed.to_str().unwrap(), &root).unwrap(),
        std::fs::canonicalize(&allowed).unwrap()
    );
    assert!(canonical_directory_within(outside.to_str().unwrap(), &root).is_err());

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[test]
fn canonical_directory_restores_the_filesystem_case() {
    let leaf = format!("HhCase-{}", Uuid::new_v4());
    let directory = std::env::temp_dir().join(&leaf);
    std::fs::create_dir_all(&directory).unwrap();
    let lowercased = directory.parent().unwrap().join(leaf.to_ascii_lowercase());
    if !lowercased.is_dir() {
        std::fs::remove_dir(&directory).unwrap();
        return;
    }

    let canonical = canonical_existing_directory(lowercased.to_str().unwrap()).unwrap();
    assert_eq!(canonical.file_name().unwrap(), leaf.as_str());
    std::fs::remove_dir(&directory).unwrap();
}

#[test]
fn worktree_branch_validation_and_path_are_bounded() {
    let repo = Path::new("/tmp/project");
    assert_eq!(
        worktree_path(repo, "feature/voice").unwrap(),
        Path::new("/tmp/project-worktrees/feature-voice")
    );
    for invalid in ["", "bad branch", "../bad!", &"a".repeat(101)] {
        assert!(
            validate_worktree_branch(invalid).is_err(),
            "branch: {invalid}"
        );
    }
}

#[test]
fn subprocess_stderr_is_drained_without_deadlock_and_bounded() {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("dd if=/dev/zero bs=1024 count=256 >&2; exit 7")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let cancelled = AtomicBool::new(false);
    let (status, stderr) = run_child_with_stderr_timeout(
        &mut command,
        Duration::from_secs(2),
        "stderr stress test",
        &cancelled,
    )
    .unwrap();
    assert_eq!(status.code(), Some(7));
    assert!(stderr.len() <= MAX_SUBPROCESS_STDERR_BYTES);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn long_subprocess_is_killed_promptly_when_tool_work_is_cancelled() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 5")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let result = run_child_with_stderr_timeout(
            &mut command,
            Duration::from_secs(30),
            "cancelled tool",
            &worker_cancelled,
        );
        let _ = done_tx.send(result);
    });

    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    cancelled.store(true, Ordering::Release);
    let error = done_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("cancelled tool did not stop within the responsiveness bound")
        .unwrap_err();

    assert!(error.to_string().contains("cancelled"));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn find_directories_matches_spoken_project_names() {
    let root = std::env::temp_dir().join(format!("hh-find-{}", Uuid::new_v4()));
    let projects = root.join("Projects");
    std::fs::create_dir_all(projects.join("proteusland")).unwrap();
    std::fs::create_dir_all(projects.join("other-app")).unwrap();
    std::fs::create_dir_all(projects.join(".hidden")).unwrap();

    let matches = find_directories(&root, "highly Proteus land");
    assert_eq!(matches.first().cloned(), Some(projects.join("proteusland")));
    assert!(find_directories(&root, "zzzqqq").is_empty());

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn directory_match_score_prefers_whole_and_token_matches() {
    assert!(directory_match_score("proteusland", "highly Proteus land") >= 1);
    assert!(directory_match_score("proteusland", "proteusland") >= 2);
    assert_eq!(directory_match_score("other", "proteus"), 0);
}

#[test]
fn subdirectory_names_skip_hidden_and_files() {
    let root = std::env::temp_dir().join(format!("hh-list-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("Visible")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join("notes.txt"), "x").unwrap();

    let (names, truncated) = subdirectory_names(&root).unwrap();
    assert_eq!(names, vec!["Visible".to_owned()]);
    assert!(!truncated);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn expand_home_rewrites_tilde_prefix() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(expand_home("~/x"), Path::new(&home).join("x"));
    assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
}

#[test]
fn not_found_error_lists_sibling_directories() {
    let root = std::env::temp_dir().join(format!("hh-notfound-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("RealName")).unwrap();

    let error = directory_not_found_error(&root.join("wrongname")).to_string();
    assert!(error.contains("RealName"), "error: {error}");

    std::fs::remove_dir_all(&root).unwrap();
}
