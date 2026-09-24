use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentContext {
    pub(crate) workspace_id: Option<Uuid>,
    pub(crate) pane_id: Option<Uuid>,
    pub(crate) gallery_dir: Option<PathBuf>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentCommand {
    pub(crate) context: AgentContext,
    pub(crate) action: AgentAction,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AgentAction {
    Browser(BrowserCommand),
    Gallery(GalleryCommand),
    Mcp,
    Skill(SkillCommand),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BrowserCommand {
    List,
    Open { url: Option<String>, group: bool },
    Goto { url: String },
    Back,
    Forward,
    Reload,
    Read { selector: Option<String> },
    Eval { expression: String },
    Screenshot { output: Option<PathBuf> },
    Click { selector: String },
    Fill { selector: String, value: String },
    Type { text: String },
    Press { key: String },
    Cdp { method: String, params: Value },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GalleryCommand {
    Add { source: PathBuf },
    List,
    Dir,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkillCommand {
    Install,
    Path,
}

pub(crate) fn parse_agent_command(arguments: &[String]) -> Result<AgentCommand> {
    let (context, arguments) = parse_context(arguments)?;
    let Some((surface, arguments)) = arguments.split_first() else {
        bail!("missing agent command");
    };
    let action = match surface.as_str() {
        "browser" => AgentAction::Browser(parse_browser(arguments)?),
        "gallery" => AgentAction::Gallery(parse_gallery(arguments)?),
        "mcp" => {
            ensure!(arguments.is_empty(), "mcp does not accept arguments");
            AgentAction::Mcp
        }
        "skill" => AgentAction::Skill(parse_skill(arguments)?),
        _ => bail!("unknown agent command {surface}"),
    };
    Ok(AgentCommand { context, action })
}

fn parse_context(arguments: &[String]) -> Result<(AgentContext, Vec<String>)> {
    let mut context = AgentContext {
        workspace_id: parse_env_uuid(hh_protocol::WORKSPACE_ID_ENV)?,
        pane_id: parse_env_uuid(hh_protocol::PANE_ID_ENV)?,
        gallery_dir: std::env::var_os(hh_protocol::GALLERY_DIR_ENV).map(PathBuf::from),
        json: false,
    };
    let mut positional = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--workspace" => {
                index += 1;
                context.workspace_id = Some(parse_uuid_flag(
                    "--workspace",
                    arguments
                        .get(index)
                        .context("missing value for --workspace")?,
                )?);
            }
            "--pane" => {
                index += 1;
                context.pane_id = Some(parse_uuid_flag(
                    "--pane",
                    arguments.get(index).context("missing value for --pane")?,
                )?);
            }
            "--gallery" => {
                index += 1;
                context.gallery_dir = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("missing value for --gallery")?,
                ));
            }
            "--json" => context.json = true,
            argument => positional.push(argument.to_owned()),
        }
        index += 1;
    }
    Ok((context, positional))
}

fn parse_env_uuid(name: &str) -> Result<Option<Uuid>> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{name} is not valid UTF-8"))?;
    Uuid::parse_str(&value)
        .with_context(|| format!("{name} is not a UUID"))
        .map(Some)
}

fn parse_uuid_flag(flag: &str, value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("{flag} must be a UUID"))
}

