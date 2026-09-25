use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hh_protocol::{
    ClientRequest, Pane, PaneLayout, PaneStatus, ServiceResponse, SessionSnapshot, Tab, Workspace,
};
use hh_session_client::SessionClient;
use regex::Regex;
use serde_json::{Value, json};
use uuid::Uuid;

use super::agent::{absolute_path, client, snapshot};
use super::args::{
    AgentContext, BotCommand, NewTerminal, TerminalCommand, TerminalInput, TerminalKey,
    WaitRequest, WaitUntil, WorkstationCommand,
};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Screen quiet period after which a pane that is not working counts as idle.
const IDLE_QUIET_MS: u64 = 3_000;
/// Pause between separately written input chunks. Agent TUIs treat text that
/// arrives in the same read as a trailing Enter as a paste and do not submit
/// it, and a lone Escape followed immediately by another sequence parses as
/// an Alt chord.
const INPUT_GAP: Duration = Duration::from_millis(40);
const MAX_INPUT_BYTES: usize = 64 * 1024;
const WAIT_TAIL_LINES: usize = 20;

pub(super) fn execute_terminal(context: &AgentContext, command: &TerminalCommand) -> Result<Value> {
    match command {
        TerminalCommand::List { mine } => list(context, *mine),
        TerminalCommand::New(request) => new_terminal(context, request),
        TerminalCommand::Send { pane, input } => send(*pane, input),
        TerminalCommand::Read { pane, lines } => read(*pane, *lines),
        TerminalCommand::Wait(request) => wait(request),
        TerminalCommand::Focus { pane } => {
            acknowledge(&ClientRequest::ActivateTab { pane_id: *pane })?;
            Ok(json!({ "pane_id": pane, "ok": true }))
        }
        TerminalCommand::Close { pane } => {
            acknowledge(&ClientRequest::ClosePane { pane_id: *pane })?;
            Ok(json!({ "pane_id": pane, "ok": true }))
        }
        TerminalCommand::Rename { tab, title } => {
            acknowledge(&ClientRequest::RenameTab {
                tab_id: *tab,
                title: title.clone(),
            })?;
            Ok(json!({ "tab_id": tab, "title": title, "ok": true }))
        }
    }
}

pub(super) fn execute_workstation(command: &WorkstationCommand) -> Result<Value> {
    match command {
        WorkstationCommand::New { cwd, title } => new_workstation(cwd, title.clone()),
    }
}

pub(super) fn execute_bot(context: &AgentContext, command: &BotCommand) -> Result<Value> {
    match command {
        BotCommand::ReportSession { session } => {
            let pane_id = context.pane_id.context(format!(
                "report-session needs the bot pane; run inside a bot terminal or pass --pane (sets {})",
                hh_protocol::PANE_ID_ENV
            ))?;
            acknowledge(&ClientRequest::ReportBotSession {
                pane_id,
                session_id: session.clone(),
            })?;
            Ok(json!({ "pane_id": pane_id, "session_id": session, "ok": true }))
        }
        BotCommand::Info => {
            let session = Session::fetch(&mut client()?)?;
            let bot = session.caller_bot(context)?;
            let workspace = session
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == bot)
                .context("the calling bot disappeared")?;
            let spec = workspace
                .bot
                .as_ref()
                .context("the calling bot disappeared")?;
            let panes = workspace
                .tabs
                .iter()
                .flat_map(|tab| layout_panes(&tab.layout))
                .map(|pane| pane.id)
                .collect::<Vec<_>>();
            // The active thread is the one the user activated last.
            let active_pane = panes.iter().copied().rev().max_by_key(|pane_id| {
                spec.thread_panes
                    .get(pane_id)
                    .map_or(0, |thread| thread.activated_ms)
            });
            let tab_id = context
                .pane_id
                .and_then(|pane_id| session.locate(pane_id).map(|location| location.tab.id));
            Ok(json!({
                "bot_id": bot,
                "name": workspace.title,
                "tab_id": tab_id,
                "pane_id": context.pane_id,
                "active_pane": active_pane,
                "panes": panes,
            }))
        }
    }
}

fn acknowledge(request: &ClientRequest) -> Result<()> {
    let mut client = client()?;
    call_ack(&mut client, request)
}

fn call_ack(client: &mut SessionClient, request: &ClientRequest) -> Result<()> {
    match client.call(request)? {
        ServiceResponse::Ack => Ok(()),
        response => bail!("unexpected response: {response:?}"),
    }
}

