//! Unix-socket RPC loop and request dispatch.
use std::time::Duration;

use crate::registry::{PaneUpdateRequest, SessionRegistry};
use anyhow::{Context, Result, bail, ensure};
use hh_protocol::{
    ClientRequest, MAX_FRAME_SIZE, PROTOCOL_VERSION, PaneRevisionCursor, ServiceResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use uuid::Uuid;

pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_mins(5);

pub async fn serve_connection(mut stream: UnixStream, sessions: &SessionRegistry) -> Result<()> {
    let peer_uid = stream
        .peer_cred()
        .context("read client peer credentials")?
        .uid();
    let effective_uid = rustix::process::geteuid().as_raw();
    if peer_uid != effective_uid {
        bail!("reject client UID {peer_uid}; service UID is {effective_uid}");
    }
    let hello: ClientRequest = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_message(&mut stream))
        .await
        .context("protocol hello timed out")?
        .context("read protocol hello")?;
    match hello {
        ClientRequest::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => {
            write_message(
                &mut stream,
                &ServiceResponse::Hello {
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .await
            .context("write protocol hello")?;
        }
        ClientRequest::Hello { protocol_version } => {
            write_message(
                &mut stream,
                &ServiceResponse::Error {
                    message: format!(
                        "protocol mismatch: client {protocol_version}, service {PROTOCOL_VERSION}"
                    ),
                },
            )
            .await?;
            bail!("protocol mismatch");
        }
        _ => bail!("client must begin with hello"),
    }

    loop {
        let request =
            match tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, read_message(&mut stream)).await {
                Ok(Ok(request)) => request,
                Ok(Err(hh_protocol::WireError::Closed)) => return Ok(()),
                Ok(Err(error)) => return Err(error).context("read client request"),
                Err(_) => bail!("client connection idle timeout"),
            };
        let one_way = request_is_one_way(&request);

        let sessions = sessions.clone();
        let response = tokio::task::spawn_blocking(move || handle_request(&sessions, request))
            .await
            .context("join blocking request handler")?
            .unwrap_or_else(|error| ServiceResponse::Error {
                message: format!("{error:#}"),
            });
        if one_way {
            continue;
        }
        tokio::time::timeout(
            RESPONSE_WRITE_TIMEOUT,
            write_message(&mut stream, &response),
        )
        .await
        .context("write service response timed out")?
        .context("write service response")?;
    }
}

/// Terminal input and selection updates are one-way: the desktop never waits
/// for them, and a queued response nobody reads would eventually block this
/// connection's writer.
pub(crate) fn request_is_one_way(request: &ClientRequest) -> bool {
    matches!(request, ClientRequest::UpdateSelection { .. })
}

pub(crate) fn handle_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::ShutdownService => {
            sessions.request_shutdown()?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::Hello { .. }
        | ClientRequest::GetSnapshot
        | ClientRequest::GetUpdates { .. }
        | ClientRequest::GetNotifications
        | ClientRequest::MarkNotificationsRead { .. }
        | ClientRequest::ClearNotifications
        | ClientRequest::GetPaneSnapshot { .. }
        | ClientRequest::GetAuthorizedPaneSnapshot { .. } => {
            handle_streaming_request(sessions, request)
        }
        ClientRequest::CreatePane { .. }
        | ClientRequest::CreateGroupTerminal { .. }
        | ClientRequest::CreateWorkspaceTerminal { .. }
        | ClientRequest::CreateWorkspaceTab { .. }
        | ClientRequest::CreateBrowserTab { .. }
        | ClientRequest::CreateGroupBrowser { .. }
        | ClientRequest::CreateGalleryTab { .. }
        | ClientRequest::CreateGroupGallery { .. }
        | ClientRequest::AddGalleryImage { .. }
        | ClientRequest::BrowserCommand { .. }
        | ClientRequest::BrowserCommandResult { .. }
        | ClientRequest::CreateWorkspaceGroup { .. }
        | ClientRequest::ConnectSsh { .. }
        | ClientRequest::RenamePane { .. }
        | ClientRequest::SetPaneProfile { .. }
        | ClientRequest::SetPaneCustomIcon { .. }
        | ClientRequest::ResetPaneIdentity { .. }
        | ClientRequest::ClosePane { .. }
        | ClientRequest::ReattachPane { .. }
        | ClientRequest::SetBrowserState { .. }
        | ClientRequest::SetPaneColor { .. } => handle_panes_request(sessions, request),
        ClientRequest::ActivateTab { .. }
        | ClientRequest::SwapPanes { .. }
        | ClientRequest::MovePaneToSplit { .. }
        | ClientRequest::MovePaneToTab { .. }
        | ClientRequest::MovePaneToGroup { .. }
        | ClientRequest::MovePaneToNewTab { .. }
        | ClientRequest::RenameTab { .. }
        | ClientRequest::SetTabCustomIcon { .. }
        | ClientRequest::CloseTab { .. }
        | ClientRequest::SetTabColor { .. }
        | ClientRequest::CreateWorkspaceProject { .. }
        | ClientRequest::CreateAuthorizedWorkspaceProject { .. }
        | ClientRequest::CreateAuthorizedWorktreeProject { .. }
        | ClientRequest::SetTabWorkingDir { .. }
        | ClientRequest::ReorderTab { .. }
        | ClientRequest::MoveTabToProject { .. }
        | ClientRequest::SetTabPinned { .. } => handle_tabs_request(sessions, request),
        ClientRequest::SetDefaultTerminalAccent { .. }
        | ClientRequest::SetDefaultWorkspaceColor { .. }
        | ClientRequest::SetWorkspaceColor { .. }
        | ClientRequest::SetWorkspaceCustomIcon { .. }
        | ClientRequest::SetWorkspaceWorkingDir { .. }
        | ClientRequest::CreateWorkspace { .. }
        | ClientRequest::CreateAuthorizedWorkspace { .. }
        | ClientRequest::CreateSshWorkspace { .. }
        | ClientRequest::RenameWorkspace { .. }
        | ClientRequest::SetWorkspacePinned { .. }
        | ClientRequest::MovePinnedWorkspace { .. }
        | ClientRequest::ReorderWorkspace { .. }
        | ClientRequest::DisconnectWorkspace { .. }
        | ClientRequest::ReconnectWorkspace { .. }
        | ClientRequest::DeleteWorkspace { .. } => handle_workspaces_request(sessions, request),
        ClientRequest::WriteInput { pane_id, bytes } => {
            Ok(match sessions.write_input_with_delivery(pane_id, &bytes) {
                Ok(()) => ServiceResponse::Ack,
                Err(error) => ServiceResponse::DeliveryError {
                    message: error.to_string(),
                    disposition: error.disposition(),
                },
            })
        }
        ClientRequest::WriteAuthorizedInput { authority, bytes } => Ok(
            match sessions.authorized_write_input_with_delivery(&authority, &bytes) {
                Ok(()) => ServiceResponse::Ack,
                Err(error) => ServiceResponse::DeliveryError {
                    message: error.to_string(),
                    disposition: error.disposition(),
                },
            },
        ),
        ClientRequest::BeginSelection { .. }
        | ClientRequest::UpdateSelection { .. }
        | ClientRequest::ClearSelection { .. }
        | ClientRequest::CopySelection { .. }
        | ClientRequest::ScrollPane { .. }
        | ClientRequest::SearchPane { .. }
        | ClientRequest::MouseInput { .. }
        | ClientRequest::ResizePane { .. } => handle_terminal_request(sessions, request),
        ClientRequest::ScanTmuxSessions { .. }
        | ClientRequest::ListRemoteDirectory { .. }
        | ClientRequest::AttachTmuxSessions { .. } => handle_remote_request(sessions, request),
        ClientRequest::SetBotSettings { .. }
        | ClientRequest::CreateBot { .. }
        | ClientRequest::SetBotAgent { .. }
        | ClientRequest::RestartBot { .. }
        | ClientRequest::SetBotHome { .. }
        | ClientRequest::CreateWorker { .. }
        | ClientRequest::ListBotThreads { .. }
        | ClientRequest::OpenBotThread { .. }
        | ClientRequest::SetBotThreadPinned { .. }
        | ClientRequest::ReportBotSession { .. }
        | ClientRequest::GetCodingAgents => handle_bots_request(sessions, request),
    }
}

