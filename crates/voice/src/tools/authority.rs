use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use hh_protocol::{SessionSnapshot, TerminalTransport};
use serde_json::Value;
use uuid::Uuid;

use super::{
    DEFAULT_PANE_LINES, MAX_PANE_LINES, collect_panes, required_str, required_uuid,
    revalidate_approved_tool_path, terminal_workspace, threads, workspace_directory_is_within,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PendingAction {
    ReadPane {
        pane_id: Uuid,
        lines: usize,
        authority: MutationAuthority,
    },
    ReadThread {
        thread_id: Uuid,
        workspace_id: Uuid,
    },
    RecallMemory {
        query: String,
    },
    ToolCall {
        name: String,
        arguments: Value,
        authority: Option<MutationAuthority>,
    },
    SendInput {
        pane_id: Uuid,
        text: String,
        submit: bool,
        authority: MutationAuthority,
    },
    SendKeys {
        pane_id: Uuid,
        keys: Vec<String>,
        authority: MutationAuthority,
    },
    CloseTab {
        tab_id: Uuid,
        authority: MutationAuthority,
    },
    CloseWorkstation {
        workspace_id: Uuid,
        authority: MutationAuthority,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MutationAuthority {
    Workspace {
        workspace_id: Uuid,
    },
    Tab {
        workspace_id: Uuid,
        tab_id: Uuid,
    },
    Pane {
        workspace_id: Uuid,
        tab_id: Uuid,
        pane_id: Uuid,
        transport: TerminalTransport,
    },
}

impl PendingAction {
    pub(super) fn description(&self) -> String {
        match self {
            Self::ReadPane { pane_id, lines, .. } => {
                format!("read {lines} recent lines from pane {pane_id}")
            }
            Self::ReadThread { thread_id, .. } => format!("read saved thread {thread_id}"),
            Self::RecallMemory { query } => format!("recall memory matching {query:?}"),
            Self::ToolCall {
                name, arguments, ..
            } => format!("run {name} with {arguments}"),
            Self::SendInput { pane_id, text, .. } => {
                format!("send potentially destructive input to pane {pane_id}: {text}")
            }
            Self::SendKeys { pane_id, keys, .. } => {
                format!("send keys {} to pane {pane_id}", keys.join(", "))
            }
            Self::CloseTab { tab_id, .. } => format!("close tab {tab_id}"),
            Self::CloseWorkstation { workspace_id, .. } => {
                format!("close workstation {workspace_id}")
            }
        }
    }
}

pub(super) fn pending_action(
    name: &str,
    arguments: &Value,
    snapshot: &SessionSnapshot,
) -> Result<PendingAction> {
    match name {
        "read_pane" => {
            let pane_id = required_uuid(arguments, "pane_id")?;
            let lines = arguments
                .get("lines")
                .and_then(Value::as_u64)
                .and_then(|lines| usize::try_from(lines).ok())
                .unwrap_or(DEFAULT_PANE_LINES)
                .clamp(1, MAX_PANE_LINES);
            Ok(PendingAction::ReadPane {
                pane_id,
                lines,
                authority: mutation_authority_for_pane(snapshot, pane_id)?,
            })
        }
        "read_thread" => {
            let thread_id = required_uuid(arguments, "thread_id")?;
            let thread = threads::read_thread(thread_id)?
                .with_context(|| format!("thread {thread_id} not found"))?;
            let workspace_id = thread
                .workspace_id
                .context("saved thread has no workspace authority")?;
            Ok(PendingAction::ReadThread {
                thread_id,
                workspace_id,
            })
        }
        "recall_memory" => Ok(PendingAction::RecallMemory {
            query: required_str(arguments, "query")?.to_owned(),
        }),
        "send_input" => pending_send_input(arguments, snapshot),
        "send_keys" => pending_send_keys(arguments, snapshot),
        "close_tab" => {
            let tab_id = required_uuid(arguments, "tab_id")?;
            Ok(PendingAction::CloseTab {
                tab_id,
                authority: mutation_authority_for_tab(snapshot, tab_id)?,
            })
        }
        "close_workstation" => {
            let workspace_id = required_uuid(arguments, "workspace_id")?;
            Ok(PendingAction::CloseWorkstation {
                workspace_id,
                authority: mutation_authority_for_workspace(snapshot, workspace_id)?,
            })
        }
        "create_workstation" => Ok(PendingAction::ToolCall {
            name: name.to_owned(),
            arguments: arguments.clone(),
            authority: None,
        }),
        "open_terminal_tab" | "open_project_tab" | "create_worktree_tab" => {
            let workspace_id = required_uuid(arguments, "workspace_id")?;
            Ok(PendingAction::ToolCall {
                name: name.to_owned(),
                arguments: arguments.clone(),
                authority: Some(mutation_authority_for_workspace(snapshot, workspace_id)?),
            })
        }
        "rename_tab" => {
            let tab_id = required_uuid(arguments, "tab_id")?;
            Ok(PendingAction::ToolCall {
                name: name.to_owned(),
                arguments: arguments.clone(),
                authority: Some(mutation_authority_for_tab(snapshot, tab_id)?),
            })
        }
        "launch_agent" => {
            let pane_id = required_uuid(arguments, "pane_id")?;
            Ok(PendingAction::ToolCall {
                name: name.to_owned(),
                arguments: arguments.clone(),
                authority: Some(mutation_authority_for_pane(snapshot, pane_id)?),
            })
        }
        _ => bail!("tool {name} cannot be approved"),
    }
}

fn pending_send_input(arguments: &Value, snapshot: &SessionSnapshot) -> Result<PendingAction> {
    let pane_id = required_uuid(arguments, "pane_id")?;
    Ok(PendingAction::SendInput {
        pane_id,
        text: required_str(arguments, "text")?.to_owned(),
        submit: arguments
            .get("submit")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        authority: mutation_authority_for_pane(snapshot, pane_id)?,
    })
}

fn pending_send_keys(arguments: &Value, snapshot: &SessionSnapshot) -> Result<PendingAction> {
    let keys = arguments
        .get("keys")
        .and_then(Value::as_array)
        .context("keys must be an array")?
        .iter()
        .map(|key| {
            key.as_str()
                .context("each key must be a string")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    for key in &keys {
        let _ = key_bytes(key)?;
    }
    let pane_id = required_uuid(arguments, "pane_id")?;
    Ok(PendingAction::SendKeys {
        pane_id,
        keys,
        authority: mutation_authority_for_pane(snapshot, pane_id)?,
    })
}

pub(super) fn key_bytes(key: &str) -> Result<&'static [u8]> {
    match key {
        "enter" => Ok(b"\r"),
        "esc" => Ok(b"\x1b"),
        "up" => Ok(b"\x1b[A"),
        "down" => Ok(b"\x1b[B"),
        "tab" => Ok(b"\t"),
        "ctrl-c" => Ok(b"\x03"),
        _ => bail!("unsupported key {key}"),
    }
}

pub(super) fn mutation_authority_for_workspace(
    snapshot: &SessionSnapshot,
    workspace_id: Uuid,
) -> Result<MutationAuthority> {
    terminal_workspace(snapshot, workspace_id)?;
    Ok(MutationAuthority::Workspace { workspace_id })
}

pub(super) fn mutation_authority_for_tab(
    snapshot: &SessionSnapshot,
    tab_id: Uuid,
) -> Result<MutationAuthority> {
    snapshot
        .workspaces
        .iter()
        .find_map(|workspace| {
            workspace
                .tabs
                .iter()
                .any(|tab| tab.id == tab_id)
                .then_some(MutationAuthority::Tab {
                    workspace_id: workspace.id,
                    tab_id,
                })
        })
        .with_context(|| format!("tab {tab_id} does not exist"))
}

pub(super) fn mutation_authority_for_pane(
    snapshot: &SessionSnapshot,
    pane_id: Uuid,
) -> Result<MutationAuthority> {
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            let mut panes = Vec::new();
            collect_panes(&tab.layout, &mut panes);
            if let Some(pane) = panes.into_iter().find(|pane| pane.id == pane_id) {
                if !pane.kind.is_terminal() {
                    bail!("pane {pane_id} is not a terminal pane");
                }
                let transport = snapshot
                    .terminal_transports
                    .get(&pane_id)
                    .filter(|transport| !matches!(transport, TerminalTransport::Unknown))
                    .cloned()
                    .with_context(|| format!("pane {pane_id} has no authoritative transport"))?;
                return Ok(MutationAuthority::Pane {
                    workspace_id: workspace.id,
                    tab_id: tab.id,
                    pane_id,
                    transport,
                });
            }
        }
    }
    bail!("pane {pane_id} does not exist")
}

pub(super) fn pane_service_authority(
    authority: &MutationAuthority,
) -> Result<hh_protocol::PaneAuthority> {
    let MutationAuthority::Pane {
        workspace_id,
        tab_id,
        pane_id,
        transport,
    } = authority
    else {
        bail!("approved action does not carry pane authority");
    };
    Ok(hh_protocol::PaneAuthority {
        workspace_id: *workspace_id,
        tab_id: *tab_id,
        pane_id: *pane_id,
        kind: hh_protocol::PaneKind::Terminal,
        transport: transport.clone(),
    })
}

pub(super) fn revalidate_provider_read(
    snapshot: &SessionSnapshot,
    action: &PendingAction,
    authorized_workspaces: &HashSet<Uuid>,
    authorized_root: Option<&Path>,
) -> Result<()> {
    let PendingAction::ReadPane { authority, .. } = action else {
        bail!("provider read does not carry pane authority");
    };
    revalidate_mutation_authority(snapshot, authority, authorized_workspaces, authorized_root)
}

pub(super) fn revalidate_pending_action(
    snapshot: &SessionSnapshot,
    action: &PendingAction,
    authorized_workspaces: &HashSet<Uuid>,
    authorized_root: Option<&Path>,
) -> Result<()> {
    let authority = match action {
        PendingAction::ReadThread { .. } | PendingAction::RecallMemory { .. } => None,
        PendingAction::ToolCall { authority, .. } => authority.as_ref(),
        PendingAction::ReadPane { authority, .. }
        | PendingAction::SendInput { authority, .. }
        | PendingAction::SendKeys { authority, .. }
        | PendingAction::CloseTab { authority, .. }
        | PendingAction::CloseWorkstation { authority, .. } => Some(authority),
    };
    if let Some(authority) = authority {
        revalidate_mutation_authority(snapshot, authority, authorized_workspaces, authorized_root)?;
    }
    if let PendingAction::ToolCall {
        name, arguments, ..
    } = action
    {
        match name.as_str() {
            "create_workstation" if arguments.get("working_dir").is_some() => {
                revalidate_approved_tool_path(action, "working_dir", authorized_root)?;
            }
            "open_project_tab" => {
                revalidate_approved_tool_path(action, "working_dir", authorized_root)?;
            }
            "create_worktree_tab" => {
                revalidate_approved_tool_path(action, "repo_dir", authorized_root)?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn revalidate_mutation_authority(
    snapshot: &SessionSnapshot,
    authority: &MutationAuthority,
    authorized_workspaces: &HashSet<Uuid>,
    authorized_root: Option<&Path>,
) -> Result<()> {
    let workspace_id = match authority {
        MutationAuthority::Workspace { workspace_id }
        | MutationAuthority::Tab { workspace_id, .. }
        | MutationAuthority::Pane { workspace_id, .. } => *workspace_id,
    };
    if !authorized_workspaces.contains(&workspace_id) {
        bail!("workspace {workspace_id} is outside the authorized workspace boundary");
    }
    let workspace = terminal_workspace(snapshot, workspace_id)?;
    if let Some(root) = authorized_root
        && !workspace_directory_is_within(workspace, root)
    {
        bail!("workspace {workspace_id} is outside the canonical authorized root");
    }

    match authority {
        MutationAuthority::Workspace { .. } => Ok(()),
        MutationAuthority::Tab { tab_id, .. } => workspace
            .tabs
            .iter()
            .any(|tab| tab.id == *tab_id)
            .then_some(())
            .with_context(|| format!("tab {tab_id} changed workspace or no longer exists")),
        MutationAuthority::Pane {
            tab_id,
            pane_id,
            transport,
            ..
        } => {
            let tab = workspace
                .tabs
                .iter()
                .find(|tab| tab.id == *tab_id)
                .with_context(|| format!("pane {pane_id} changed workspace or tab"))?;
            let mut panes = Vec::new();
            collect_panes(&tab.layout, &mut panes);
            let pane = panes
                .into_iter()
                .find(|pane| pane.id == *pane_id)
                .with_context(|| format!("pane {pane_id} changed workspace or tab"))?;
            if !pane.kind.is_terminal() {
                bail!("pane {pane_id} is no longer a terminal pane");
            }
            let current_transport = snapshot
                .terminal_transports
                .get(pane_id)
                .filter(|current| !matches!(current, TerminalTransport::Unknown))
                .with_context(|| format!("pane {pane_id} has no authoritative transport"))?;
            if current_transport != transport {
                bail!("pane {pane_id} terminal transport changed");
            }
            Ok(())
        }
    }
}