/// Session layout plus the runtime exit state that only the update stream carries.
struct Session {
    snapshot: SessionSnapshot,
    exited: HashSet<Uuid>,
}

struct PaneLocation<'a> {
    workspace: &'a Workspace,
    tab: &'a Tab,
    pane: &'a Pane,
}

impl Session {
    fn fetch(client: &mut SessionClient) -> Result<Self> {
        let response = client.call(&ClientRequest::GetUpdates {
            snapshot_revision: None,
            pane_revisions: Vec::new(),
            subscribed_panes: Vec::new(),
            notifications_after: u64::MAX,
            browser_executor: false,
        })?;
        match response {
            ServiceResponse::Updates {
                snapshot: Some(snapshot),
                pane_states,
                ..
            } => Ok(Self {
                snapshot,
                exited: pane_states
                    .into_iter()
                    .filter(|state| state.exited)
                    .map(|state| state.pane_id)
                    .collect(),
            }),
            response => bail!("unexpected session update response: {response:?}"),
        }
    }

    fn locate(&self, pane_id: Uuid) -> Option<PaneLocation<'_>> {
        self.snapshot.workspaces.iter().find_map(|workspace| {
            workspace.tabs.iter().find_map(|tab| {
                layout_panes(&tab.layout)
                    .into_iter()
                    .find(|pane| pane.id == pane_id)
                    .map(|pane| PaneLocation {
                        workspace,
                        tab,
                        pane,
                    })
            })
        })
    }

    /// The bot (workspace) whose thread pane runs the caller (`HH_PANE_ID`).
    fn caller_bot(&self, context: &AgentContext) -> Result<Uuid> {
        let pane_id = context.pane_id.context(format!(
            "this command needs the calling pane; run inside a bot terminal or pass --pane (sets {})",
            hh_protocol::PANE_ID_ENV
        ))?;
        let location = self
            .locate(pane_id)
            .with_context(|| format!("unknown calling pane {pane_id}"))?;
        ensure!(
            location.workspace.is_bot(),
            "the calling pane {pane_id} is not a bot terminal"
        );
        Ok(location.workspace.id)
    }
}

fn layout_panes(layout: &PaneLayout) -> Vec<&Pane> {
    fn collect<'a>(layout: &'a PaneLayout, panes: &mut Vec<&'a Pane>) {
        match layout {
            PaneLayout::Leaf { pane } => panes.push(pane),
            PaneLayout::Stack { panes: stack, .. } => panes.extend(stack),
            PaneLayout::Split { first, second, .. } => {
                collect(first, panes);
                collect(second, panes);
            }
        }
    }
    let mut panes = Vec::new();
    collect(layout, &mut panes);
    panes
}

fn tab_cwd<'a>(workspace: &'a Workspace, tab: &'a Tab) -> Option<&'a str> {
    tab.project_dir
        .as_deref()
        .or(workspace.working_dir.as_deref())
}

fn list(context: &AgentContext, mine: bool) -> Result<Value> {
    let mut client = client()?;
    let session = Session::fetch(&mut client)?;
    let owner = mine.then(|| session.caller_bot(context)).transpose()?;
    let workstations = session
        .snapshot
        .workspaces
        .iter()
        .filter(|workspace| !workspace.is_bot())
        .filter_map(|workspace| {
            let tabs = workspace
                .tabs
                .iter()
                .filter(|tab| owner.is_none_or(|bot| tab.owner_bot == Some(bot)))
                .filter_map(|tab| {
                    let panes = layout_panes(&tab.layout)
                        .into_iter()
                        .filter(|pane| pane.kind.is_terminal())
                        .map(|pane| {
                            json!({
                                "pane_id": pane.id,
                                "title": pane.title,
                                "profile": pane.identity.profile,
                                "status": pane.status,
                                "status_changed_at_ms": pane.status_changed_at_ms,
                                "exited": session.exited.contains(&pane.id),
                            })
                        })
                        .collect::<Vec<_>>();
                    (!panes.is_empty()).then(|| {
                        json!({
                            "tab_id": tab.id,
                            "title": tab.title,
                            "cwd": tab_cwd(workspace, tab),
                            "owner_bot": tab.owner_bot,
                            "owner_thread": tab.owner_thread,
                            "owner_thread_live": tab
                                .owner_thread
                                .is_some_and(|pane| session.locate(pane).is_some()),
                            "panes": panes,
                        })
                    })
                })
                .collect::<Vec<_>>();
            (owner.is_none() || !tabs.is_empty()).then(|| {
                json!({
                    "workstation_id": workspace.id,
                    "title": workspace.title,
                    "working_dir": workspace.working_dir,
                    "owner_bot": workspace.owner_bot,
                    "tabs": tabs,
                })
            })
        })
        .collect();
    Ok(Value::Array(workstations))
}

