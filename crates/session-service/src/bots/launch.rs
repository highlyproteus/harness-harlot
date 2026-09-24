//! Launch command lines typed into bot terminals.
//!
//! Each agent's own interface runs in the bot's shell. Agents that support it
//! get the coordinator prompt and the Harness Harlot tools wired in on the
//! command line; every other agent starts plain and relies on the skill.
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hh_protocol::{BotSpec, CodingAgent, TerminalProfile};
use uuid::Uuid;

const COORDINATOR_PROMPT: &str = include_str!("../../bundled/bot-prompt.md");
const OMP_EXTENSION: &[u8] = include_bytes!("../../bundled/hh-omp.ts");
const OMP_EXTENSION_FILE: &str = "hh-omp.ts";
const MCP_SERVER_NAME: &str = "harness-harlot";
const MAX_BUNDLED_FILE_BYTES: u64 = 1024 * 1024;

/// The bot identity a launch command is built for.
pub(crate) struct BotLaunch<'a> {
    pub(crate) tab_id: Uuid,
    pub(crate) name: &'a str,
    pub(crate) spec: &'a BotSpec,
}

/// Directory holding generated bot launch files: `<state>/bots`.
pub(crate) fn bots_directory() -> Result<PathBuf> {
    Ok(hh_protocol::state_directory()
        .context("state directory is unavailable")?
        .join("bots"))
}