fn parse_browser(arguments: &[String]) -> Result<BrowserCommand> {
    let Some((command, arguments)) = arguments.split_first() else {
        bail!("missing browser command");
    };
    match command.as_str() {
        "list" => no_arguments(arguments, BrowserCommand::List),
        "open" => {
            let mut group = false;
            let mut url = None;
            for argument in arguments {
                if argument == "--group" {
                    ensure!(!group, "--group may be specified only once");
                    group = true;
                } else {
                    ensure!(url.is_none(), "browser open accepts at most one URL");
                    url = Some(argument.clone());
                }
            }
            Ok(BrowserCommand::Open { url, group })
        }
        "goto" => one_argument(arguments, "URL").map(|url| BrowserCommand::Goto {
            url: url.to_owned(),
        }),
        "back" => no_arguments(arguments, BrowserCommand::Back),
        "forward" => no_arguments(arguments, BrowserCommand::Forward),
        "reload" => no_arguments(arguments, BrowserCommand::Reload),
        "read" => {
            ensure!(
                arguments.len() <= 1,
                "browser read accepts at most one selector"
            );
            Ok(BrowserCommand::Read {
                selector: arguments.first().cloned(),
            })
        }
        "eval" => one_argument(arguments, "JavaScript expression").map(|expression| {
            BrowserCommand::Eval {
                expression: expression.to_owned(),
            }
        }),
        "screenshot" => match arguments {
            [] => Ok(BrowserCommand::Screenshot { output: None }),
            [flag, path] if flag == "--out" => Ok(BrowserCommand::Screenshot {
                output: Some(PathBuf::from(path)),
            }),
            _ => bail!("usage: hh browser screenshot [--out PATH]"),
        },
        "click" => one_argument(arguments, "selector").map(|selector| BrowserCommand::Click {
            selector: selector.to_owned(),
        }),
        "fill" => match arguments {
            [selector, value] => Ok(BrowserCommand::Fill {
                selector: selector.clone(),
                value: value.clone(),
            }),
            _ => bail!("usage: hh browser fill SELECTOR VALUE"),
        },
        "type" => one_argument(arguments, "text").map(|text| BrowserCommand::Type {
            text: text.to_owned(),
        }),
        "press" => one_argument(arguments, "key").map(|key| BrowserCommand::Press {
            key: key.to_owned(),
        }),
        "cdp" => match arguments {
            [method] => Ok(BrowserCommand::Cdp {
                method: method.clone(),
                params: serde_json::json!({}),
            }),
            [method, params] => Ok(BrowserCommand::Cdp {
                method: method.clone(),
                params: serde_json::from_str(params).context("parse CDP params JSON")?,
            }),
            _ => bail!("usage: hh browser cdp METHOD [PARAMS_JSON]"),
        },
        _ => bail!("unknown browser command {command}"),
    }
}

fn parse_gallery(arguments: &[String]) -> Result<GalleryCommand> {
    let Some((command, arguments)) = arguments.split_first() else {
        bail!("missing gallery command");
    };
    match command.as_str() {
        "add" => one_argument(arguments, "image path").map(|source| GalleryCommand::Add {
            source: PathBuf::from(source),
        }),
        "list" => no_arguments(arguments, GalleryCommand::List),
        "dir" => no_arguments(arguments, GalleryCommand::Dir),
        _ => bail!("unknown gallery command {command}"),
    }
}

fn parse_skill(arguments: &[String]) -> Result<SkillCommand> {
    match arguments {
        [command] if command == "install" => Ok(SkillCommand::Install),
        [command] if command == "path" => Ok(SkillCommand::Path),
        _ => bail!("usage: hh skill install|path"),
    }
}

fn no_arguments<T>(arguments: &[String], value: T) -> Result<T> {
    ensure!(arguments.is_empty(), "unexpected command arguments");
    Ok(value)
}

fn one_argument<'a>(arguments: &'a [String], label: &str) -> Result<&'a str> {
    match arguments {
        [value] => Ok(value),
        _ => bail!("expected exactly one {label}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_commands_parse_global_context_anywhere() {
        let workspace = Uuid::new_v4();
        let pane = Uuid::new_v4();
        let command = parse_agent_command(&[
            "browser".into(),
            "fill".into(),
            "#name".into(),
            "Ada".into(),
            "--workspace".into(),
            workspace.to_string(),
            "--pane".into(),
            pane.to_string(),
            "--json".into(),
        ])
        .unwrap();
        assert_eq!(command.context.workspace_id, Some(workspace));
        assert_eq!(command.context.pane_id, Some(pane));
        assert!(command.context.json);
        assert_eq!(
            command.action,
            AgentAction::Browser(BrowserCommand::Fill {
                selector: "#name".into(),
                value: "Ada".into(),
            })
        );
    }

    #[test]
    fn cdp_params_must_be_json() {
        assert!(
            parse_agent_command(&[
                "browser".into(),
                "cdp".into(),
                "Page.enable".into(),
                "{".into()
            ])
            .is_err()
        );
    }
}