fn new_terminal(context: &AgentContext, request: &NewTerminal) -> Result<Value> {
    let mut client = client()?;
    // Without an explicit workstation, a workstation terminal opens workers
    // beside itself; a bot leaves the choice to the service (its own workstation).
    let workspace_id = match (request.workstation, context.workspace_id) {
        (Some(workstation), _) => Some(workstation),
        (None, Some(current)) => snapshot(&mut client)?
            .workspaces
            .iter()
            .any(|workspace| workspace.id == current && !workspace.is_bot())
            .then_some(current),
        (None, None) => None,
    };
    let working_dir = request
        .cwd
        .as_deref()
        .map(|cwd| absolute_path(cwd).map(|path| path.display().to_string()))
        .transpose()?;
    match client.call(&ClientRequest::CreateWorker {
        workspace_id,
        working_dir,
        title: request.title.clone(),
        command: request.command.clone(),
        requester_pane: context.pane_id,
    })? {
        ServiceResponse::WorkerCreated {
            workspace_id,
            tab_id,
            pane_id,
        } => Ok(json!({
            "workstation_id": workspace_id,
            "tab_id": tab_id,
            "pane_id": pane_id,
        })),
        response => bail!("unexpected worker creation response: {response:?}"),
    }
}

fn send(pane_id: Uuid, input: &TerminalInput) -> Result<Value> {
    let mut chunks = Vec::with_capacity(input.keys.len() + 2);
    if let Some(text) = input.text.as_deref().filter(|text| !text.is_empty()) {
        ensure!(
            text.len() <= MAX_INPUT_BYTES,
            "text exceeds {MAX_INPUT_BYTES} bytes"
        );
        chunks.push(text.as_bytes());
    }
    chunks.extend(input.keys.iter().map(|key| key.bytes()));
    if input.enter {
        chunks.push(TerminalKey::Enter.bytes());
    }
    let mut client = client()?;
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            thread::sleep(INPUT_GAP);
        }
        call_ack(
            &mut client,
            &ClientRequest::WriteInput {
                pane_id,
                bytes: chunk.to_vec(),
            },
        )?;
    }
    Ok(json!({
        "pane_id": pane_id,
        "bytes": chunks.iter().map(|chunk| chunk.len()).sum::<usize>(),
    }))
}

fn screen_text(client: &mut SessionClient, pane_id: Uuid) -> Result<String> {
    let screen = match client.call(&ClientRequest::GetPaneSnapshot { pane_id })? {
        ServiceResponse::PaneSnapshot { screen, .. } => screen,
        response => bail!("unexpected pane snapshot response: {response:?}"),
    };
    let lines = screen
        .lines
        .iter()
        .map(|line| {
            let text = line
                .runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>();
            text.trim_end().to_owned()
        })
        .collect::<Vec<_>>();
    let used = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |last| last + 1);
    Ok(lines[..used].join("\n"))
}

fn tail(text: &str, lines: usize) -> &str {
    if lines == 0 {
        return "";
    }
    text.rmatch_indices('\n')
        .nth(lines - 1)
        .map_or(text, |(index, _)| &text[index + 1..])
}

fn terminal_location(session: &Session, pane_id: Uuid) -> Result<PaneLocation<'_>> {
    let location = session
        .locate(pane_id)
        .with_context(|| format!("unknown pane {pane_id}"))?;
    ensure!(
        location.pane.kind.is_terminal(),
        "pane {pane_id} is not a terminal"
    );
    Ok(location)
}

fn read(pane_id: Uuid, lines: Option<usize>) -> Result<Value> {
    let mut client = client()?;
    let session = Session::fetch(&mut client)?;
    let location = terminal_location(&session, pane_id)?;
    let text = screen_text(&mut client, pane_id)?;
    Ok(json!({
        "pane_id": pane_id,
        "tab_id": location.tab.id,
        "workstation_id": location.workspace.id,
        "title": location.tab.title,
        "status": location.pane.status,
        "status_changed_at_ms": location.pane.status_changed_at_ms,
        "exited": session.exited.contains(&pane_id),
        "text": lines.map_or(text.as_str(), |lines| tail(&text, lines)),
    }))
}