/// Builds the shell command line that starts the bot's agent, writing the
/// prompt, extension and MCP files it references into `bots_dir`.
/// `hh_cli` is the `hh` executable that serves the Harness Harlot MCP tools.
pub(crate) fn launch_command(
    bot: &BotLaunch<'_>,
    agents: &[CodingAgent],
    bots_dir: &Path,
    hh_cli: Option<&Path>,
) -> Result<String> {
    let agent = bot.spec.agent;
    if matches!(agent, TerminalProfile::Terminal | TerminalProfile::Tmux) {
        bail!("{} is not a coding agent", agent.display_name());
    }
    let executable = agents
        .iter()
        .find(|candidate| candidate.profile == agent)
        .with_context(|| format!("{} is not installed", agent.display_name()))?;
    let mut argv = vec![executable.path.clone()];
    match agent {
        TerminalProfile::Omp => {
            prepare_directory(bots_dir)?;
            let extension = bots_dir.join(OMP_EXTENSION_FILE);
            write_if_changed(&extension, OMP_EXTENSION)?;
            let prompt = write_prompt(bot, bots_dir)?;
            argv.extend([
                "-e".to_owned(),
                utf8_path(&extension)?,
                "--append-system-prompt".to_owned(),
                utf8_path(&prompt)?,
            ]);
        }
        TerminalProfile::Claude => {
            prepare_directory(bots_dir)?;
            let prompt = write_prompt(bot, bots_dir)?;
            argv.extend([
                "--append-system-prompt-file".to_owned(),
                utf8_path(&prompt)?,
            ]);
            if let Some(hh_cli) = hh_cli {
                let config = bots_dir.join(format!("{}.mcp.json", bot.tab_id));
                let json = serde_json::json!({
                    "mcpServers": {
                        MCP_SERVER_NAME: { "command": utf8_path(hh_cli)?, "args": ["mcp"] }
                    }
                });
                let bytes = serde_json::to_vec(&json).context("encode bot MCP config")?;
                write_if_changed(&config, &bytes)?;
                argv.extend(["--mcp-config".to_owned(), utf8_path(&config)?]);
            }
        }
        TerminalProfile::Codex => {
            prepare_directory(bots_dir)?;
            let prompt = write_prompt(bot, bots_dir)?;
            if let Some(hh_cli) = hh_cli {
                argv.extend([
                    "-c".to_owned(),
                    format!(
                        "mcp_servers.{MCP_SERVER_NAME}.command={}",
                        toml_string(&utf8_path(hh_cli)?)?
                    ),
                    "-c".to_owned(),
                    format!("mcp_servers.{MCP_SERVER_NAME}.args=[\"mcp\"]"),
                ]);
            }
            // The full prompt stays in a file: a multi-line argument typed
            // into an interactive shell is fragile, so point Codex at it.
            let instructions = format!(
                "You are \"{}\", a Harness Harlot bot. Before replying to anything, read {} and follow it for this whole session.",
                bot.name,
                utf8_path(&prompt)?
            );
            argv.extend([
                "-c".to_owned(),
                format!("developer_instructions={}", toml_string(&instructions)?),
            ]);
        }
        _ => {}
    }
    Ok(argv
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Removes the launch files generated for a deleted bot.
pub(crate) fn remove_bot_files(bots_dir: &Path, tab_id: Uuid) {
    for suffix in ["prompt.md", "mcp.json"] {
        let _ = std::fs::remove_file(bots_dir.join(format!("{tab_id}.{suffix}")));
    }
}

fn coordinator_prompt(bot: &BotLaunch<'_>) -> String {
    let mut prompt = COORDINATOR_PROMPT.trim_end().to_owned();
    prompt.push_str("\n\n## Your identity\nYour name is \"");
    prompt.push_str(bot.name);
    prompt.push_str("\". The user may address you by it.\n");
    if let Some(instructions) = bot.spec.instructions.as_deref() {
        prompt.push_str("\n## Standing instructions from the user\n");
        prompt.push_str(instructions.trim());
        prompt.push('\n');
    }
    prompt
}

fn write_prompt(bot: &BotLaunch<'_>, bots_dir: &Path) -> Result<PathBuf> {
    let path = bots_dir.join(format!("{}.prompt.md", bot.tab_id));
    hh_protocol::atomic_write_private(&path, coordinator_prompt(bot).as_bytes())
        .with_context(|| format!("write bot prompt {}", path.display()))?;
    Ok(path)
}

fn prepare_directory(directory: &Path) -> Result<()> {
    hh_protocol::ensure_private_directory(directory)
        .with_context(|| format!("prepare bot directory {}", directory.display()))
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    let unchanged = match hh_protocol::read_private_file(path, MAX_BUNDLED_FILE_BYTES) {
        Ok(existing) => existing == bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if !unchanged {
        hh_protocol::atomic_write_private(path, bytes)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn utf8_path(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}

/// A TOML basic string: JSON string escaping is a subset of TOML's.
fn toml_string(value: &str) -> Result<String> {
    serde_json::to_string(value).context("encode TOML string")
}

/// POSIX single-quoting, left bare when every byte is shell-safe. The
/// `'\''` escape also works in fish.
fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&byte));
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents() -> Vec<CodingAgent> {
        [
            (TerminalProfile::Omp, "/opt/bin/omp"),
            (TerminalProfile::Claude, "/opt/bin/claude"),
            (TerminalProfile::Codex, "/opt/bin/codex"),
            (TerminalProfile::Hermes, "/opt/my tools/hermes"),
        ]
        .into_iter()
        .map(|(profile, path)| CodingAgent {
            profile,
            command: path.rsplit('/').next().unwrap().to_owned(),
            path: path.to_owned(),
        })
        .collect()
    }

    fn bots_dir() -> PathBuf {
        std::env::temp_dir().join(format!("hh bots {}", Uuid::new_v4()))
    }

    fn command_for(agent: TerminalProfile, bots_dir: &Path, hh_cli: Option<&Path>) -> String {
        let spec = BotSpec {
            agent,
            instructions: Some("Prefer small PRs.".to_owned()),
        };
        let bot = BotLaunch {
            tab_id: Uuid::nil(),
            name: "Hive3",
            spec: &spec,
        };
        launch_command(&bot, &agents(), bots_dir, hh_cli).unwrap()
    }

    #[test]
    fn omp_loads_the_extension_and_appends_the_prompt_file() {
        let directory = bots_dir();
        let command = command_for(TerminalProfile::Omp, &directory, None);
        let extension = directory.join(OMP_EXTENSION_FILE);
        let prompt = directory.join(format!("{}.prompt.md", Uuid::nil()));
        assert_eq!(
            command,
            format!(
                "/opt/bin/omp -e '{}' --append-system-prompt '{}'",
                extension.display(),
                prompt.display()
            )
        );
        assert_eq!(std::fs::read(&extension).unwrap(), OMP_EXTENSION);
        let prompt = std::fs::read_to_string(prompt).unwrap();
        assert!(prompt.starts_with(COORDINATOR_PROMPT.trim_end()));
        assert!(prompt.contains("Your name is \"Hive3\""));
        assert!(prompt.ends_with("Prefer small PRs.\n"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn claude_gets_the_prompt_file_and_the_harness_harlot_mcp_server() {
        let directory = bots_dir();
        let hh = Path::new("/Applications/Harness Harlot.app/Contents/MacOS/hh");
        let command = command_for(TerminalProfile::Claude, &directory, Some(hh));
        let prompt = directory.join(format!("{}.prompt.md", Uuid::nil()));
        let config = directory.join(format!("{}.mcp.json", Uuid::nil()));
        assert_eq!(
            command,
            format!(
                "/opt/bin/claude --append-system-prompt-file '{}' --mcp-config '{}'",
                prompt.display(),
                config.display()
            )
        );
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
        assert_eq!(
            config,
            serde_json::json!({
                "mcpServers": {"harness-harlot": {"command": hh.to_str().unwrap(), "args": ["mcp"]}}
            })
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn codex_wires_the_mcp_server_and_points_its_instructions_at_the_prompt() {
        let directory = bots_dir();
        let command = command_for(
            TerminalProfile::Codex,
            &directory,
            Some(Path::new("/opt/hh")),
        );
        let prompt = directory.join(format!("{}.prompt.md", Uuid::nil()));
        assert_eq!(
            command,
            format!(
                "/opt/bin/codex -c 'mcp_servers.harness-harlot.command=\"/opt/hh\"' -c 'mcp_servers.harness-harlot.args=[\"mcp\"]' -c 'developer_instructions=\"You are \\\"Hive3\\\", a Harness Harlot bot. Before replying to anything, read {} and follow it for this whole session.\"'",
                prompt.display()
            )
        );
        assert!(prompt.is_file());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn other_agents_start_plain_without_writing_files() {
        let directory = bots_dir();
        let command = command_for(
            TerminalProfile::Hermes,
            &directory,
            Some(Path::new("/opt/hh")),
        );
        assert_eq!(command, "'/opt/my tools/hermes'");
        assert!(!directory.exists());
    }

    #[test]
    fn missing_and_non_agent_profiles_are_rejected() {
        let spec = BotSpec {
            agent: TerminalProfile::Gemini,
            instructions: None,
        };
        let bot = BotLaunch {
            tab_id: Uuid::nil(),
            name: "Hive3",
            spec: &spec,
        };
        let error = launch_command(&bot, &agents(), &bots_dir(), None).unwrap_err();
        assert_eq!(error.to_string(), "Gemini CLI is not installed");
        let spec = BotSpec {
            agent: TerminalProfile::Terminal,
            instructions: None,
        };
        let bot = BotLaunch { spec: &spec, ..bot };
        let error = launch_command(&bot, &agents(), &bots_dir(), None).unwrap_err();
        assert_eq!(error.to_string(), "Terminal is not a coding agent");
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(shell_quote("/usr/bin/omp"), "/usr/bin/omp");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