fn handle_streaming_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::Hello { .. } => Ok(ServiceResponse::Error {
            message: "hello was already completed".to_owned(),
        }),
        ClientRequest::GetSnapshot => Ok(ServiceResponse::Snapshot {
            snapshot: sessions.snapshot()?,
        }),
        ClientRequest::GetUpdates {
            snapshot_revision,
            pane_revisions,
            subscribed_panes,
            notifications_after,
            browser_executor,
        } => handle_get_updates(
            sessions,
            snapshot_revision,
            &pane_revisions,
            &subscribed_panes,
            browser_executor,
            notifications_after,
        ),
        ClientRequest::GetNotifications => Ok(ServiceResponse::Notifications {
            items: sessions.notifications()?,
        }),
        ClientRequest::MarkNotificationsRead { ids } => {
            sessions.mark_notifications_read(&ids);
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ClearNotifications => {
            sessions.clear_notifications();
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::GetPaneSnapshot { pane_id } => handle_get_pane_snapshot(sessions, pane_id),
        ClientRequest::GetAuthorizedPaneSnapshot { authority } => {
            let (screen, diagnostics) = sessions.authorized_pane_snapshot(&authority)?;
            Ok(ServiceResponse::PaneSnapshot {
                screen,
                diagnostics,
            })
        }
        _ => unreachable!("streaming request dispatched to the wrong handler"),
    }
}

