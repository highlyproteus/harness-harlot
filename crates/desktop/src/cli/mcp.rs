use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::agent;
use super::args::{AgentAction, AgentCommand, AgentContext, BrowserCommand, GalleryCommand};

pub(crate) fn server_config(command: &Path) -> Value {
    json!({
        "mcpServers": {
            "harness-harlot": {
                "command": command,
                "args": ["mcp"]
            }
        }
    })
}

pub(crate) fn run(context: &AgentContext) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("read MCP request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(&line).context("parse MCP JSON-RPC request")?;
        if request.get("id").is_none() {
            continue;
        }
        let response = handle_request(context, &request);
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle_request(base_context: &AgentContext, request: &Value) -> Value {
    let null_id = Value::Null;
    let id = request.get("id").unwrap_or(&null_id);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let outcome = match method {
        "initialize" => Ok(json!({
            "protocolVersion": request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05"),
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "harness-harlot",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(base_context, request.get("params").unwrap_or(&Value::Null)),
        _ => return json_rpc_error(id, -32601, &format!("method not found: {method}")),
    };
    match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json_rpc_error(id, -32602, &format!("{error:#}")),
    }
}

fn json_rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn call_tool(base_context: &AgentContext, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call requires a tool name")?;
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let command = tool_command(base_context, name, &arguments)?;
    match agent::execute(&command) {
        Ok(value) => Ok(json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value)? }],
            "structuredContent": value,
            "isError": false,
        })),
        Err(error) => Ok(json!({
            "content": [{ "type": "text", "text": format!("{error:#}") }],
            "isError": true,
        })),
    }
}

fn tool_command(
    base_context: &AgentContext,
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<AgentCommand> {
    let context = context_from_arguments(base_context, arguments)?;
    let action = match name {
        "browser_list" => AgentAction::Browser(BrowserCommand::List),
        "browser_open" => AgentAction::Browser(BrowserCommand::Open {
            url: optional_string(arguments, "url")?,
            group: optional_bool(arguments, "group")?.unwrap_or(false),
        }),
        "browser_goto" => AgentAction::Browser(BrowserCommand::Goto {
            url: required_string(arguments, "url")?,
        }),
        "browser_back" => AgentAction::Browser(BrowserCommand::Back),
        "browser_forward" => AgentAction::Browser(BrowserCommand::Forward),
        "browser_reload" => AgentAction::Browser(BrowserCommand::Reload),
        "browser_read" => AgentAction::Browser(BrowserCommand::Read {
            selector: optional_string(arguments, "selector")?,
        }),
        "browser_eval" => AgentAction::Browser(BrowserCommand::Eval {
            expression: required_string(arguments, "expression")?,
        }),
        "browser_screenshot" => AgentAction::Browser(BrowserCommand::Screenshot {
            output: optional_string(arguments, "output")?.map(PathBuf::from),
        }),
        "browser_click" => AgentAction::Browser(BrowserCommand::Click {
            selector: required_string(arguments, "selector")?,
        }),
        "browser_fill" => AgentAction::Browser(BrowserCommand::Fill {
            selector: required_string(arguments, "selector")?,
            value: required_string(arguments, "value")?,
        }),
        "browser_type" => AgentAction::Browser(BrowserCommand::Type {
            text: required_string(arguments, "text")?,
        }),
        "browser_press" => AgentAction::Browser(BrowserCommand::Press {
            key: required_string(arguments, "key")?,
        }),
        "browser_cdp" => AgentAction::Browser(BrowserCommand::Cdp {
            method: required_string(arguments, "method")?,
            params: arguments
                .get("params")
                .cloned()
                .unwrap_or_else(|| json!({})),
        }),
        "gallery_add" => AgentAction::Gallery(GalleryCommand::Add {
            source: PathBuf::from(required_string(arguments, "source")?),
        }),
        "gallery_list" => AgentAction::Gallery(GalleryCommand::List),
        "gallery_dir" => AgentAction::Gallery(GalleryCommand::Dir),
        _ => bail!("unknown tool {name}"),
    };
    Ok(AgentCommand { context, action })
}

fn context_from_arguments(
    base: &AgentContext,
    arguments: &Map<String, Value>,
) -> Result<AgentContext> {
    let mut context = base.clone();
    if let Some(value) = optional_string(arguments, "workspace_id")? {
        context.workspace_id =
            Some(Uuid::parse_str(&value).context("workspace_id must be a UUID")?);
    }
    if let Some(value) = optional_string(arguments, "pane_id")? {
        context.pane_id = Some(Uuid::parse_str(&value).context("pane_id must be a UUID")?);
    }
    if let Some(value) = optional_string(arguments, "gallery_dir")? {
        context.gallery_dir = Some(PathBuf::from(value));
    }
    Ok(context)
}

fn required_string(arguments: &Map<String, Value>, name: &str) -> Result<String> {
    optional_string(arguments, name)?.with_context(|| format!("missing {name}"))
}

fn optional_string(arguments: &Map<String, Value>, name: &str) -> Result<Option<String>> {
    arguments
        .get(name)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("{name} must be a string"))
        })
        .transpose()
}

