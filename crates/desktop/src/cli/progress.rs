use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
/// Hooks run inline in the agent's tool loop: the whole exchange with the
/// service (connect, handshake, report) gets this long, then the hook exits.
const HOOK_DEADLINE: Duration = Duration::from_millis(2500);
/// Claude task files larger than this are not tasks.
const MAX_CLAUDE_TASK_FILE_BYTES: u64 = 256 * 1024;
/// Claude task files read per update; a list this long is not a todo list.
const MAX_CLAUDE_TASK_FILES: usize = 4096;

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
            send(pane_id, Some(progress.clone()))?;
            Ok(json!({ "pane_id": pane_id, "ok": true }))
        }
        ProgressCommand::Clear => {
            let pane_id = required_pane(context)?;
            send(pane_id, None)?;
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

fn send(pane_id: Uuid, progress: Option<PaneProgress>) -> Result<()> {
    let mut client = SessionClient::connect()?;
    match client.call(&ClientRequest::ReportPaneProgress { pane_id, progress })? {
        ServiceResponse::Ack => Ok(()),
        response => anyhow::bail!("unexpected response: {response:?}"),
    }
}

/// Runs `job` on its own thread and gives up waiting after `deadline`; the
/// abandoned thread ends with the process.
fn within<T: Send + 'static>(
    deadline: Duration,
    job: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("hh-progress-hook".into())
        .spawn(move || {
            let _ = sender.send(job());
        })
        .ok()?;
    receiver.recv_timeout(deadline).ok()
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
    // Without `HH_SOCKET` this is not a Harness Harlot terminal; with it the
    // client never falls back to the legacy socket.
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
        ProgressAgent::Claude => claude_hook(&payload, &ClaudeTaskStore::from_env()),
        ProgressAgent::Codex => codex_hook(&payload),
        ProgressAgent::Omp => HookUpdate::Ignore,
    };
    if let HookUpdate::Report(progress) = update {
        // The agent must not notice a stopped, wedged or older service.
        let _ = within(HOOK_DEADLINE, move || send(pane_id, progress));
    }
}

