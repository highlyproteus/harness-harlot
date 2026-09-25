use hh_protocol::{
    ClientRequest, MAX_FRAME_SIZE, PROTOCOL_VERSION, PaneKind, PaneLayout, ServiceResponse,
    WireError,
};
use hh_session_service::{SessionRegistry, serve_connection};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[tokio::test]
async fn client_can_handshake_and_fetch_snapshot() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        serve_connection(
            server,
            &SessionRegistry::new().expect("start seeded configured-shell PTY"),
        )
        .await
        .unwrap();
    });

    write_message(
        &mut client,
        &ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_message::<ServiceResponse>(&mut client).await.unwrap(),
        ServiceResponse::Hello {
            protocol_version: PROTOCOL_VERSION
        }
    ));

    write_message(&mut client, &ClientRequest::GetSnapshot)
        .await
        .unwrap();
    let response: ServiceResponse = read_message(&mut client).await.unwrap();
    match response {
        ServiceResponse::Snapshot { snapshot } => {
            assert_eq!(snapshot.workspaces.len(), 1);
            let target_pane = match &snapshot.workspaces[0].tabs[0].layout {
                hh_protocol::PaneLayout::Leaf { pane } => pane.id,
                other => panic!("unexpected initial layout: {other:?}"),
            };
            write_message(
                &mut client,
                &ClientRequest::GetUpdates {
                    snapshot_revision: Some(snapshot.revision),
                    pane_revisions: Vec::new(),
                    subscribed_panes: vec![target_pane],
                    notifications_after: 0,
                    browser_executor: false,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_message::<ServiceResponse>(&mut client).await.unwrap(),
                ServiceResponse::Updates {
                    snapshot: None,
                    screens,
                    pane_states,
                    ..
                } if screens.len() == 1
                    && screens[0].pane_id == target_pane
                    && pane_states.len() == 1
                    && !pane_states[0].dirty
            ));
            write_message(
                &mut client,
                &ClientRequest::ConnectSsh {
                    target_pane,
                    host: "-A".to_owned(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_message::<ServiceResponse>(&mut client).await.unwrap(),
                ServiceResponse::Error { message }
                    if message.contains("must start with a letter or number")
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    drop(client);
    server_task.await.unwrap();
}

#[tokio::test]
async fn create_worker_opens_a_titled_tab_and_types_its_command() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let registry = SessionRegistry::new().expect("start seeded configured-shell PTY");
    let snapshot = registry.snapshot().unwrap();
    let workspace_id = snapshot.workspaces[0].id;
    let PaneLayout::Leaf { pane } = &snapshot.workspaces[0].tabs[0].layout else {
        panic!("seeded workstation holds one terminal");
    };
    let requester_pane = pane.id;
    let server_task = tokio::spawn(async move {
        serve_connection(server, &registry).await.unwrap();
    });
    write_message(
        &mut client,
        &ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    read_message::<ServiceResponse>(&mut client).await.unwrap();

    let worker = |workspace_id| ClientRequest::CreateWorker {
        workspace_id,
        working_dir: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        title: Some("api-fix".to_owned()),
        command: Some("echo HH_WORKER_$((6 * 7))".to_owned()),
        requester_pane: Some(requester_pane),
    };
    write_message(&mut client, &worker(None)).await.unwrap();
    assert!(matches!(
        read_message::<ServiceResponse>(&mut client).await.unwrap(),
        ServiceResponse::Error { message } if message.contains("workspace_id")
    ));

    write_message(&mut client, &worker(Some(workspace_id)))
        .await
        .unwrap();
    let ServiceResponse::WorkerCreated {
        workspace_id: created_in,
        tab_id,
        pane_id,
    } = read_message::<ServiceResponse>(&mut client).await.unwrap()
    else {
        panic!("worker creation did not report its tab");
    };
    assert_eq!(created_in, workspace_id);

    write_message(&mut client, &ClientRequest::GetSnapshot)
        .await
        .unwrap();
    let ServiceResponse::Snapshot { snapshot } =
        read_message::<ServiceResponse>(&mut client).await.unwrap()
    else {
        panic!("snapshot request returned an unexpected response");
    };
    let tab = snapshot.workspaces[0]
        .tabs
        .iter()
        .find(|tab| tab.id == tab_id)
        .expect("worker tab in its workstation");
    assert_eq!(tab.custom_title.as_deref(), Some("api-fix"));
    assert_eq!(tab.owner_bot, None);
    let PaneLayout::Leaf { pane } = &tab.layout else {
        panic!("worker tab holds one terminal");
    };
    assert_eq!(pane.id, pane_id);
    assert_eq!(pane.kind, PaneKind::Terminal);
    assert_eq!(pane.title, "api-fix");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        write_message(&mut client, &ClientRequest::GetPaneSnapshot { pane_id })
            .await
            .unwrap();
        let ServiceResponse::PaneSnapshot { screen, .. } =
            read_message::<ServiceResponse>(&mut client).await.unwrap()
        else {
            panic!("pane snapshot request returned an unexpected response");
        };
        let text = screen
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .map(|run| run.text.as_str())
            .collect::<String>();
        if text.contains("HH_WORKER_42") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker command never ran; screen: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    drop(client);
    server_task.await.unwrap();
}

#[tokio::test]
async fn older_full_state_protocol_is_rejected_before_any_request() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        assert!(
            serve_connection(
                server,
                &SessionRegistry::new().expect("start seeded configured-shell PTY"),
            )
            .await
            .is_err()
        );
    });

    write_message(
        &mut client,
        &ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION - 1,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_message::<ServiceResponse>(&mut client).await.unwrap(),
        ServiceResponse::Error { message }
            if message.contains("protocol mismatch")
                && message.contains(&PROTOCOL_VERSION.to_string())
    ));

    drop(client);
    server_task.await.unwrap();
}

