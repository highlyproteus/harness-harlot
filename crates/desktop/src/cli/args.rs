use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail, ensure};
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
    Bot(BotCommand),
    Browser(BrowserCommand),
    Gallery(GalleryCommand),
    Terminal(TerminalCommand),
    Workstation(WorkstationCommand),
    Mcp,
    Skill(SkillCommand),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BotCommand {
    /// Records the agent session the calling bot pane now shows.
    ReportSession { session: String },
    /// The calling bot pane, its bot (workspace) and the bot's live and active thread panes.
    Info,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TerminalCommand {
    List { mine: bool },
    New(NewTerminal),
    Send { pane: Uuid, input: TerminalInput },
    Read { pane: Uuid, lines: Option<usize> },
    Wait(WaitRequest),
    Focus { pane: Uuid },
    Close { pane: Uuid },
    Rename { tab: Uuid, title: String },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct NewTerminal {
    pub(crate) workstation: Option<Uuid>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) title: Option<String>,
    pub(crate) command: Option<String>,
}

/// Terminal input written in order: text, then each key, then Enter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalInput {
    pub(crate) text: Option<String>,
    pub(crate) keys: Vec<TerminalKey>,
    pub(crate) enter: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalKey {
    Enter,
    CtrlC,
    CtrlD,
    Escape,
    Tab,
    ShiftTab,
    Up,
    Down,
    Left,
    Right,
    Backspace,
    Space,
}

impl TerminalKey {
    pub(crate) const NAMES: [&'static str; 12] = [
        "enter",
        "ctrl-c",
        "ctrl-d",
        "escape",
        "tab",
        "shift-tab",
        "up",
        "down",
        "left",
        "right",
        "backspace",
        "space",
    ];

    pub(crate) fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "enter" => Self::Enter,
            "ctrl-c" => Self::CtrlC,
            "ctrl-d" => Self::CtrlD,
            "escape" => Self::Escape,
            "tab" => Self::Tab,
            "shift-tab" => Self::ShiftTab,
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "backspace" => Self::Backspace,
            "space" => Self::Space,
            _ => bail!(
                "unknown terminal key {name}; expected one of {}",
                Self::NAMES.join(", ")
            ),
        })
    }

    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Enter => b"\r",
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::Escape => b"\x1b",
            Self::Tab => b"\t",
            Self::ShiftTab => b"\x1b[Z",
            Self::Up => b"\x1b[A",
            Self::Down => b"\x1b[B",
            Self::Right => b"\x1b[C",
            Self::Left => b"\x1b[D",
            Self::Backspace => b"\x7f",
            Self::Space => b" ",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WaitRequest {
    pub(crate) pane: Uuid,
    /// `None` waits for `pattern` alone when one is given, otherwise for any condition.
    pub(crate) until: Option<WaitUntil>,
    pub(crate) pattern: Option<String>,
    pub(crate) timeout_ms: u64,
}

pub(crate) const DEFAULT_WAIT_TIMEOUT_MS: u64 = 120_000;
pub(crate) const MAX_WAIT_TIMEOUT_MS: u64 = 600_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitUntil {
    NeedsYou,
    Done,
    Idle,
    Exited,
    Any,
}

