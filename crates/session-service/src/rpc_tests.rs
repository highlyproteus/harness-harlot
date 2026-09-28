use super::*;

use crate::layout::first_pane_id;

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

#[test]
fn authorized_workspace_creation_starts_its_first_terminal_in_the_root() {
    let registry = SessionRegistry::new().unwrap();
    let root = std::env::temp_dir().join(format!("hh-authorized-root-{}", Uuid::new_v4()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = std::fs::canonicalize(project).unwrap();

    let request = ClientRequest::CreateAuthorizedWorkspace {
        title: None,
        working_dir: project.to_string_lossy().into_owned(),
        authorized_root: root.to_string_lossy().into_owned(),
    };
    let ServiceResponse::WorkspaceCreated {
        workspace_id,
        pane_id,
    } = handle_request(&registry, request).unwrap()
    else {
        panic!("authorized workspace creation must report the new workstation");
    };
    let snapshot = registry.snapshot().unwrap();
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .unwrap();
    assert_eq!(workspace.title, "project");
    assert_eq!(workspace.parent_workstation, None);
    assert_eq!(registry.cwd_for_pane(pane_id).unwrap(), project);

    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
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
