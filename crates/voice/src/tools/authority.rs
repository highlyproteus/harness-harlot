use anyhow::{Context, Result, bail};
use hh_protocol::{SessionSnapshot, TerminalTransport};
use serde_json::Value;
use uuid::Uuid;

use super::{
    mutation_authority_for_pane, mutation_authority_for_tab, mutation_authority_for_workspace,
    required_str, required_uuid,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PendingAction {
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
