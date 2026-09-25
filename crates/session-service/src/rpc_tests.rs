use super::*;

use crate::layout::first_pane_id;

fn init_test_git_repository(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(path.join("tracked"), "test repository\n").unwrap();
    for args in [vec!["add", "tracked"], vec!["commit", "-qm", "initial"]] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "user.name=HH Test"])
            .args(["-c", "user.email=hh-test@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn authority_bound_pane_read_rejects_tab_rebinding_at_read_edge() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let original_tab_id = snapshot.workspaces[0].tabs[0].id;
    let pane_id = first_pane_id(&snapshot).unwrap();
    let target_pane = registry.create_workspace_tab(workspace_id).unwrap();
    registry.move_pane_to_tab(pane_id, target_pane).unwrap();

    let request: ClientRequest = serde_json::from_value(serde_json::json!({
        "type": "get_authorized_pane_snapshot",
        "authority": {
            "workspace_id": workspace_id,
            "tab_id": original_tab_id,
            "pane_id": pane_id,
            "kind": {"type": "terminal"},
            "transport": {"type": "local"}
        }
    }))
    .expect("authority-bound pane read must be part of the wire contract");
    let error = handle_request(&registry, request)
        .expect_err("read must reject a pane rebound to another tab");
    assert!(error.to_string().contains("authority"), "{error:#}");
}

#[test]
fn authority_bound_pane_operations_reject_every_tuple_change_at_service_edge() {
    let registry = SessionRegistry::new().unwrap();
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let tab_id = snapshot.workspaces[0].tabs[0].id;
    let pane_id = first_pane_id(&snapshot).unwrap();
    let authority = hh_protocol::PaneAuthority {
        workspace_id,
        tab_id,
        pane_id,
        kind: hh_protocol::PaneKind::Terminal,
        transport: hh_protocol::TerminalTransport::Local,
    };
    let cases = [
        hh_protocol::PaneAuthority {
            workspace_id: Uuid::new_v4(),
            ..authority.clone()
        },
        hh_protocol::PaneAuthority {
            tab_id: Uuid::new_v4(),
            ..authority.clone()
        },
        hh_protocol::PaneAuthority {
            pane_id: Uuid::new_v4(),
            ..authority.clone()
        },
        hh_protocol::PaneAuthority {
            kind: hh_protocol::PaneKind::Browser {
                url: "https://example.invalid".to_owned(),
            },
            ..authority.clone()
        },
        hh_protocol::PaneAuthority {
            transport: hh_protocol::TerminalTransport::SystemSsh {
                destination: "different.example".to_owned(),
            },
            ..authority
        },
    ];

    for changed in cases {
        let read_error = handle_request(
            &registry,
            ClientRequest::GetAuthorizedPaneSnapshot {
                authority: changed.clone(),
            },
        )
        .expect_err("authority-bound read must reject a changed tuple component");
        assert!(
            read_error.to_string().contains("authority"),
            "{read_error:#}"
        );
        let write_response = handle_request(
            &registry,
            ClientRequest::WriteAuthorizedInput {
                authority: changed,
                bytes: b"must not be written".to_vec(),
            },
        )
        .expect("authority-bound write rejection must be delivery-classified");
        assert!(
            matches!(
                write_response,
                ServiceResponse::DeliveryError {
                    disposition: hh_protocol::DeliveryDisposition::DefinitelyUnsent,
                    ref message,
                } if message.contains("authority")
            ),
            "{write_response:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn authorized_worktree_creation_rejects_replaced_repository_at_service_edge() {
    use std::os::unix::fs::symlink;

    let registry = SessionRegistry::new().unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let root = std::env::temp_dir().join(format!("hh-worktree-root-{}", Uuid::new_v4()));
    let approved = root.join("repo");
    let displaced = root.join("displaced");
    let outside = std::env::temp_dir().join(format!("hh-worktree-outside-{}", Uuid::new_v4()));
    std::fs::create_dir_all(approved.join(".git")).unwrap();
    std::fs::create_dir_all(outside.join(".git")).unwrap();
    std::fs::rename(&approved, &displaced).unwrap();
    symlink(&outside, &approved).unwrap();

    let request: ClientRequest = serde_json::from_value(serde_json::json!({
        "type": "create_authorized_worktree_project",
        "workspace_id": workspace_id,
        "repo_dir": approved,
        "authorized_root": root,
        "branch": "feature/blocked",
        "base": null
    }))
    .expect("authorized worktree request must be part of the wire contract");
    let before = registry.snapshot().unwrap().workspaces[0].tabs.len();
    let error = handle_request(&registry, request)
        .expect_err("service must reject a replaced repository outside the authorized root");
    assert!(error.to_string().contains("authorized root"), "{error:#}");
    assert_eq!(
        registry.snapshot().unwrap().workspaces[0].tabs.len(),
        before
    );

    std::fs::remove_file(&approved).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[cfg(unix)]
#[test]
fn worktree_helper_rejects_repository_substitution_at_mutation_edge() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!("hh-worktree-edge-root-{}", Uuid::new_v4()));
    let approved = root.join("repo");
    let displaced = root.join("displaced");
    let outside = std::env::temp_dir().join(format!("hh-worktree-edge-outside-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&approved).unwrap();
    init_test_git_repository(&outside);
    std::fs::rename(&approved, &displaced).unwrap();
    symlink(&outside, &approved).unwrap();

    let result = create_git_worktree_within(
        approved.to_str().unwrap(),
        root.to_str().unwrap(),
        "feature/substituted",
        None,
    );

    std::fs::remove_dir_all(&root).unwrap();
    std::fs::remove_dir_all(&outside).unwrap();
    let error = result.expect_err("helper must reject a replaced repository at its own edge");
    assert!(error.to_string().contains("authorized root"), "{error:#}");
}

#[cfg(unix)]
#[test]
fn failed_git_worktree_add_removes_partial_app_owned_artifacts() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = std::env::temp_dir().join(format!("hh-worktree-partial-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let parent = root.join("repo-worktrees");
    let target = parent.join("feature-hook-failure");
    init_test_git_repository(&repo);
    let hook = repo.join(".git/hooks/post-checkout");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();

    let result = create_git_worktree_within(
        repo.to_str().unwrap(),
        root.to_str().unwrap(),
        "feature/hook-failure",
        None,
    );
    let source_preserved = repo.join("tracked").exists();
    let target_preserved = target.exists();
    let parent_preserved = parent.exists();
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/feature/hook-failure",
        ])
        .status()
        .unwrap()
        .success();
    std::fs::remove_dir_all(&root).unwrap();

    let error = result.expect_err("failing post-checkout hook must fail worktree creation");
    assert!(
        error.to_string().contains("git worktree add failed"),
        "{error:#}"
    );
    assert!(
        source_preserved,
        "cleanup must preserve the source repository"
    );
    assert!(
        !target_preserved,
        "cleanup must remove the partial worktree"
    );
    assert!(
        !branch,
        "cleanup must remove the partial app-created branch"
    );
    assert!(!parent_preserved, "cleanup must remove its empty parent");
}

#[cfg(unix)]
#[test]
fn authorized_worktree_failure_removes_only_created_artifacts() {
    let registry = SessionRegistry::new().unwrap();
    let root = std::env::temp_dir().join(format!("hh-worktree-cleanup-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let parent = root.join("repo-worktrees");
    let target = parent.join("feature-cleanup");
    init_test_git_repository(&repo);

    let request = ClientRequest::CreateAuthorizedWorktreeProject {
        workspace_id: Uuid::new_v4(),
        repo_dir: repo.to_string_lossy().into_owned(),
        authorized_root: root.to_string_lossy().into_owned(),
        branch: "feature/cleanup".to_owned(),
        base: None,
    };
    let error = handle_request(&registry, request)
        .expect_err("missing workspace must fail after worktree preparation");
    let source_preserved = repo.join("tracked").exists();
    let target_preserved = target.exists();
    let parent_preserved = parent.exists();
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/feature/cleanup",
        ])
        .status()
        .unwrap()
        .success();
    std::fs::remove_dir_all(&root).unwrap();

    assert!(error.to_string().contains("does not exist"), "{error:#}");
    assert!(
        source_preserved,
        "cleanup must preserve the source repository"
    );
    assert!(
        !target_preserved,
        "cleanup must remove the created worktree"
    );
    assert!(!branch, "cleanup must remove the app-created branch");
    assert!(
        !parent_preserved,
        "cleanup must remove the app-created empty parent"
    );
}

#[cfg(unix)]
#[test]
fn authorized_worktree_failure_preserves_preexisting_parent_content() {
    let registry = SessionRegistry::new().unwrap();
    let root = std::env::temp_dir().join(format!("hh-worktree-owned-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let parent = root.join("repo-worktrees");
    let sentinel = parent.join("user-owned");
    init_test_git_repository(&repo);
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(&sentinel, "preserve\n").unwrap();

    let request = ClientRequest::CreateAuthorizedWorktreeProject {
        workspace_id: Uuid::new_v4(),
        repo_dir: repo.to_string_lossy().into_owned(),
        authorized_root: root.to_string_lossy().into_owned(),
        branch: "feature/preserve-parent".to_owned(),
        base: None,
    };
    handle_request(&registry, request)
        .expect_err("missing workspace must fail after worktree preparation");
    let sentinel_preserved = sentinel.exists();
    let target_preserved = parent.join("feature-preserve-parent").exists();
    std::fs::remove_dir_all(&root).unwrap();

    assert!(
        sentinel_preserved,
        "cleanup must preserve user-owned content"
    );
    assert!(
        !target_preserved,
        "cleanup must remove its created worktree"
    );
}

#[cfg(unix)]
#[test]
fn authorized_workspace_creation_rejects_replaced_canonical_directory_at_service_edge() {
    use std::os::unix::fs::symlink;

    let registry = SessionRegistry::new().unwrap();
    let root = std::env::temp_dir().join(format!("hh-workspace-root-{}", Uuid::new_v4()));
    let approved = root.join("approved");
    let displaced = root.join("displaced");
    let outside = std::env::temp_dir().join(format!("hh-workspace-outside-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&approved).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::rename(&approved, &displaced).unwrap();
    symlink(&outside, &approved).unwrap();

    let request: ClientRequest = serde_json::from_value(serde_json::json!({
        "type": "create_authorized_workspace",
        "title": "Rejected",
        "working_dir": approved,
        "authorized_root": root
    }))
    .expect("authorized workspace request must be part of the wire contract");
    let before = registry.snapshot().unwrap().workspaces.len();
    let error = handle_request(&registry, request)
        .expect_err("service must reject a replaced path outside the authorized root");
    assert!(error.to_string().contains("authorized root"), "{error:#}");
    assert_eq!(registry.snapshot().unwrap().workspaces.len(), before);

    std::fs::remove_file(&approved).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[cfg(unix)]
#[test]
fn authorized_project_creation_rejects_replaced_canonical_directory_at_service_edge() {
    use std::os::unix::fs::symlink;

    let registry = SessionRegistry::new().unwrap();
    let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
    let root = std::env::temp_dir().join(format!("hh-service-root-{}", Uuid::new_v4()));
    let approved = root.join("approved");
    let displaced = root.join("displaced");
    let outside = std::env::temp_dir().join(format!("hh-service-outside-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&approved).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::rename(&approved, &displaced).unwrap();
    symlink(&outside, &approved).unwrap();

    let request: ClientRequest = serde_json::from_value(serde_json::json!({
        "type": "create_authorized_workspace_project",
        "workspace_id": workspace_id,
        "working_dir": approved,
        "authorized_root": root,
        "title": "Rejected"
    }))
    .expect("authorized project request must be part of the wire contract");
    let before = registry.snapshot().unwrap().workspaces[0].tabs.len();
    let error = handle_request(&registry, request)
        .expect_err("service must reject a replaced path outside the authorized root");
    assert!(error.to_string().contains("authorized root"), "{error:#}");
    assert_eq!(
        registry.snapshot().unwrap().workspaces[0].tabs.len(),
        before
    );

    std::fs::remove_file(&approved).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

#[test]
fn write_input_rejects_an_exited_terminal_instead_of_acknowledging_delivery() {
    let registry = SessionRegistry::new().unwrap();
    let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
    registry
        .pane(pane_id)
        .unwrap()
        .terminate_child_for_test()
        .unwrap();

    let response = handle_request(
        &registry,
        ClientRequest::WriteInput {
            pane_id,
            bytes: b"must fail".to_vec(),
        },
    )
    .unwrap();

    assert!(matches!(
        response,
        ServiceResponse::DeliveryError {
            message,
            disposition: hh_protocol::DeliveryDisposition::DefinitelyUnsent,
        } if message.contains("terminal process has exited")
    ));
}
#[test]
fn bot_settings_dispatch_updates_the_snapshot() {
    let registry = SessionRegistry::new().unwrap();
    let settings = hh_protocol::BotSettings {
        default_agent: Some(hh_protocol::TerminalProfile::Omp),
    };
    let response = handle_request(
        &registry,
        ClientRequest::SetBotSettings {
            settings: settings.clone(),
        },
    )
    .unwrap();
    assert_eq!(response, ServiceResponse::Ack);
    assert_eq!(registry.snapshot().unwrap().bots, settings);
}

#[test]
fn coding_agents_dispatch_returns_a_list() {
    let registry = SessionRegistry::new().unwrap();
    let response = handle_request(&registry, ClientRequest::GetCodingAgents).unwrap();
    assert!(matches!(response, ServiceResponse::CodingAgents { .. }));
}