impl WaitUntil {
    pub(crate) const NAMES: [&'static str; 5] = ["needs-you", "done", "idle", "exited", "any"];

    pub(crate) fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "needs-you" => Self::NeedsYou,
            "done" => Self::Done,
            "idle" => Self::Idle,
            "exited" => Self::Exited,
            "any" => Self::Any,
            _ => bail!(
                "unknown wait condition {name}; expected one of {}",
                Self::NAMES.join(", ")
            ),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkstationCommand {
    New { cwd: PathBuf, title: Option<String> },
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
        "bot" => AgentAction::Bot(parse_bot(arguments)?),
        "browser" => AgentAction::Browser(parse_browser(arguments)?),
        "gallery" => AgentAction::Gallery(parse_gallery(arguments)?),
        "terminal" => AgentAction::Terminal(parse_terminal(arguments)?),
        "workstation" => AgentAction::Workstation(parse_workstation(arguments)?),
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

fn parse_bot(arguments: &[String]) -> Result<BotCommand> {
    let Some((command, arguments)) = arguments.split_first() else {
        bail!("missing bot command");
    };
    match command.as_str() {
        "report-session" => {
            let options = Options::scan(arguments, &["--session"], &[])?;
            options.positionals::<0>("hh bot report-session --session ID [--pane ID]")?;
            Ok(BotCommand::ReportSession {
                session: options
                    .single("--session")?
                    .context("bot report-session requires --session ID")?
                    .to_owned(),
            })
        }
        "info" => no_arguments(arguments, BotCommand::Info),
        _ => bail!("unknown bot command {command}"),
    }
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

fn parse_terminal(arguments: &[String]) -> Result<TerminalCommand> {
    let Some((command, arguments)) = arguments.split_first() else {
        bail!("missing terminal command");
    };
    match command.as_str() {
        "list" => {
            let options = Options::scan(arguments, &[], &["--mine"])?;
            options.positionals::<0>("hh terminal list [--mine]")?;
            Ok(TerminalCommand::List {
                mine: options.switch("--mine")?,
            })
        }
        "new" => {
            let options = Options::scan(
                arguments,
                &["--workstation", "--cwd", "--title", "--command"],
                &[],
            )?;
            options.positionals::<0>(
                "hh terminal new [--workstation ID] [--cwd DIR] [--title T] [--command CMD]",
            )?;
            Ok(TerminalCommand::New(NewTerminal {
                workstation: options
                    .single("--workstation")?
                    .map(|value| parse_uuid_flag("--workstation", value))
                    .transpose()?,
                cwd: options.single("--cwd")?.map(PathBuf::from),
                title: options.single("--title")?.map(str::to_owned),
                command: options.single("--command")?.map(str::to_owned),
            }))
        }
        "send" => {
            let options = Options::scan(arguments, &["--text", "--key"], &["--enter"])?;
            let [pane] =
                options.positionals("hh terminal send PANE [--text T] [--key K]... [--enter]")?;
            let input = TerminalInput {
                text: options.single("--text")?.map(str::to_owned),
                keys: options
                    .all("--key")
                    .map(TerminalKey::parse)
                    .collect::<Result<_>>()?,
                enter: options.switch("--enter")?,
            };
            ensure!(
                input.text.is_some() || !input.keys.is_empty() || input.enter,
                "terminal send needs --text, --key, or --enter"
            );
            Ok(TerminalCommand::Send {
                pane: parse_uuid_flag("PANE", pane)?,
                input,
            })
        }
        "read" => {
            let options = Options::scan(arguments, &["--lines"], &[])?;
            let [pane] = options.positionals("hh terminal read PANE [--lines N]")?;
            Ok(TerminalCommand::Read {
                pane: parse_uuid_flag("PANE", pane)?,
                lines: options
                    .single("--lines")?
                    .map(|value| value.parse().context("--lines must be a whole number"))
                    .transpose()?,
            })
        }
        "wait" => {
            let options = Options::scan(arguments, &["--until", "--pattern", "--timeout-ms"], &[])?;
            let [pane] = options.positionals(
                "hh terminal wait PANE [--until needs-you|done|idle|exited|any] [--pattern RE] [--timeout-ms MS]",
            )?;
            Ok(TerminalCommand::Wait(WaitRequest {
                pane: parse_uuid_flag("PANE", pane)?,
                until: options
                    .single("--until")?
                    .map(WaitUntil::parse)
                    .transpose()?,
                pattern: options.single("--pattern")?.map(str::to_owned),
                timeout_ms: wait_timeout(
                    options
                        .single("--timeout-ms")?
                        .map(|value| value.parse().context("--timeout-ms must be a whole number"))
                        .transpose()?,
                )?,
            }))
        }
        "focus" => {
            let [pane] =
                Options::scan(arguments, &[], &[])?.positionals("hh terminal focus PANE")?;
            Ok(TerminalCommand::Focus {
                pane: parse_uuid_flag("PANE", pane)?,
            })
        }
        "close" => {
            let [pane] =
                Options::scan(arguments, &[], &[])?.positionals("hh terminal close PANE")?;
            Ok(TerminalCommand::Close {
                pane: parse_uuid_flag("PANE", pane)?,
            })
        }
        "rename" => {
            let [tab, title] =
                Options::scan(arguments, &[], &[])?.positionals("hh terminal rename TAB TITLE")?;
            Ok(TerminalCommand::Rename {
                tab: parse_uuid_flag("TAB", tab)?,
                title: title.to_owned(),
            })
        }
        _ => bail!("unknown terminal command {command}"),
    }
}

pub(crate) fn wait_timeout(timeout_ms: Option<u64>) -> Result<u64> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    ensure!(
        timeout_ms <= MAX_WAIT_TIMEOUT_MS,
        "wait timeout may not exceed {MAX_WAIT_TIMEOUT_MS} ms"
    );
    Ok(timeout_ms)
}

fn parse_workstation(arguments: &[String]) -> Result<WorkstationCommand> {
    let Some((command, arguments)) = arguments.split_first() else {
        bail!("missing workstation command");
    };
    match command.as_str() {
        "new" => {
            let options = Options::scan(arguments, &["--cwd", "--title"], &[])?;
            options.positionals::<0>("hh workstation new --cwd DIR [--title T]")?;
            Ok(WorkstationCommand::New {
                cwd: PathBuf::from(
                    options
                        .single("--cwd")?
                        .context("workstation new requires --cwd DIR")?,
                ),
                title: options.single("--title")?.map(str::to_owned),
            })
        }
        _ => bail!("unknown workstation command {command}"),
    }
}

/// Command-local flags scanned in order: a value flag always consumes the next
/// argument, so values may start with dashes.
struct Options<'a> {
    values: Vec<(&'a str, &'a str)>,
    switches: Vec<&'a str>,
    positionals: Vec<&'a str>,
}

impl<'a> Options<'a> {
    fn scan(arguments: &'a [String], value_flags: &[&str], switch_flags: &[&str]) -> Result<Self> {
        let mut options = Self {
            values: Vec::new(),
            switches: Vec::new(),
            positionals: Vec::new(),
        };
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            let argument = argument.as_str();
            if value_flags.contains(&argument) {
                let value = arguments
                    .next()
                    .with_context(|| format!("missing value for {argument}"))?;
                options.values.push((argument, value));
            } else if switch_flags.contains(&argument) {
                options.switches.push(argument);
            } else if argument.starts_with("--") {
                bail!("unknown option {argument}");
            } else {
                options.positionals.push(argument);
            }
        }
        Ok(options)
    }

    fn single(&self, flag: &str) -> Result<Option<&'a str>> {
        let mut values = self.all(flag);
        let value = values.next();
        ensure!(values.next().is_none(), "{flag} may be specified only once");
        Ok(value)
    }