fn optional_bool(arguments: &Map<String, Value>, name: &str) -> Result<Option<bool>> {
    arguments
        .get(name)
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("{name} must be a boolean"))
        })
        .transpose()
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool("browser_list", "List browser panes", json!({})),
        tool(
            "browser_open",
            "Open a browser pane",
            properties(&[
                ("url", "string"),
                ("group", "boolean"),
                ("pane_id", "string"),
                ("workspace_id", "string"),
            ]),
        ),
        tool(
            "browser_goto",
            "Navigate a browser pane and wait for load",
            required_properties(&[("url", "string")], &[("pane_id", "string")]),
        ),
        tool(
            "browser_back",
            "Navigate back",
            properties(&[("pane_id", "string")]),
        ),
        tool(
            "browser_forward",
            "Navigate forward",
            properties(&[("pane_id", "string")]),
        ),
        tool(
            "browser_reload",
            "Reload the page",
            properties(&[("pane_id", "string")]),
        ),
        tool(
            "browser_read",
            "Read visible page or element text",
            properties(&[("selector", "string"), ("pane_id", "string")]),
        ),
        tool(
            "browser_eval",
            "Evaluate JavaScript and return its value",
            required_properties(&[("expression", "string")], &[("pane_id", "string")]),
        ),
        tool(
            "browser_screenshot",
            "Capture a screenshot to a file or gallery",
            properties(&[
                ("output", "string"),
                ("pane_id", "string"),
                ("workspace_id", "string"),
            ]),
        ),
        tool(
            "browser_click",
            "Click the center of a matching element",
            required_properties(&[("selector", "string")], &[("pane_id", "string")]),
        ),
        tool(
            "browser_fill",
            "Replace a form control value",
            required_properties(
                &[("selector", "string"), ("value", "string")],
                &[("pane_id", "string")],
            ),
        ),
        tool(
            "browser_type",
            "Insert text at the browser focus",
            required_properties(&[("text", "string")], &[("pane_id", "string")]),
        ),
        tool(
            "browser_press",
            "Dispatch a browser key press",
            required_properties(&[("key", "string")], &[("pane_id", "string")]),
        ),
        tool_with_schema(
            "browser_cdp",
            "Call a Chrome DevTools Protocol method",
            json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string" },
                    "params": { "type": "object" },
                    "pane_id": { "type": "string" }
                },
                "required": ["method"],
                "additionalProperties": false
            }),
        ),
        tool(
            "gallery_add",
            "Publish an image to the workstation gallery",
            required_properties(
                &[("source", "string")],
                &[("workspace_id", "string"), ("pane_id", "string")],
            ),
        ),
        tool(
            "gallery_list",
            "List workstation gallery images",
            properties(&[("workspace_id", "string"), ("gallery_dir", "string")]),
        ),
        tool(
            "gallery_dir",
            "Return the workstation gallery directory",
            properties(&[("workspace_id", "string"), ("gallery_dir", "string")]),
        ),
    ]
}

fn tool(name: &str, description: &str, properties: Value) -> Value {
    let schema = if properties.get("type").is_some() {
        properties
    } else {
        json!({ "type": "object", "properties": properties, "additionalProperties": false })
    };
    tool_with_schema(name, description, schema)
}

fn tool_with_schema(name: &str, description: &str, input_schema: Value) -> Value {
    let mut tool = Map::new();
    tool.insert("name".into(), Value::String(name.to_owned()));
    tool.insert("description".into(), Value::String(description.to_owned()));
    tool.insert("inputSchema".into(), input_schema);
    Value::Object(tool)
}

fn properties(entries: &[(&str, &str)]) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(name, kind)| ((*name).to_owned(), json!({ "type": kind })))
            .collect(),
    )
}

fn required_properties(required: &[(&str, &str)], optional: &[(&str, &str)]) -> Value {
    let mut entries = required
        .iter()
        .chain(optional.iter())
        .map(|(name, kind)| ((*name).to_owned(), json!({ "type": kind })))
        .collect::<Map<_, _>>();
    let required = required
        .iter()
        .map(|(name, _)| Value::String((*name).to_owned()))
        .collect::<Vec<_>>();
    json!({
        "type": "object",
        "properties": Value::Object(std::mem::take(&mut entries)),
        "required": required,
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_has_stable_unique_names() {
        let definitions = tool_definitions();
        let mut names = definitions
            .iter()
            .map(|tool| tool.get("name").and_then(Value::as_str).unwrap())
            .collect::<Vec<_>>();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
        assert!(names.contains(&"browser_screenshot"));
        assert!(names.contains(&"gallery_add"));
    }

    #[test]
    fn initialize_returns_mcp_server_identity() {
        let response = handle_request(
            &AgentContext::default(),
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
        );
        assert_eq!(
            response.pointer("/result/serverInfo/name"),
            Some(&json!("harness-harlot"))
        );
    }
}
