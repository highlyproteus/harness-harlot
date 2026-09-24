use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use hh_protocol::{
    BrowserAction, BrowserCommandOutcome, ClientRequest, Pane, PaneKind, PaneLayout,
    ServiceResponse, SessionSnapshot,
};
use hh_session_client::SessionClient;
use serde_json::{Value, json};
use uuid::Uuid;

use super::args::{AgentAction, AgentCommand, AgentContext, BrowserCommand, GalleryCommand};
use super::terminal;

const BROWSER_TIMEOUT: Duration = Duration::from_secs(45);
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn execute(command: &AgentCommand) -> Result<Value> {
    match &command.action {
        AgentAction::Browser(browser) => execute_browser(&command.context, browser),
        AgentAction::Gallery(gallery) => execute_gallery(&command.context, gallery),
        AgentAction::Terminal(terminal_command) => {
            terminal::execute_terminal(&command.context, terminal_command)
        }
        AgentAction::Workstation(workstation) => terminal::execute_workstation(workstation),
        AgentAction::Mcp | AgentAction::Skill(_) => {
            bail!("agent action must be dispatched directly")
        }
    }
}

pub(crate) fn print_result(result: &Value, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else if let Some(text) = result.as_str() {
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(result)?);
    }
    Ok(())
}

pub(super) fn client() -> Result<SessionClient> {
    let client = SessionClient::connect()?;
    client.set_read_timeout(BROWSER_TIMEOUT)?;
    Ok(client)
}

fn execute_browser(context: &AgentContext, command: &BrowserCommand) -> Result<Value> {
    match command {
        BrowserCommand::List => browser_list(),
        BrowserCommand::Open { url, group } => browser_open(context, url.clone(), *group),
        BrowserCommand::Goto { url } => browser_goto(required_pane(context)?, url),
        BrowserCommand::Back => simple_browser_action(required_pane(context)?, BrowserAction::Back),
        BrowserCommand::Forward => {
            simple_browser_action(required_pane(context)?, BrowserAction::Forward)
        }
        BrowserCommand::Reload => {
            simple_browser_action(required_pane(context)?, BrowserAction::Reload)
        }
        BrowserCommand::Read { selector } => {
            browser_read(required_pane(context)?, selector.as_deref())
        }
        BrowserCommand::Eval { expression } => browser_eval(required_pane(context)?, expression),
        BrowserCommand::Screenshot { output } => {
            browser_screenshot(context, required_pane(context)?, output.as_deref())
        }
        BrowserCommand::Click { selector } => browser_click(required_pane(context)?, selector),
        BrowserCommand::Fill { selector, value } => {
            browser_fill(required_pane(context)?, selector, value)
        }
        BrowserCommand::Type { text } => browser_type(required_pane(context)?, text),
        BrowserCommand::Press { key } => browser_press(required_pane(context)?, key),
        BrowserCommand::Cdp { method, params } => {
            cdp(required_pane(context)?, method, params.clone())
        }
    }
}

fn execute_gallery(context: &AgentContext, command: &GalleryCommand) -> Result<Value> {
    match command {
        GalleryCommand::Add { source } => gallery_add(context, source),
        GalleryCommand::List => gallery_list(context),
        GalleryCommand::Dir => Ok(Value::String(
            gallery_directory(context)?.display().to_string(),
        )),
    }
}

fn required_pane(context: &AgentContext) -> Result<Uuid> {
    context.pane_id.context(format!(
        "browser pane is required; pass --pane or set {}",
        hh_protocol::PANE_ID_ENV
    ))
}

fn required_workspace(context: &AgentContext) -> Result<Uuid> {
    context.workspace_id.context(format!(
        "workspace is required; pass --workspace or set {}",
        hh_protocol::WORKSPACE_ID_ENV
    ))
}

pub(super) fn snapshot(client: &mut SessionClient) -> Result<SessionSnapshot> {
    match client.call(&ClientRequest::GetSnapshot)? {
        ServiceResponse::Snapshot { snapshot } => Ok(snapshot),
        response => bail!("unexpected snapshot response: {response:?}"),
    }
}

fn browser_list() -> Result<Value> {
    let mut client = client()?;
    let snapshot = snapshot(&mut client)?;
    let mut browsers = Vec::new();
    for workspace in snapshot.workspaces {
        for tab in workspace.tabs {
            visit_panes(&tab.layout, &mut |pane| {
                if let PaneKind::Browser { url } = &pane.kind {
                    browsers.push(json!({
                        "workspace_id": workspace.id,
                        "workspace_title": workspace.title,
                        "tab_id": tab.id,
                        "tab_title": tab.title,
                        "pane_id": pane.id,
                        "pane_title": pane.title,
                        "url": url,
                    }));
                }
            });
        }
    }
    Ok(Value::Array(browsers))
}