    fn all(&self, flag: &str) -> impl Iterator<Item = &'a str> {
        self.values
            .iter()
            .filter(move |(name, _)| *name == flag)
            .map(|(_, value)| *value)
    }

    fn switch(&self, flag: &str) -> Result<bool> {
        let count = self.switches.iter().filter(|name| **name == flag).count();
        ensure!(count <= 1, "{flag} may be specified only once");
        Ok(count == 1)
    }

    fn positionals<const N: usize>(&self, usage: &str) -> Result<[&'a str; N]> {
        <[&str; N]>::try_from(self.positionals.as_slice()).map_err(|_| anyhow!("usage: {usage}"))
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

    fn parse(arguments: &[&str]) -> Result<AgentAction> {
        let arguments = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        parse_agent_command(&arguments).map(|command| command.action)
    }

    #[test]
    fn terminal_send_keeps_text_keys_and_enter_in_order() {
        let pane = Uuid::new_v4();
        let pane_arg = pane.to_string();
        assert_eq!(
            parse(&[
                "terminal", "send", &pane_arg, "--key", "down", "--text", "--yes", "--key",
                "ctrl-c", "--enter", "--json",
            ])
            .unwrap(),
            AgentAction::Terminal(TerminalCommand::Send {
                pane,
                input: TerminalInput {
                    text: Some("--yes".into()),
                    keys: vec![TerminalKey::Down, TerminalKey::CtrlC],
                    enter: true,
                },
            })
        );
        assert!(parse(&["terminal", "send", &pane_arg]).is_err());
        assert!(parse(&["terminal", "send", &pane_arg, "--key", "f13"]).is_err());
        assert!(parse(&["terminal", "send", "not-a-pane", "--enter"]).is_err());
        assert!(parse(&["terminal", "send", &pane_arg, "--text", "a", "--text", "b"]).is_err());
    }

    #[test]
    fn terminal_wait_defaults_and_bounds() {
        let pane = Uuid::new_v4();
        let pane_arg = pane.to_string();
        assert_eq!(
            parse(&["terminal", "wait", &pane_arg]).unwrap(),
            AgentAction::Terminal(TerminalCommand::Wait(WaitRequest {
                pane,
                until: None,
                pattern: None,
                timeout_ms: DEFAULT_WAIT_TIMEOUT_MS,
            }))
        );
        assert_eq!(
            parse(&[
                "terminal",
                "wait",
                &pane_arg,
                "--until",
                "needs-you",
                "--pattern",
                "y/N",
                "--timeout-ms",
                "5000",
            ])
            .unwrap(),
            AgentAction::Terminal(TerminalCommand::Wait(WaitRequest {
                pane,
                until: Some(WaitUntil::NeedsYou),
                pattern: Some("y/N".into()),
                timeout_ms: 5_000,
            }))
        );
        assert!(parse(&["terminal", "wait", &pane_arg, "--until", "later"]).is_err());
        assert!(parse(&["terminal", "wait", &pane_arg, "--timeout-ms", "600001"]).is_err());
    }

    #[test]
    fn terminal_and_workstation_commands_parse_their_options() {
        let id = Uuid::new_v4();
        let id_arg = id.to_string();
        assert_eq!(
            parse(&["terminal", "list", "--mine"]).unwrap(),
            AgentAction::Terminal(TerminalCommand::List { mine: true })
        );
        assert_eq!(
            parse(&[
                "terminal",
                "new",
                "--workstation",
                &id_arg,
                "--cwd",
                "/tmp",
                "--title",
                "api",
                "--command",
                "omp \"fix it\"",
            ])
            .unwrap(),
            AgentAction::Terminal(TerminalCommand::New(NewTerminal {
                workstation: Some(id),
                cwd: Some(PathBuf::from("/tmp")),
                title: Some("api".into()),
                command: Some("omp \"fix it\"".into()),
            }))
        );
        assert_eq!(
            parse(&["terminal", "read", &id_arg, "--lines", "40"]).unwrap(),
            AgentAction::Terminal(TerminalCommand::Read {
                pane: id,
                lines: Some(40),
            })
        );
        assert_eq!(
            parse(&["terminal", "rename", &id_arg, "API worker"]).unwrap(),
            AgentAction::Terminal(TerminalCommand::Rename {
                tab: id,
                title: "API worker".into(),
            })
        );
        assert_eq!(
            parse(&["workstation", "new", "--cwd", "/srv/app"]).unwrap(),
            AgentAction::Workstation(WorkstationCommand::New {
                cwd: PathBuf::from("/srv/app"),
                title: None,
            })
        );
        assert!(parse(&["workstation", "new"]).is_err());
        assert_eq!(
            parse(&["bot", "report-session", "--session", "0193-abc"]).unwrap(),
            AgentAction::Bot(BotCommand::ReportSession {
                session: "0193-abc".to_owned(),
            })
        );
        assert!(parse(&["bot", "report-session"]).is_err());
        assert_eq!(
            parse(&["bot", "info"]).unwrap(),
            AgentAction::Bot(BotCommand::Info)
        );
        assert!(parse(&["terminal", "list", "--all"]).is_err());
        assert!(parse(&["terminal", "focus"]).is_err());
    }
}
