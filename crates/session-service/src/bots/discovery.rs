use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use hh_protocol::{CodingAgent, TERMINAL_PROFILE_REGISTRY, TerminalProfile};

use crate::process::{configured_shell, is_trusted_executable_file, run_bounded_command};

const CODING_AGENT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
/// Brackets the probed `PATH`, so anything the user's shell startup files
/// print cannot corrupt it.
const PATH_MARKER: &str = "__HH_AGENT_PATH__";
/// Per-user directories agent installers put their commands in, searched
/// after the shell's `PATH` in case startup files only set it up for
/// terminals attached to a TTY.
const USER_AGENT_DIRECTORIES: [&str; 8] = [
    ".local/bin",
    ".bun/bin",
    ".npm-global/bin",
    ".opencode/bin",
    ".grok/bin",
    ".amp/bin",
    ".cargo/bin",
    ".claude/local",
];
const SYSTEM_AGENT_DIRECTORIES: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// Every registry command that can host a coding agent, in registry order.
/// tmux is a multiplexer, not a coding agent, and is excluded.
fn coding_agent_candidates() -> Vec<(TerminalProfile, &'static str)> {
    TERMINAL_PROFILE_REGISTRY
        .iter()
        .filter(|definition| definition.profile != TerminalProfile::Tmux)
        .flat_map(|definition| {
            definition
                .commands
                .iter()
                .map(move |command| (definition.profile, *command))
        })
        .collect()
}

/// The `PATH` a terminal shell gets. Terminals run interactive shells, and
/// most agent installers add their directory in the interactive startup file
/// (`~/.zshrc`, `~/.bashrc`), which a login-only shell never reads. Falls back
/// to a login-only shell when the interactive one fails or times out.
fn shell_path() -> Result<OsString> {
    shell_path_with(&["-ilc"]).or_else(|_| shell_path_with(&["-lc"]))
}

fn shell_path_with(flags: &[&str]) -> Result<OsString> {
    let shell = configured_shell();
    let script = if shell.rsplit('/').next() == Some("fish") {
        format!("printf '{PATH_MARKER}%s{PATH_MARKER}' (string join : $PATH)")
    } else {
        format!("printf '{PATH_MARKER}%s{PATH_MARKER}' \"$PATH\"")
    };
    let mut command = Command::new(shell);
    command
        .args(flags)
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded_command(
        command,
        CODING_AGENT_DISCOVERY_TIMEOUT,
        "shell PATH discovery",
    )?;
    marked_path(&output.stdout)
        .map(OsString::from)
        .context("shell printed no PATH")
}

fn marked_path(stdout: &str) -> Option<&str> {
    let (_, rest) = stdout.split_once(PATH_MARKER)?;
    let (path, _) = rest.split_once(PATH_MARKER)?;
    Some(path)
}

/// The shell's `PATH` directories, then the known agent install directories
/// it lacks, each once and in order.
fn search_directories(shell_path: &OsString, home: Option<&Path>) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let known = home
        .iter()
        .flat_map(|home| USER_AGENT_DIRECTORIES.map(|directory| home.join(directory)))
        .chain(SYSTEM_AGENT_DIRECTORIES.map(PathBuf::from));
    for directory in std::env::split_paths(shell_path).chain(known) {
        if directory.is_absolute() && !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    directories
}

/// Resolves installed coding agent CLIs where a terminal shell would find
/// them. An empty result is valid: it means no supported agent is installed.
/// Each call re-reads the shell configuration, so Rescan picks up a newly
/// installed agent or `PATH` change; the registry caches the result.
pub(crate) fn discover_coding_agents() -> Vec<CodingAgent> {
    let shell_path = shell_path().unwrap_or_else(|error| {
        eprintln!("coding agent discovery could not read the shell PATH: {error:#}");
        OsString::new()
    });
    let directories = search_directories(&shell_path, std::env::home_dir().as_deref());
    let mut agents: Vec<CodingAgent> = Vec::new();
    for (profile, name) in coding_agent_candidates() {
        if agents.iter().any(|agent| agent.profile == profile) {
            continue;
        }
        let resolved = directories
            .iter()
            .map(|directory| directory.join(name))
            .filter_map(|candidate| candidate.canonicalize().ok())
            .find(|candidate| is_trusted_executable_file(candidate));
        if let Some(path) = resolved {
            agents.push(CodingAgent {
                profile,
                command: name.to_owned(),
                path: path.to_string_lossy().into_owned(),
            });
        }
    }
    agents
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_agent_candidates_skip_tmux_and_keep_registry_order() {
        let candidates = coding_agent_candidates();
        assert_eq!(
            candidates.first(),
            Some(&(TerminalProfile::Hermes, "hermes"))
        );
        assert!(
            candidates
                .iter()
                .all(|(profile, _)| *profile != TerminalProfile::Tmux)
        );
        let position = |needle: TerminalProfile| {
            candidates
                .iter()
                .position(|(profile, _)| *profile == needle)
                .expect("profile is a discovery candidate")
        };
        assert!(position(TerminalProfile::Omp) < position(TerminalProfile::Codex));
    }

    #[test]
    fn shell_startup_output_around_the_path_is_ignored() {
        let noisy = "nvm: prefix warning\n__HH_AGENT_PATH__/a:/b__HH_AGENT_PATH__\nbye";
        assert_eq!(marked_path(noisy), Some("/a:/b"));
        assert_eq!(marked_path("no marker"), None);
        assert_eq!(marked_path("__HH_AGENT_PATH__/a unterminated"), None);
    }

    #[test]
    fn search_keeps_shell_order_then_adds_missing_install_directories() {
        let home = PathBuf::from("/Users/me");
        let shell = OsString::from("/opt/homebrew/bin:relative:/Users/me/.bun/bin:/usr/bin");
        let directories = search_directories(&shell, Some(&home));
        assert_eq!(
            directories[..3],
            [
                PathBuf::from("/opt/homebrew/bin"),
                home.join(".bun/bin"),
                PathBuf::from("/usr/bin")
            ]
        );
        assert!(directories.contains(&home.join(".local/bin")));
        assert!(!directories.iter().any(|directory| directory.is_relative()));
        let unique = directories.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), directories.len());
    }
}