/// Where Claude Code keeps its task lists: one JSON file per task in
/// `<config dir>/tasks/<task list id>/`.
struct ClaudeTaskStore {
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`; `None` without either.
    config_dir: Option<PathBuf>,
    /// `$CLAUDE_CODE_TASK_LIST_ID`, which replaces the session's own list.
    list_id: Option<String>,
}

impl ClaudeTaskStore {
    fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        Self {
            config_dir: var("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".claude"))),
            list_id: var("CLAUDE_CODE_TASK_LIST_ID").and_then(|id| id.into_string().ok()),
        }
    }

    /// The task list the main agent of `session_id` works on.
    fn list_dir(&self, session_id: Option<&str>) -> Option<PathBuf> {
        let id = self.list_id.as_deref().or(session_id)?;
        // The id names one directory; never let it walk elsewhere.
        if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\0']) {
            return None;
        }
        Some(self.config_dir.as_ref()?.join("tasks").join(id))
    }
}

/// Claude Code `PostToolUse` in the main agent: `TodoWrite` carries the whole
/// list (`content`, `status`, `activeForm`); `TaskCreate` and `TaskUpdate`
/// change one task, so the list is re-read from Claude's task store.
fn claude_hook(payload: &str, store: &ClaudeTaskStore) -> HookUpdate {
    let Ok(payload) = serde_json::from_str::<Value>(payload) else {
        return HookUpdate::Ignore;
    };
    // Subagents' task lists are theirs, not the pane's.
    if is_subagent(&payload) {
        return HookUpdate::Ignore;
    }
    match payload.get("tool_name").and_then(Value::as_str) {
        Some("TodoWrite") => claude_todo_write(&payload),
        Some("TaskCreate" | "TaskUpdate") => {
            let session_id = payload.get("session_id").and_then(Value::as_str);
            store
                .list_dir(session_id)
                .map_or(HookUpdate::Ignore, |dir| claude_task_list(&dir))
        }
        _ => HookUpdate::Ignore,
    }
}

fn claude_todo_write(payload: &Value) -> HookUpdate {
    let Some(todos) = payload
        .pointer("/tool_input/todos")
        .and_then(Value::as_array)
    else {
        return HookUpdate::Ignore;
    };
    HookUpdate::Report(count(
        todos.iter().map(|todo| {
            let status = todo.get("status").and_then(Value::as_str);
            (status, || claude_task_text(todo, "content"))
        }),
        ProgressSource::Claude,
    ))
}

/// `activeForm`, else `fallback` (the task's imperative text).
fn claude_task_text(task: &Value, fallback: &str) -> Option<String> {
    ["activeForm", fallback]
        .into_iter()
        .filter_map(|key| task.get(key).and_then(Value::as_str))
        .find_map(clean_text)
}

/// Counts the task files in `dir` in id order: `status` is `pending`,
/// `in_progress`, `completed` or `deleted`, and deleted tasks do not count.
/// Unreadable or malformed files are skipped; no tasks clears the progress.
fn claude_task_list(dir: &Path) -> HookUpdate {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return HookUpdate::Report(None);
        }
        Err(_) => return HookUpdate::Ignore,
    };
    let mut tasks: Vec<((Option<u64>, String), Value)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .take(MAX_CLAUDE_TASK_FILES)
        .filter_map(|entry| {
            let path = entry.path();
            let stem = path
                .extension()
                .is_some_and(|extension| extension == "json")
                .then(|| path.file_stem()?.to_str().map(str::to_owned))??;
            let mut text = String::new();
            std::fs::File::open(&path)
                .ok()?
                .take(MAX_CLAUDE_TASK_FILE_BYTES)
                .read_to_string(&mut text)
                .ok()?;
            let task: Value = serde_json::from_str(&text).ok()?;
            let status = task.get("status")?.as_str()?;
            if status == "deleted" {
                return None;
            }
            let id = match task.get("id") {
                Some(Value::String(id)) => id.parse().ok(),
                Some(Value::Number(id)) => id.as_u64(),
                _ => None,
            }
            .or_else(|| stem.parse().ok());
            Some(((id, stem), task))
        })
        .collect();
    // Numeric ids first, in creation order; anything else after, by name.
    tasks.sort_by(|(left, _), (right, _)| {
        (left.0.is_none(), left.0, &left.1).cmp(&(right.0.is_none(), right.0, &right.1))
    });
    HookUpdate::Report(count(
        tasks.iter().map(|(_, task)| {
            let status = task.get("status").and_then(Value::as_str);
            (status, || claude_task_text(task, "subject"))
        }),
        ProgressSource::Claude,
    ))
}

/// Hook payloads from a subagent carry its `agent_id`.
fn is_subagent(payload: &Value) -> bool {
    payload.get("agent_id").is_some_and(|id| !id.is_null())
}

/// Codex `PostToolUse` for `update_plan` in the main agent: `plan` steps
/// carry `step` and `status` (`pending`, `in_progress`, `completed`).
fn codex_hook(payload: &str) -> HookUpdate {
    let Some(payload) = hook_payload(payload, "update_plan") else {
        return HookUpdate::Ignore;
    };
    // A subagent's plan is its own, not the pane's.
    if is_subagent(&payload) {
        return HookUpdate::Ignore;
    }
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

    /// A Claude environment without a task store.
    const NO_STORE: ClaudeTaskStore = ClaudeTaskStore {
        config_dir: None,
        list_id: None,
    };

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
            claude_hook(&payload.to_string(), &NO_STORE),
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
            claude_hook(&payload.to_string(), &NO_STORE),
            progress(1, 2, Some("Ship it"), ProgressSource::Claude)
        );
        let empty = json!({ "tool_name": "TodoWrite", "tool_input": { "todos": [] } });
        assert_eq!(
            claude_hook(&empty.to_string(), &NO_STORE),
            HookUpdate::Report(None)
        );
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
        assert_eq!(
            claude_hook(&subagent.to_string(), &NO_STORE),
            HookUpdate::Ignore
        );
        let edit = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": "/a", "old_string": "a", "new_string": "b" },
        });
        assert_eq!(
            claude_hook(&edit.to_string(), &NO_STORE),
            HookUpdate::Ignore
        );
        assert_eq!(claude_hook("{ not json", &NO_STORE), HookUpdate::Ignore);
        assert_eq!(claude_hook("", &NO_STORE), HookUpdate::Ignore);
        let no_todos = json!({ "tool_name": "TodoWrite", "tool_input": {} });
        assert_eq!(
            claude_hook(&no_todos.to_string(), &NO_STORE),
            HookUpdate::Ignore
        );
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

    /// A temporary Claude config directory with one session's task files.
    struct TaskStoreFixture {
        root: PathBuf,
    }

    impl TaskStoreFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("hh-claude-tasks-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn store(&self, list_id: Option<&str>) -> ClaudeTaskStore {
            ClaudeTaskStore {
                config_dir: Some(self.root.clone()),
                list_id: list_id.map(str::to_owned),
            }
        }

        fn write(&self, list: &str, name: &str, contents: &str) {
            let dir = self.root.join("tasks").join(list);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), contents).unwrap();
        }

        fn task(&self, list: &str, id: u32, status: &str, subject: &str, active: Option<&str>) {
            let mut task = json!({
                "id": id.to_string(),
                "subject": subject,
                "description": format!("{subject} in detail"),
                "status": status,
                "blocks": [],
                "blockedBy": [],
            });
            if let Some(active) = active {
                task["activeForm"] = json!(active);
            }
            self.write(list, &format!("{id}.json"), &task.to_string());
        }
    }

    impl Drop for TaskStoreFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn task_tool(tool: &str, session_id: &str) -> String {
        json!({
            "session_id": session_id,
            "hook_event_name": "PostToolUse",
            "tool_name": tool,
            "tool_input": { "taskId": "3", "status": "in_progress" },
            "tool_response": { "success": true },
        })
        .to_string()
    }

    #[test]
    fn claude_task_tools_rebuild_counts_from_the_session_task_store() {
        let fixture = TaskStoreFixture::new();
        let session = "5f1c0e2a-session";
        fixture.task(
            session,
            1,
            "completed",
            "Read the parser",
            Some("Reading the parser"),
        );
        fixture.task(session, 2, "completed", "Fix the bug", None);
        fixture.task(session, 3, "deleted", "Abandoned idea", None);
        // Id order, not name order: 10 comes after 9.
        fixture.task(
            session,
            10,
            "in_progress",
            "Write docs",
            Some("Writing docs"),
        );
        fixture.task(
            session,
            9,
            "in_progress",
            "Run tests",
            Some("Running tests"),
        );
        fixture.task(session, 11, "pending", "Ship", None);
        fixture.task(session, 12, "blocked_on_review", "Review", None);
        fixture.write(session, "13.json", "{ not json");
        fixture.write(session, "14.json", r#"{ "id": "14" }"#);
        fixture.write(session, ".lock", "");
        // Another session's list is not this pane's.
        fixture.task("other-session", 1, "pending", "Elsewhere", None);

        for tool in ["TaskCreate", "TaskUpdate"] {
            assert_eq!(
                claude_hook(&task_tool(tool, session), &fixture.store(None)),
                progress(2, 6, Some("Running tests"), ProgressSource::Claude),
                "{tool}"
            );
        }

        // Without activeForm the subject is the current task.
        fixture.task(session, 9, "completed", "Run tests", Some("Running tests"));
        fixture.task(session, 10, "in_progress", "Write docs", None);
        assert_eq!(
            claude_hook(&task_tool("TaskUpdate", session), &fixture.store(None)),
            progress(3, 6, Some("Write docs"), ProgressSource::Claude)
        );
    }

    #[test]
    fn claude_task_list_id_override_and_empty_stores() {
        let fixture = TaskStoreFixture::new();
        fixture.task("shared-list", 1, "pending", "Plan", None);
        assert_eq!(
            claude_hook(
                &task_tool("TaskCreate", "session-a"),
                &fixture.store(Some("shared-list"))
            ),
            progress(0, 1, None, ProgressSource::Claude)
        );

        // A missing list, an empty one and one with only deleted tasks clear.
        let store = fixture.store(None);
        assert_eq!(
            claude_hook(&task_tool("TaskUpdate", "missing"), &store),
            HookUpdate::Report(None)
        );
        std::fs::create_dir_all(fixture.root.join("tasks/empty")).unwrap();
        assert_eq!(
            claude_hook(&task_tool("TaskUpdate", "empty"), &store),
            HookUpdate::Report(None)
        );
        fixture.task("gone", 1, "deleted", "Old", None);
        assert_eq!(
            claude_hook(&task_tool("TaskUpdate", "gone"), &store),
            HookUpdate::Report(None)
        );
    }

    #[test]
    fn claude_task_tools_ignore_subagents_and_unsafe_list_ids() {
        let fixture = TaskStoreFixture::new();
        fixture.task("session", 1, "pending", "Plan", None);
        let store = fixture.store(None);
        let mut subagent: Value =
            serde_json::from_str(&task_tool("TaskUpdate", "session")).unwrap();
        subagent["agent_id"] = json!("agent-7f3");
        assert_eq!(
            claude_hook(&subagent.to_string(), &store),
            HookUpdate::Ignore
        );
        for id in ["", ".", "..", "../session", "a/b"] {
            assert_eq!(
                claude_hook(&task_tool("TaskUpdate", id), &store),
                HookUpdate::Ignore,
                "{id:?}"
            );
        }
        let no_session = json!({ "tool_name": "TaskCreate", "tool_input": {} });
        assert_eq!(
            claude_hook(&no_session.to_string(), &store),
            HookUpdate::Ignore
        );
        assert_eq!(
            claude_hook(&task_tool("TaskUpdate", "session"), &NO_STORE),
            HookUpdate::Ignore
        );
    }

    #[test]
    fn codex_ignores_subagent_plans() {
        let payload = json!({
            "session_id": "0199",
            "hook_event_name": "PostToolUse",
            "agent_id": "019a-sub",
            "tool_name": "update_plan",
            "tool_input": { "plan": [{ "step": "Theirs", "status": "in_progress" }] },
        });
        assert_eq!(codex_hook(&payload.to_string()), HookUpdate::Ignore);
    }

    const HOOK_CHILD_ENV: &str = "HH_PROGRESS_HOOK_TEST_CHILD";

    /// Runs the real hook in a child test process; a no-op in normal runs.
    #[test]
    #[ignore = "child process of a_wedged_service_never_holds_the_hook_past_its_deadline"]
    fn hook_child_process() {
        if std::env::var_os(HOOK_CHILD_ENV).is_some() {
            run_hook(ProgressAgent::Codex);
        }
    }

    #[test]
    fn a_wedged_service_never_holds_the_hook_past_its_deadline() {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        use std::os::unix::net::UnixListener;
        use std::process::{Command, Stdio};
        use std::time::Instant;

        // Short: Unix socket paths are limited to about 100 bytes.
        let dir =
            std::env::temp_dir().join(format!("hh{}", &Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let socket = dir.join("s");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (accepted, connections) = mpsc::channel();
        // Accepts every connection and never answers, like a wedged service.
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
                let _ = accepted.send(());
            }
        });

        let started = Instant::now();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::progress::tests::hook_child_process",
                "--ignored",
                "--test-threads=1",
            ])
            .env(HOOK_CHILD_ENV, "1")
            .env(hh_protocol::SOCKET_ENV, &socket)
            .env(hh_protocol::PANE_ID_ENV, Uuid::new_v4().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let payload = json!({
            "tool_name": "update_plan",
            "tool_input": { "plan": [{ "step": "Wait", "status": "in_progress" }] },
        });
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(payload.to_string().as_bytes()).unwrap();
        drop(stdin);
        let status = child.wait().unwrap();
        let elapsed = started.elapsed();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(status.success());
        assert!(
            connections.try_recv().is_ok(),
            "the hook never reached the service"
        );
        assert!(elapsed < Duration::from_secs(4), "hook took {elapsed:?}");
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