/// Terminal input is authoritative: callers receive the service result before
/// they may claim that input was delivered.
#[tokio::test]
async fn terminal_input_is_acknowledged_on_the_wire() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let registry = SessionRegistry::new().expect("start seeded configured-shell PTY");
    let snapshot = registry.snapshot().unwrap();
    let pane_id = match &snapshot.workspaces[0].tabs[0].layout {
        hh_protocol::PaneLayout::Leaf { pane } => pane.id,
        other => panic!("unexpected initial layout: {other:?}"),
    };
    let server_task = tokio::spawn(async move {
        serve_connection(server, &registry).await.unwrap();
    });

    write_message(
        &mut client,
        &ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_message::<ServiceResponse>(&mut client).await.unwrap(),
        ServiceResponse::Hello {
            protocol_version: PROTOCOL_VERSION
        }
    ));

    write_message(
        &mut client,
        &ClientRequest::WriteInput {
            pane_id,
            bytes: b"x".to_vec(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        read_message::<ServiceResponse>(&mut client).await.unwrap(),
        ServiceResponse::Ack,
        "WriteInput must return the service's authoritative result"
    );
    write_message(
        &mut client,
        &ClientRequest::UpdateSelection {
            pane_id,
            point: hh_protocol::TerminalPoint { row: 0, column: 0 },
        },
    )
    .await
    .unwrap();
    write_message(&mut client, &ClientRequest::GetSnapshot)
        .await
        .unwrap();
    assert!(
        matches!(
            read_message::<ServiceResponse>(&mut client).await.unwrap(),
            ServiceResponse::Snapshot { .. }
        ),
        "the first frame after one-way requests must be the snapshot response"
    );

    drop(client);
    server_task.await.unwrap();
}

/// A client that connects but never sends a hello must be disconnected by
/// the handshake timeout instead of holding a connection slot forever.
#[tokio::test]
async fn silent_client_is_disconnected_after_the_handshake_timeout() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        assert!(
            serve_connection(
                server,
                &SessionRegistry::new().expect("start seeded configured-shell PTY"),
            )
            .await
            .is_err()
        );
    });

    let mut buffer = [0_u8; 8];
    let read = tokio::time::timeout(std::time::Duration::from_secs(7), client.read(&mut buffer))
        .await
        .expect("server must close a silent client within the handshake window");
    match read {
        // The server closed the connection (EOF or reset) without sending
        // anything.
        Ok(0) | Err(_) => {}
        Ok(bytes) => panic!("server sent {bytes} bytes before a valid hello"),
    }
    server_task.await.unwrap();
}

async fn write_message<T: Serialize>(
    stream: &mut UnixStream,
    message: &T,
) -> Result<(), WireError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge(payload.len()));
    }
    let length =
        u32::try_from(payload.len()).map_err(|_| WireError::FrameTooLarge(payload.len()))?;
    stream.write_u32(length).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

async fn read_message<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T, WireError> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_FRAME_SIZE {
        return Err(WireError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}
