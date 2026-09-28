use std::io::Read as _;
use std::time::Duration;

use anyhow::{Context, Result};
use hh_protocol::{
    ClientRequest, MAX_PROGRESS_TASKS, MAX_PROGRESS_TEXT_CHARS, PaneProgress, ProgressSource,
    ServiceResponse,
};
use hh_session_client::SessionClient;
use serde_json::{Value, json};
use uuid::Uuid;

use super::args::{AgentContext, ProgressCommand};
use crate::agent_progress::{self, ProgressAgent};

/// Hook payloads larger than this are not todo updates worth reading.
const MAX_HOOK_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024;
/// Hooks run inline in the agent's tool loop, so the service gets little time.
const HOOK_RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);

/// Strips control characters (runs become one space) and truncates to the
/// protocol's text bound; blank text is `None`.
pub(super) fn clean_text(text: &str) -> Option<String> {
    let mut cleaned = String::with_capacity(text.len().min(MAX_PROGRESS_TEXT_CHARS * 4));
    let mut pending_space = false;
    for character in text.trim().chars() {
        if character.is_control() {
            pending_space = true;
            continue;
        }
        if pending_space && !cleaned.is_empty() {
            cleaned.push(' ');
        }
        pending_space = false;
        cleaned.push(character);
    }
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.chars().take(MAX_PROGRESS_TEXT_CHARS).collect())
}

pub(super) fn execute(context: &AgentContext, command: &ProgressCommand) -> Result<Value> {
    match command {
        ProgressCommand::Report(progress) => {
            let pane_id = required_pane(context)?;
            send(pane_id, Some(progress.clone()), None)?;
            Ok(json!({ "pane_id": pane_id, "ok": true }))
        }
        ProgressCommand::Clear => {
            let pane_id = required_pane(context)?;
            send(pane_id, None, None)?;
            Ok(json!({ "pane_id": pane_id, "ok": true }))
        }
        ProgressCommand::Hook(agent) => {
            run_hook(*agent);
            Ok(Value::Null)
        }
        ProgressCommand::Install(agent) => {
            let report = agent_progress::install(*agent)?;
            if context.json {
                return Ok(json!({
                    "agent": agent.id(),
                    "path": report.path,
                    "note": report.note,
                }));
            }
            let mut text = format!("installed {}", report.path.display());
            if let Some(note) = report.note {
                text.push_str("\nnote: ");
                text.push_str(note);
            }
            Ok(Value::String(text))
        }
        ProgressCommand::Uninstall(agent) => {
            agent_progress::uninstall(*agent)?;
            let path = agent_progress::target_path(*agent)?;
            if context.json {
                return Ok(json!({ "agent": agent.id(), "path": path, "ok": true }));
            }
            Ok(Value::String(format!(
                "removed Harness Harlot progress from {}",
                path.display()
            )))
        }
        ProgressCommand::Status => status(context.json),
    }
}

