use nah_protocol::{ClientRequest, MAX_FRAME_SIZE, PROTOCOL_VERSION, ServiceResponse, WireError};
use nah_session_service::{SessionRegistry, serve_connection};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[tokio::test]
async fn client_can_handshake_and_fetch_snapshot() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        serve_connection(server, &SessionRegistry::default())
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
                nah_protocol::PaneLayout::Leaf { pane } => pane.id,
                other => panic!("unexpected initial layout: {other:?}"),
            };
            write_message(
                &mut client,
                &ClientRequest::GetUpdates {
                    snapshot_revision: Some(snapshot.revision),
                    pane_revisions: Vec::new(),
                    subscribed_panes: vec![target_pane],
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
async fn older_full_state_protocol_is_rejected_before_any_request() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        assert!(
            serve_connection(server, &SessionRegistry::default())
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