fn wait(request: &WaitRequest) -> Result<Value> {
    let pattern = request
        .pattern
        .as_deref()
        .map(Regex::new)
        .transpose()
        .context("wait pattern is not a valid regular expression")?;
    let mut tracker = WaitTracker::new(request.until, pattern, request.timeout_ms);
    let started = Instant::now();
    let mut client = client()?;
    loop {
        let session = Session::fetch(&mut client)?;
        let Some(location) = session.locate(request.pane) else {
            return Ok(json!({ "pane_id": request.pane, "reason": "closed" }));
        };
        ensure!(
            location.pane.kind.is_terminal(),
            "pane {} is not a terminal",
            request.pane
        );
        let exited = session.exited.contains(&request.pane);
        let text = screen_text(&mut client, request.pane)?;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(reason) = tracker.observe(elapsed_ms, location.pane.status, exited, &text) {
            return Ok(json!({
                "pane_id": request.pane,
                "reason": reason.as_str(),
                "title": location.tab.title,
                "status": location.pane.status,
                "exited": exited,
                "tail": tail(&text, WAIT_TAIL_LINES),
            }));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitReason {
    Pattern,
    Exited,
    NeedsYou,
    Done,
    Idle,
    Timeout,
}

impl WaitReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pattern => "pattern",
            Self::Exited => "exited",
            Self::NeedsYou => "needs-you",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Timeout => "timeout",
        }
    }
}

/// Decides when `hh terminal wait` stops, from successive samples of a
/// pane's status, exit state and screen text.
struct WaitTracker {
    until: Option<WaitUntil>,
    pattern: Option<Regex>,
    timeout_ms: u64,
    screen: Option<String>,
    screen_changed_ms: u64,
}

impl WaitTracker {
    fn new(until: Option<WaitUntil>, pattern: Option<Regex>, timeout_ms: u64) -> Self {
        // A pattern alone waits for that pattern; otherwise wait for anything.
        let until = until.or_else(|| pattern.is_none().then_some(WaitUntil::Any));
        Self {
            until,
            pattern,
            timeout_ms,
            screen: None,
            screen_changed_ms: 0,
        }
    }

    fn observe(
        &mut self,
        elapsed_ms: u64,
        status: PaneStatus,
        exited: bool,
        screen: &str,
    ) -> Option<WaitReason> {
        if self.screen.as_deref() != Some(screen) {
            self.screen = Some(screen.to_owned());
            self.screen_changed_ms = elapsed_ms;
        }
        if self
            .pattern
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(screen))
        {
            return Some(WaitReason::Pattern);
        }
        // An exited pane can never reach any other condition.
        if exited {
            return Some(WaitReason::Exited);
        }
        let needs_you = matches!(
            status,
            PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention
        );
        let done = status == PaneStatus::Done;
        let idle = status != PaneStatus::Working
            && elapsed_ms.saturating_sub(self.screen_changed_ms) >= IDLE_QUIET_MS;
        let reached = match self.until {
            Some(WaitUntil::NeedsYou) => needs_you.then_some(WaitReason::NeedsYou),
            Some(WaitUntil::Done) => done.then_some(WaitReason::Done),
            Some(WaitUntil::Idle) => idle.then_some(WaitReason::Idle),
            Some(WaitUntil::Any) => {
                if needs_you {
                    Some(WaitReason::NeedsYou)
                } else if done {
                    Some(WaitReason::Done)
                } else {
                    idle.then_some(WaitReason::Idle)
                }
            }
            Some(WaitUntil::Exited) | None => None,
        };
        reached.or_else(|| (elapsed_ms >= self.timeout_ms).then_some(WaitReason::Timeout))
    }
}