fn visit_panes(layout: &PaneLayout, visitor: &mut impl FnMut(&Pane)) {
    match layout {
        PaneLayout::Leaf { pane } => visitor(pane),
        PaneLayout::Stack { panes, .. } => panes.iter().for_each(visitor),
        PaneLayout::Split { first, second, .. } => {
            visit_panes(first, visitor);
            visit_panes(second, visitor);
        }
    }
}

fn browser_open(context: &AgentContext, url: Option<String>, force_group: bool) -> Result<Value> {
    let mut client = client()?;
    let request = if force_group || context.pane_id.is_some() {
        ClientRequest::CreateGroupBrowser {
            target_pane: required_pane(context)?,
            url,
        }
    } else {
        ClientRequest::CreateBrowserTab {
            workspace_id: required_workspace(context)?,
            url,
        }
    };
    match client.call(&request)? {
        ServiceResponse::PaneCreated { pane_id } => Ok(json!({ "pane_id": pane_id })),
        response => bail!("unexpected browser creation response: {response:?}"),
    }
}

fn call_browser(pane_id: Uuid, action: BrowserAction) -> Result<Value> {
    let mut client = client()?;
    match client.call(&ClientRequest::BrowserCommand { pane_id, action })? {
        ServiceResponse::BrowserCommandResult {
            outcome: BrowserCommandOutcome::Ok { result },
        } => Ok(result),
        ServiceResponse::BrowserCommandResult {
            outcome: BrowserCommandOutcome::Error { message },
        } => bail!("browser command failed: {message}"),
        response => bail!("unexpected browser command response: {response:?}"),
    }
}

fn simple_browser_action(pane_id: Uuid, action: BrowserAction) -> Result<Value> {
    call_browser(pane_id, action)?;
    Ok(json!({ "pane_id": pane_id, "ok": true }))
}

pub(crate) fn cdp(pane_id: Uuid, method: &str, params: Value) -> Result<Value> {
    call_browser(
        pane_id,
        BrowserAction::DevTools {
            method: method.to_owned(),
            params,
        },
    )
}

fn runtime_evaluate(pane_id: Uuid, expression: &str) -> Result<Value> {
    let response = cdp(
        pane_id,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "awaitPromise": true,
            "returnByValue": true,
        }),
    )?;
    if let Some(exception) = response.get("exceptionDetails") {
        bail!("JavaScript evaluation failed: {exception}");
    }
    let remote = response
        .get("result")
        .context("CDP Runtime.evaluate omitted result")?;
    if remote.get("subtype").and_then(Value::as_str) == Some("error") {
        bail!(
            "JavaScript evaluation failed: {}",
            remote
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        );
    }
    Ok(remote.get("value").cloned().unwrap_or(Value::Null))
}

fn browser_eval(pane_id: Uuid, expression: &str) -> Result<Value> {
    runtime_evaluate(pane_id, expression)
}

fn browser_read(pane_id: Uuid, selector: Option<&str>) -> Result<Value> {
    let target = serde_json::to_string(&selector.unwrap_or("body"))?;
    let expression = format!(
        "(() => {{ const node = document.querySelector({target}); return node === null ? null : (node.innerText ?? node.textContent ?? ''); }})()"
    );
    let value = runtime_evaluate(pane_id, &expression)?;
    ensure!(
        !value.is_null(),
        "no element matches {}",
        selector.unwrap_or("body")
    );
    Ok(value)
}

fn current_location(pane_id: Uuid) -> Result<Option<(String, String)>> {
    let value = runtime_evaluate(
        pane_id,
        "({ href: window.location.href, readyState: document.readyState })",
    )?;
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    Ok(Some((
        object
            .get("href")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        object
            .get("readyState")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    )))
}

