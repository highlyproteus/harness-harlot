//! Wire-level request and response message enums.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::history::{
    HistoryArchiveStatus, HistoryClearScope, HistoryCursor, HistoryPageDirection, HistorySettings,
    TerminalHistoryPage,
};
use crate::model::{
    AppearanceColor, SessionSnapshot, SplitAxis, TmuxScanScope, TmuxSession,
    TmuxSessionAttachIssue, TmuxSessionId, WorkspacePinMove,
};
use crate::profile::TerminalProfile;
use crate::terminal::{
    DropPlacement, PaneRevisionCursor, PaneStreamState, SessionNotification, StreamDiagnostics,
    TerminalModifiers, TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalScreen,
    TerminalSelectionKind,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    Hello {
        protocol_version: u16,
    },
    GetSnapshot,
    /// Stops the service only after every terminal session has ended.
    ShutdownService,
    GetUpdates {
        snapshot_revision: Option<u64>,
        pane_revisions: Vec<PaneRevisionCursor>,
        subscribed_panes: Vec<Uuid>,
        notifications_after: u64,
    },
    GetNotifications,
    MarkNotificationsRead {
        ids: Vec<u64>,
    },
    ClearNotifications,
    GetPaneSnapshot {
        pane_id: Uuid,
    },
    CreatePane {
        target_pane: Uuid,
        axis: SplitAxis,
    },
    CreateGroupTerminal {
        target_pane: Uuid,
    },
    CreateWorkspaceTerminal {
        workspace_id: Uuid,
    },
    CreateWorkspaceTab {
        workspace_id: Uuid,
    },
    CreateBrowserTab {
        workspace_id: Uuid,
        url: Option<String>,
    },
    CreateGroupBrowser {
        target_pane: Uuid,
        url: Option<String>,
    },
    CreateAssistantTab {
        workspace_id: Uuid,
    },
    CreateGroupAssistant {
        target_pane: Uuid,
    },
    CreateWorkspaceGroup {
        workspace_id: Uuid,
        #[serde(default)]
        parent_tab: Option<Uuid>,
    },
    SetWorkspaceWorkingDir {
        workspace_id: Uuid,
        working_dir: Option<String>,
    },
    CreateWorkspaceProject {
        workspace_id: Uuid,
        working_dir: String,
        title: Option<String>,
    },
    SetTabWorkingDir {
        tab_id: Uuid,
        working_dir: String,
    },
    SetTabColor {
        tab_id: Uuid,
        color: Option<AppearanceColor>,
    },
    SetTabCustomIcon {
        tab_id: Uuid,
        icon: Option<String>,
    },
    CloseTab {
        tab_id: Uuid,
    },
    ListRemoteDirectory {
        workspace_id: Uuid,
        path: String,
    },
    ConnectSsh {
        target_pane: Uuid,
        host: String,
    },
    /// Explicitly reads bounded tmux session metadata for one workstation.
    ScanTmuxSessions {
        workspace_id: Uuid,
    },
    /// Opens selected existing tmux sessions as independent runtime-only tabs.
    AttachTmuxSessions {
        workspace_id: Uuid,
        session_ids: Vec<TmuxSessionId>,
    },
    ActivateTab {
        pane_id: Uuid,
    },
    SwapPanes {
        source_pane: Uuid,
        target_pane: Uuid,
    },
    MovePaneToSplit {
        source_pane: Uuid,
        target_pane: Uuid,
        placement: DropPlacement,
    },
    MovePaneToTab {
        source_pane: Uuid,
        target_pane: Uuid,
    },
    MovePaneToGroup {
        source_pane: Uuid,
        target_tab: Uuid,
    },
    MovePaneToNewTab {
        source_pane: Uuid,
        target_tab: Uuid,
        after: bool,
        #[serde(default)]
        parent_tab: Option<Uuid>,
    },
    ReorderTab {
        tab_id: Uuid,
        target_tab_id: Uuid,
        after: bool,
    },
    MoveTabToProject {
        tab_id: Uuid,
        project_tab: Uuid,
    },
    SetTabPinned {
        tab_id: Uuid,
        pinned: bool,
    },
    RenamePane {
        pane_id: Uuid,
        title: String,
    },
    RenameTab {
        tab_id: Uuid,
        title: String,
    },
    SetPaneProfile {
        pane_id: Uuid,
        profile: Option<TerminalProfile>,
    },
    SetPaneCustomIcon {
        pane_id: Uuid,
        icon: Option<String>,
    },
    ResetPaneIdentity {
        pane_id: Uuid,
    },
    ClosePane {
        pane_id: Uuid,
    },
    /// Respawns a runtime-only pane whose process exited, in place. For a tmux
    /// pane this is a plain re-`attach-session`; it never creates a session.
    ReattachPane {
        pane_id: Uuid,
    },
    SetBrowserState {
        pane_id: Uuid,
        url: String,
        title: Option<String>,
    },
    SetDefaultTerminalAccent {
        color: AppearanceColor,
    },
    SetDefaultWorkspaceColor {
        color: AppearanceColor,
    },
    SetPaneColor {
        pane_id: Uuid,
        color: Option<AppearanceColor>,
    },
    SetWorkspaceColor {
        workspace_id: Uuid,
        color: Option<AppearanceColor>,
    },
    SetWorkspaceCustomIcon {
        workspace_id: Uuid,
        icon: Option<String>,
    },
    CreateWorkspace {
        title: Option<String>,
    },
    CreateAssistantWorkspace {
        title: Option<String>,
        working_dir: Option<String>,
        instructions: Option<String>,
    },
    CreateSshWorkspace {
        title: Option<String>,
        destination: String,
    },
    RenameWorkspace {
        workspace_id: Uuid,
        title: String,
    },
    SetWorkspacePinned {
        workspace_id: Uuid,
        pinned: bool,
    },
    MovePinnedWorkspace {
        workspace_id: Uuid,
        direction: WorkspacePinMove,
    },
    ReorderWorkspace {
        workspace_id: Uuid,
        target_workspace_id: Uuid,
        after: bool,
    },
    DisconnectWorkspace {
        workspace_id: Uuid,
    },
    ReconnectWorkspace {
        workspace_id: Uuid,
    },
    DeleteWorkspace {
        workspace_id: Uuid,
    },
    WriteInput {
        pane_id: Uuid,
        bytes: Vec<u8>,
    },
    BeginSelection {
        pane_id: Uuid,
        point: TerminalPoint,
        kind: TerminalSelectionKind,
    },
    UpdateSelection {
        pane_id: Uuid,
        point: TerminalPoint,
    },
    ClearSelection {
        pane_id: Uuid,
    },
    CopySelection {
        pane_id: Uuid,
    },
    ScrollPane {
        pane_id: Uuid,
        lines: i32,
    },
    SearchPane {
        pane_id: Uuid,
        query: String,
        forward: bool,
    },
    MouseInput {
        pane_id: Uuid,
        point: TerminalPoint,
        button: TerminalMouseButton,
        action: TerminalMouseAction,
        modifiers: TerminalModifiers,
    },
    ResizePane {
        pane_id: Uuid,
        columns: u16,
        rows: u16,
    },
    GetHistoryStatus,
    SetHistorySettings {
        settings: HistorySettings,
    },
    ClearHistory {
        scope: HistoryClearScope,
    },
    LoadHistoryPage {
        pane_id: Uuid,
        cursor: Option<HistoryCursor>,
        direction: HistoryPageDirection,
    },
    SearchArchivedHistory {
        pane_id: Uuid,
        query: String,
        before: Option<HistoryCursor>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryDisposition {
    DefinitelyUnsent,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceResponse {
    Hello {
        protocol_version: u16,
    },
    Snapshot {
        snapshot: SessionSnapshot,
    },
    Updates {
        session_revision: u64,
        snapshot: Option<SessionSnapshot>,
        screens: Vec<TerminalScreen>,
        pane_states: Vec<PaneStreamState>,
        notifications: Vec<SessionNotification>,
        diagnostics: StreamDiagnostics,
    },
    Notifications {
        items: Vec<SessionNotification>,
    },
    PaneSnapshot {
        screen: TerminalScreen,
        diagnostics: StreamDiagnostics,
    },
    PaneCreated {
        pane_id: Uuid,
    },
    WorkspaceCreated {
        workspace_id: Uuid,
        pane_id: Uuid,
    },
    TmuxSessions {
        scope: TmuxScanScope,
        sessions: Vec<TmuxSession>,
        /// Sessions this workstation already shows in a tab. The picker marks
        /// them instead of offering a selection the service would only skip.
        open_session_ids: Vec<TmuxSessionId>,
        no_server: bool,
    },
    TmuxSessionsAttached {
        pane_ids: Vec<Uuid>,
        skipped: Vec<TmuxSessionAttachIssue>,
    },
    RemoteDirectory {
        path: String,
        entries: Vec<String>,
    },
    Ack,
    SelectionText {
        text: Option<String>,
    },
    SearchResult {
        found: bool,
    },
    HistoryStatus {
        status: HistoryArchiveStatus,
    },
    HistoryPage {
        page: Option<TerminalHistoryPage>,
    },
    HistorySearchResult {
        page: Option<TerminalHistoryPage>,
    },
    Error {
        message: String,
    },
    DeliveryError {
        message: String,
        disposition: DeliveryDisposition,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_request_json_round_trips(
        cases: impl IntoIterator<Item = (ClientRequest, serde_json::Value)>,
    ) {
        for (request, expected) in cases {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(encoded, expected);
            assert_eq!(
                serde_json::from_value::<ClientRequest>(encoded).unwrap(),
                request
            );
        }
    }

    #[test]
    fn browser_and_tab_movement_requests_use_stable_snake_case_tags_and_round_trip() {
        let workspace_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let pane_id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let tab_id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
        let cases = [
            (
                ClientRequest::CreateBrowserTab {
                    workspace_id,
                    url: Some("https://example.com".to_owned()),
                },
                serde_json::json!({
                    "type": "create_browser_tab",
                    "workspace_id": workspace_id,
                    "url": "https://example.com",
                }),
            ),
            (
                ClientRequest::CreateAssistantTab { workspace_id },
                serde_json::json!({
                    "type": "create_assistant_tab",
                    "workspace_id": workspace_id,
                }),
            ),
            (
                ClientRequest::CreateAssistantWorkspace {
                    title: Some("Research".to_owned()),
                    working_dir: Some("/srv/projects".to_owned()),
                    instructions: Some("Answer tersely".to_owned()),
                },
                serde_json::json!({
                    "type": "create_assistant_workspace",
                    "title": "Research",
                    "working_dir": "/srv/projects",
                    "instructions": "Answer tersely",
                }),
            ),
            (
                ClientRequest::CreateGroupAssistant {
                    target_pane: pane_id,
                },
                serde_json::json!({
                    "type": "create_group_assistant",
                    "target_pane": pane_id,
                }),
            ),
            (
                ClientRequest::SetBrowserState {
                    pane_id,
                    url: "https://example.com/next".to_owned(),
                    title: Some("Example".to_owned()),
                },
                serde_json::json!({
                    "type": "set_browser_state",
                    "pane_id": pane_id,
                    "url": "https://example.com/next",
                    "title": "Example",
                }),
            ),
            (
                ClientRequest::MovePaneToGroup {
                    source_pane: pane_id,
                    target_tab: tab_id,
                },
                serde_json::json!({
                    "type": "move_pane_to_group",
                    "source_pane": pane_id,
                    "target_tab": tab_id,
                }),
            ),
            (
                ClientRequest::MovePaneToNewTab {
                    source_pane: pane_id,
                    target_tab: tab_id,
                    after: true,
                    parent_tab: None,
                },
                serde_json::json!({
                    "type": "move_pane_to_new_tab",
                    "source_pane": pane_id,
                    "target_tab": tab_id,
                    "after": true,
                    "parent_tab": null,
                }),
            ),
            (
                ClientRequest::MoveTabToProject {
                    tab_id,
                    project_tab: workspace_id,
                },
                serde_json::json!({
                    "type": "move_tab_to_project",
                    "tab_id": tab_id,
                    "project_tab": workspace_id,
                }),
            ),
            (
                ClientRequest::SetTabPinned {
                    tab_id,
                    pinned: true,
                },
                serde_json::json!({
                    "type": "set_tab_pinned",
                    "tab_id": tab_id,
                    "pinned": true,
                }),
            ),
            (
                ClientRequest::CloseTab { tab_id },
                serde_json::json!({
                    "type": "close_tab",
                    "tab_id": tab_id,
                }),
            ),
        ];

        assert_request_json_round_trips(cases);
    }

    #[test]
    fn working_directory_and_tab_metadata_requests_use_stable_snake_case_tags_and_round_trip() {
        let workspace_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let tab_id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
        let cases = [
            (
                ClientRequest::SetWorkspaceWorkingDir {
                    workspace_id,
                    working_dir: Some("/srv/app".to_owned()),
                },
                serde_json::json!({
                    "type": "set_workspace_working_dir",
                    "workspace_id": workspace_id,
                    "working_dir": "/srv/app",
                }),
            ),
            (
                ClientRequest::CreateWorkspaceProject {
                    workspace_id,
                    working_dir: "/srv/project".to_owned(),
                    title: None,
                },
                serde_json::json!({
                    "type": "create_workspace_project",
                    "workspace_id": workspace_id,
                    "working_dir": "/srv/project",
                    "title": null,
                }),
            ),
            (
                ClientRequest::SetTabWorkingDir {
                    tab_id,
                    working_dir: "/srv/project".to_owned(),
                },
                serde_json::json!({
                    "type": "set_tab_working_dir",
                    "tab_id": tab_id,
                    "working_dir": "/srv/project",
                }),
            ),
            (
                ClientRequest::SetTabColor {
                    tab_id,
                    color: Some(AppearanceColor::new(0x12, 0x34, 0x56)),
                },
                serde_json::json!({
                    "type": "set_tab_color",
                    "tab_id": tab_id,
                    "color": { "red": 0x12, "green": 0x34, "blue": 0x56 },
                }),
            ),
            (
                ClientRequest::SetTabCustomIcon {
                    tab_id,
                    icon: Some("00000000-0000-4000-8000-000000000004.png".to_owned()),
                },
                serde_json::json!({
                    "type": "set_tab_custom_icon",
                    "tab_id": tab_id,
                    "icon": "00000000-0000-4000-8000-000000000004.png",
                }),
            ),
            (
                ClientRequest::SetWorkspaceCustomIcon {
                    workspace_id,
                    icon: Some("00000000-0000-4000-8000-000000000004.png".to_owned()),
                },
                serde_json::json!({
                    "type": "set_workspace_custom_icon",
                    "workspace_id": workspace_id,
                    "icon": "00000000-0000-4000-8000-000000000004.png",
                }),
            ),
            (
                ClientRequest::ListRemoteDirectory {
                    workspace_id,
                    path: "/srv/pro".to_owned(),
                },
                serde_json::json!({
                    "type": "list_remote_directory",
                    "workspace_id": workspace_id,
                    "path": "/srv/pro",
                }),
            ),
        ];

        assert_request_json_round_trips(cases);
    }

    #[test]
    fn shutdown_service_request_uses_stable_snake_case_tag_and_round_trips() {
        assert_request_json_round_trips([(
            ClientRequest::ShutdownService,
            serde_json::json!({"type": "shutdown_service"}),
        )]);
    }
}