fn new_workstation(cwd: &Path, title: Option<String>) -> Result<Value> {
    let path = absolute_path(cwd)?;
    let directory = fs::canonicalize(&path)
        .with_context(|| format!("workstation directory {} does not exist", path.display()))?;
    ensure!(
        directory.is_dir(),
        "workstation directory {} is not a directory",
        directory.display()
    );
    let directory = directory
        .to_str()
        .context("workstation directory is not valid UTF-8")?
        .to_owned();
    let mut client = client()?;
    match client.call(&ClientRequest::CreateAuthorizedWorkspace {
        title,
        working_dir: directory.clone(),
        authorized_root: directory.clone(),
    })? {
        ServiceResponse::WorkspaceCreated {
            workspace_id,
            pane_id,
        } => Ok(json!({
            "workstation_id": workspace_id,
            "pane_id": pane_id,
            "working_dir": directory,
        })),
        response => bail!("unexpected workstation creation response: {response:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(until: Option<WaitUntil>, pattern: Option<&str>) -> WaitTracker {
        WaitTracker::new(
            until,
            pattern.map(|pattern| Regex::new(pattern).unwrap()),
            10_000,
        )
    }

    #[test]
    fn needs_you_covers_every_attention_status_and_ignores_done() {
        for status in [
            PaneStatus::NeedsApproval,
            PaneStatus::NeedsInput,
            PaneStatus::Attention,
        ] {
            assert_eq!(
                tracker(Some(WaitUntil::NeedsYou), None).observe(0, status, false, "?"),
                Some(WaitReason::NeedsYou)
            );
        }
        let mut waiting = tracker(Some(WaitUntil::NeedsYou), None);
        assert_eq!(waiting.observe(0, PaneStatus::Done, false, "ok"), None);
        assert_eq!(waiting.observe(9_000, PaneStatus::Done, false, "ok"), None);
    }

    #[test]
    fn idle_requires_a_quiet_screen_while_not_working() {
        let mut waiting = tracker(Some(WaitUntil::Idle), None);
        assert_eq!(waiting.observe(0, PaneStatus::Idle, false, "a"), None);
        assert_eq!(waiting.observe(2_000, PaneStatus::Idle, false, "b"), None);
        assert_eq!(waiting.observe(4_000, PaneStatus::Idle, false, "b"), None);
        assert_eq!(
            waiting.observe(5_000, PaneStatus::Idle, false, "b"),
            Some(WaitReason::Idle)
        );

        let mut working = tracker(Some(WaitUntil::Idle), None);
        assert_eq!(working.observe(0, PaneStatus::Working, false, "a"), None);
        assert_eq!(
            working.observe(5_000, PaneStatus::Working, false, "a"),
            None
        );
    }

    #[test]
    fn any_prefers_needs_you_then_done_then_idle() {
        let mut waiting = tracker(None, None);
        assert_eq!(waiting.observe(0, PaneStatus::Working, false, "a"), None);
        assert_eq!(
            waiting.observe(500, PaneStatus::Done, false, "a"),
            Some(WaitReason::Done)
        );
        assert_eq!(
            tracker(Some(WaitUntil::Any), None).observe(0, PaneStatus::NeedsInput, false, "a"),
            Some(WaitReason::NeedsYou)
        );
    }

    #[test]
    fn exit_ends_every_wait_but_a_matching_pattern_wins() {
        for until in [WaitUntil::NeedsYou, WaitUntil::Done, WaitUntil::Exited] {
            assert_eq!(
                tracker(Some(until), None).observe(0, PaneStatus::Working, true, "bye"),
                Some(WaitReason::Exited)
            );
        }
        assert_eq!(
            tracker(Some(WaitUntil::Exited), Some("b.e")).observe(0, PaneStatus::Idle, true, "bye"),
            Some(WaitReason::Pattern)
        );
    }

    #[test]
    fn a_pattern_alone_ignores_status_until_it_matches_or_times_out() {
        let mut waiting = tracker(None, Some(r"\[y/N\]"));
        assert_eq!(waiting.observe(0, PaneStatus::Done, false, "done"), None);
        assert_eq!(
            waiting.observe(5_000, PaneStatus::Idle, false, "done"),
            None
        );
        assert_eq!(
            waiting.observe(6_000, PaneStatus::Idle, false, "Proceed? [y/N]"),
            Some(WaitReason::Pattern)
        );
        assert_eq!(
            tracker(None, Some("never")).observe(10_000, PaneStatus::Idle, false, "x"),
            Some(WaitReason::Timeout)
        );
    }

    #[test]
    fn tail_returns_the_last_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a\nb\nc", 5), "a\nb\nc");
        assert_eq!(tail("a\nb\nc", 0), "");
    }
}