fn browser_goto(pane_id: Uuid, url: &str) -> Result<Value> {
    let previous = current_location(pane_id)
        .ok()
        .flatten()
        .map(|(href, _)| href);
    call_browser(
        pane_id,
        BrowserAction::Navigate {
            url: url.to_owned(),
        },
    )?;
    let deadline = Instant::now() + NAVIGATION_TIMEOUT;
    loop {
        if let Ok(Some((href, ready_state))) = current_location(pane_id)
            && ready_state == "complete"
            && previous.as_deref() != Some(href.as_str())
        {
            return Ok(json!({ "pane_id": pane_id, "url": href, "ready_state": ready_state }));
        }
        ensure!(
            Instant::now() < deadline,
            "browser navigation timed out after 30 seconds"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn browser_click(pane_id: Uuid, selector: &str) -> Result<Value> {
    let selector_json = serde_json::to_string(selector)?;
    let value = runtime_evaluate(
        pane_id,
        &format!(
            "(() => {{ const node = document.querySelector({selector_json}); if (!node) return null; node.scrollIntoView({{block:'center',inline:'center'}}); const rect = node.getBoundingClientRect(); return {{x:rect.left+rect.width/2,y:rect.top+rect.height/2}}; }})()"
        ),
    )?;
    let center = value
        .as_object()
        .with_context(|| format!("no element matches {selector}"))?;
    let x = center
        .get("x")
        .and_then(Value::as_f64)
        .context("element has no x coordinate")?;
    let y = center
        .get("y")
        .and_then(Value::as_f64)
        .context("element has no y coordinate")?;
    for kind in ["mousePressed", "mouseReleased"] {
        cdp(
            pane_id,
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )?;
    }
    Ok(json!({ "pane_id": pane_id, "selector": selector, "x": x, "y": y }))
}

fn browser_fill(pane_id: Uuid, selector: &str, value: &str) -> Result<Value> {
    let selector_json = serde_json::to_string(selector)?;
    let value_json = serde_json::to_string(value)?;
    let result = runtime_evaluate(
        pane_id,
        &format!(
            "(() => {{ const node = document.querySelector({selector_json}); if (!node) return false; node.focus(); const setter = Object.getOwnPropertyDescriptor(node instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value')?.set; if (setter) setter.call(node, {value_json}); else node.value = {value_json}; node.dispatchEvent(new Event('input', {{bubbles:true}})); node.dispatchEvent(new Event('change', {{bubbles:true}})); return true; }})()"
        ),
    )?;
    ensure!(
        result == Value::Bool(true),
        "no form control matches {selector}"
    );
    Ok(json!({ "pane_id": pane_id, "selector": selector, "ok": true }))
}

fn browser_type(pane_id: Uuid, text: &str) -> Result<Value> {
    cdp(pane_id, "Input.insertText", json!({ "text": text }))?;
    Ok(json!({ "pane_id": pane_id, "ok": true }))
}

fn browser_press(pane_id: Uuid, key: &str) -> Result<Value> {
    let (code, windows_virtual_key_code) = match key {
        "Enter" => ("Enter", 13),
        "Tab" => ("Tab", 9),
        "Escape" | "Esc" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Delete" => ("Delete", 46),
        "ArrowLeft" => ("ArrowLeft", 37),
        "ArrowUp" => ("ArrowUp", 38),
        "ArrowRight" => ("ArrowRight", 39),
        "ArrowDown" => ("ArrowDown", 40),
        other => (other, 0),
    };
    for kind in ["rawKeyDown", "keyUp"] {
        cdp(
            pane_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": kind,
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": windows_virtual_key_code,
                "nativeVirtualKeyCode": windows_virtual_key_code,
            }),
        )?;
    }
    Ok(json!({ "pane_id": pane_id, "key": key, "ok": true }))
}

fn browser_screenshot(
    context: &AgentContext,
    pane_id: Uuid,
    output: Option<&Path>,
) -> Result<Value> {
    let result = cdp(
        pane_id,
        "Page.captureScreenshot",
        json!({ "format": "png", "fromSurface": true }),
    )?;
    let data = result
        .get("data")
        .and_then(Value::as_str)
        .context("CDP screenshot omitted image data")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("decode CDP screenshot")?;
    if let Some(output) = output {
        let path = absolute_path(output)?;
        write_private_file(&path, &bytes)?;
        return Ok(json!({ "path": path, "pane_id": pane_id }));
    }

    let temporary = std::env::temp_dir().join(format!("hh-browser-{}.png", Uuid::new_v4()));
    write_private_file(&temporary, &bytes)?;
    let added = gallery_add(context, &temporary);
    let _ = fs::remove_file(&temporary);
    added
}

pub(super) fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("output path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn gallery_add(context: &AgentContext, source: &Path) -> Result<Value> {
    let source = absolute_path(source)?;
    let mut client = client()?;
    match client.call(&ClientRequest::AddGalleryImage {
        workspace_id: required_workspace(context)?,
        origin_pane: context.pane_id,
        source: source.display().to_string(),
    })? {
        ServiceResponse::GalleryImageAdded { path, pane_id } => {
            Ok(json!({ "path": path, "pane_id": pane_id }))
        }
        response => bail!("unexpected gallery add response: {response:?}"),
    }
}

fn gallery_directory(context: &AgentContext) -> Result<PathBuf> {
    if let Some(directory) = &context.gallery_dir {
        return Ok(directory.clone());
    }
    hh_protocol::gallery_directory(required_workspace(context)?)
        .context("HOME is not set and no gallery directory was supplied")
}

fn gallery_list(context: &AgentContext) -> Result<Value> {
    let directory = gallery_directory(context)?;
    let mut images = Vec::new();
    match fs::read_dir(&directory) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.with_context(|| format!("read {}", directory.display()))?;
                let file_type = entry.file_type()?;
                if !file_type.is_file() {
                    continue;
                }
                let path = entry.path();
                if is_image_path(&path) {
                    let metadata = entry.metadata()?;
                    images.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "path": path,
                        "bytes": metadata.len(),
                    }));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", directory.display())),
    }
    images.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    Ok(Value::Array(images))
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}