fn handle_panes_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::CreatePane { target_pane, axis } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_pane(target_pane, axis)?,
        }),
        ClientRequest::CreateGroupTerminal { target_pane } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_group_terminal(target_pane)?,
        }),
        ClientRequest::CreateWorkspaceTerminal { workspace_id } => {
            Ok(ServiceResponse::PaneCreated {
                pane_id: sessions.create_workspace_terminal(workspace_id)?,
            })
        }
        ClientRequest::CreateWorkspaceTab { workspace_id } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_workspace_tab(workspace_id)?,
        }),
        ClientRequest::CreateBrowserTab { workspace_id, url } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_browser_tab(workspace_id, url.as_deref())?,
        }),
        ClientRequest::CreateGroupBrowser { target_pane, url } => {
            Ok(ServiceResponse::PaneCreated {
                pane_id: sessions.create_group_browser(target_pane, url.as_deref())?,
            })
        }
        ClientRequest::CreateGalleryTab { workspace_id } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_gallery_tab(workspace_id)?,
        }),
        ClientRequest::CreateGroupGallery { target_pane } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_group_gallery(target_pane, true)?,
        }),
        ClientRequest::AddGalleryImage {
            workspace_id,
            origin_pane,
            source,
        } => {
            let (path, pane_id) = sessions.add_gallery_image(workspace_id, origin_pane, &source)?;
            Ok(ServiceResponse::GalleryImageAdded {
                path: path.to_string_lossy().into_owned(),
                pane_id,
            })
        }
        ClientRequest::BrowserCommand { pane_id, action } => {
            Ok(ServiceResponse::BrowserCommandResult {
                outcome: sessions.browser_command(pane_id, action)?,
            })
        }
        ClientRequest::BrowserCommandResult {
            request_id,
            outcome,
        } => {
            sessions.resolve_browser_command(request_id, outcome);
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateWorkspaceGroup {
            workspace_id,
            parent_tab,
        } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_workspace_group(workspace_id, parent_tab)?,
        }),
        ClientRequest::ConnectSsh { target_pane, host } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.connect_ssh(target_pane, &host)?,
        }),
        ClientRequest::RenamePane { pane_id, title } => {
            sessions.rename_pane(pane_id, &title)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetPaneProfile { pane_id, profile } => {
            sessions.set_pane_profile(pane_id, profile)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetPaneCustomIcon { pane_id, icon } => {
            sessions.set_pane_custom_icon(pane_id, icon)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ResetPaneIdentity { pane_id } => {
            sessions.reset_pane_identity(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ClosePane { pane_id } => {
            sessions.close_pane(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReattachPane { pane_id } => {
            sessions.reattach_pane(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetBrowserState {
            pane_id,
            url,
            title,
        } => {
            sessions.set_browser_state(pane_id, &url, title.as_deref())?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetPaneColor { pane_id, color } => {
            sessions.set_pane_color(pane_id, color)?;
            Ok(ServiceResponse::Ack)
        }
        _ => unreachable!("panes request dispatched to the wrong handler"),
    }
}

fn handle_tabs_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::ActivateTab { pane_id } => {
            sessions.activate_tab(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SwapPanes {
            source_pane,
            target_pane,
        } => {
            sessions.swap_panes(source_pane, target_pane)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToSplit {
            source_pane,
            target_pane,
            placement,
        } => {
            sessions.move_pane_to_split(source_pane, target_pane, placement)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToTab {
            source_pane,
            target_pane,
        } => {
            sessions.move_pane_to_tab(source_pane, target_pane)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToGroup {
            source_pane,
            target_tab,
        } => {
            sessions.move_pane_to_group(source_pane, target_tab)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePaneToNewTab {
            source_pane,
            target_tab,
            after,
            parent_tab,
        } => {
            sessions.move_pane_to_new_tab(source_pane, target_tab, after, parent_tab)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::RenameTab { tab_id, title } => {
            sessions.rename_tab(tab_id, &title)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetTabCustomIcon { tab_id, icon } => {
            sessions.set_tab_custom_icon(tab_id, icon)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CloseTab { tab_id } => {
            sessions.close_tab(tab_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetTabColor { tab_id, color } => {
            sessions.set_tab_color(tab_id, color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateWorkspaceProject {
            workspace_id,
            working_dir,
            title,
        } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.create_workspace_project(
                workspace_id,
                &working_dir,
                title.as_deref(),
            )?,
        }),
        ClientRequest::CreateAuthorizedWorkspaceProject {
            workspace_id,
            working_dir,
            authorized_root,
            title,
        } => {
            let working_dir = canonical_directory_within(&working_dir, &authorized_root)?;
            Ok(ServiceResponse::PaneCreated {
                pane_id: sessions.create_workspace_project(
                    workspace_id,
                    &working_dir,
                    title.as_deref(),
                )?,
            })
        }
        ClientRequest::CreateAuthorizedWorktreeProject {
            workspace_id,
            repo_dir,
            authorized_root,
            branch,
            base,
        } => {
            let repo_dir = canonical_directory_within(&repo_dir, &authorized_root)?;
            let worktree =
                create_git_worktree_within(&repo_dir, &authorized_root, &branch, base.as_deref())?;
            match sessions.create_workspace_project(workspace_id, worktree.path(), Some(&branch)) {
                Ok(pane_id) => Ok(ServiceResponse::PaneCreated { pane_id }),
                Err(error) => {
                    if let Err(cleanup_error) = worktree.rollback() {
                        bail!(
                            "{error:#}; additionally failed to roll back worktree: {cleanup_error:#}"
                        );
                    }
                    Err(error)
                }
            }
        }
        ClientRequest::SetTabWorkingDir {
            tab_id,
            working_dir,
        } => {
            sessions.set_tab_working_dir(tab_id, working_dir)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReorderTab {
            tab_id,
            target_tab_id,
            after,
        } => {
            sessions.reorder_tab(tab_id, target_tab_id, after)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MoveTabToProject {
            tab_id,
            project_tab,
        } => {
            sessions.move_tab_to_project(tab_id, project_tab)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetTabPinned { tab_id, pinned } => {
            sessions.set_tab_pinned(tab_id, pinned)?;
            Ok(ServiceResponse::Ack)
        }
        _ => unreachable!("tabs request dispatched to the wrong handler"),
    }
}

fn handle_workspaces_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::SetDefaultTerminalAccent { color } => {
            sessions.set_default_terminal_accent(color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetDefaultWorkspaceColor { color } => {
            sessions.set_default_workspace_color(color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetWorkspaceColor {
            workspace_id,
            color,
        } => {
            sessions.set_workspace_color(workspace_id, color)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetWorkspaceWorkingDir {
            workspace_id,
            working_dir,
        } => {
            sessions.set_workspace_working_dir(workspace_id, working_dir)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetWorkspaceCustomIcon { workspace_id, icon } => {
            sessions.set_workspace_custom_icon(workspace_id, icon)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateWorkspace { title } => {
            let (workspace_id, pane_id) = sessions.create_workspace(title.as_deref())?;
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            })
        }
        ClientRequest::CreateAuthorizedWorkspace {
            title,
            working_dir,
            authorized_root,
        } => {
            let working_dir = canonical_directory_within(&working_dir, &authorized_root)?;
            let (workspace_id, pane_id) = sessions.create_workspace(title.as_deref())?;
            if let Err(error) = sessions.set_workspace_working_dir(workspace_id, Some(working_dir))
            {
                let _ = sessions.delete_workspace(workspace_id);
                return Err(error);
            }
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            })
        }
        ClientRequest::CreateSshWorkspace { title, destination } => {
            let (workspace_id, pane_id) =
                sessions.create_ssh_workspace(title.as_deref(), &destination)?;
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            })
        }
        ClientRequest::RenameWorkspace {
            workspace_id,
            title,
        } => {
            sessions.rename_workspace(workspace_id, &title)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetWorkspacePinned {
            workspace_id,
            pinned,
        } => {
            sessions.set_workspace_pinned(workspace_id, pinned)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::MovePinnedWorkspace {
            workspace_id,
            direction,
        } => {
            sessions.move_pinned_workspace(workspace_id, direction)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReorderWorkspace {
            workspace_id,
            target_workspace_id,
            after,
        } => {
            sessions.reorder_workspace(workspace_id, target_workspace_id, after)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::DisconnectWorkspace { workspace_id } => {
            sessions.disconnect_workspace(workspace_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReconnectWorkspace { workspace_id } => Ok(ServiceResponse::PaneCreated {
            pane_id: sessions.reconnect_workspace(workspace_id)?,
        }),
        ClientRequest::DeleteWorkspace { workspace_id } => {
            sessions.delete_workspace(workspace_id)?;
            Ok(ServiceResponse::Ack)
        }
        _ => unreachable!("workspaces request dispatched to the wrong handler"),
    }
}

fn handle_terminal_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::BeginSelection {
            pane_id,
            point,
            kind,
        } => {
            sessions.begin_selection(pane_id, point, kind)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::UpdateSelection { pane_id, point } => {
            sessions.update_selection(pane_id, point)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ClearSelection { pane_id } => {
            sessions.clear_selection(pane_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CopySelection { pane_id } => Ok(ServiceResponse::SelectionText {
            text: sessions.selected_text(pane_id)?,
        }),
        ClientRequest::ScrollPane { pane_id, lines } => {
            sessions.scroll_pane(pane_id, lines)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SearchPane {
            pane_id,
            query,
            forward,
        } => Ok(ServiceResponse::SearchResult {
            found: sessions.search_pane(pane_id, &query, forward)?,
        }),
        ClientRequest::MouseInput {
            pane_id,
            point,
            button,
            action,
            modifiers,
        } => {
            sessions.mouse_input(pane_id, point, button, action, modifiers)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ResizePane {
            pane_id,
            columns,
            rows,
        } => {
            sessions.resize_pane(pane_id, columns, rows)?;
            Ok(ServiceResponse::Ack)
        }
        _ => unreachable!("terminal request dispatched to the wrong handler"),
    }
}

fn handle_remote_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::ScanTmuxSessions { workspace_id } => {
            let scan = sessions.scan_tmux_sessions(workspace_id)?;
            Ok(ServiceResponse::TmuxSessions {
                scope: scan.scope,
                sessions: scan.sessions,
                open_session_ids: scan.open_session_ids,
                no_server: scan.no_server,
            })
        }
        ClientRequest::ListRemoteDirectory { workspace_id, path } => {
            let entries = sessions.list_remote_directory(workspace_id, &path)?;
            Ok(ServiceResponse::RemoteDirectory { path, entries })
        }
        ClientRequest::AttachTmuxSessions {
            workspace_id,
            session_ids,
        } => {
            let result = sessions.attach_tmux_sessions(workspace_id, &session_ids)?;
            Ok(ServiceResponse::TmuxSessionsAttached {
                pane_ids: result.pane_ids,
                skipped: result.skipped,
            })
        }
        _ => unreachable!("remote request dispatched to the wrong handler"),
    }
}

fn handle_bots_request(
    sessions: &SessionRegistry,
    request: ClientRequest,
) -> Result<ServiceResponse> {
    match request {
        ClientRequest::SetBotSettings { settings } => {
            sessions.set_bot_settings(settings)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateBot {
            name,
            agent,
            working_dir,
            instructions,
        } => {
            let (workspace_id, tab_id, pane_id) =
                sessions.create_bot(name.as_deref(), agent, working_dir, instructions)?;
            Ok(ServiceResponse::BotCreated {
                workspace_id,
                tab_id,
                pane_id,
            })
        }
        ClientRequest::SetBotAgent { tab_id, agent } => {
            sessions.set_bot_agent(tab_id, agent)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::RestartBot { tab_id } => {
            sessions.restart_bot(tab_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetBotHome { tab_id, home } => {
            sessions.set_bot_home(tab_id, home)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::CreateWorker {
            workspace_id,
            working_dir,
            title,
            command,
            requester_pane,
        } => {
            let (workspace_id, tab_id, pane_id) = sessions.create_worker(
                workspace_id,
                working_dir.as_deref(),
                title.as_deref(),
                command.as_deref(),
                requester_pane,
            )?;
            Ok(ServiceResponse::WorkerCreated {
                workspace_id,
                tab_id,
                pane_id,
            })
        }
        ClientRequest::ListBotThreads { tab_id } => Ok(ServiceResponse::BotThreads {
            threads: sessions.list_bot_threads(tab_id)?,
        }),
        ClientRequest::OpenBotThread { tab_id, thread_id } => {
            sessions.open_bot_thread(tab_id, thread_id.as_deref())?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::SetBotThreadPinned {
            tab_id,
            thread_id,
            pinned,
        } => {
            sessions.set_bot_thread_pinned(tab_id, &thread_id, pinned)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::ReportBotSession {
            pane_id,
            session_id,
        } => {
            sessions.report_bot_session(pane_id, &session_id)?;
            Ok(ServiceResponse::Ack)
        }
        ClientRequest::GetCodingAgents => Ok(ServiceResponse::CodingAgents {
            agents: sessions.coding_agents(true)?,
        }),
        _ => unreachable!("bots request dispatched to the wrong handler"),
    }
}

pub(crate) fn handle_get_updates(
    sessions: &SessionRegistry,
    snapshot_revision: Option<u64>,
    pane_revisions: &[PaneRevisionCursor],
    subscribed_panes: &[Uuid],
    browser_executor: bool,
    notifications_after: u64,
) -> Result<ServiceResponse> {
    let update = sessions.pane_updates_for(PaneUpdateRequest {
        snapshot_revision,
        pane_revisions,
        subscribed_panes,
        browser_executor,
        measure_bytes: false,
        notifications_after,
    })?;
    Ok(ServiceResponse::Updates {
        session_revision: update.session_revision,
        snapshot: update.snapshot,
        screens: update.screens,
        pane_states: update.pane_states,
        notifications: update.notifications,
        diagnostics: update.diagnostics,
        browser_commands: update.browser_commands,
    })
}

pub(crate) fn handle_get_pane_snapshot(
    sessions: &SessionRegistry,
    pane_id: Uuid,
) -> Result<ServiceResponse> {
    let (screen, diagnostics) = sessions.pane_snapshot(pane_id)?;
    Ok(ServiceResponse::PaneSnapshot {
        screen,
        diagnostics,
    })
}

pub(crate) async fn write_message<T: Serialize>(
    stream: &mut UnixStream,
    message: &T,
) -> Result<(), hh_protocol::WireError> {
    let frame = hh_protocol::encode_frame(message)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

pub(crate) async fn read_message<T: DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, hh_protocol::WireError> {
    let mut length = [0_u8; 4];
    if let Err(error) = stream.read_exact(&mut length).await {
        return if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Err(hh_protocol::WireError::Closed)
        } else {
            Err(hh_protocol::WireError::Io(error))
        };
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(hh_protocol::WireError::FrameTooLarge(length));
    }
    let mut payload = vec![0_u8; length];
    tokio::time::timeout(REQUEST_BODY_TIMEOUT, stream.read_exact(&mut payload))
        .await
        .map_err(|_| {
            hh_protocol::WireError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request body timed out",
            ))
        })??;
    hh_protocol::decode_frame(&payload)
}

fn canonical_directory_within(path: &str, authorized_root: &str) -> Result<String> {
    hh_protocol::validate_workspace_dir(path).map_err(anyhow::Error::from)?;
    hh_protocol::validate_workspace_dir(authorized_root).map_err(anyhow::Error::from)?;
    let root = std::fs::canonicalize(authorized_root)
        .with_context(|| format!("resolve authorized root {authorized_root}"))?;
    let directory = std::fs::canonicalize(path)
        .with_context(|| format!("resolve authorized directory {path}"))?;
    ensure!(
        directory.is_dir() && directory.starts_with(&root),
        "directory {} is outside authorized root {}",
        directory.display(),
        root.display()
    );
    Ok(directory.to_string_lossy().into_owned())
}

#[derive(Debug)]
struct CreatedGitWorktree {
    repo: std::path::PathBuf,
    parent: std::path::PathBuf,
    target: std::path::PathBuf,
    branch: String,
    parent_created: bool,
}

impl CreatedGitWorktree {
    fn path(&self) -> &str {
        self.target
            .to_str()
            .expect("canonical worktree path was validated as UTF-8")
    }

    fn rollback(&self) -> Result<()> {
        if std::fs::symlink_metadata(&self.target).is_ok() {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "remove", "--force", "--"])
                .arg(&self.target)
                .output()
                .context("launch git worktree remove")?;
            ensure!(
                output.status.success(),
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let branch_ref = format!("refs/heads/{}", self.branch);
        let branch_exists = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["show-ref", "--verify", "--quiet", &branch_ref])
            .status()
            .context("inspect Git branch during cleanup")?
            .success();
        if branch_exists {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["branch", "-D", "--", &self.branch])
                .output()
                .context("launch git branch cleanup")?;
            ensure!(
                output.status.success(),
                "git branch cleanup failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if self.parent_created {
            std::fs::remove_dir(&self.parent)
                .with_context(|| format!("remove worktree parent {}", self.parent.display()))?;
        }
        Ok(())
    }
}

fn create_git_worktree_within(
    repo_dir: &str,
    authorized_root: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<CreatedGitWorktree> {
    ensure!(
        !branch.is_empty()
            && branch.len() <= 100
            && branch
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b'/' | b'-')),
        "branch must match [A-Za-z0-9._/-]{{1,100}}"
    );
    if let Some(base) = base {
        let valid = !base.is_empty()
            && base.len() <= 200
            && !base.starts_with(['-', '/'])
            && !base.ends_with(['/', '.'])
            && !base.contains("..")
            && !base.contains("//")
            && !base.contains("@{")
            && base.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-')
            });
        ensure!(valid, "base must be a conservative Git ref");
    }

    let root = std::fs::canonicalize(authorized_root)
        .with_context(|| format!("resolve authorized root {authorized_root}"))?;
    ensure!(root.is_dir(), "authorized root must be a directory");
    let repo = std::fs::canonicalize(repo_dir)
        .with_context(|| format!("resolve repository directory {repo_dir}"))?;
    ensure!(
        repo.is_dir() && repo.starts_with(&root),
        "repository {} is outside authorized root {}",
        repo.display(),
        root.display()
    );
    ensure!(
        repo.join(".git").exists(),
        "repository directory must contain .git"
    );
    let repo_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .context("repository directory has no UTF-8 name")?;
    let parent = repo
        .parent()
        .context("repository directory has no parent")?
        .join(format!("{repo_name}-worktrees"));
    let parent_created = match std::fs::create_dir(&parent) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create worktree parent {}", parent.display()));
        }
    };
    let parent = std::fs::canonicalize(&parent)
        .with_context(|| format!("resolve worktree parent {}", parent.display()))?;
    if !parent.starts_with(&root) {
        if parent_created {
            let _ = std::fs::remove_dir(&parent);
        }
        bail!(
            "worktree parent {} is outside authorized root {}",
            parent.display(),
            root.display()
        );
    }
    let target = parent.join(branch.replace('/', "-"));
    if std::fs::symlink_metadata(&target).is_ok() {
        if parent_created {
            let _ = std::fs::remove_dir(&parent);
        }
        bail!("worktree target {} already exists", target.display());
    }

    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(&repo)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(branch)
        .arg("--")
        .arg(&target);
    if let Some(base) = base {
        command.arg(base);
    }
    let cleanup = CreatedGitWorktree {
        repo: repo.clone(),
        parent: parent.clone(),
        target: target.clone(),
        branch: branch.to_owned(),
        parent_created,
    };
    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            let _ = cleanup.rollback();
            return Err(error).context("launch git worktree add");
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if let Err(cleanup_error) = cleanup.rollback() {
            bail!(
                "git worktree add failed: {stderr}; additionally failed to clean partial worktree: {cleanup_error:#}"
            );
        }
        bail!("git worktree add failed: {stderr}");
    }
    let target = match std::fs::canonicalize(&target)
        .with_context(|| format!("resolve created worktree {}", target.display()))
    {
        Ok(target) if target.starts_with(&root) => target,
        Ok(target) => {
            let cleanup_error = cleanup.rollback().err();
            if let Some(cleanup_error) = cleanup_error {
                bail!(
                    "created worktree {} is outside authorized root {}; additionally failed to roll back: {cleanup_error:#}",
                    target.display(),
                    root.display()
                );
            }
            bail!(
                "created worktree {} is outside authorized root {}",
                target.display(),
                root.display()
            );
        }
        Err(error) => {
            if let Err(cleanup_error) = cleanup.rollback() {
                bail!("{error:#}; additionally failed to roll back: {cleanup_error:#}");
            }
            return Err(error);
        }
    };
    ensure!(
        target.to_str().is_some(),
        "created worktree path is not UTF-8"
    );
    Ok(CreatedGitWorktree { target, ..cleanup })
}

#[cfg(test)]
#[path = "rpc_tests.rs"]
mod tests;