fn status(json_output: bool) -> Result<Value> {
    let mut rows = Vec::with_capacity(ProgressAgent::ALL.len());
    let mut lines = Vec::with_capacity(ProgressAgent::ALL.len());
    for agent in ProgressAgent::ALL {
        let path = agent_progress::target_path(agent)?;
        let (state, error) = match agent_progress::status(agent) {
            Ok(state) => (Some(state.id()), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        lines.push(format!(
            "{}: {} ({})",
            agent.id(),
            error.as_deref().map_or_else(
                || state.unwrap_or_default().replace('_', " "),
                |error| { format!("error: {error}") }
            ),
            path.display()
        ));
        rows.push(json!({
            "agent": agent.id(),
            "state": state,
            "path": path,
            "error": error,
        }));
    }
    Ok(if json_output {
        Value::Array(rows)
    } else {
        Value::String(lines.join("\n"))
    })
}

fn required_pane(context: &AgentContext) -> Result<Uuid> {
    context.pane_id.context(format!(
        "progress needs the agent's pane; run inside a Harness Harlot terminal or pass --pane (sets {})",
        hh_protocol::PANE_ID_ENV
    ))
}

fn send(pane_id: Uuid, progress: Option<PaneProgress>, timeout: Option<Duration>) -> Result<()> {
    let mut client = SessionClient::connect()?;
    if let Some(timeout) = timeout {
        client.set_read_timeout(timeout)?;
    }
    match client.call(&ClientRequest::ReportPaneProgress { pane_id, progress })? {
        ServiceResponse::Ack => Ok(()),
        response => anyhow::bail!("unexpected response: {response:?}"),
    }
}

/// What one hook payload means for the pane's progress.
#[derive(Debug, Eq, PartialEq)]
enum HookUpdate {
    /// Not a main-agent todo update.
    Ignore,
    /// Replace the pane's progress; `None` clears it (the list is empty).
    Report(Option<PaneProgress>),
}

/// Runs a `PostToolUse` hook: never fails, never writes to stdout, and does
/// nothing outside a Harness Harlot terminal.
pub(super) fn run_hook(agent: ProgressAgent) {
    let mut payload = String::new();
    if std::io::stdin()
        .take(MAX_HOOK_PAYLOAD_BYTES)
        .read_to_string(&mut payload)
        .is_err()
    {
        return;
    }
    if std::env::var_os(hh_protocol::SOCKET_ENV).is_none() {
        return;
    }
    let Some(pane_id) = std::env::var(hh_protocol::PANE_ID_ENV)
        .ok()
        .and_then(|value| Uuid::parse_str(&value).ok())
    else {
        return;
    };
    let update = match agent {
        ProgressAgent::Claude => claude_hook(&payload),
        ProgressAgent::Codex => codex_hook(&payload),
        ProgressAgent::Omp => HookUpdate::Ignore,
    };
    if let HookUpdate::Report(progress) = update {
        // The agent must not notice a stopped or older service.
        let _ = send(pane_id, progress, Some(HOOK_RESPONSE_TIMEOUT));
    }
}

/// Claude Code `PostToolUse` for `TodoWrite` in the main agent: todos carry
/// `content`, `status` (`pending`, `in_progress`, `completed`) and `activeForm`.
fn claude_hook(payload: &str) -> HookUpdate {
    let Some(payload) = hook_payload(payload, "TodoWrite") else {
        return HookUpdate::Ignore;
    };
    // Subagents' todo lists are theirs, not the pane's.
    if payload.get("agent_id").is_some_and(|id| !id.is_null()) {
        return HookUpdate::Ignore;
    }
    let Some(todos) = payload
        .pointer("/tool_input/todos")
        .and_then(Value::as_array)
    else {
        return HookUpdate::Ignore;
    };
    HookUpdate::Report(count(
        todos.iter().map(|todo| {
            let status = todo.get("status").and_then(Value::as_str);
            let text = || {
                ["activeForm", "content"]
                    .into_iter()
                    .filter_map(|key| todo.get(key).and_then(Value::as_str))
                    .find_map(clean_text)
            };
            (status, text)
        }),
        ProgressSource::Claude,
    ))
}

/// Codex `PostToolUse` for `update_plan`: `plan` steps carry `step` and
/// `status` (`pending`, `in_progress`, `completed`).
fn codex_hook(payload: &str) -> HookUpdate {
    let Some(payload) = hook_payload(payload, "update_plan") else {
        return HookUpdate::Ignore;
    };
    let Some(plan) = payload
        .pointer("/tool_input/plan")
        .and_then(Value::as_array)
    else {
        return HookUpdate::Ignore;
    };
    HookUpdate::Report(count(
        plan.iter().map(|item| {
            let status = item.get("status").and_then(Value::as_str);
            let text = || {
                item.get("step")
                    .and_then(Value::as_str)
                    .and_then(clean_text)
            };
            (status, text)
        }),
        ProgressSource::Codex,
    ))
}

fn hook_payload(payload: &str, tool: &str) -> Option<Value> {
    let payload: Value = serde_json::from_str(payload).ok()?;
    (payload.get("tool_name").and_then(Value::as_str) == Some(tool)).then_some(payload)
}

/// Completed tasks of all but abandoned ones; `current` is the first task in
/// progress. An empty list clears the pane's progress.
fn count<'a, T>(
    tasks: impl Iterator<Item = (Option<&'a str>, T)>,
    source: ProgressSource,
) -> Option<PaneProgress>
where
    T: FnOnce() -> Option<String>,
{
    let mut done = 0_u32;
    let mut total = 0_u32;
    let mut current = None;
    for (status, text) in tasks {
        match status {
            Some("abandoned" | "cancelled" | "canceled") => continue,
            Some("completed") => done += 1,
            Some("in_progress") if current.is_none() => current = text(),
            _ => {}
        }
        total += 1;
    }
    if total == 0 {
        return None;
    }
    let total = total.min(MAX_PROGRESS_TASKS);
    Some(PaneProgress {
        done: done.min(total),
        total,
        current,
        phase: None,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(
        done: u32,
        total: u32,
        current: Option<&str>,
        source: ProgressSource,
    ) -> HookUpdate {
        HookUpdate::Report(Some(PaneProgress {
            done,
            total,
            current: current.map(str::to_owned),
            phase: None,
            source,
        }))
    }

    #[test]
    fn claude_todo_write_reports_counts_and_the_active_form() {
        let payload = json!({
            "session_id": "abc123",
            "transcript_path": "/Users/me/.claude/projects/app/abc123.jsonl",
            "cwd": "/Users/me/app",
            "permission_mode": "default",
            "hook_event_name": "PostToolUse",
            "tool_name": "TodoWrite",
            "tool_input": { "todos": [
                { "content": "Read the parser", "status": "completed", "activeForm": "Reading the parser" },
                { "content": "Fix the bug", "status": "in_progress", "activeForm": "Fixing the bug" },
                { "content": "Run tests", "status": "pending", "activeForm": "Running tests" },
            ]},
            "tool_response": { "oldTodos": [], "newTodos": [] },
        });
        assert_eq!(
            claude_hook(&payload.to_string()),
            progress(1, 3, Some("Fixing the bug"), ProgressSource::Claude)
        );
    }

    #[test]
    fn claude_falls_back_to_content_and_clears_on_an_empty_list() {
        let payload = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "TodoWrite",
            "tool_input": { "todos": [
                { "content": "Ship\nit", "status": "in_progress" },
                { "content": "Done", "status": "completed" },
            ]},
        });
        assert_eq!(
            claude_hook(&payload.to_string()),
            progress(1, 2, Some("Ship it"), ProgressSource::Claude)
        );
        let empty = json!({ "tool_name": "TodoWrite", "tool_input": { "todos": [] } });
        assert_eq!(claude_hook(&empty.to_string()), HookUpdate::Report(None));
    }

    #[test]
    fn claude_ignores_subagents_other_tools_and_malformed_payloads() {
        let subagent = json!({
            "hook_event_name": "PostToolUse",
            "agent_id": "agent-7f3",
            "agent_type": "general-purpose",
            "tool_name": "TodoWrite",
            "tool_input": { "todos": [{ "content": "x", "status": "pending", "activeForm": "x" }] },
        });
        assert_eq!(claude_hook(&subagent.to_string()), HookUpdate::Ignore);
        let edit = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": "/a", "old_string": "a", "new_string": "b" },
        });
        assert_eq!(claude_hook(&edit.to_string()), HookUpdate::Ignore);
        assert_eq!(claude_hook("{ not json"), HookUpdate::Ignore);
        assert_eq!(claude_hook(""), HookUpdate::Ignore);
        let no_todos = json!({ "tool_name": "TodoWrite", "tool_input": {} });
        assert_eq!(claude_hook(&no_todos.to_string()), HookUpdate::Ignore);
    }

    #[test]
    fn codex_update_plan_reports_steps() {
        let payload = json!({
            "session_id": "0199",
            "cwd": "/Users/me/app",
            "hook_event_name": "PostToolUse",
            "model": "gpt-5-codex",
            "tool_name": "update_plan",
            "tool_use_id": "call_1",
            "tool_input": {
                "explanation": "Starting the fix",
                "plan": [
                    { "step": "Inspect config loading", "status": "completed" },
                    { "step": "Patch the loader", "status": "in_progress" },
                    { "step": "Add a regression test", "status": "pending" },
                    { "step": "Update docs", "status": "pending" },
                ],
            },
            "tool_response": "Plan updated",
        });
        assert_eq!(
            codex_hook(&payload.to_string()),
            progress(1, 4, Some("Patch the loader"), ProgressSource::Codex)
        );
        let shell = json!({ "tool_name": "shell", "tool_input": { "command": ["ls"] } });
        assert_eq!(codex_hook(&shell.to_string()), HookUpdate::Ignore);
        assert_eq!(codex_hook("[]"), HookUpdate::Ignore);
    }

    #[test]
    fn text_is_single_line_and_bounded() {
        assert_eq!(clean_text("  a\u{1b}[31m\r\nb  "), Some("a [31m b".into()));
        assert_eq!(clean_text("\n\t"), None);
        assert_eq!(
            clean_text(&"x".repeat(MAX_PROGRESS_TEXT_CHARS + 50))
                .unwrap()
                .len(),
            MAX_PROGRESS_TEXT_CHARS
        );
    }
}
